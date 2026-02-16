use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        OnceLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::encoding::append_utf8_json_line;

/// 统一结构化日志事件（JSON Line）。
///
/// 设计要点：
/// - 用 `trace_id` 串起一次请求的全链路：TUI 输入 -> 模型请求 -> tool 调用 -> TUI 输出；
/// - 采用 JSON Line，便于用 `rg`/脚本快速过滤（尤其是按 trace_id 定位问题）；
/// - 事件字段尽量稳定：未来新增字段只做向后兼容扩展，避免破坏已有解析。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    /// 用户在 TUI 提交了一次输入。
    TuiInput {
        ts: String,
        trace_id: String,
        input_len: usize,
    },
    /// TUI 已拿到最终输出（成功或失败）。
    TuiOutput {
        ts: String,
        trace_id: String,
        ok: bool,
        output_len: Option<usize>,
        error: Option<String>,
    },
    /// 模型请求开始。
    RequestStart {
        ts: String,
        trace_id: String,
        provider: String,
        model: String,
        endpoint: String,
        tools: bool,
        system_preamble: bool,
        /// 记录协商来源，便于解释“为什么没开 tools/为什么走 chat/completions”。
        capability_sources: Vec<String>,
    },
    /// 模型请求结束。
    RequestEnd {
        ts: String,
        trace_id: String,
        ok: bool,
        duration_ms: u128,
        attempts: u32,
        endpoint: String,
        tools: bool,
        system_preamble: bool,
        error: Option<String>,
    },
    /// 计划重试（通常由能力降级触发）。
    RetryScheduled {
        ts: String,
        trace_id: String,
        attempt: u32,
        reason: String,
    },
    /// 错误分类结果（用于解释“为何触发/不触发降级”）。
    ErrorClassified {
        ts: String,
        trace_id: String,
        category: String,
        status_code: Option<u16>,
        provider_error_code: Option<String>,
        endpoint: String,
        tools: bool,
        stream: bool,
        responses: bool,
        degradable: bool,
        summary: String,
    },
    /// 重试用尽（本实现当前最多重试一次，保留事件用于后续扩展）。
    RetryExhausted {
        ts: String,
        trace_id: String,
        attempts: u32,
        last_error: String,
    },
    /// 一次“能力降级/回退”已经应用。
    FallbackApplied {
        ts: String,
        trace_id: String,
        reason: String,
        from_endpoint: String,
        to_endpoint: String,
        tools_from: bool,
        tools_to: bool,
        system_from: bool,
        system_to: bool,
    },
    /// tool 调用开始。
    ToolCallStart {
        ts: String,
        trace_id: String,
        tool: String,
    },
    /// tool 调用结束。
    ToolCallEnd {
        ts: String,
        trace_id: String,
        tool: String,
        ok: bool,
        duration_ms: u128,
        error: Option<String>,
    },
    /// 能力缓存重置事件（用于审计“手工重置”的来源）。
    CapabilityCacheReset {
        ts: String,
        provider: Option<String>,
        model: Option<String>,
        removed: usize,
    },
    /// 自动验证开始。
    ValidationStart {
        ts: String,
        trace_id: String,
        commands: Vec<String>,
    },
    /// 自动验证结束。
    ValidationEnd {
        ts: String,
        trace_id: String,
        ok: bool,
        duration_ms: u128,
        failed_command: Option<String>,
    },
}

tokio::task_local! {
    /// 当前异步任务的 trace_id。
    ///
    /// 选择 task-local 的原因：
    /// - tool 调用发生在模型请求的异步链路内部；
    /// - 通过 scope 绑定后，链路内的日志都能拿到同一个 trace_id；
    /// - 避免把 trace_id 作为参数层层传递，降低侵入性。
    static TRACE_ID: String;
}

static TRACE_COUNTER: AtomicU64 = AtomicU64::new(0);
static TRACE_ID_FALLBACK: OnceLock<RwLock<Option<String>>> = OnceLock::new();

/// 获取 trace_id 回退槽位。
///
/// 该槽位只用于补偿“task-local 未自动透传到新任务”的场景，
/// 正常路径仍以 task-local 为准。
fn trace_id_fallback_slot() -> &'static RwLock<Option<String>> {
    TRACE_ID_FALLBACK.get_or_init(|| RwLock::new(None))
}

/// 在 `with_trace_id` 生命周期内维护“请求级回退 trace_id”。
///
/// 设计原因：
/// - 某些 SDK/tool server 会在内部 `tokio::spawn` 新任务；
/// - `task_local` 默认不会跨任务自动继承，导致工具侧拿不到 trace_id；
/// - 用回退槽位可保证同一请求内仍能关联到正确 trace_id。
struct TraceIdFallbackGuard {
    previous: Option<String>,
}

impl TraceIdFallbackGuard {
    fn install(current: &str) -> Self {
        let slot = trace_id_fallback_slot();
        let mut guard = slot.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = guard.clone();
        *guard = Some(current.to_string());
        Self { previous }
    }
}

impl Drop for TraceIdFallbackGuard {
    fn drop(&mut self) {
        let slot = trace_id_fallback_slot();
        let mut guard = slot.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = self.previous.clone();
    }
}

/// 生成一个新的 trace_id。
///
/// 目标是“足够唯一且易检索”，而不是强随机：
/// - 采用 unix 时间戳 + 单调计数器；
/// - 输出为十六进制，便于复制与在日志中搜索。
pub fn new_trace_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let counter = TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}{:x}", now.as_secs(), now.subsec_nanos(), counter)
}

/// 在给定 trace_id 的 scope 内执行异步逻辑。
pub async fn with_trace_id<T>(trace_id: String, fut: impl std::future::Future<Output = T>) -> T {
    let _fallback_guard = TraceIdFallbackGuard::install(&trace_id);
    TRACE_ID.scope(trace_id, fut).await
}

/// 获取当前任务绑定的 trace_id（若不存在则返回 None）。
pub fn current_trace_id() -> Option<String> {
    TRACE_ID.try_with(|value| value.clone()).ok().or_else(|| {
        let slot = trace_id_fallback_slot();
        let guard = slot.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone()
    })
}

/// 获取当前时间（本地时区）字符串。
fn now_timestamp() -> String {
    Local::now().to_rfc3339()
}

/// 生成日志目录：`<workspace>/.order/logs/`。
fn logs_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".order").join("logs")
}

/// 生成当天的日志文件路径：`agent-YYYYMMDD.log`。
fn daily_log_path(workspace_root: &Path) -> PathBuf {
    let filename = format!("agent-{}.log", Local::now().format("%Y%m%d"));
    logs_dir(workspace_root).join(filename)
}

/// 将事件写入日志（JSON Line）。
///
/// 注意：日志失败不应影响主流程，因此对外通常使用 `log_event_best_effort`。
pub fn log_event(workspace_root: &Path, event: &AgentEvent) -> io::Result<()> {
    let dir = logs_dir(workspace_root);
    fs::create_dir_all(&dir)?;

    let path = daily_log_path(workspace_root);
    let json =
        serde_json::to_string(event).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    append_utf8_json_line(&path, &json)?;
    Ok(())
}

/// 尽力写日志：失败时只告警，不中断主流程。
pub fn log_event_best_effort(workspace_root: &Path, event: AgentEvent) {
    if let Err(error) = log_event(workspace_root, &event) {
        let path = daily_log_path(workspace_root);
        eprintln!(
            "写入结构化日志失败（已忽略，不影响主流程）: {} ({error})",
            path.display()
        );
    }
}

/// 便捷：构建带 `ts` 字段的事件时间戳。
pub fn ts() -> String {
    now_timestamp()
}

/// 尽力获取工作区根目录（用于落盘日志）。
///
/// 这里不强依赖任何“工作区配置”：
/// - 对 CLI/TUI 来说，通常从仓库根目录启动；
/// - 即使用户从子目录启动，日志仍会落到当前目录的 `.order/` 下，避免意外写到系统目录。
pub fn workspace_root_best_effort() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::{current_trace_id, with_trace_id};

    #[tokio::test(flavor = "current_thread")]
    async fn current_trace_id_should_be_available_in_scope() {
        let trace_id = "trace-in-scope".to_string();
        let got = with_trace_id(trace_id.clone(), async { current_trace_id() }).await;
        assert_eq!(got, Some(trace_id));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_trace_id_should_fallback_in_spawned_task() {
        let trace_id = "trace-fallback".to_string();
        let got = with_trace_id(trace_id.clone(), async {
            tokio::spawn(async { current_trace_id() })
                .await
                .expect("spawned task should finish")
        })
        .await;
        assert_eq!(got, Some(trace_id));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_trace_id_should_restore_after_nested_scope() {
        let outer = "trace-outer".to_string();
        let inner = "trace-inner".to_string();

        with_trace_id(outer.clone(), async {
            assert_eq!(current_trace_id(), Some(outer.clone()));
            with_trace_id(inner.clone(), async {
                assert_eq!(current_trace_id(), Some(inner.clone()));
            })
            .await;
            assert_eq!(current_trace_id(), Some(outer.clone()));
        })
        .await;
    }
}
