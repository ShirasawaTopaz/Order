use std::path::Path;

use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio::fs;

use crate::observability::{
    AgentEvent, current_trace_id, log_event_best_effort, ts, workspace_root_best_effort,
};

use super::workspace::{
    MAX_SEARCH_FILE_BYTES, MAX_SEARCH_FILES, MAX_SEARCH_RESULTS,
    ensure_no_symlink_in_existing_path, resolve_workspace_relative_path, workspace_root,
};

#[derive(Clone, Deserialize)]
pub struct SearchFileToolArgs {
    pub path: String,
    pub keyword: String,
}

#[derive(Debug)]
pub enum SearchFileToolError {
    IoError(std::io::Error),
    Other(String),
}

impl std::fmt::Display for SearchFileToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchFileToolError::IoError(error) => write!(f, "I/O error: {error}"),
            SearchFileToolError::Other(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SearchFileToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SearchFileToolError::IoError(error) => Some(error),
            SearchFileToolError::Other(_) => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchFileTool;

impl Tool for SearchFileTool {
    const NAME: &'static str = "SearchFileTool";
    type Error = SearchFileToolError;
    type Args = SearchFileToolArgs;
    type Output = Vec<String>;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description:
                "在工作区内递归搜索关键字并返回匹配行（仅允许相对路径，结果默认有数量上限）"
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对路径（搜索根目录），例如 `crates` 或 `.`"
                    },
                    "keyword": {
                        "type": "string",
                        "description": "要搜索的关键字（区分大小写）"
                    }
                },
                "required": ["path", "keyword"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let workspace_root_for_log = workspace_root_best_effort();
        let trace_id = current_trace_id();
        let started_at = Instant::now();
        if let Some(ref trace_id) = trace_id {
            log_event_best_effort(
                &workspace_root_for_log,
                AgentEvent::ToolCallStart {
                    ts: ts(),
                    trace_id: trace_id.clone(),
                    tool: Self::NAME.to_string(),
                },
            );
        }

        let result: Result<Vec<String>, SearchFileToolError> = (async {
            if args.keyword.trim().is_empty() {
                return Err(SearchFileToolError::Other(
                    "keyword must not be empty".to_string(),
                ));
            }

            // 通过“工作区内相对路径”约束搜索范围，避免递归扫描到工作区外部目录。
            let workspace_root = workspace_root().map_err(SearchFileToolError::Other)?;
            let root = resolve_workspace_relative_path(&workspace_root, &args.path)
                .map_err(SearchFileToolError::Other)?;
            ensure_no_symlink_in_existing_path(&workspace_root, &root)
                .map_err(SearchFileToolError::Other)?;

            let root_metadata = fs::metadata(&root)
                .await
                .map_err(SearchFileToolError::IoError)?;
            if !root_metadata.is_dir() {
                return Err(SearchFileToolError::Other(format!(
                    "path 不是目录: {}",
                    root.display()
                )));
            }

            let mut matches = Vec::new();
            let mut stack = vec![root];
            let mut scanned_files = 0usize;
            let mut truncated_by_limit = false;

            while let Some(current) = stack.pop() {
                let mut read_dir = fs::read_dir(&current)
                    .await
                    .map_err(SearchFileToolError::IoError)?;

                while let Some(entry) = read_dir
                    .next_entry()
                    .await
                    .map_err(SearchFileToolError::IoError)?
                {
                    let path = entry.path();
                    let file_type = entry
                        .file_type()
                        .await
                        .map_err(SearchFileToolError::IoError)?;

                    // 避免通过符号链接跳出工作区或扫描到意外目录。
                    if file_type.is_symlink() {
                        continue;
                    }

                    if file_type.is_dir() {
                        stack.push(path);
                        continue;
                    }

                    if !file_type.is_file() {
                        continue;
                    }

                    scanned_files += 1;
                    if scanned_files > MAX_SEARCH_FILES {
                        truncated_by_limit = true;
                        break;
                    }

                    let metadata = entry
                        .metadata()
                        .await
                        .map_err(SearchFileToolError::IoError)?;
                    if metadata.len() > MAX_SEARCH_FILE_BYTES {
                        continue;
                    }

                    if let Ok(file_matches) = search_in_file(&path, &args.keyword).await {
                        matches.extend(file_matches);
                        if matches.len() >= MAX_SEARCH_RESULTS {
                            matches.truncate(MAX_SEARCH_RESULTS);
                            truncated_by_limit = true;
                            break;
                        }
                    }
                }

                if truncated_by_limit {
                    break;
                }
            }

            if truncated_by_limit {
                matches.push(format!(
                    "[结果已截断] 扫描文件数上限={}，命中条数上限={}；请缩小 path 或更换更精确的 keyword",
                    MAX_SEARCH_FILES, MAX_SEARCH_RESULTS
                ));
            }

            Ok(matches)
        })
        .await;

        if let Some(ref trace_id) = trace_id {
            log_event_best_effort(
                &workspace_root_for_log,
                AgentEvent::ToolCallEnd {
                    ts: ts(),
                    trace_id: trace_id.clone(),
                    tool: Self::NAME.to_string(),
                    ok: result.is_ok(),
                    duration_ms: started_at.elapsed().as_millis(),
                    error: result.as_ref().err().map(|e| e.to_string()),
                },
            );
        }

        result
    }
}

/// 在单个文件中搜索关键字，返回匹配行。
///
/// 输出格式：`<path>:<line_number>:<line_content>`。
async fn search_in_file(path: &Path, keyword: &str) -> Result<Vec<String>, SearchFileToolError> {
    let text = match fs::read_to_string(path).await {
        Ok(value) => value,
        Err(error) => {
            // 非 UTF-8 或权限等问题时，直接跳过该文件，避免影响整体搜索体验。
            if error.kind() == std::io::ErrorKind::InvalidData {
                return Ok(Vec::new());
            }
            return Err(SearchFileToolError::IoError(error));
        }
    };

    let mut result = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.contains(keyword) {
            result.push(format!("{}:{}:{}", path.display(), index + 1, line));
        }
    }

    Ok(result)
}
