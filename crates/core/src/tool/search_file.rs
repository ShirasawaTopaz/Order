use std::{collections::VecDeque, path::Path};

use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio::fs;

use crate::observability::{
    AgentEvent, current_trace_id, log_event_best_effort, ts, workspace_root_best_effort,
};

use super::workspace::{
    MAX_SEARCH_FILE_BYTES, MAX_SEARCH_FILES, MAX_SEARCH_RESULTS,
    ensure_no_symlink_in_existing_path, format_workspace_relative_path,
    resolve_workspace_relative_path, workspace_root,
};

/// 默认跳过的目录名（仅在递归搜索时生效）。
///
/// 这些目录通常体积大、噪声高或包含构建产物，默认搜索它们会让模型更难快速定位源码。
const DEFAULT_IGNORED_DIR_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".order",
    "target",
    "node_modules",
    ".next",
    ".turbo",
    "dist",
    "build",
    "out",
];

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

            // path 为空时回退到当前工作区根目录，提升模型工具调用的容错能力。
            let normalized_path = if args.path.trim().is_empty() {
                "."
            } else {
                args.path.trim()
            };

            // 通过“工作区内相对路径”约束搜索范围，避免递归扫描到工作区外部目录。
            let workspace_root = workspace_root().map_err(SearchFileToolError::Other)?;
            let root = resolve_workspace_relative_path(&workspace_root, normalized_path)
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
            let mut pending_dirs = VecDeque::from([root.clone()]);
            let mut scanned_files = 0usize;
            let mut truncated_by_limit = false;

            while let Some(current) = pending_dirs.pop_front() {
                let mut read_dir = fs::read_dir(&current)
                    .await
                    .map_err(SearchFileToolError::IoError)?;
                let mut child_dirs = Vec::new();

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
                        // 跳过典型噪声目录，避免在大仓库里过早耗尽扫描预算。
                        if should_skip_directory(&root, &path) {
                            continue;
                        }
                        child_dirs.push(path);
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

                    if let Ok(file_matches) =
                        search_in_file(&workspace_root, &path, &args.keyword).await
                    {
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

                // 稳定排序后按 BFS 继续遍历，减少不同平台下搜索结果顺序波动。
                child_dirs.sort();
                for directory in child_dirs {
                    pending_dirs.push_back(directory);
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
async fn search_in_file(
    workspace_root: &Path,
    path: &Path,
    keyword: &str,
) -> Result<Vec<String>, SearchFileToolError> {
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

    // 统一输出工作区相对路径，确保结果可直接用于 `ReadTool/WriteTool`。
    let display_path =
        format_workspace_relative_path(workspace_root, path).map_err(SearchFileToolError::Other)?;

    let mut result = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.contains(keyword) {
            result.push(format!("{display_path}:{}:{}", index + 1, line));
        }
    }

    Ok(result)
}

/// 判断目录是否属于默认噪声目录。
///
/// 仅跳过“搜索根目录之下的子目录”，若用户显式把根目录设置为噪声目录本身，仍允许搜索。
fn should_skip_directory(search_root: &Path, candidate: &Path) -> bool {
    if candidate == search_root {
        return false;
    }

    let Some(name) = candidate.file_name().and_then(|value| value.to_str()) else {
        return false;
    };

    is_default_ignored_directory_name(name)
}

fn is_default_ignored_directory_name(name: &str) -> bool {
    DEFAULT_IGNORED_DIR_NAMES
        .iter()
        .any(|ignored| ignored.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    use tokio::fs;

    use super::{is_default_ignored_directory_name, search_in_file, should_skip_directory};

    #[test]
    fn should_skip_directory_should_skip_target_under_workspace() {
        let root = Path::new("D:/order");
        let target = Path::new("D:/order/target");
        assert!(should_skip_directory(root, target));
    }

    #[test]
    fn should_skip_directory_should_not_skip_when_root_itself_is_target() {
        let root = Path::new("D:/order/target");
        assert!(!should_skip_directory(root, root));
    }

    #[test]
    fn is_default_ignored_directory_name_should_be_case_insensitive() {
        assert!(is_default_ignored_directory_name("TARGET"));
        assert!(is_default_ignored_directory_name(".Git"));
        assert!(!is_default_ignored_directory_name("crates"));
    }

    #[tokio::test]
    async fn search_in_file_should_emit_workspace_relative_path() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("order-search-tool-{suffix}"));
        let source_dir = root.join("src");
        let file_path = source_dir.join("main.rs");

        fs::create_dir_all(&source_dir)
            .await
            .expect("temp source dir should be creatable");
        fs::write(
            &file_path,
            "fn main() {\n    println!(\"hello from search tool\");\n}\n",
        )
        .await
        .expect("temp source file should be writable");

        let matches = search_in_file(&root, &file_path, "println!")
            .await
            .expect("search should succeed for utf-8 file");
        assert_eq!(matches.len(), 1);
        assert!(
            matches[0].starts_with("src/main.rs:2:"),
            "match line should start with relative path and line number, got: {}",
            matches[0]
        );

        // 测试完成后清理临时目录，避免污染系统临时空间。
        let _ = fs::remove_dir_all(&root).await;
    }
}
