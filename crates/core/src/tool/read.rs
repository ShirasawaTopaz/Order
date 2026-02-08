use std::path::Path;
use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use tokio::{fs::File, io::AsyncReadExt};

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

    type Output = Vec<u8>;

    async fn definition(
        &self,
        _prompt: String,
    ) -> ToolDefinition {
        ToolDefinition{
            name: Self::NAME.to_string(),
            description: "Reads the content of a file".to_string(),
            parameters: serde_json::json!({
              "type": "string",
              "properties":{
                  "path": {
                      "type": "string",
                      "description": "Path to the file to read"
                  }
              }
            })
        }
    }

    async fn call(
        &self,
        args: Self::Args,
    ) -> Result<Vec<u8>, ReadToolError> {
        // 使用异步文件读取，避免阻塞运行时线程。
        let path = Path::new(&args.path);
        let mut file = File::open(path).await.map_err(ReadToolError::IoError)?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).await.map_err(ReadToolError::IoError)?;
        Ok(content)
    }
}
