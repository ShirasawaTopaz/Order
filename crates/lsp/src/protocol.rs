use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::ChildStdin,
};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::types::{
    DiagnosticItem, DiagnosticSeverity, LspCompletionItem, LspSemanticToken, LspTextEdit,
};

/// 从 LSP 输出流读取下一条 JSON-RPC 消息。
///
/// 返回 `Ok(None)` 表示流结束，调用方应将其视为语言服务器退出。
pub fn read_next_message(reader: &mut BufReader<impl Read>) -> Result<Option<Value>> {
    let mut content_length: usize = 0;
    let mut header_line = String::new();

    loop {
        header_line.clear();
        let read_size = reader
            .read_line(&mut header_line)
            .context("读取 LSP 消息头失败")?;
        if read_size == 0 {
            return Ok(None);
        }

        let trimmed = header_line.trim();
        if trimmed.is_empty() {
            break;
        }

        let lower = trimmed.to_ascii_lowercase();
        if let Some(length_text) = lower.strip_prefix("content-length:") {
            content_length = length_text.trim().parse::<usize>().unwrap_or(0);
        }
    }

    if content_length == 0 {
        // 某些服务端实现会发送不带 `Content-Length` 的杂项输出，
        // 这里返回空 JSON 作为“可忽略消息”，避免被误判为进程退出。
        return Ok(Some(Value::Null));
    }

    let mut payload = vec![0u8; content_length];
    reader
        .read_exact(&mut payload)
        .context("读取 LSP 消息体失败")?;
    let json_value = serde_json::from_slice::<Value>(&payload).context("解析 LSP JSON 失败")?;
    Ok(Some(json_value))
}

/// 将 JSON-RPC 消息写入 LSP 输入流。
pub fn send_message(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let payload = serde_json::to_vec(value).context("序列化 LSP 消息失败")?;
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());

    stdin
        .write_all(header.as_bytes())
        .context("写入 LSP 消息头失败")?;
    stdin.write_all(&payload).context("写入 LSP 消息体失败")?;
    stdin.flush().context("刷新 LSP 输出流失败")
}

/// 判断消息是否为 `publishDiagnostics` 通知。
pub fn is_publish_diagnostics(value: &Value) -> bool {
    value
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| method == "textDocument/publishDiagnostics")
}

/// 解析 `publishDiagnostics`。
pub fn parse_publish_diagnostics(value: &Value) -> (Option<PathBuf>, Vec<DiagnosticItem>) {
    let params = value.get("params").and_then(Value::as_object);
    let file_path = params
        .and_then(|map| map.get("uri"))
        .and_then(Value::as_str)
        .and_then(file_uri_to_path);
    let diagnostics = params
        .and_then(|map| map.get("diagnostics"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut items = Vec::new();
    for diagnostic in diagnostics {
        let message = diagnostic
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("<no message>")
            .to_string();

        let severity = diagnostic
            .get("severity")
            .and_then(Value::as_u64)
            .map(DiagnosticSeverity::from_lsp_number)
            .unwrap_or(DiagnosticSeverity::Warning);

        let range = diagnostic.get("range").and_then(Value::as_object);
        let start = range
            .and_then(|map| map.get("start"))
            .and_then(Value::as_object);
        let line = start
            .and_then(|map| map.get("line"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let column = start
            .and_then(|map| map.get("character"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);

        let file_path = file_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("<unknown>"));
        items.push(DiagnosticItem {
            file_path,
            line,
            column,
            severity,
            message,
        });
    }

    (file_path, items)
}

/// 解析 `textDocument/completion` 响应。
pub fn parse_completion_items_from_response(value: &Value) -> Vec<LspCompletionItem> {
    let mut items = Vec::new();
    let Some(result) = value.get("result") else {
        return items;
    };

    let raw_items: Vec<Value> = if let Some(array) = result.as_array() {
        array.clone()
    } else {
        result
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };

    for item in raw_items {
        let label = item
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if label.is_empty() {
            continue;
        }

        let insert_text = item
            .get("insertText")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let detail = item
            .get("detail")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        items.push(LspCompletionItem {
            label,
            insert_text,
            detail,
        });
    }

    items
}

/// 解析 `textDocument/semanticTokens/full` 响应。
pub fn parse_semantic_tokens_from_response(
    value: &Value,
    token_types: &[String],
    token_modifiers: &[String],
) -> Vec<LspSemanticToken> {
    let data = value
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| result.get("data"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if data.is_empty() {
        return Vec::new();
    }

    let mut tokens = Vec::new();
    let mut line = 0usize;
    let mut start = 0usize;
    let mut index = 0usize;
    while index + 4 < data.len() {
        let delta_line = data[index]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let delta_start = data[index + 1]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let length = data[index + 2]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let token_type_index = data[index + 3]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let modifier_bits = data[index + 4].as_u64().unwrap_or(0);

        if delta_line == 0 {
            start = start.saturating_add(delta_start);
        } else {
            line = line.saturating_add(delta_line);
            start = delta_start;
        }

        let token_type = token_types
            .get(token_type_index)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        let mut modifiers = Vec::new();
        for (modifier_index, modifier_name) in token_modifiers.iter().enumerate() {
            if modifier_bits & (1 << modifier_index) != 0 {
                modifiers.push(modifier_name.clone());
            }
        }

        tokens.push(LspSemanticToken {
            line,
            start,
            length,
            token_type,
            token_modifiers: modifiers,
        });

        index += 5;
    }

    tokens
}

/// 从 `initialize` 响应中解析服务端语义 token legend。
///
/// LSP 规范中语义 token 的 type/modifier 索引由“服务端 legend”定义，
/// 因此客户端必须以服务端返回为准，不能仅使用本地默认表。
pub fn parse_semantic_legend_from_initialize_response(
    value: &Value,
) -> Option<(Vec<String>, Vec<String>)> {
    let capabilities = value
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| result.get("capabilities"))
        .and_then(Value::as_object)?;

    let legend = capabilities
        .get("semanticTokensProvider")
        .and_then(Value::as_object)
        .and_then(|provider| provider.get("legend"))
        .and_then(Value::as_object)?;

    let token_types = legend
        .get("tokenTypes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let token_modifiers = legend
        .get("tokenModifiers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if token_types.is_empty() {
        return None;
    }

    Some((token_types, token_modifiers))
}

/// 解析 `textDocument/willSaveWaitUntil` 响应中的 text edits。
pub fn parse_text_edits_from_response(value: &Value) -> Vec<LspTextEdit> {
    let Some(items) = value.get("result").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut edits = Vec::new();
    for item in items {
        let range = item.get("range").and_then(Value::as_object);
        let start = range
            .and_then(|map| map.get("start"))
            .and_then(Value::as_object);
        let end = range
            .and_then(|map| map.get("end"))
            .and_then(Value::as_object);

        let start_line = start
            .and_then(|map| map.get("line"))
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let start_character = start
            .and_then(|map| map.get("character"))
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let end_line = end
            .and_then(|map| map.get("line"))
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let end_character = end
            .and_then(|map| map.get("character"))
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let new_text = item
            .get("newText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        edits.push(LspTextEdit {
            start_line,
            start_character,
            end_line,
            end_character,
            new_text,
        });
    }

    edits
}

/// 将本地路径转换为 `file://` URI。
pub fn path_to_file_uri(path: &Path) -> Result<String> {
    let absolute = path.canonicalize().context("规范化路径失败")?;
    let display = absolute.to_string_lossy().replace('\\', "/");

    if display.chars().nth(1) == Some(':') {
        Ok(format!("file:///{}", display))
    } else {
        Ok(format!("file://{}", display))
    }
}

/// 将 `file://` URI 转换回本地路径。
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    if !uri.starts_with("file://") {
        return None;
    }
    let mut path = uri.trim_start_matches("file://").to_string();

    // Windows `file:///C:/...` 会得到 `/C:/...`，需要去掉开头斜杠。
    if path.starts_with('/') && path.chars().nth(2) == Some(':') {
        path.remove(0);
    }

    Some(PathBuf::from(path))
}

/// 计算 `old_text` 到 `new_text` 的增量变更集合。
///
/// 当前策略为“按行切片 + 单区间增量”，能在保证实现复杂度可控的前提下，
/// 覆盖常见输入场景并减少整文同步开销。
pub fn compute_incremental_changes(old_text: &str, new_text: &str) -> Vec<LspTextEdit> {
    if old_text == new_text {
        return Vec::new();
    }

    let old_lines: Vec<&str> = old_text.split('\n').collect();
    let new_lines: Vec<&str> = new_text.split('\n').collect();

    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut old_suffix = old_lines.len();
    let mut new_suffix = new_lines.len();
    while old_suffix > prefix
        && new_suffix > prefix
        && old_lines[old_suffix - 1] == new_lines[new_suffix - 1]
    {
        old_suffix -= 1;
        new_suffix -= 1;
    }

    let changed_old = &old_lines[prefix..old_suffix];
    let changed_new = &new_lines[prefix..new_suffix];
    let max_len = changed_old.len().max(changed_new.len());

    let mut edits = Vec::new();
    for index in 0..max_len {
        let old_line = changed_old.get(index).copied().unwrap_or("");
        let new_line = changed_new.get(index).copied().unwrap_or("");
        if old_line == new_line {
            continue;
        }

        let line_number = prefix + index;
        if let Some(mut edit) = compute_incremental_change(old_line, new_line) {
            edit.start_line += line_number;
            edit.end_line += line_number;
            edits.push(edit);
        }
    }

    if edits.is_empty()
        && let Some(edit) = compute_incremental_change(old_text, new_text)
    {
        edits.push(edit);
    }

    edits
}

/// 计算单区间增量变更。
fn compute_incremental_change(old_text: &str, new_text: &str) -> Option<LspTextEdit> {
    if old_text == new_text {
        return None;
    }

    let old_chars: Vec<char> = old_text.chars().collect();
    let new_chars: Vec<char> = new_text.chars().collect();

    let mut prefix = 0usize;
    while prefix < old_chars.len()
        && prefix < new_chars.len()
        && old_chars[prefix] == new_chars[prefix]
    {
        prefix += 1;
    }

    let mut old_suffix = old_chars.len();
    let mut new_suffix = new_chars.len();
    while old_suffix > prefix
        && new_suffix > prefix
        && old_chars[old_suffix - 1] == new_chars[new_suffix - 1]
    {
        old_suffix -= 1;
        new_suffix -= 1;
    }

    let (start_line, start_character) = char_index_to_line_col(&old_chars, prefix);
    let (end_line, end_character) = char_index_to_line_col(&old_chars, old_suffix);
    let replacement_text: String = new_chars[prefix..new_suffix].iter().collect();

    Some(LspTextEdit {
        start_line,
        start_character,
        end_line,
        end_character,
        new_text: replacement_text,
    })
}

/// 将字符索引转换为 `(line, column)`（0-based）。
fn char_index_to_line_col(chars: &[char], index: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for ch in chars.iter().take(index) {
        if *ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// 校验并抽取响应消息中的请求 id。
pub fn response_request_id(value: &Value) -> Option<u64> {
    value.get("id").and_then(Value::as_u64)
}

/// 判断响应是否存在 `result` 字段。
pub fn has_result(value: &Value) -> bool {
    value.get("result").is_some()
}

/// 判断消息是否为 `$/progress` 通知。
pub fn is_progress_notification(value: &Value) -> bool {
    value
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| method == "$/progress")
}

/// 从 `$/progress` 中提取 rust-analyzer 项目加载状态。
///
/// rust-analyzer 会使用 begin/report/end 三种 kind 汇报索引与构建进度，
/// 这里统一归一化为“消息 + 是否完成”，便于 UI 直接展示。
pub fn parse_rust_analyzer_progress(value: &Value) -> Option<(String, bool)> {
    let params = value.get("params")?.as_object()?;
    let payload = params.get("value")?.as_object()?;

    let kind = payload.get("kind")?.as_str()?;
    let title = payload.get("title").and_then(Value::as_str).unwrap_or("");
    let message = payload.get("message").and_then(Value::as_str).unwrap_or("");

    let normalized = match kind {
        "begin" => {
            if message.is_empty() {
                format!("rust-analyzer 加载中：{}", title)
            } else {
                format!("rust-analyzer 加载中：{} - {}", title, message)
            }
        }
        "report" => {
            if message.is_empty() {
                format!("rust-analyzer 进行中：{}", title)
            } else {
                format!("rust-analyzer 进行中：{} - {}", title, message)
            }
        }
        "end" => {
            if message.is_empty() {
                "rust-analyzer 项目加载完成".to_string()
            } else {
                format!("rust-analyzer 项目加载完成：{}", message)
            }
        }
        _ => return None,
    };

    Some((normalized, kind == "end"))
}
