use std::path::{Path, PathBuf};

use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncReadExt};

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
            description: "Search files for a keyword and return matched lines".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Root path to search recursively"
                    },
                    "keyword": {
                        "type": "string",
                        "description": "Keyword to search"
                    }
                },
                "required": ["path", "keyword"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if args.keyword.trim().is_empty() {
            return Err(SearchFileToolError::Other(
                "keyword must not be empty".to_string(),
            ));
        }

        let root = PathBuf::from(args.path);
        if !root.exists() {
            return Err(SearchFileToolError::Other(format!(
                "path does not exist: {}",
                root.display()
            )));
        }

        let mut matches = Vec::new();
        let mut stack = vec![root];

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
                let metadata = entry
                    .metadata()
                    .await
                    .map_err(SearchFileToolError::IoError)?;

                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }

                if !metadata.is_file() {
                    continue;
                }

                if let Ok(file_matches) = search_in_file(&path, &args.keyword).await {
                    matches.extend(file_matches);
                }
            }
        }

        Ok(matches)
    }
}

/// 在单个文件中搜索关键字，返回匹配行。
///
/// 输出格式：`<path>:<line_number>:<line_content>`。
async fn search_in_file(path: &Path, keyword: &str) -> Result<Vec<String>, SearchFileToolError> {
    let mut file = fs::File::open(path)
        .await
        .map_err(SearchFileToolError::IoError)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)
        .await
        .map_err(SearchFileToolError::IoError)?;

    let text = match String::from_utf8(content) {
        Ok(value) => value,
        Err(_) => return Ok(Vec::new()),
    };

    let mut result = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.contains(keyword) {
            result.push(format!("{}:{}:{}", path.display(), index + 1, line));
        }
    }

    Ok(result)
}
