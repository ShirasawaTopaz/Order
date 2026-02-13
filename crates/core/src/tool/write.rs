use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::{
    observability::{
        AgentEvent, current_trace_id, log_event_best_effort, ts, workspace_root_best_effort,
    },
    safety::ExecutionGuard,
};

use super::workspace::MAX_WRITE_BYTES;

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
            description: "写入工作区内文件内容（高风险：默认不直接落盘，会生成预览并要求用户二次确认；默认将 CRLF 规范为 LF）"
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

        let append = args.append.unwrap_or(false);

        // 统一将 CRLF 规范为 LF，减少跨平台协作时的 diff 噪音。
        let content = args.content.replace("\r\n", "\n");
        if content.as_bytes().len() > MAX_WRITE_BYTES {
            let result: Result<String, WriteToolError> = Err(WriteToolError::Other(format!(
                "写入内容过大（{} bytes），已拒绝写入；请拆分或提高上限",
                content.as_bytes().len()
            )));

            if let Some(ref trace_id) = trace_id {
                log_event_best_effort(
                    &workspace_root_for_log,
                    AgentEvent::ToolCallEnd {
                        ts: ts(),
                        trace_id: trace_id.clone(),
                        tool: Self::NAME.to_string(),
                        ok: false,
                        duration_ms: started_at.elapsed().as_millis(),
                        error: result.as_ref().err().map(|e| e.to_string()),
                    },
                );
            }

            return result;
        }

        let result: Result<String, WriteToolError> = (|| {
            let trace_id = trace_id.as_ref().ok_or_else(|| {
                WriteToolError::Other("缺少 trace_id：无法进入安全写入确认流程".to_string())
            })?;

            let guard = ExecutionGuard::default();
            let summary = guard
                .stage_write(trace_id, &args.path, &content, append)
                .map_err(|error| WriteToolError::Other(error.to_string()))?;

            Err(WriteToolError::Other(format!(
                "高风险写入已拦截（未落盘）。\n\
trace_id: {trace_id}\n\
path: {}\n\
append: {}\n\
diff: existed={} old_lines={} new_lines={} +{} -{}\n\
\n\
请在 TUI 中执行：\n\
- `/approve {trace_id}` 确认写入（将自动创建快照，必要时可回滚）\n\
- `/reject {trace_id}` 取消本次写入\n\
\n\
确认写入后，如需回滚：`/rollback {trace_id}`",
                summary.path,
                summary.append,
                summary.diff.existed,
                summary.diff.old_lines,
                summary.diff.new_lines,
                summary.diff.added_lines,
                summary.diff.removed_lines
            )))
        })();

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
