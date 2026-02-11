use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use tokio::{fs::File, io::AsyncWriteExt};

use super::workspace::{
    MAX_WRITE_BYTES, ensure_no_symlink_in_existing_path, resolve_workspace_relative_path,
    workspace_root,
};

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
            description: "写入工作区内文件内容（仅允许相对路径，默认将 CRLF 规范为 LF）"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对路径（基于当前工作区根目录），例如 `crates/core/src/lib.rs`"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的文本内容（UTF-8）"
                    },
                    "append": {
                        "type": "boolean",
                        "description": "为 true 时追加；为 false 时覆盖（默认 false）"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // 通过“工作区内相对路径”约束模型可写入的范围，避免越权写文件。
        let root = workspace_root().map_err(WriteToolError::Other)?;
        let path =
            resolve_workspace_relative_path(&root, &args.path).map_err(WriteToolError::Other)?;
        ensure_no_symlink_in_existing_path(&root, &path).map_err(WriteToolError::Other)?;

        let append = args.append.unwrap_or(false);

        // 统一将 CRLF 规范为 LF，减少跨平台协作时的 diff 噪音。
        let content = args.content.replace("\r\n", "\n");
        if content.as_bytes().len() > MAX_WRITE_BYTES {
            return Err(WriteToolError::Other(format!(
                "写入内容过大（{} bytes），已拒绝写入；请拆分或提高上限",
                content.as_bytes().len()
            )));
        }

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
                .open(&path)
                .await
                .map_err(WriteToolError::IoError)?;
            file.write_all(content.as_bytes())
                .await
                .map_err(WriteToolError::IoError)?;
            file.flush().await.map_err(WriteToolError::IoError)?;
        } else {
            let mut file = File::create(&path).await.map_err(WriteToolError::IoError)?;
            file.write_all(content.as_bytes())
                .await
                .map_err(WriteToolError::IoError)?;
            file.flush().await.map_err(WriteToolError::IoError)?;
        }

        Ok(format!("write success: {}", path.display()))
    }
}
