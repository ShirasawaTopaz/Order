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
            description: "Stage a workspace file change for approval (high risk: does not write to disk immediately). For code edit requests, always call this tool directly and do NOT ask for write permission in chat first; approval is handled by the TUI via /approve or /reject after staging.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative file path, for example `crates/core/src/lib.rs`."
                    },
                    "content": {
                        "type": "string",
                        "description": "Text content to write (UTF-8)."
                    },
                    "append": {
                        "type": "boolean",
                        "description": "When true append to file; when false replace file content (default false)."
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

        // 统一将 CRLF 规范为 LF，避免跨平台协作时出现无意义换行差异。
        let content = args.content.replace("\r\n", "\n");
        if content.as_bytes().len() > MAX_WRITE_BYTES {
            let result: Result<String, WriteToolError> = Err(WriteToolError::Other(format!(
                "write content too large ({} bytes), refusing write; split content or increase limit",
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
                WriteToolError::Other(
                    "missing trace_id: cannot enter safe write approval flow".to_string(),
                )
            })?;

            let guard = ExecutionGuard::default();
            let keep_snapshots = guard.keep_snapshots_enabled();
            let summary = guard
                .stage_write(trace_id, &args.path, &content, append)
                .map_err(|error| WriteToolError::Other(error.to_string()))?;
            let snapshot_policy = if keep_snapshots {
                format!("After approval, rollback if needed: `/rollback {trace_id}`")
            } else {
                "默认会在写入成功后自动清理 `.order/snapshots/<trace_id>`；如需保留快照用于 /rollback，请设置 ORDER_KEEP_SNAPSHOTS=1".to_string()
            };

            // 暂存成功必须返回 Ok，避免模型把“待审批写入”误判为失败并退化成纯口头承诺。
            Ok(format!(
                "Staged high-risk write (not written to disk yet).\n\
trace_id: {trace_id}\n\
path: {}\n\
append: {}\n\
diff: existed={} old_lines={} new_lines={} +{} -{}\n\
\n\
Run in TUI:\n\
- `/approve {trace_id}` to apply writes (snapshot will be created automatically)\n\
- `/reject {trace_id}` to discard staged writes\n\
\n\
{snapshot_policy}",
                summary.path,
                summary.append,
                summary.diff.existed,
                summary.diff.old_lines,
                summary.diff.new_lines,
                summary.diff.added_lines,
                summary.diff.removed_lines,
                snapshot_policy = snapshot_policy
            ))
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
