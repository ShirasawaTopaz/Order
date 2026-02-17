use std::{
    process::Stdio,
    time::{Duration, Instant},
};

use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use tokio::{process::Command as TokioCommand, time};

use crate::observability::{
    AgentEvent, current_trace_id, log_event_best_effort, ts, workspace_root_best_effort,
};

use super::workspace::workspace_root;

/// 命令执行超时默认值（秒）。
///
/// 默认给出较短超时，避免模型误触发长时间阻塞命令时拖垮交互体验。
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
/// 命令执行超时上限（秒）。
///
/// 允许用户按需放宽，但仍要限制上界，防止“忘记结束”的命令长期占用进程。
const MAX_TIMEOUT_SECONDS: u64 = 300;
/// 单条命令字符串长度上限。
///
/// 过长命令通常意味着上下文污染或拼接异常，直接拒绝更容易暴露根因。
const MAX_COMMAND_LENGTH: usize = 4096;
/// 标准输出/错误输出的单通道字节上限。
///
/// 仅保留头尾片段，避免大体量输出淹没模型上下文和日志。
const MAX_COMMAND_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Deserialize)]
pub struct CommandToolArgs {
    pub command: String,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug)]
pub enum CommandToolError {
    IoError(std::io::Error),
    Other(String),
}

impl std::fmt::Display for CommandToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandToolError::IoError(error) => write!(f, "I/O error: {error}"),
            CommandToolError::Other(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CommandToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CommandToolError::IoError(error) => Some(error),
            CommandToolError::Other(_) => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommandTool;

impl Tool for CommandTool {
    const NAME: &'static str = "CommandTool";
    type Error = CommandToolError;
    type Args = CommandToolArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Run a shell command in workspace root and return exit code/stdout/stderr (with timeout and output truncation).".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command line to run in workspace root."
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": format!("Optional timeout in seconds (default {}, max {}).", DEFAULT_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS)
                    }
                },
                "required": ["command"]
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

        let result: Result<String, CommandToolError> = (async {
            let command_line = args.command.trim();
            if command_line.is_empty() {
                return Err(CommandToolError::Other(
                    "command must not be empty".to_string(),
                ));
            }
            if command_line.chars().count() > MAX_COMMAND_LENGTH {
                return Err(CommandToolError::Other(format!(
                    "command too long (>{MAX_COMMAND_LENGTH} chars), refusing to execute"
                )));
            }

            let root = workspace_root().map_err(CommandToolError::Other)?;
            let timeout_seconds = normalize_timeout_seconds(args.timeout_seconds);

            // 始终在工作区根目录执行，并关闭 stdin，避免交互式命令卡住工具调用链路。
            let mut command = build_shell_command(command_line);
            command.current_dir(&root);
            command.stdin(Stdio::null());
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
            command.kill_on_drop(true);

            // 使用超时包裹子进程等待，超时时丢弃 future 触发 kill_on_drop。
            let output = time::timeout(Duration::from_secs(timeout_seconds), command.output())
                .await
                .map_err(|_| {
                    CommandToolError::Other(format!(
                        "command timed out after {timeout_seconds} seconds"
                    ))
                })?
                .map_err(CommandToolError::IoError)?;

            let (stdout, stdout_truncated) =
                truncate_output(&output.stdout, MAX_COMMAND_OUTPUT_BYTES);
            let (stderr, stderr_truncated) =
                truncate_output(&output.stderr, MAX_COMMAND_OUTPUT_BYTES);

            // 命令本身失败（非 0 退出码）仍返回结构化结果，让模型可基于 stderr 继续诊断。
            let payload = serde_json::json!({
                "command": command_line,
                "ok": output.status.success(),
                "exit_code": output.status.code(),
                "timeout_seconds": timeout_seconds,
                "stdout": stdout,
                "stderr": stderr,
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated
            });
            let mut text = serde_json::to_string_pretty(&payload).map_err(|error| {
                CommandToolError::Other(format!("serialize command result failed: {error}"))
            })?;
            text.push('\n');
            Ok(text)
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

#[cfg(target_os = "windows")]
fn build_shell_command(command_line: &str) -> TokioCommand {
    let mut command = TokioCommand::new("cmd");
    command.arg("/C").arg(command_line);
    command
}

#[cfg(not(target_os = "windows"))]
fn build_shell_command(command_line: &str) -> TokioCommand {
    let mut command = TokioCommand::new("sh");
    command.arg("-lc").arg(command_line);
    command
}

fn normalize_timeout_seconds(requested: Option<u64>) -> u64 {
    match requested {
        Some(value) if value > 0 => value.min(MAX_TIMEOUT_SECONDS),
        _ => DEFAULT_TIMEOUT_SECONDS,
    }
}

fn truncate_output(bytes: &[u8], max_bytes: usize) -> (String, bool) {
    if bytes.len() <= max_bytes {
        return (String::from_utf8_lossy(bytes).to_string(), false);
    }

    // 同时保留头尾，既能看到报错上下文，也不会丢掉命令收尾状态信息。
    let head_size = max_bytes / 2;
    let tail_size = max_bytes.saturating_sub(head_size);
    let head = String::from_utf8_lossy(&bytes[..head_size]).to_string();
    let tail = String::from_utf8_lossy(&bytes[bytes.len() - tail_size..]).to_string();
    (
        format!(
            "{head}\n...[输出已截断，省略 {} 字节]...\n{tail}",
            bytes.len().saturating_sub(max_bytes)
        ),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS, normalize_timeout_seconds, truncate_output,
    };

    #[test]
    fn normalize_timeout_seconds_should_apply_default_and_cap() {
        assert_eq!(normalize_timeout_seconds(None), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(normalize_timeout_seconds(Some(0)), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(
            normalize_timeout_seconds(Some(MAX_TIMEOUT_SECONDS + 120)),
            MAX_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn truncate_output_should_keep_head_and_tail_when_exceeds_limit() {
        let text = "A".repeat(64) + &"B".repeat(64);
        let (truncated, is_truncated) = truncate_output(text.as_bytes(), 32);
        assert!(is_truncated);
        assert!(truncated.contains("输出已截断"));
        assert!(truncated.contains("AAAA"));
        assert!(truncated.contains("BBBB"));
    }
}
