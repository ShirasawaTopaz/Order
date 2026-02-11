use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use tokio::{fs::File, io::AsyncReadExt};

use super::workspace::{
    MAX_READ_BYTES, ensure_no_symlink_in_existing_path, resolve_workspace_relative_path,
    workspace_root,
};

#[derive(Clone, Deserialize)]
pub struct ReadToolArgs {
    pub path: String,
}

#[derive(Debug)]
pub enum ReadToolError {
    IoError(std::io::Error),
    Other(String),
}

impl std::fmt::Display for ReadToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadToolError::IoError(e) => write!(f, "I/O error: {e}"),
            ReadToolError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ReadToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadToolError::IoError(e) => Some(e),
            ReadToolError::Other(_) => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReadTool;

impl Tool for ReadTool {
    const NAME: &'static str = "ReadTool";
    type Error = ReadToolError;

    type Args = ReadToolArgs;

    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "读取工作区内文件内容（仅允许相对路径，且需为 UTF-8 文本）".to_string(),
            parameters: serde_json::json!({
              "type": "object",
              "properties": {
                "path": {
                  "type": "string",
                  "description": "相对路径（基于当前工作区根目录），例如 `crates/core/src/lib.rs`"
                }
              },
              "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ReadToolError> {
        // 通过“工作区内相对路径”约束模型可访问的文件范围，避免越权读取。
        let root = workspace_root().map_err(ReadToolError::Other)?;
        let resolved =
            resolve_workspace_relative_path(&root, &args.path).map_err(ReadToolError::Other)?;
        ensure_no_symlink_in_existing_path(&root, &resolved).map_err(ReadToolError::Other)?;

        let metadata = tokio::fs::metadata(&resolved)
            .await
            .map_err(ReadToolError::IoError)?;
        if !metadata.is_file() {
            return Err(ReadToolError::Other(format!(
                "path 不是文件: {}",
                resolved.display()
            )));
        }
        if metadata.len() > MAX_READ_BYTES {
            return Err(ReadToolError::Other(format!(
                "文件过大（{} bytes），已拒绝读取；请缩小文件或提高上限",
                metadata.len()
            )));
        }

        // 使用异步读取避免阻塞运行时线程，并且只接受 UTF-8 文本。
        let mut file = File::open(&resolved)
            .await
            .map_err(ReadToolError::IoError)?;
        let mut content = String::new();
        if let Err(error) = file.read_to_string(&mut content).await {
            if error.kind() == std::io::ErrorKind::InvalidData {
                return Err(ReadToolError::Other(format!(
                    "文件不是 UTF-8 文本，无法读取: {}",
                    resolved.display()
                )));
            }
            return Err(ReadToolError::IoError(error));
        }

        Ok(content)
    }
}
