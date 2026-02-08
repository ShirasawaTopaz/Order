use std::path::Path;

use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use tokio::{fs::File, io::AsyncWriteExt};

#[derive(Clone, Deserialize)]
pub struct WriteToolArgs {
    pub path: String,
    pub content: String,
    pub append: Option<bool>,
}

#[derive(Debug)]
pub enum WriteToolError {
    IoError(std::io::Error),
    Other(String),
}

impl std::fmt::Display for WriteToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteToolError::IoError(error) => write!(f, "I/O error: {error}"),
            WriteToolError::Other(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WriteToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WriteToolError::IoError(error) => Some(error),
            WriteToolError::Other(_) => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WriteTool;

impl Tool for WriteTool {
    const NAME: &'static str = "WriteTool";
    type Error = WriteToolError;
    type Args = WriteToolArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Writes content to a file".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write"
                    },
                    "append": {
                        "type": "boolean",
                        "description": "Append content if true, overwrite if false (default false)"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = Path::new(&args.path);
        let append = args.append.unwrap_or(false);

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(WriteToolError::IoError)?;
        }

        if append {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .map_err(WriteToolError::IoError)?;
            file.write_all(args.content.as_bytes())
                .await
                .map_err(WriteToolError::IoError)?;
            file.flush().await.map_err(WriteToolError::IoError)?;
        } else {
            let mut file = File::create(path).await.map_err(WriteToolError::IoError)?;
            file.write_all(args.content.as_bytes())
                .await
                .map_err(WriteToolError::IoError)?;
            file.flush().await.map_err(WriteToolError::IoError)?;
        }

        Ok(format!("write success: {}", path.display()))
    }
}

