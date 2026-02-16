use crate::{
    editor::Editor,
    focus_status::{CURRENT_FOCUS, FocusStatus},
    history::{ContextManager, ContextMessage, ContextModelLimits, ContextRole},
    widget::input_widget::{InputState, InputWidget},
};
use anyhow::{Context, anyhow};
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use core::{
    commands::{EXIT, get_exit},
    encoding::{read_utf8_text_with_report, write_utf8_text_with_report},
    model::{
        capabilities::CapabilityResolver,
        connection::{Connection, ModelStreamEvent, Provider},
        info::{get_current_model_info, get_current_model_info_from_config},
    },
    observability::{
        AgentEvent, log_event_best_effort, new_trace_id, ts, workspace_root_best_effort,
    },
    safety::{ExecutionGuard, PendingWriteSummary},
    validation::ValidationPipeline,
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
};
use rig::completion::Message as RigMessage;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use unicode_width::UnicodeWidthStr;

use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Widget},
};

/// 对话消息角色。
#[derive(Debug, Clone, Copy)]
enum ChatRole {
    /// 用户输入消息，渲染在右侧。
    User,
    /// 大模型返回消息，渲染在左侧。
    Llm,
    /// 发送失败等错误消息，渲染在左侧。
    Error,
}

/// 对话消息实体。
#[derive(Debug, Clone)]
struct ChatMessage {
    /// 消息角色，用于决定颜色与左右对齐。
    role: ChatRole,
    /// 消息正文。
    content: String,
    /// 是否写入历史文件。
    ///
    /// `/history` 命令回显的历史消息属于临时展示数据，
    /// 应设置为 `false`，避免被重复写入历史造成污染。
    persist_to_history: bool,
}

/// 历史文件中的对话条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryConversation {
    /// 对话角色：`user` / `assistant` / `error`。
    role: String,
    /// 对话内容。
    content: String,
}

/// 历史文件中的单次会话快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistorySession {
    /// 会话记录时间。
    timestamp: String,
    /// 会话中的完整消息列表。
    conversations: Vec<HistoryConversation>,
}

/// 历史文件中的单日记录结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryRecord {
    /// 日期，例如 `2026-2-8`。
    date: String,
    /// 当前会话使用的模型名称。
    model: String,
    /// 历史会话列表（保持与示例中的 `History` 字段命名一致）。
    #[serde(rename = "History")]
    history: Vec<HistorySession>,
}

/// 历史文件根结构。
///
/// 采用数组包装，便于后续扩展多日记录或多模型记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HistoryFile {
    records: Vec<HistoryRecord>,
}

/// 历史选择界面的单条列表项。
#[derive(Debug, Clone)]
struct HistoryListItem {
    /// 会话日期。
    date: String,
    /// 会话模型名。
    model: String,
    /// 会话时间戳。
    timestamp: String,
    /// 会话消息总数。
    message_count: usize,
    /// 会话原始消息，用于按回车后回显到对话区。
    conversations: Vec<HistoryConversation>,
}

/// 历史选择界面状态。
#[derive(Debug, Clone)]
struct HistoryBrowserState {
    /// 可选择的会话列表。
    items: Vec<HistoryListItem>,
    /// 当前选中项索引。
    selected: usize,
}

/// 最近一次失败的摘要信息（用于状态栏展示）。
#[derive(Debug, Clone)]
struct FailureSummary {
    trace_id: String,
    reason: String,
}

/// 后台补全线程向主线程回传的事件。
///
/// 设计原因：
/// - TUI 主循环必须保持可响应输入（例如 `/cancel`、滚动、快捷键）；
/// - 模型请求和重试在后台线程执行，主线程只消费事件并刷新界面。
#[derive(Debug)]
enum CompletionWorkerEvent {
    Stream(ModelStreamEvent),
    Completed(Result<(), String>),
}

/// 当前正在进行中的模型请求状态。
#[derive(Debug)]
struct ActiveCompletion {
    trace_id: String,
    receiver: Receiver<CompletionWorkerEvent>,
    cancel_flag: Arc<AtomicBool>,
    user_message_index: usize,
    assistant_message_index: usize,
    received_delta: bool,
    last_tool_progress: Option<String>,
    started_at: Instant,
}

const WRITE_APPROVAL_OPTIONS: [&str; 3] = ["1. 同意", "2. 不同意", "3. 同意之后一切修改"];
/// 当识别为“代码变更请求”时，附加到发送给模型的执行约束提示。
///
/// 目标是把“先口头确认再行动”改为“直接工具执行 + TUI 审批写入”。
const EXECUTION_ENFORCER_SUFFIX: &str = r#"

[执行约束]
这是代码变更请求。请直接执行：
1. 先调用 SearchFileTool/ReadTool 定位并确认目标代码。
2. 需要改动时直接调用 WriteTool 暂存变更，不要在聊天里先请求“允许写入”。
3. 写入审批由系统通过 /approve /reject 处理，你只需完成工具调用与结果说明。
4. 若首次写入失败，请报告错误并继续尝试可行替代，不要只给计划。"#;
/// 当检测到“代码请求但未执行工具”时，附加的补偿提示。
///
/// 用于在同一请求内自动纠偏，避免模型连续输出“会继续执行”却不落盘。
const EXECUTION_RETRY_SUFFIX: &str = r#"

[强制执行补偿]
上一轮没有实际工具执行。此轮必须：
1. 至少调用一次 SearchFileTool 或 ReadTool 确认目标代码；
2. 再调用 WriteTool 暂存改动；
3. 不允许只回复计划或承诺语句。"#;
/// 当请求明确要求“写入/改动代码”时，附加的写入硬约束。
const WRITE_EXECUTION_ENFORCER_SUFFIX: &str = r#"

[写入硬约束]
本请求要求实际改动代码。你必须至少调用一次 WriteTool 暂存补丁；
若未调用 WriteTool，本轮输出将被判定为失败并自动重试。"#;

/// 待确认写入的交互菜单状态。
#[derive(Debug, Clone)]
struct WriteApprovalPrompt {
    trace_id: String,
    selected: usize,
    files: Vec<String>,
}

pub struct OrderTui<'a> {
    /// 全局退出标记。
    exit: &'a AtomicBool,
    /// 输入组件状态。
    input_state: InputState,
    /// 输入光标闪烁时钟。
    last_tick: Instant,
    /// 预留上下文剩余量。
    context_remaining: u32,
    /// 分层上下文管理器。
    ///
    /// 负责短期上下文裁剪、中期摘要生成与长期记忆持久化。
    context_manager: ContextManager,
    /// 回车后待处理的输入文本。
    pending_command: Option<String>,
    /// 与大模型通信的连接。
    ///
    /// 这里使用 `Option` 的原因是延迟初始化：
    /// 只有用户第一次发送非命令文本时才尝试创建连接。
    connection: Option<Connection>,
    /// 对话消息流。
    ///
    /// 消息分左右展示：用户在右侧，LLM 与错误在左侧。
    messages: Vec<ChatMessage>,
    /// 当前运行会话的起始时间。
    ///
    /// 用于将本次运行期间的消息归并到同一个 `History` 会话节点。
    session_timestamp: String,
    /// 历史选择界面状态。
    ///
    /// 当该字段为 `Some` 时，主界面切换为历史会话列表浏览模式。
    history_browser: Option<HistoryBrowserState>,
    /// 对话区域滚动偏移量。
    ///
    /// 0 表示显示最新消息（底部），大于 0 表示向上滚动的行数。
    conversation_scroll: usize,
    /// 最近一次失败摘要（用于状态栏快速定位）。
    last_failure: Option<FailureSummary>,
    /// 当前是否存在正在执行的流式请求。
    active_completion: Option<ActiveCompletion>,
    /// 写入确认菜单状态；有值时输入会被菜单优先消费。
    write_approval_prompt: Option<WriteApprovalPrompt>,
    /// 会话级“自动同意后续所有写入”开关。
    approve_all_writes: bool,
    /// 当前是否已开启鼠标捕获。
    ///
    /// 在 Windows 控制台中，未初始化就直接执行 `DisableMouseCapture`
    /// 会触发 `Initial console modes not set`，因此这里显式跟踪状态，
    /// 只在“确实开启过”时再关闭。
    mouse_capture_enabled: bool,
}

impl Default for OrderTui<'_> {
    fn default() -> Self {
        let now = Local::now();
        Self {
            exit: &EXIT,
            input_state: InputState::default(),
            last_tick: Instant::now(),
            context_remaining: 100,
            context_manager: ContextManager::new(),
            pending_command: None,
            connection: None,
            messages: Vec::new(),
            session_timestamp: now.format("%Y-%-m-%-d %H:%M:%S").to_string(),
            history_browser: None,
            conversation_scroll: 0,
            last_failure: None,
            active_completion: None,
            write_approval_prompt: None,
            approve_all_writes: false,
            mouse_capture_enabled: false,
        }
    }
}

impl OrderTui<'_> {
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        // 主对话界面默认不启用鼠标捕获，优先保证“可直接框选历史文本进行复制”。
        // 注意：这里不能无条件执行 DisableMouseCapture，Windows 下未初始化时会报错。

        // 先渲染一次主界面，避免启动阶段的 Codex 探测阻塞导致黑屏。
        terminal.draw(|frame| self.draw(frame))?;

        // 启动后默认尝试使用 Codex：
        // - 仅当用户未提供任何模型配置文件时才触发（避免覆盖用户偏好）；
        // - 探测失败不影响主流程，仅作为“尽量启用 Codex”的优化路径。
        if let Err(error) = self.try_auto_configure_codex_on_startup(terminal) {
            // 在 TUI 模式下直接弹错误消息会打断主界面体验，
            // 因此这里只做轻量日志，方便排查即可。
            eprintln!("auto configure codex failed: {error}");
        }

        // 若启动探测发生阻塞，需要重置闪烁时钟，避免首帧就快速闪烁。
        self.last_tick = Instant::now();

        // 降低 tick 间隔，保证流式增量渲染时界面刷新更及时。
        let tick_rate = Duration::from_millis(100);
        while !get_exit().load(Ordering::Relaxed) {
            self.poll_active_completion_events();
            terminal.draw(|frame| self.draw(frame))?;

            let timeout = tick_rate
                .checked_sub(self.last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0));

            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) => {
                        self.handle_key_event(&key);
                        self.process_pending_command(terminal)?;
                        self.poll_active_completion_events();
                        self.input_state.set_cursor_visible(true);
                        self.last_tick = Instant::now();
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse_event(&mouse);
                    }
                    _ => {}
                }
            }

            if self.last_tick.elapsed() >= tick_rate {
                self.input_state.toggle_cursor_visibility();
                self.last_tick = Instant::now();
            }
        }

        self.set_mouse_capture(false)?;
        terminal.clear()?;
        Ok(())
    }

    /// 统一管理鼠标捕获状态，避免重复/非法切换导致控制台报错。
    fn set_mouse_capture(&mut self, enabled: bool) -> anyhow::Result<()> {
        if self.mouse_capture_enabled == enabled {
            return Ok(());
        }

        if enabled {
            execute!(std::io::stdout(), EnableMouseCapture)?;
        } else {
            execute!(std::io::stdout(), DisableMouseCapture)?;
        }
        self.mouse_capture_enabled = enabled;
        Ok(())
    }

    /// 启动时尝试启用 Codex，并在可用时落盘到 `.order/model.json`。
    ///
    /// 设计目标：
    /// - “默认尝试 Codex”，让首次启动即具备更强的编码模型能力；
    /// - 不破坏用户显式配置：只在未检测到任何模型配置文件时才自动写入；
    /// - 写入后立即刷新界面，让 `Model` 面板反映最新配置。
    fn try_auto_configure_codex_on_startup(
        &mut self,
        terminal: &mut DefaultTerminal,
    ) -> anyhow::Result<()> {
        if self.has_any_model_config_file()? {
            return Ok(());
        }

        let codex_model = env::var("CODEX_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        let codex_base_url = env::var("CODEX_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                env::var("OPENAI_BASE_URL")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_default();

        // 启动阶段希望尽量让 Codex 成为默认模型，但又要区分"无 Key"与"探测失败"两类场景：
        // - 无 Key：仍写入默认配置，便于用户补充 Key 后直接使用；
        // - 探测失败（网络/API 错误）：仍写入默认配置，避免阻塞启动，但给出友好提示。
        match self.probe_codex_availability(&codex_model, &codex_base_url) {
            Ok(Some(api_key)) => {
                let config_path = self.model_config_path()?;
                self.write_model_config_file(
                    &config_path,
                    "codex",
                    &codex_model,
                    &codex_base_url,
                    &api_key,
                )?;
                self.connection = None;

                // 立刻刷新一次，让用户看到 Model 面板变化（`codex/<model>`）。
                terminal.draw(|frame| self.draw(frame))?;
            }
            Ok(None) => {
                let config_path = self.model_config_path()?;
                self.write_model_config_file(
                    &config_path,
                    "codex",
                    &codex_model,
                    &codex_base_url,
                    "",
                )?;
                self.connection = None;

                // 无可用 Key 时仍给出默认 Codex 配置，避免“未配置模型”导致无法发送。
                self.push_chat_message(
                    ChatRole::Error,
                    "未检测到可用的 OpenAI Key（CODEX_API_KEY / OPENAI_API_KEY），已写入默认 Codex 配置，请补充 Key 后重试"
                        .to_string(),
                    false,
                );

                // 刷新界面，展示最新模型与提示信息。
                terminal.draw(|frame| self.draw(frame))?;
            }
            Err(error) => {
                // 探测失败时仍写入默认配置，避免阻塞启动流程。
                // 这样做的原因：网络问题或 API 暂时不可用不应阻止用户使用 Order。
                let config_path = self.model_config_path()?;
                self.write_model_config_file(
                    &config_path,
                    "codex",
                    &codex_model,
                    &codex_base_url,
                    "",
                )?;
                self.connection = None;

                // 给出友好提示，说明探测失败但已写入默认配置。
                self.push_chat_message(
                    ChatRole::Error,
                    format!(
                        "Codex 探测失败（{}），已写入默认配置。请检查网络或 API Key 后重试",
                        error
                    ),
                    false,
                );

                // 刷新界面，展示最新模型与提示信息。
                terminal.draw(|frame| self.draw(frame))?;
            }
        }

        Ok(())
    }

    /// 判断用户是否已提供“任何形式”的模型配置文件。
    ///
    /// 之所以不直接调用 `get_current_model_info()`：
    /// - 该函数会在环境变量存在时生成兜底模型；
    /// - 启动自动写配置时，我们只想在“完全没有配置文件”的情况下触发。
    fn has_any_model_config_file(&self) -> anyhow::Result<bool> {
        // 若用户通过环境变量显式指定了 provider/model，也视为“已有配置”。
        // 这样做可以避免启动时自动写配置干扰容器/CI 场景下的环境变量驱动配置。
        let read_non_empty_env = |key: &str| -> Option<String> {
            env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let explicit_provider = read_non_empty_env("ORDER_MODEL_PROVIDER")
            .or_else(|| read_non_empty_env("ORDER_PROVIDER"));
        let explicit_model =
            read_non_empty_env("ORDER_MODEL_NAME").or_else(|| read_non_empty_env("ORDER_MODEL"));
        if explicit_provider.is_some() && explicit_model.is_some() {
            return Ok(true);
        }

        if let Ok(explicit_path) = env::var("ORDER_MODEL_CONFIG") {
            let path = explicit_path.trim();
            if !path.is_empty() && PathBuf::from(path).exists() {
                return Ok(true);
            }
        }

        match get_current_model_info_from_config() {
            Ok(Some(_)) => return Ok(true),
            Ok(None) => {}
            Err(error) => {
                // 配置文件可能为空或已损坏，此时允许继续自动探测，
                // 以免“有文件但无可用模型”导致启动后无法启用 Codex。
                eprintln!("invalid model config ignored during startup: {error}");
            }
        }

        Ok(false)
    }

    fn draw(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_key_event(&mut self, key: &KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // 写入确认菜单是强交互状态：必须先完成同意/拒绝决策，避免输入误发到主对话。
        if self.write_approval_prompt.is_some() {
            self.handle_write_approval_key_event(key);
            return;
        }

        // 若当前处于历史选择界面，优先消费历史浏览相关按键。
        if self.history_browser.is_some() {
            self.handle_history_browser_key_event(key);
            return;
        }

        match key.code {
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    && CURRENT_FOCUS == FocusStatus::InputWidget
                {
                    // TODO: 后续可支持 Shift+Enter 多行输入。
                } else if CURRENT_FOCUS == FocusStatus::InputWidget {
                    if self.input_state.show_completion {
                        self.input_state.confirm_completion();
                    } else {
                        // 回车提交输入内容，由统一入口处理。
                        //
                        // 这里先 `trim` 再入队，避免把纯空白字符当成有效输入。
                        let input = self.input_state.input.trim().to_string();
                        if !input.is_empty() {
                            self.pending_command = Some(input);
                        }
                        self.input_state.clear();
                    }
                } else {
                    self.input_state.clear();
                }
            }
            KeyCode::Tab if CURRENT_FOCUS == FocusStatus::InputWidget => {
                if self.input_state.show_completion {
                    self.input_state.confirm_completion();
                }
            }
            KeyCode::Esc if CURRENT_FOCUS == FocusStatus::InputWidget => {
                if self.input_state.show_completion {
                    self.input_state.cancel_completion();
                }
            }
            KeyCode::Up if CURRENT_FOCUS == FocusStatus::InputWidget => {
                if self.input_state.show_completion {
                    self.input_state.completion_up();
                }
            }
            KeyCode::Down if CURRENT_FOCUS == FocusStatus::InputWidget => {
                if self.input_state.show_completion {
                    self.input_state.completion_down();
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.active_completion.is_some() {
                    self.cancel_active_completion("已发送取消信号（Ctrl+C）".to_string());
                } else {
                    self.exit.store(true, Ordering::Relaxed);
                }
            }
            KeyCode::Char(to_insert) if CURRENT_FOCUS == FocusStatus::InputWidget => {
                self.input_state.insert_char(to_insert);
            }
            KeyCode::Backspace if CURRENT_FOCUS == FocusStatus::InputWidget => {
                self.input_state.delete_char();
            }
            KeyCode::Left if CURRENT_FOCUS == FocusStatus::InputWidget => {
                self.input_state.move_cursor_left();
            }
            KeyCode::Right if CURRENT_FOCUS == FocusStatus::InputWidget => {
                self.input_state.move_cursor_right();
            }
            _ => {}
        }
    }

    /// 处理“写入确认菜单”的按键事件。
    ///
    /// 交互约束：
    /// - 仅支持 `Up` / `Down` / `Enter`，与需求中的方向键 + 回车保持一致；
    /// - `Enter` 会立刻执行对应动作，确保决策可追踪且不悬空。
    fn handle_write_approval_key_event(&mut self, key: &KeyEvent) {
        // 保留全局退出快捷键，避免菜单态下出现“无法退出”的死锁体验。
        if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.exit.store(true, Ordering::Relaxed);
            return;
        }

        match key.code {
            KeyCode::Up => {
                if let Some(prompt) = self.write_approval_prompt.as_mut() {
                    if prompt.selected == 0 {
                        prompt.selected = WRITE_APPROVAL_OPTIONS.len().saturating_sub(1);
                    } else {
                        prompt.selected = prompt.selected.saturating_sub(1);
                    }
                }
            }
            KeyCode::Down => {
                if let Some(prompt) = self.write_approval_prompt.as_mut() {
                    prompt.selected = (prompt.selected + 1) % WRITE_APPROVAL_OPTIONS.len();
                }
            }
            KeyCode::Enter => {
                let Some(prompt) = self.write_approval_prompt.take() else {
                    return;
                };
                match prompt.selected {
                    0 => {
                        self.approve_pending_writes_by_trace_id(&prompt.trace_id);
                    }
                    1 => {
                        self.reject_pending_writes_by_trace_id(&prompt.trace_id);
                    }
                    2 => {
                        self.approve_all_writes = true;
                        self.push_chat_message(
                            ChatRole::Llm,
                            "已开启“同意之后一切修改”（仅本次会话生效）".to_string(),
                            false,
                        );
                        self.approve_pending_writes_by_trace_id(&prompt.trace_id);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// 处理鼠标事件。
    ///
    /// 说明：
    /// - 主对话界面默认关闭鼠标捕获以支持文本框选复制；
    /// - 当启用鼠标捕获（例如 editor 场景）时，仍可复用本逻辑处理滚动。
    ///
    /// 支持操作：
    /// - 鼠标滚轮向上：向上滚动对话区
    /// - 鼠标滚轮向下：向下滚动对话区（向最新消息方向）
    fn handle_mouse_event(&mut self, mouse: &MouseEvent) {
        if self.history_browser.is_some() {
            return;
        }

        if self.messages.is_empty() {
            return;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.conversation_scroll = self.conversation_scroll.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                self.conversation_scroll = self.conversation_scroll.saturating_sub(3);
            }
            _ => {}
        }
    }

    /// 处理历史选择界面的按键事件。
    ///
    /// 支持按键：
    /// - `Up` / `Down`：移动选择
    /// - `Enter`：加载选中会话到对话区
    /// - `Esc`：退出历史选择界面
    fn handle_history_browser_key_event(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.history_browser = None;
            }
            KeyCode::Up => {
                if let Some(browser) = self.history_browser.as_mut() {
                    if browser.items.is_empty() {
                        return;
                    }
                    if browser.selected == 0 {
                        browser.selected = browser.items.len().saturating_sub(1);
                    } else {
                        browser.selected = browser.selected.saturating_sub(1);
                    }
                }
            }
            KeyCode::Down => {
                if let Some(browser) = self.history_browser.as_mut() {
                    if browser.items.is_empty() {
                        return;
                    }
                    browser.selected = (browser.selected + 1) % browser.items.len();
                }
            }
            KeyCode::Enter => {
                if let Some(selected_item) = self.selected_history_item().cloned() {
                    self.history_browser = None;
                    self.push_chat_message(
                        ChatRole::Llm,
                        format!(
                            "已加载历史会话：{} {} {}（{} 条消息）",
                            selected_item.date,
                            selected_item.model,
                            selected_item.timestamp,
                            selected_item.message_count
                        ),
                        false,
                    );

                    for conversation in selected_item.conversations {
                        match conversation.role.as_str() {
                            "user" => {
                                self.push_chat_message(ChatRole::User, conversation.content, false)
                            }
                            "assistant" => {
                                self.push_chat_message(ChatRole::Llm, conversation.content, false)
                            }
                            _ => {
                                self.push_chat_message(ChatRole::Error, conversation.content, false)
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// 统一消费输入框提交内容。
    ///
    /// 处理规则：
    /// - 已知命令（如 `/editor`、`/exit`）走本地命令分支；
    /// - 未知的 `/xxx` 仍视为命令输入，不发送给 LLM；
    /// - 非命令文本会通过 `Connection` 内部的 `client` 发送到 LLM。
    fn process_pending_command(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let Some(command) = self.pending_command.take() else {
            return Ok(());
        };

        if command.starts_with('/') {
            self.process_command(&command, terminal)?;
        } else {
            self.process_plain_input(command);
        }
        Ok(())
    }

    /// 处理普通输入（非 `/` 命令）。
    fn process_plain_input(&mut self, input: String) {
        // 同一时刻只允许一个模型请求，避免多路流式结果交错污染渲染与上下文状态。
        if self.active_completion.is_some() {
            self.push_chat_message(
                ChatRole::Error,
                "当前已有进行中的请求，请先使用 /cancel 或 Ctrl+C 取消".to_string(),
                false,
            );
            return;
        }

        // 发送新消息时重置滚动，显示最新内容。
        self.conversation_scroll = 0;

        // 改为后台线程流式执行，主循环继续可响应输入和中断。
        if let Err(error) = self.start_streaming_completion(input) {
            let error_msg = error.to_string();
            if error_msg.contains("API Key 未配置") {
                self.push_chat_message(
                    ChatRole::Error,
                    format!(
                        "{}\n\n配置方式：\n1. 设置环境变量 CODEX_API_KEY 或 OPENAI_API_KEY\n2. 或在 .order/model.json 中设置 token 字段",
                        error_msg
                    ),
                    false,
                );
            } else {
                self.push_chat_message(ChatRole::Error, format!("发送失败：{error}"), false);
            }
        }
    }

    /// 处理 `/` 命令。
    fn process_command(
        &mut self,
        command_line: &str,
        terminal: &mut DefaultTerminal,
    ) -> anyhow::Result<()> {
        let mut segments = command_line.split_whitespace();
        let Some(command) = segments.next() else {
            return Ok(());
        };

        // 请求进行中时限制高风险命令，避免状态交错导致界面与上下文不一致。
        if self.active_completion.is_some() && !matches!(command, "/cancel" | "/status" | "/exit") {
            self.push_chat_message(
                ChatRole::Error,
                "当前请求进行中，仅支持 /cancel、/status、/exit".to_string(),
                false,
            );
            return Ok(());
        }

        match command {
            "/editor" => self.launch_editor(terminal)?,
            "/exit" => self.exit.store(true, Ordering::Relaxed),
            "/cancel" => {
                if self.active_completion.is_some() {
                    self.cancel_active_completion("已发送取消信号（/cancel）".to_string());
                } else {
                    self.push_chat_message(
                        ChatRole::Llm,
                        "当前没有可取消的进行中请求".to_string(),
                        false,
                    );
                }
            }
            "/approve" => {
                let Some(trace_id) = segments.next() else {
                    self.push_chat_message(
                        ChatRole::Error,
                        "用法：/approve <trace_id>".to_string(),
                        false,
                    );
                    return Ok(());
                };
                self.approve_pending_writes_by_trace_id(trace_id);
            }
            "/reject" => {
                let Some(trace_id) = segments.next() else {
                    self.push_chat_message(
                        ChatRole::Error,
                        "用法：/reject <trace_id>".to_string(),
                        false,
                    );
                    return Ok(());
                };
                self.reject_pending_writes_by_trace_id(trace_id);
            }
            "/rollback" => {
                let guard = ExecutionGuard::default();
                match segments.next() {
                    Some(trace_id) => match guard.rollback(trace_id) {
                        Ok(result) => self.push_chat_message(
                            ChatRole::Llm,
                            format!(
                                "已回滚快照（trace_id={}），影响文件数={}：\n{}",
                                result.trace_id,
                                result.files.len(),
                                result
                                    .files
                                    .iter()
                                    .map(|path| format!("- {path}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            ),
                            false,
                        ),
                        Err(error) => self.push_chat_message(
                            ChatRole::Error,
                            format!("回滚失败：{error}"),
                            false,
                        ),
                    },
                    None => match guard.rollback_last() {
                        Ok(Some(result)) => self.push_chat_message(
                            ChatRole::Llm,
                            format!(
                                "已回滚最近一次快照（trace_id={}），影响文件数={}：\n{}",
                                result.trace_id,
                                result.files.len(),
                                result
                                    .files
                                    .iter()
                                    .map(|path| format!("- {path}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            ),
                            false,
                        ),
                        Ok(None) => self.push_chat_message(
                            ChatRole::Error,
                            "未找到可回滚的快照".to_string(),
                            false,
                        ),
                        Err(error) => self.push_chat_message(
                            ChatRole::Error,
                            format!("回滚失败：{error}"),
                            false,
                        ),
                    },
                }
            }
            "/capability" => {
                if let Err(error) = self.handle_capability_command(&mut segments) {
                    self.push_chat_message(
                        ChatRole::Error,
                        format!("能力缓存操作失败：{error}"),
                        false,
                    );
                }
            }
            "/status" => {
                if let Err(error) = self.show_status_summary() {
                    self.push_chat_message(
                        ChatRole::Error,
                        format!("状态查询失败：{error}"),
                        false,
                    );
                }
            }
            "/settings" => {
                // 配置入口：默认探测 Codex，并在可用时写入模型配置文件。
                //
                // 之所以做成显式命令而不是启动即探测：
                // - 避免每次启动都产生额外的网络请求和模型消耗；
                // - 由用户主动触发更可控，也更符合“配置”这一语义。
                let force = segments
                    .next()
                    .map(|value| value.eq_ignore_ascii_case("force"))
                    .unwrap_or(false);

                if let Err(error) = self.configure_model_with_codex(force, terminal) {
                    self.push_chat_message(ChatRole::Error, format!("配置失败：{error}"), false);
                }
            }
            "/history" => {
                match segments.next() {
                    Some(argument) if argument.eq_ignore_ascii_case("clear") => {
                        match self.clear_history_file() {
                            Ok(()) => {
                                self.push_chat_message(
                                    ChatRole::Llm,
                                    "已清空运行目录下的 History.json".to_string(),
                                    false,
                                );
                            }
                            Err(error) => {
                                self.push_chat_message(
                                    ChatRole::Error,
                                    format!("清空历史失败：{error}"),
                                    false,
                                );
                            }
                        }
                    }
                    maybe_rounds => {
                        // `/history` 无参数：进入可上下选择的历史浏览界面。
                        if maybe_rounds.is_none() {
                            if let Err(error) = self.enter_history_browser() {
                                self.push_chat_message(
                                    ChatRole::Error,
                                    format!("打开历史选择界面失败：{error}"),
                                    false,
                                );
                            }
                        } else {
                            // `/history N`：保持快速回显最近 N 轮能力。
                            let rounds_parse_result =
                                maybe_rounds.map(Self::parse_history_rounds).transpose();

                            match rounds_parse_result {
                                Ok(rounds) => {
                                    let final_rounds = rounds.unwrap_or(5);
                                    if let Err(error) = self.show_history_in_chat(final_rounds) {
                                        self.push_chat_message(
                                            ChatRole::Error,
                                            format!("读取历史失败：{error}"),
                                            false,
                                        );
                                    }
                                }
                                Err(error) => {
                                    self.push_chat_message(
                                        ChatRole::Error,
                                        error.to_string(),
                                        false,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// 执行待确认写入的“同意”动作，并在成功后触发最小验证。
    ///
    /// 返回值语义：
    /// - `true`：写入已落盘（验证失败不影响该结果）；
    /// - `false`：写入未落盘。
    fn approve_pending_writes_by_trace_id(&mut self, trace_id: &str) -> bool {
        if self
            .write_approval_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.trace_id == trace_id)
        {
            self.write_approval_prompt = None;
        }

        let guard = ExecutionGuard::default();
        match guard.apply_pending_writes(trace_id) {
            Ok(result) => {
                self.push_chat_message(
                    ChatRole::Llm,
                    format!(
                        "已确认写入（trace_id={}），影响文件数={}：\n{}",
                        result.trace_id,
                        result.files.len(),
                        result
                            .files
                            .iter()
                            .map(|path| format!("- {path}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                    false,
                );

                // 确认写入后自动跑最小验证闭环，并把结果归档到 `.order/reports/<trace_id>/validation.json`。
                let pipeline = ValidationPipeline::default();
                match pipeline.run(trace_id, &result.files) {
                    Ok(report) => {
                        if report.ok {
                            self.push_chat_message(
                                ChatRole::Llm,
                                format!(
                                    "自动验证通过（耗时={}ms）。报告已写入 `.order/reports/{}/validation.json`",
                                    report.duration_ms, report.trace_id
                                ),
                                false,
                            );
                        } else {
                            self.push_chat_message(
                                ChatRole::Error,
                                format!(
                                    "自动验证失败（耗时={}ms）。失败命令：{}\n报告已写入 `.order/reports/{}/validation.json`\n{}",
                                    report.duration_ms,
                                    report.failed_command.clone().unwrap_or_else(|| "<unknown>".to_string()),
                                    report.trace_id,
                                    report.suggestion.clone().unwrap_or_default()
                                ),
                                false,
                            );
                        }
                    }
                    Err(error) => {
                        self.push_chat_message(
                            ChatRole::Error,
                            format!("自动验证执行失败：{error}"),
                            false,
                        );
                    }
                }
                true
            }
            Err(error) => {
                self.push_chat_message(ChatRole::Error, format!("确认写入失败：{error}"), false);
                false
            }
        }
    }

    /// 执行待确认写入的“拒绝”动作。
    fn reject_pending_writes_by_trace_id(&mut self, trace_id: &str) -> bool {
        if self
            .write_approval_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.trace_id == trace_id)
        {
            self.write_approval_prompt = None;
        }

        let guard = ExecutionGuard::default();
        match guard.reject_pending_writes(trace_id) {
            Ok(()) => {
                self.push_chat_message(
                    ChatRole::Llm,
                    format!("已取消待确认写入：trace_id={trace_id}"),
                    false,
                );
                true
            }
            Err(error) => {
                self.push_chat_message(
                    ChatRole::Error,
                    format!("取消待确认写入失败：{error}"),
                    false,
                );
                false
            }
        }
    }

    /// 请求结束后检查 pending write，并按当前策略决定是弹菜单还是自动同意。
    fn refresh_write_approval_prompt_after_request(&mut self, trace_id: &str) {
        let guard = ExecutionGuard::default();
        let summaries = match guard.list_pending_writes(trace_id) {
            Ok(items) => items,
            Err(error) => {
                self.push_chat_message(
                    ChatRole::Error,
                    format!("读取待确认写入失败：{error}"),
                    false,
                );
                return;
            }
        };

        if summaries.is_empty() {
            return;
        }

        if self.approve_all_writes {
            self.push_chat_message(
                ChatRole::Llm,
                format!(
                    "检测到待确认写入（trace_id={}，文件数={}），已按“同意之后一切修改”自动确认。",
                    trace_id,
                    summaries.len()
                ),
                false,
            );
            if self.approve_pending_writes_by_trace_id(trace_id) {
                return;
            }

            // 自动确认失败时回落到手动模式，避免持续失败却无可操作入口。
            self.approve_all_writes = false;
            self.push_chat_message(
                ChatRole::Error,
                "自动同意失败，已回退为手动确认模式".to_string(),
                false,
            );
        }

        self.open_write_approval_prompt(trace_id, summaries);
    }

    /// 打开写入确认菜单，并带上本次写入的文件摘要。
    fn open_write_approval_prompt(&mut self, trace_id: &str, summaries: Vec<PendingWriteSummary>) {
        let files = summaries
            .iter()
            .map(|item| item.path.clone())
            .collect::<Vec<_>>();
        self.write_approval_prompt = Some(WriteApprovalPrompt {
            trace_id: trace_id.to_string(),
            selected: 0,
            files,
        });
        self.push_chat_message(
            ChatRole::Llm,
            format!(
                "检测到待确认写入（trace_id={}，文件数={}）。请使用 ↑/↓ 选择并按 Enter 确认。",
                trace_id,
                summaries.len()
            ),
            false,
        );
    }

    /// 生成/更新模型配置：优先尝试 Codex。
    ///
    /// 规则：
    /// - 默认不覆盖已有 `.order/model.json`；需要用户显式传入 `force` 才允许覆盖；
    /// - 仅当探测到 Codex 可正常调用时才写入配置，避免把用户带入“不可用模型”的坑；
    /// - 写入后清空已缓存的 `Connection`，保证后续请求按新配置重建连接。
    fn configure_model_with_codex(
        &mut self,
        force: bool,
        terminal: &mut DefaultTerminal,
    ) -> anyhow::Result<()> {
        let config_path = self.model_config_path()?;

        if config_path.exists() && !force {
            self.push_chat_message(
                ChatRole::Llm,
                format!(
                    "检测到已有模型配置：{}；如需覆盖请使用 `/settings force`",
                    config_path.display()
                ),
                false,
            );
            return Ok(());
        }

        let codex_model = env::var("CODEX_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        let codex_base_url = env::var("CODEX_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                env::var("OPENAI_BASE_URL")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_default();

        self.push_chat_message(
            ChatRole::Llm,
            format!("正在探测 Codex 可用性（model={codex_model}）..."),
            false,
        );
        // 先刷新一次界面，让用户看到“探测中”，再开始阻塞等待网络请求。
        terminal.draw(|frame| self.draw(frame))?;

        let probe_result = self.probe_codex_availability(&codex_model, &codex_base_url);
        match probe_result {
            Ok(Some(api_key)) => {
                self.write_model_config_file(
                    &config_path,
                    "codex",
                    &codex_model,
                    &codex_base_url,
                    &api_key,
                )?;
                // 清空缓存连接，确保后续发送走新配置。
                self.connection = None;

                self.push_chat_message(
                    ChatRole::Llm,
                    format!(
                        "Codex 可用：已写入配置 {}，Model 面板将自动更新",
                        config_path.display()
                    ),
                    false,
                );
            }
            Ok(None) => {
                self.push_chat_message(
                    ChatRole::Error,
                    "未检测到可用的 OpenAI Key（CODEX_API_KEY / OPENAI_API_KEY / 当前配置 token），已跳过 Codex 配置"
                        .to_string(),
                    false,
                );
            }
            Err(error) => {
                // 探测失败时不写配置：用户仍可继续使用当前配置或其它 provider。
                self.push_chat_message(ChatRole::Error, format!("Codex 不可用：{error}"), false);
            }
        }

        Ok(())
    }

    /// 探测 Codex 是否可调用。
    ///
    /// 返回值：
    /// - `Ok(Some(api_key))`：探测请求成功，返回可用的 API Key；
    /// - `Ok(None)`：未发现可用于探测的 API Key，直接跳过；
    /// - `Err(_)`：已尝试调用但失败（含超时、鉴权失败、模型不可用等）。
    fn probe_codex_availability(
        &self,
        model_name: &str,
        api_url: &str,
    ) -> anyhow::Result<Option<String>> {
        // 优先读取环境变量；若不存在则尝试复用“当前模型配置”的 token。
        //
        // 这样做的原因：用户可能把 key 写在 `.order/model.json` 里而不是环境变量里，
        // 配置命令应尽量减少额外的手工迁移成本。
        let env_api_key = env::var("CODEX_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            });

        let api_key = if let Some(value) = env_api_key {
            value
        } else if let Ok(Some(model_info)) = get_current_model_info() {
            let provider = model_info.provider_name.trim().to_ascii_lowercase();
            let is_openai_like = matches!(
                provider.as_str(),
                "openai" | "codex" | "openaiapi" | "openai_api"
            );
            if is_openai_like && !model_info.token.trim().is_empty() {
                model_info.token
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        };

        // 探测仅用于确认模型是否可用，因此关闭 tools，避免产生不必要的工具协商开销。
        let mut connection = Connection::new(
            Provider::Codex,
            api_url.to_string(),
            api_key.clone(),
            model_name.to_string(),
            false,
            None,
            None,
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("创建异步运行时失败")?;

        let probe_result = runtime.block_on(async {
            // 使用超时包裹，避免网络异常导致配置流程卡死。
            tokio::time::timeout(
                Duration::from_secs(12),
                connection.response("请只回复 OK".to_string()),
            )
            .await
        });

        match probe_result {
            Ok(Ok(_)) => Ok(Some(api_key)),
            Ok(Err(error)) => Err(error).context("Codex 探测请求失败"),
            Err(_) => Err(anyhow!("Codex 探测超时（12s）")),
        }
    }

    /// 写入模型配置文件（UTF-8 JSON + LF）。
    ///
    /// 目前固定写到 `.order/model.json`，并启用 `support_tools`，让 Codex 更像“编码助手”。
    ///
    /// 同时写入 `default_max_turns`，让用户可直接通过配置调节工具多轮上限。
    fn write_model_config_file(
        &self,
        config_path: &PathBuf,
        provider: &str,
        model: &str,
        api_url: &str,
        token: &str,
    ) -> anyhow::Result<()> {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }

        let value = serde_json::json!({
            "current": {
                "provider": provider,
                "model": model,
                "api_url": api_url,
                "token": token,
                "support_tools": true,
                "model_max_context": 0,
                "model_max_output": 0,
                "model_max_tokens": 0,
                "default_max_turns": 12
            },
            "models": []
        });
        let mut content = serde_json::to_string_pretty(&value).context("序列化模型配置失败")?;
        content.push('\n');

        let report = write_utf8_text_with_report(config_path, &content)
            .with_context(|| format!("写入模型配置失败: {}", config_path.display()))?;
        if report.has_warning() {
            for warning in report.warnings_for(config_path) {
                eprintln!("model config encoding warning: {warning}");
            }
        }
        Ok(())
    }

    /// 计算模型配置文件路径：运行目录下的 `.order/model.json`。
    fn model_config_path(&self) -> anyhow::Result<PathBuf> {
        let current_dir = env::current_dir().context("获取运行目录失败")?;
        Ok(current_dir.join(".order").join("model.json"))
    }

    /// 启动一次新的流式补全请求，并把执行交给后台线程。
    ///
    /// 这里先将用户消息与助手占位消息放入对话区，但默认不持久化：
    /// 只有请求真正成功结束后才转为持久消息，避免取消/失败污染后续上下文。
    fn start_streaming_completion(&mut self, prompt: String) -> anyhow::Result<()> {
        self.ensure_connection()?;
        let chat_history = self.build_chat_history_for_llm(&prompt);
        let enforce_tool_execution = Self::looks_like_code_change_request(&prompt);
        let require_write_tool = Self::should_require_write_tool(&prompt);
        let request_prompt = Self::prepare_request_prompt(&prompt, require_write_tool);
        let connection = self
            .connection
            .as_ref()
            .context("LLM 连接初始化后仍不可用")?
            .clone();

        let trace_id = new_trace_id();
        let workspace_root = workspace_root_best_effort();
        log_event_best_effort(
            &workspace_root,
            AgentEvent::TuiInput {
                ts: ts(),
                trace_id: trace_id.clone(),
                input_len: prompt.chars().count(),
            },
        );

        let user_message_index = self
            .push_chat_message_with_index(ChatRole::User, prompt.clone(), false)
            .context("用户消息入队失败")?;
        let assistant_message_index = self
            .push_chat_message_with_index(ChatRole::Llm, "正在生成...".to_string(), false)
            .context("助手占位消息入队失败")?;

        let (sender, receiver) = mpsc::channel::<CompletionWorkerEvent>();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        Self::spawn_completion_worker(
            connection,
            trace_id.clone(),
            request_prompt,
            enforce_tool_execution,
            require_write_tool,
            chat_history,
            sender,
            cancel_flag.clone(),
        );

        self.active_completion = Some(ActiveCompletion {
            trace_id,
            receiver,
            cancel_flag,
            user_message_index,
            assistant_message_index,
            received_delta: false,
            last_tool_progress: Some("请求已发送，等待首个增量...".to_string()),
            started_at: Instant::now(),
        });
        self.last_failure = None;
        Ok(())
    }

    /// 创建后台线程执行模型请求，避免阻塞 TUI 主事件循环。
    fn spawn_completion_worker(
        mut connection: Connection,
        trace_id: String,
        prompt: String,
        enforce_tool_execution: bool,
        require_write_tool: bool,
        history: Vec<RigMessage>,
        sender: Sender<CompletionWorkerEvent>,
        cancel_flag: Arc<AtomicBool>,
    ) {
        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(CompletionWorkerEvent::Completed(Err(format!(
                        "创建异步运行时失败：{error}"
                    ))));
                    return;
                }
            };

            let result = runtime.block_on(Self::run_completion_worker(
                &mut connection,
                trace_id,
                prompt,
                enforce_tool_execution,
                require_write_tool,
                history,
                sender.clone(),
                cancel_flag,
            ));

            let mapped = result.map(|_| ()).map_err(|error| error.to_string());
            let _ = sender.send(CompletionWorkerEvent::Completed(mapped));
        });
    }

    /// 后台线程中的请求执行逻辑：流式拉取 + 超时控制 + 指数退避重试。
    async fn run_completion_worker(
        connection: &mut Connection,
        trace_id: String,
        prompt: String,
        enforce_tool_execution: bool,
        require_write_tool: bool,
        history: Vec<RigMessage>,
        sender: Sender<CompletionWorkerEvent>,
        cancel_flag: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        const MAX_ATTEMPTS: u32 = 3;
        const REQUEST_TIMEOUT_SECS: u64 = 90;

        for attempt in 1..=MAX_ATTEMPTS {
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(anyhow!("请求已取消"));
            }

            let emitted_delta = Arc::new(AtomicBool::new(false));
            let emitted_delta_for_stream = emitted_delta.clone();
            let emitted_tool_progress = Arc::new(AtomicBool::new(false));
            let emitted_tool_progress_for_stream = emitted_tool_progress.clone();
            let emitted_write_tool = Arc::new(AtomicBool::new(false));
            let emitted_write_tool_for_stream = emitted_write_tool.clone();
            let sender_for_stream = sender.clone();
            let request_prompt = if enforce_tool_execution && attempt > 1 {
                format!("{prompt}{EXECUTION_RETRY_SUFFIX}")
            } else {
                prompt.clone()
            };

            let result = tokio::time::timeout(
                Duration::from_secs(REQUEST_TIMEOUT_SECS),
                connection.response_with_history_streamed_traced(
                    trace_id.clone(),
                    request_prompt,
                    history.clone(),
                    cancel_flag.as_ref(),
                    move |event| {
                        if matches!(event, ModelStreamEvent::Delta { .. }) {
                            emitted_delta_for_stream.store(true, Ordering::Relaxed);
                        }
                        if let ModelStreamEvent::ToolProgress { message } = &event {
                            emitted_tool_progress_for_stream.store(true, Ordering::Relaxed);
                            if message.contains("WriteTool") {
                                emitted_write_tool_for_stream.store(true, Ordering::Relaxed);
                            }
                        }
                        let _ = sender_for_stream.send(CompletionWorkerEvent::Stream(event));
                    },
                ),
            )
            .await;

            let error_message = match result {
                Ok(Ok(response)) => {
                    let tool_called = emitted_tool_progress.load(Ordering::Relaxed);
                    let write_tool_called = emitted_write_tool.load(Ordering::Relaxed);
                    let missed_tool_execution = enforce_tool_execution && !tool_called;
                    let missed_write_execution = require_write_tool && !write_tool_called;

                    if missed_tool_execution || missed_write_execution {
                        if attempt < MAX_ATTEMPTS {
                            let reason = if missed_write_execution {
                                "未调用 WriteTool"
                            } else {
                                "未执行任何工具调用"
                            };
                            let delay = Self::retry_backoff_with_jitter(attempt);
                            let _ = sender.send(CompletionWorkerEvent::Stream(
                                ModelStreamEvent::ToolProgress {
                                    message: format!(
                                        "第 {attempt} 次回复{reason}，将在 {}ms 后强制重试",
                                        delay.as_millis()
                                    ),
                                },
                            ));
                            tokio::time::sleep(delay).await;
                            continue;
                        }

                        let fallback_hint = if Self::is_non_executing_commitment_response(&response.content) {
                            "（检测到承诺式文本）"
                        } else {
                            ""
                        };
                        return Err(anyhow!(
                            "模型未满足执行约束{}：{}",
                            fallback_hint,
                            if missed_write_execution {
                                "本轮未调用 WriteTool 暂存补丁"
                            } else {
                                "本轮未触发任何工具调用"
                            }
                        ));
                    }
                    return Ok(());
                }
                Ok(Err(error)) => error.to_string(),
                Err(_) => format!("请求超时（>{REQUEST_TIMEOUT_SECS}s）"),
            };

            if cancel_flag.load(Ordering::Relaxed) {
                return Err(anyhow!("请求已取消"));
            }

            let can_retry = attempt < MAX_ATTEMPTS
                && !emitted_delta.load(Ordering::Relaxed)
                && Self::is_retryable_stream_error(&error_message);
            if can_retry {
                let delay = Self::retry_backoff_with_jitter(attempt);
                let _ = sender.send(CompletionWorkerEvent::Stream(
                    ModelStreamEvent::ToolProgress {
                        message: format!(
                            "第 {attempt} 次请求失败：{}；将在 {}ms 后重试",
                            shorten_reason(&error_message, 80),
                            delay.as_millis()
                        ),
                    },
                ));
                tokio::time::sleep(delay).await;
                continue;
            }

            return Err(anyhow!(error_message));
        }

        Err(anyhow!("请求失败：已超过最大重试次数"))
    }

    /// 判断错误是否属于“用户主动取消”。
    ///
    /// 这里同时兼容中英文关键字，原因是不同 provider / SDK 在取消场景的报错文案并不一致。
    fn is_cancelled_completion_error(error: &str) -> bool {
        let normalized = error.to_ascii_lowercase();
        error.contains("请求已取消")
            || normalized.contains("cancelled")
            || normalized.contains("canceled")
            || normalized.contains("cancel")
    }

    /// 构造实际发送给模型的请求文本。
    ///
    /// 仅当识别为“代码变更请求”时追加执行约束，避免影响普通问答体验。
    fn prepare_request_prompt(prompt: &str, require_write_tool: bool) -> String {
        if Self::looks_like_code_change_request(prompt) {
            if require_write_tool {
                return format!(
                    "{prompt}{EXECUTION_ENFORCER_SUFFIX}{WRITE_EXECUTION_ENFORCER_SUFFIX}"
                );
            }
            return format!("{prompt}{EXECUTION_ENFORCER_SUFFIX}");
        }
        prompt.to_string()
    }

    /// 识别输入是否属于“应直接落代码”的请求。
    ///
    /// 这是启发式检测：宁可多命中少量问答，也要避免在明确改码需求下再次退化为口头承诺。
    fn looks_like_code_change_request(prompt: &str) -> bool {
        let normalized = prompt.to_ascii_lowercase();
        let keywords = [
            "修复", "实现", "修改", "改动", "新增", "重构", "补丁", "写入文件", "改代码",
            "fix", "implement", "modify", "refactor", "patch", "write", "edit",
            "code", "bug", ".rs", "crates/", "cargo", "todo.md",
        ];
        keywords
            .iter()
            .any(|keyword| prompt.contains(keyword) || normalized.contains(keyword))
    }

    /// 判断该请求是否明确要求“实际写入代码补丁”。
    fn should_require_write_tool(prompt: &str) -> bool {
        let normalized = prompt.to_ascii_lowercase();
        [
            "写入",
            "补丁",
            "改代码",
            "改文件",
            "直接改",
            "落盘",
            "修复",
            "实现",
            "修改",
            "write patch",
            "apply patch",
            "edit file",
            "fix",
            "implement",
            "modify",
        ]
        .iter()
        .any(|keyword| prompt.contains(keyword) || normalized.contains(keyword))
    }

    /// 判断回复是否属于“无执行结果的承诺性文本”。
    ///
    /// 用于代码请求自动纠偏：若未出现工具调用且回复仅是承诺语句，则触发强制重试。
    fn is_non_executing_commitment_response(response: &str) -> bool {
        let normalized = response.to_ascii_lowercase();
        let has_commitment = [
            "我会",
            "我将",
            "下一步",
            "开始写补丁",
            "继续执行",
            "已收到同意",
            "还没真正调用",
            "I will",
            "next step",
            "not yet called",
        ]
        .iter()
        .any(|keyword| response.contains(keyword) || normalized.contains(keyword));
        if !has_commitment {
            return false;
        }

        // 一旦包含明确执行痕迹（文件路径、测试命令、trace_id、写入状态），就不视为“空话”。
        ![
            ".rs",
            ".toml",
            "crates/",
            "trace_id=",
            "Staged high-risk write",
            "cargo test",
            "cargo check",
            "已修改",
            "已写入",
            "已完成",
        ]
        .iter()
        .any(|keyword| response.contains(keyword) || normalized.contains(keyword))
    }

    /// 判断错误是否可重试（网络抖动、限流、网关瞬时故障等）。
    fn is_retryable_stream_error(error: &str) -> bool {
        if Self::is_cancelled_completion_error(error) {
            return false;
        }
        let normalized = error.to_ascii_lowercase();
        if Self::extract_retryable_http_status(&normalized).is_some() {
            return true;
        }

        [
            "timeout",
            "timed out",
            "rate limit",
            "connection reset",
            "connection refused",
            "broken pipe",
            "temporarily unavailable",
            "internal server error",
            "service unavailable",
            "gateway timeout",
            "bad gateway",
            "network",
            "dns",
            "transport",
        ]
        .iter()
        .any(|keyword| normalized.contains(keyword))
    }

    /// 从错误文案中提取“可重试”的 HTTP 状态码。
    ///
    /// 只在常见状态字段附近解析数字，避免被 trace_id 这类无关数字误判触发重试。
    fn extract_retryable_http_status(normalized_error: &str) -> Option<u16> {
        const RETRYABLE_STATUS_CODES: [u16; 6] = [408, 429, 500, 502, 503, 504];
        for marker in ["status code", "status=", "\"status\":", "http "] {
            let Some(code) = Self::extract_status_after_marker(normalized_error, marker) else {
                continue;
            };
            if RETRYABLE_STATUS_CODES.contains(&code) {
                return Some(code);
            }
        }
        None
    }

    /// 在指定 marker 之后提取连续数字状态码。
    ///
    /// 这里限定为 3 位数字，确保只匹配标准 HTTP 状态码。
    fn extract_status_after_marker(normalized_error: &str, marker: &str) -> Option<u16> {
        let index = normalized_error.find(marker)?;
        let suffix = normalized_error.get(index + marker.len()..)?;
        let digits = suffix
            .chars()
            .skip_while(|ch| !ch.is_ascii_digit())
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.len() != 3 {
            return None;
        }
        digits.parse::<u16>().ok()
    }

    /// 计算指数退避 + 抖动延迟，避免并发失败时重试风暴。
    fn retry_backoff_with_jitter(attempt: u32) -> Duration {
        let base_ms: u64 = 600;
        let cap_ms: u64 = 8_000;
        let exp = 2u64.saturating_pow(attempt.saturating_sub(1));
        let backoff_ms = (base_ms.saturating_mul(exp)).min(cap_ms);

        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        let jitter_bound = backoff_ms / 3 + 1;
        let jitter_ms = seed % jitter_bound;
        Duration::from_millis(backoff_ms.saturating_add(jitter_ms))
    }

    /// 拉取后台线程事件并增量更新对话区。
    fn poll_active_completion_events(&mut self) {
        let mut buffered = Vec::new();

        if let Some(active) = self.active_completion.as_mut() {
            loop {
                match active.receiver.try_recv() {
                    Ok(event) => buffered.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        buffered.push(CompletionWorkerEvent::Completed(Err(
                            "后台补全线程异常退出".to_string(),
                        )));
                        break;
                    }
                }
            }
        }

        for event in buffered {
            match event {
                CompletionWorkerEvent::Stream(stream_event) => {
                    self.handle_completion_stream_event(stream_event);
                }
                CompletionWorkerEvent::Completed(result) => {
                    self.finalize_active_completion(result);
                    break;
                }
            }
        }
    }

    /// 处理单个流式事件并更新占位消息。
    fn handle_completion_stream_event(&mut self, event: ModelStreamEvent) {
        match event {
            ModelStreamEvent::Delta { content } => {
                let (index, received_delta_before) = match self.active_completion.as_ref() {
                    Some(active) => (active.assistant_message_index, active.received_delta),
                    None => return,
                };
                if let Some(message) = self.messages.get_mut(index) {
                    if received_delta_before {
                        message.content.push_str(&content);
                    } else {
                        message.content = content;
                    }
                }
                if let Some(active) = self.active_completion.as_mut() {
                    active.received_delta = true;
                    active.last_tool_progress = None;
                }
            }
            ModelStreamEvent::ToolProgress { message } => {
                if let Some(active) = self.active_completion.as_mut() {
                    active.last_tool_progress = Some(message);
                }
            }
            ModelStreamEvent::Done => {
                if let Some(active) = self.active_completion.as_mut() {
                    active.last_tool_progress = Some("响应已完成".to_string());
                }
            }
            ModelStreamEvent::Error { message } => {
                if let Some(active) = self.active_completion.as_mut() {
                    active.last_tool_progress = Some(shorten_reason(&message, 80));
                }
            }
        }
    }

    /// 结束当前请求并处理成功/失败收尾逻辑。
    fn finalize_active_completion(&mut self, result: Result<(), String>) {
        let Some(active) = self.active_completion.take() else {
            return;
        };

        let trace_id = active.trace_id.clone();
        let workspace_root = workspace_root_best_effort();
        let output_len = self
            .messages
            .get(active.assistant_message_index)
            .map(|message| message.content.chars().count());

        match result {
            Ok(()) => {
                if let Some(user_message) = self.messages.get_mut(active.user_message_index) {
                    user_message.persist_to_history = true;
                }
                if let Some(assistant_message) =
                    self.messages.get_mut(active.assistant_message_index)
                {
                    assistant_message.persist_to_history = true;
                }

                if let Err(error) = self.persist_history() {
                    let warning = format!("历史写入失败（请检查文件编码）: {error}");
                    eprintln!("{warning}");
                    self.push_chat_message(ChatRole::Error, warning, false);
                }
                if let Err(error) = self.persist_context_memory() {
                    let warning = format!("上下文记忆写入失败（请检查文件编码）: {error}");
                    eprintln!("{warning}");
                    self.push_chat_message(ChatRole::Error, warning, false);
                }

                log_event_best_effort(
                    &workspace_root,
                    AgentEvent::TuiOutput {
                        ts: ts(),
                        trace_id: trace_id.clone(),
                        ok: true,
                        output_len,
                        error: None,
                    },
                );
                self.last_failure = None;
            }
            Err(error_message) => {
                let cancelled = Self::is_cancelled_completion_error(&error_message);
                if cancelled {
                    // 取消场景直接移除当前轮次，确保不会污染后续上下文与历史。
                    self.discard_pending_turn(
                        active.user_message_index,
                        active.assistant_message_index,
                    );
                    self.push_chat_message(
                        ChatRole::Llm,
                        format!("已取消当前请求（trace_id={}）", trace_id),
                        false,
                    );
                    self.last_failure = None;
                } else {
                    let reason = shorten_reason(&error_message, 80);
                    self.last_failure = Some(FailureSummary {
                        trace_id: trace_id.clone(),
                        reason: reason.clone(),
                    });
                    self.push_chat_message(
                        ChatRole::Error,
                        format!("发送失败（trace_id={}）：{}", trace_id, error_message),
                        false,
                    );
                }

                log_event_best_effort(
                    &workspace_root,
                    AgentEvent::TuiOutput {
                        ts: ts(),
                        trace_id: trace_id.clone(),
                        ok: false,
                        output_len: None,
                        error: Some(shorten_reason(&error_message, 80)),
                    },
                );
            }
        }

        self.refresh_write_approval_prompt_after_request(&trace_id);
    }

    /// 发起取消信号；真正结束由后台线程回传 `Completed` 事件统一收尾。
    fn cancel_active_completion(&mut self, status: String) {
        if let Some(active) = self.active_completion.as_mut() {
            active.cancel_flag.store(true, Ordering::Relaxed);
            active.last_tool_progress = Some(status);
        }
    }

    /// 丢弃当前未完成轮次，避免取消后留下半截消息影响后续会话。
    fn discard_pending_turn(&mut self, user_index: usize, assistant_index: usize) {
        let mut indexes = [user_index, assistant_index];
        indexes.sort_by(|left, right| right.cmp(left));
        for index in indexes {
            if index < self.messages.len() {
                self.messages.remove(index);
            }
        }
    }

    /// 构建发送给 LLM 的历史上下文。
    ///
    /// 关键策略：
    /// - 采用三层上下文：短期上下文 + 中期摘要 + 长期记忆；
    /// - 中期摘要仅在历史发生裁剪时注入，避免短会话噪声；
    /// - 长期记忆按任务 ID 持久化并按需注入；
    /// - 同步回写 `context_remaining`，用于输入框右上角的剩余百分比提示。
    fn build_chat_history_for_llm(&mut self, current_prompt: &str) -> Vec<RigMessage> {
        let context_messages = self.context_messages_for_manager();
        let limits = self.current_model_limits();
        let build_result =
            self.context_manager
                .build_history(current_prompt, &context_messages, limits);

        self.context_remaining = build_result.context_remaining;
        build_result.history
    }

    /// 将运行时消息转换为上下文管理器可消费的结构。
    fn context_messages_for_manager(&self) -> Vec<ContextMessage> {
        self.messages
            .iter()
            .map(|message| {
                let role = match message.role {
                    ChatRole::User => ContextRole::User,
                    ChatRole::Llm => ContextRole::Assistant,
                    ChatRole::Error => ContextRole::Error,
                };
                ContextMessage {
                    role,
                    content: message.content.clone(),
                    persist_to_history: message.persist_to_history,
                }
            })
            .collect()
    }

    /// 读取当前模型上下文限制参数。
    ///
    /// 若读取失败或模型未配置，则回退为 0（交由压缩器使用默认预算）。
    fn current_model_limits(&self) -> ContextModelLimits {
        if let Ok(Some(model_info)) = get_current_model_info() {
            ContextModelLimits {
                model_max_context: model_info.model_max_context,
                model_max_tokens: model_info.model_max_tokens,
                model_max_output: model_info.model_max_output,
            }
        } else {
            ContextModelLimits::default()
        }
    }

    /// 将当前会话增量同步到长期记忆文件。
    fn persist_context_memory(&mut self) -> anyhow::Result<()> {
        let context_messages = self.context_messages_for_manager();
        self.context_manager
            .update_long_term_memory(&context_messages)
    }

    /// 向对话流追加一条消息并返回其索引。
    ///
    /// 返回 `None` 表示内容为空白被忽略。
    fn push_chat_message_with_index(
        &mut self,
        role: ChatRole,
        content: String,
        persist_to_history: bool,
    ) -> Option<usize> {
        const MAX_MESSAGES: usize = 200;

        let normalized_content = content.trim().to_string();
        if normalized_content.is_empty() {
            return None;
        }

        self.messages.push(ChatMessage {
            role,
            content: normalized_content,
            persist_to_history,
        });
        let mut index = self.messages.len().saturating_sub(1);

        if self.messages.len() > MAX_MESSAGES {
            let overflow = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(0..overflow);
            // 溢出裁剪会导致索引左移，这里同步修正返回值。
            index = index.saturating_sub(overflow);
        }

        // 消息入队后立即尝试持久化到运行目录。
        // 失败时会同步回显到对话区，避免“写盘失败但界面无感知”的静默问题。
        if persist_to_history {
            if let Err(error) = self.persist_history() {
                let warning = format!("历史写入失败（请检查文件编码）: {error}");
                eprintln!("{warning}");
                self.push_chat_message(ChatRole::Error, warning, false);
            }
            if let Err(error) = self.persist_context_memory() {
                let warning = format!("上下文记忆写入失败（请检查文件编码）: {error}");
                eprintln!("{warning}");
                self.push_chat_message(ChatRole::Error, warning, false);
            }
        }
        Some(index)
    }

    /// 向对话流追加一条消息。
    ///
    /// 为防止内存无限增长，仅保留最近 200 条消息。
    fn push_chat_message(&mut self, role: ChatRole, content: String, persist_to_history: bool) {
        let _ = self.push_chat_message_with_index(role, content, persist_to_history);
    }

    /// 将当前对话消息持久化到运行目录下的 `History.json`。
    ///
    /// 结构与 `docs/History.md` 示例保持一致：
    /// - 顶层包含 `date`、`model`、`History`
    /// - `History` 内每个元素包含 `timestamp` 与 `conversations`
    fn persist_history(&self) -> anyhow::Result<()> {
        let path = self.history_file_path()?;
        let mut file = Self::read_history_file(&path)?;

        let today = Local::now().format("%Y-%-m-%-d").to_string();
        let model_name = self.current_model_name_for_history();
        let conversations = self.history_conversations();

        let session = HistorySession {
            timestamp: self.session_timestamp.clone(),
            conversations,
        };

        if let Some(record) = file
            .records
            .iter_mut()
            .find(|record| record.date == today && record.model == model_name)
        {
            if let Some(existing_session) = record
                .history
                .iter_mut()
                .find(|history| history.timestamp == self.session_timestamp)
            {
                existing_session.conversations = session.conversations;
            } else {
                record.history.push(session);
            }
        } else {
            file.records.push(HistoryRecord {
                date: today,
                model: model_name,
                history: vec![session],
            });
        }

        Self::write_history_file(&path, &file)
    }

    /// 计算历史文件路径：运行目录下的 `History.json`。
    fn history_file_path(&self) -> anyhow::Result<PathBuf> {
        let current_dir = std::env::current_dir().context("获取运行目录失败")?;
        Ok(current_dir.join("History.json"))
    }

    /// 读取历史文件，不存在时返回空结构。
    fn read_history_file(path: &PathBuf) -> anyhow::Result<HistoryFile> {
        if !path.exists() {
            return Ok(HistoryFile::default());
        }

        let (content, report) = read_utf8_text_with_report(path)
            .with_context(|| format!("读取历史文件失败: {}", path.display()))?;
        if report.has_warning() {
            for warning in report.warnings_for(path) {
                // 历史文件由多处入口读取，统一使用标准错误输出提示编码修复信息。
                eprintln!("history encoding warning: {warning}");
            }
        }

        if content.trim().is_empty() {
            return Ok(HistoryFile::default());
        }

        let records: Vec<HistoryRecord> = serde_json::from_str(&content)
            .with_context(|| format!("解析历史文件失败: {}", path.display()))?;
        Ok(HistoryFile { records })
    }

    /// 将历史结构以 UTF-8 JSON（pretty）写回磁盘。
    fn write_history_file(path: &PathBuf, file: &HistoryFile) -> anyhow::Result<()> {
        let mut content =
            serde_json::to_string_pretty(&file.records).context("序列化历史记录失败")?;
        content.push('\n');
        let report = write_utf8_text_with_report(path, &content)
            .with_context(|| format!("写入历史文件失败: {}", path.display()))?;
        if report.has_warning() {
            for warning in report.warnings_for(path) {
                eprintln!("history encoding warning: {warning}");
            }
        }
        Ok(())
    }

    /// 获取用于历史记录的模型名。
    ///
    /// 若当前模型信息不可用，则写入 `None`，便于排查配置问题。
    fn current_model_name_for_history(&self) -> String {
        if let Ok(Some(model_info)) = get_current_model_info() {
            model_info.model_name
        } else {
            "None".to_string()
        }
    }

    /// 将运行时消息映射为历史文件中的对话结构。
    fn history_conversations(&self) -> Vec<HistoryConversation> {
        self.messages
            .iter()
            // 仅持久化真实会话消息，避免将 `/history` 回显再次写回历史。
            .filter(|message| message.persist_to_history)
            .map(|message| {
                let role = match message.role {
                    ChatRole::User => "user",
                    ChatRole::Llm => "assistant",
                    ChatRole::Error => "error",
                }
                .to_string();

                HistoryConversation {
                    role,
                    content: message.content.clone(),
                }
            })
            .collect()
    }

    /// 解析 `/history N` 的轮数参数。
    ///
    /// - `N` 必须为正整数；
    /// - 取值上限为 100，避免一次回显过多导致界面拥挤。
    fn parse_history_rounds(raw: &str) -> anyhow::Result<usize> {
        let value = raw
            .parse::<usize>()
            .map_err(|_| anyhow!("/history 参数必须是正整数，例如 `/history 5`"))?;

        if value == 0 {
            return Err(anyhow!("/history 参数必须大于 0"));
        }

        Ok(value.min(100))
    }

    /// 清空运行目录下的 `History.json`。
    ///
    /// 这里采用写入空数组 `[]` 的方式清空，
    /// 便于后续继续追加历史记录。
    fn clear_history_file(&self) -> anyhow::Result<()> {
        let path = self.history_file_path()?;
        Self::write_history_file(&path, &HistoryFile::default())
    }

    /// 将 `History.json` 中最近 N 轮会话回显到当前对话区。
    ///
    /// “一轮”定义为一组 `user -> assistant`（或 `error`）的连续消息。
    fn show_history_in_chat(&mut self, rounds: usize) -> anyhow::Result<()> {
        let path = self.history_file_path()?;
        let file = Self::read_history_file(&path)?;

        if file.records.is_empty() {
            self.push_chat_message(
                ChatRole::Error,
                "未找到历史记录（History.json 为空）".to_string(),
                false,
            );
            return Ok(());
        }

        let mut sessions: Vec<&HistorySession> = file
            .records
            .iter()
            .flat_map(|record| record.history.iter())
            .collect();

        if sessions.is_empty() {
            self.push_chat_message(
                ChatRole::Error,
                "未找到历史会话（History 字段为空）".to_string(),
                false,
            );
            return Ok(());
        }

        // 按时间字符串逆序排序（格式固定可进行字典序近似比较）。
        sessions.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));

        let recent_conversations = sessions
            .into_iter()
            .flat_map(|session| session.conversations.iter().cloned())
            .collect::<Vec<HistoryConversation>>();

        let selected = Self::select_recent_rounds(&recent_conversations, rounds);
        if selected.is_empty() {
            self.push_chat_message(
                ChatRole::Error,
                "历史记录中没有可回显的对话内容".to_string(),
                false,
            );
            return Ok(());
        }

        self.push_chat_message(
            ChatRole::Llm,
            format!("以下为最近 {rounds} 轮历史对话回显："),
            false,
        );

        for item in selected {
            match item.role.as_str() {
                "user" => self.push_chat_message(ChatRole::User, item.content, false),
                "assistant" => self.push_chat_message(ChatRole::Llm, item.content, false),
                _ => self.push_chat_message(ChatRole::Error, item.content, false),
            }
        }

        Ok(())
    }

    /// 进入历史选择界面。
    ///
    /// 数据来源：运行目录下 `History.json`。
    fn enter_history_browser(&mut self) -> anyhow::Result<()> {
        let path = self.history_file_path()?;
        let file = Self::read_history_file(&path)?;

        let mut items = file
            .records
            .into_iter()
            .flat_map(|record| {
                let date = record.date;
                let model = record.model;
                record
                    .history
                    .into_iter()
                    .map(move |session| HistoryListItem {
                        date: date.clone(),
                        model: model.clone(),
                        timestamp: session.timestamp,
                        message_count: session.conversations.len(),
                        conversations: session.conversations,
                    })
            })
            .collect::<Vec<HistoryListItem>>();

        if items.is_empty() {
            self.push_chat_message(ChatRole::Error, "没有可选择的历史会话".to_string(), false);
            return Ok(());
        }

        // 新会话放在前面，便于快速查看最近记录。
        items.sort_by(|left, right| {
            right
                .date
                .cmp(&left.date)
                .then_with(|| right.timestamp.cmp(&left.timestamp))
        });

        self.history_browser = Some(HistoryBrowserState { items, selected: 0 });
        Ok(())
    }

    /// 获取当前历史选择界面的选中项。
    fn selected_history_item(&self) -> Option<&HistoryListItem> {
        let browser = self.history_browser.as_ref()?;
        browser.items.get(browser.selected)
    }

    /// 构建历史选择界面的渲染行。
    fn build_history_browser_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(
            "History Browser: Up/Down 选择，Enter 加载，Esc 返回",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))];

        let Some(browser) = self.history_browser.as_ref() else {
            return lines;
        };

        for (index, item) in browser.items.iter().enumerate() {
            let is_selected = index == browser.selected;
            let marker = if is_selected { ">" } else { " " };
            let raw = format!(
                "{} [{}] {} | {} | {} 条消息",
                marker, item.date, item.model, item.timestamp, item.message_count
            );

            let mut text = raw;
            let max_chars = width.saturating_sub(1).max(1);
            if text.chars().count() > max_chars {
                text = text
                    .chars()
                    .take(max_chars.saturating_sub(2))
                    .collect::<String>()
                    + "..";
            }

            let style = if is_selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(Span::styled(text, style)));
        }

        lines
    }

    /// 从历史消息中截取最近 N 轮。
    ///
    /// 算法说明：
    /// - 从尾部向前扫描，遇到 `user` 角色视为一轮起点；
    /// - 收集到 N 个起点后截取对应后缀；
    /// - 若不足 N 轮，则返回全部可用消息。
    fn select_recent_rounds(
        conversations: &[HistoryConversation],
        rounds: usize,
    ) -> Vec<HistoryConversation> {
        if conversations.is_empty() || rounds == 0 {
            return Vec::new();
        }

        let mut user_round_count = 0usize;
        let mut start_index = 0usize;

        for (index, item) in conversations.iter().enumerate().rev() {
            if item.role == "user" {
                user_round_count += 1;
                start_index = index;
                if user_round_count >= rounds {
                    break;
                }
            }
        }

        if user_round_count < rounds {
            conversations.to_vec()
        } else {
            conversations[start_index..].to_vec()
        }
    }

    /// 确保 LLM 连接已初始化。
    ///
    /// 当连接尚未创建时，基于当前模型配置构建一次连接并缓存，后续输入复用同一连接。
    fn ensure_connection(&mut self) -> anyhow::Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }

        let model_info = get_current_model_info()?
            .ok_or_else(|| anyhow!("未配置当前模型信息，无法发送到 LLM"))?;
        let provider = Self::parse_provider(model_info.provider_name.as_str())?;

        self.connection = Some(Connection::new(
            provider,
            model_info.api_url,
            model_info.token,
            model_info.model_name,
            model_info.support_tools,
            // 约定：配置值为 0 表示“使用系统默认上限”，避免每次都必须显式填写。
            if model_info.default_max_turns > 0 {
                Some(model_info.default_max_turns as usize)
            } else {
                None
            },
            model_info.capabilities,
        ));

        Ok(())
    }

    /// 将配置中的 provider 名称映射为连接层枚举。
    ///
    /// 为了兼容不同配置写法，这里支持多个同义值：
    /// - `claude` 与 `anthropic` 统一映射到 `Provider::Claude`
    /// - `openaiapi` 与 `openai_api` 统一映射到 `Provider::OpenAIAPI`
    /// - `codex` 映射到 `Provider::Codex`（内部仍使用 OpenAI 客户端）
    fn parse_provider(provider_name: &str) -> anyhow::Result<Provider> {
        match provider_name.trim().to_ascii_lowercase().as_str() {
            "openai" => Ok(Provider::OpenAI),
            "codex" => Ok(Provider::Codex),
            "claude" | "anthropic" => Ok(Provider::Claude),
            "gemini" => Ok(Provider::Gemini),
            "openaiapi" | "openai_api" => Ok(Provider::OpenAIAPI),
            value => Err(anyhow!("不支持的 provider: {value}")),
        }
    }

    /// 根据给定宽度将消息按显示宽度分段换行。
    ///
    /// 使用 unicode-width 计算实际显示宽度，正确处理中文字符（占2个显示宽度）。
    fn wrap_message(content: &str, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }

        let mut lines = Vec::new();
        for raw_line in content.lines() {
            if raw_line.is_empty() {
                lines.push(String::new());
                continue;
            }

            let mut current = String::new();
            let mut current_width = 0usize;
            for ch in raw_line.chars() {
                let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
                if current_width + ch_width > width && !current.is_empty() {
                    lines.push(current);
                    current = String::new();
                    current_width = 0;
                }
                current.push(ch);
                current_width += ch_width;
            }

            if !current.is_empty() {
                lines.push(current);
            }
        }

        if lines.is_empty() {
            lines.push(String::new());
        }

        lines
    }

    /// 构建对话区域渲染文本，满足“用户右侧、LLM 与错误左侧”的展示要求。
    fn build_conversation_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();

        for message in &self.messages {
            let (prefix, style, is_right_aligned) = match message.role {
                ChatRole::User => (
                    "",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    true,
                ),
                ChatRole::Llm => (
                    "LLM",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                    false,
                ),
                ChatRole::Error => (
                    "Error",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    false,
                ),
            };

            let wrapped = Self::wrap_message(&message.content, width.saturating_sub(2).max(1));

            for (index, segment) in wrapped.into_iter().enumerate() {
                let content = if index == 0 && prefix.is_empty() {
                    segment
                } else if index == 0 {
                    format!("{prefix}: {segment}")
                } else {
                    format!("  {segment}")
                };

                let rendered = if is_right_aligned {
                    let content_width = UnicodeWidthStr::width(content.as_str());
                    let padding = width.saturating_sub(content_width);
                    format!("{}{}", " ".repeat(padding), content)
                } else {
                    content
                };

                lines.push(Line::from(Span::styled(rendered, style)));
            }

            lines.push(Line::from(""));
        }

        lines
    }

    /// 展示最近 24 小时的结构化日志统计（成功率/耗时/重试率）。
    ///
    /// 统计口径：
    /// - 以 `AgentEvent::RequestEnd` 为“完成一次请求”的标记；
    /// - `attempts > 1` 视为发生过降级重试；
    /// - 只统计最近 24 小时（跨天会读取昨日与今日日志文件）。
    fn show_status_summary(&mut self) -> anyhow::Result<()> {
        let workspace_root = workspace_root_best_effort();
        let logs_dir = workspace_root.join(".order").join("logs");
        if !logs_dir.exists() {
            self.push_chat_message(
                ChatRole::Error,
                "未找到日志目录：.order/logs/（尚未产生可统计事件）".to_string(),
                false,
            );
            return Ok(());
        }

        let now = Local::now();
        let today = now.format("%Y%m%d").to_string();
        let yesterday = (now - ChronoDuration::days(1)).format("%Y%m%d").to_string();
        let candidates = [
            logs_dir.join(format!("agent-{yesterday}.log")),
            logs_dir.join(format!("agent-{today}.log")),
        ];

        let cutoff = Utc::now() - ChronoDuration::hours(24);
        let mut total: u64 = 0;
        let mut success: u64 = 0;
        let mut sum_duration_ms: u128 = 0;
        let mut retry: u64 = 0;
        let mut malformed_lines: u64 = 0;

        for path in candidates {
            if !path.exists() {
                continue;
            }
            let (content, report) = read_utf8_text_with_report(&path)
                .with_context(|| format!("读取日志失败: {}", path.display()))?;
            if report.has_warning() {
                for warning in report.warnings_for(&path) {
                    self.push_chat_message(
                        ChatRole::Error,
                        format!("日志编码提醒：{warning}"),
                        false,
                    );
                }
            }
            for line in content.lines() {
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<AgentEvent>(text) else {
                    malformed_lines += 1;
                    continue;
                };
                let AgentEvent::RequestEnd {
                    ts,
                    ok,
                    duration_ms,
                    attempts,
                    ..
                } = event
                else {
                    continue;
                };

                let Ok(parsed) = DateTime::parse_from_rfc3339(&ts) else {
                    malformed_lines += 1;
                    continue;
                };
                let event_time = parsed.with_timezone(&Utc);
                if event_time < cutoff {
                    continue;
                }

                total += 1;
                sum_duration_ms += duration_ms;
                if ok {
                    success += 1;
                }
                if attempts > 1 {
                    retry += 1;
                }
            }
        }

        if malformed_lines > 0 {
            self.push_chat_message(
                ChatRole::Error,
                format!(
                    "日志自检提醒：检测到 {} 行无法解析的事件，请确认日志文件编码为 UTF-8 + LF。",
                    malformed_lines
                ),
                false,
            );
        }

        if total == 0 {
            self.push_chat_message(
                ChatRole::Llm,
                "最近 24 小时内没有可统计的请求记录（RequestEnd 事件为 0）".to_string(),
                false,
            );
            return Ok(());
        }

        let success_rate = (success as f64 / total as f64) * 100.0;
        let avg_duration = sum_duration_ms / total as u128;
        let retry_rate = (retry as f64 / total as f64) * 100.0;

        let mut summary = format!(
            "近 24h 统计：总请求={} 成功={} 成功率={:.2}% 平均耗时={}ms 重试率={:.2}%",
            total, success, success_rate, avg_duration, retry_rate
        );
        if let Some(ref failure) = self.last_failure {
            summary.push_str(&format!(
                "\n最近失败：trace_id={} 原因={}",
                failure.trace_id, failure.reason
            ));
        }
        let approval_mode = if self.approve_all_writes {
            "自动同意（本会话）"
        } else {
            "手动确认"
        };
        summary.push_str(&format!("\n写入同意策略：{approval_mode}"));
        summary.push_str(&format!("\n日志目录：{}", logs_dir.display()));
        if let Err(error) = self.append_capability_status_summary(&mut summary, &workspace_root) {
            summary.push_str(&format!("\n能力诊断失败：{error}"));
        }

        self.push_chat_message(ChatRole::Llm, summary, false);
        Ok(())
    }

    /// 处理能力缓存命令。
    ///
    /// 支持：
    /// - `/capability reset`：重置当前模型对应缓存；
    /// - `/capability reset all`：清空全部能力缓存；
    /// - `/capability reset <provider>`：按 provider 清理；
    /// - `/capability reset <provider> <model>`：按 provider + model 精确清理。
    fn handle_capability_command<'a>(
        &mut self,
        segments: &mut std::str::SplitWhitespace<'a>,
    ) -> anyhow::Result<()> {
        let Some(subcommand) = segments.next() else {
            self.push_chat_message(
                ChatRole::Error,
                "用法：/capability reset [all|<provider>|<provider> <model>]".to_string(),
                false,
            );
            return Ok(());
        };

        if !subcommand.eq_ignore_ascii_case("reset") {
            self.push_chat_message(ChatRole::Error, "仅支持子命令：reset".to_string(), false);
            return Ok(());
        }

        let args = segments.collect::<Vec<_>>();
        if args.len() > 2 {
            self.push_chat_message(
                ChatRole::Error,
                "用法：/capability reset [all|<provider>|<provider> <model>]".to_string(),
                false,
            );
            return Ok(());
        }

        let mut provider_filter: Option<String> = None;
        let mut model_filter: Option<String> = None;
        match args.as_slice() {
            [] => {
                let Some(current) = get_current_model_info()? else {
                    self.push_chat_message(
                        ChatRole::Error,
                        "当前未检测到模型配置，无法推断要重置的 provider/model".to_string(),
                        false,
                    );
                    return Ok(());
                };
                provider_filter = Some(current.provider_name);
                model_filter = Some(current.model_name);
            }
            [one] if one.eq_ignore_ascii_case("all") => {}
            [provider] => {
                provider_filter = Some((*provider).to_string());
            }
            [provider, model] => {
                provider_filter = Some((*provider).to_string());
                model_filter = Some((*model).to_string());
            }
            _ => {}
        }

        let resolver = CapabilityResolver::default();
        let workspace_root = workspace_root_best_effort();
        let removed = resolver.reset_cache_entries(
            &workspace_root,
            provider_filter.as_deref(),
            model_filter.as_deref(),
        )?;
        log_event_best_effort(
            &workspace_root,
            AgentEvent::CapabilityCacheReset {
                ts: ts(),
                provider: provider_filter.clone(),
                model: model_filter.clone(),
                removed,
            },
        );

        let provider_label = provider_filter.unwrap_or_else(|| "*".to_string());
        let model_label = model_filter.unwrap_or_else(|| "*".to_string());
        self.push_chat_message(
            ChatRole::Llm,
            format!(
                "能力缓存重置完成：provider={} model={}，已移除 {} 条记录",
                provider_label, model_label, removed
            ),
            false,
        );
        Ok(())
    }

    /// 将“当前生效能力 + 最近缓存降级原因”拼接到 `/status` 输出中。
    fn append_capability_status_summary(
        &self,
        summary: &mut String,
        workspace_root: &Path,
    ) -> anyhow::Result<()> {
        let Some(model_info) = get_current_model_info()? else {
            summary.push_str("\n当前能力：未检测到模型配置");
            summary.push_str("\n降级缓存：未命中");
            return Ok(());
        };

        let provider = Self::parse_provider(model_info.provider_name.as_str())?;
        let api_url = model_info.api_url.trim();
        let api_url = if api_url.is_empty() {
            None
        } else {
            Some(api_url)
        };

        let resolver = CapabilityResolver::default();
        let negotiated = resolver.resolve(
            workspace_root,
            provider,
            api_url,
            &model_info.model_name,
            model_info.support_tools,
            model_info.capabilities.as_ref(),
        )?;
        let enabled = |value: bool| if value { "on" } else { "off" };
        summary.push_str(&format!(
            "\n当前能力：endpoint={} tools={} stream={} system_preamble={} sources={}",
            negotiated.endpoint.as_str(),
            enabled(negotiated.tools_enabled),
            enabled(negotiated.stream_enabled),
            enabled(negotiated.system_preamble_enabled),
            negotiated.sources.join(",")
        ));

        if let Some(snapshot) = resolver.inspect_cache_entry(
            workspace_root,
            provider,
            api_url,
            &model_info.model_name,
        )? {
            let state = if snapshot.expired {
                "expired"
            } else {
                "active"
            };
            let reason = snapshot
                .reason
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let ttl = snapshot
                .remaining_ttl_seconds
                .map(|seconds| format!("{seconds}s"))
                .unwrap_or_else(|| "0s".to_string());
            summary.push_str(&format!(
                "\n降级缓存：state={} source={} reason={} confidence={:.2} ttl={} first_seen={} last_seen={}",
                state,
                snapshot.source.as_str(),
                reason,
                snapshot.confidence,
                ttl,
                snapshot.first_seen_at,
                snapshot.last_seen_at
            ));
        } else {
            summary.push_str("\n降级缓存：未命中");
        }

        Ok(())
    }

    /// 在主界面上方渲染写入确认菜单。
    ///
    /// 采用弹层而不是普通消息的原因：
    /// - 该交互属于“需要立即决策”的阻断点；
    /// - 明确的焦点态能降低误输入到主对话框的风险。
    fn render_write_approval_prompt(&self, area: Rect, buf: &mut Buffer) {
        let Some(prompt) = self.write_approval_prompt.as_ref() else {
            return;
        };

        let max_width = area.width.saturating_sub(2);
        let max_height = area.height.saturating_sub(2);
        if max_width < 24 || max_height < 8 {
            return;
        }

        let width = max_width.min(78);
        let preview_count = prompt.files.len().min(3);
        let mut height = 11 + preview_count as u16;
        if prompt.files.len() > preview_count {
            height = height.saturating_add(1);
        }
        height = height.min(max_height);

        let popup = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };

        Clear.render(popup, buf);

        let block = Block::bordered()
            .title(" 写入确认 ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(popup);
        block.render(popup, buf);

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let mut lines = vec![
            Line::from(format!("trace_id: {}", prompt.trace_id)),
            Line::from(format!("待确认文件数: {}", prompt.files.len())),
            Line::from(""),
            Line::from("文件预览："),
        ];

        for file in prompt.files.iter().take(preview_count) {
            lines.push(Line::from(format!("- {file}")));
        }
        if prompt.files.len() > preview_count {
            lines.push(Line::from(format!(
                "... 还有 {} 个文件",
                prompt.files.len() - preview_count
            )));
        }

        lines.push(Line::from(""));

        for (index, option) in WRITE_APPROVAL_OPTIONS.iter().enumerate() {
            let selected = index == prompt.selected;
            let prefix = if selected { ">" } else { " " };
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix} {option}"),
                style,
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "↑/↓ 选择，Enter 确认",
            Style::default().fg(Color::DarkGray),
        )));

        let max_lines = inner.height as usize;
        if lines.len() > max_lines {
            lines.truncate(max_lines);
        }

        Paragraph::new(Text::from(lines)).render(inner, buf);
    }

    /// 进入 editor 子界面，退出后回到主界面。
    fn launch_editor(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        // editor 依赖鼠标拖拽与滚轮交互，因此进入 editor 前临时开启鼠标捕获。
        self.set_mouse_capture(true)?;
        let mut editor = Editor::default();
        let run_result = editor.run(terminal);
        // 返回主界面后恢复“可框选文本”的默认行为。
        let restore_result = self.set_mouse_capture(false);
        restore_result?;
        run_result?;
        terminal.clear()?;
        self.last_tick = Instant::now();
        Ok(())
    }
}

/// 截断错误原因，避免状态栏被长文本撑爆。
fn shorten_reason(text: &str, max_chars: usize) -> String {
    let mut line = text.lines().next().unwrap_or(text).trim().to_string();
    if line.starts_with("[trace_id=") {
        if let Some(index) = line.find("] ") {
            line = line[index + 2..].trim().to_string();
        }
    }

    let char_count = line.chars().count();
    if char_count <= max_chars {
        return line;
    }

    let mut shortened = line
        .chars()
        .take(max_chars.saturating_sub(2))
        .collect::<String>();
    shortened.push_str("..");
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_message(role: ChatRole, content: &str, persist_to_history: bool) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            persist_to_history,
        }
    }

    #[test]
    fn build_chat_history_should_skip_current_prompt_duplicate() {
        let mut tui = OrderTui::default();
        tui.messages
            .push(chat_message(ChatRole::User, "第一问", true));
        tui.messages
            .push(chat_message(ChatRole::Llm, "第一答", true));
        tui.messages
            .push(chat_message(ChatRole::User, "第二问", true));

        let history = tui.build_chat_history_for_llm("第二问");
        assert_eq!(
            history,
            vec![RigMessage::user("第一问"), RigMessage::assistant("第一答")]
        );
    }

    #[test]
    fn build_chat_history_should_ignore_error_and_non_persistent_messages() {
        let mut tui = OrderTui::default();
        tui.messages
            .push(chat_message(ChatRole::User, "问题", true));
        tui.messages.push(chat_message(ChatRole::Llm, "回答", true));
        tui.messages
            .push(chat_message(ChatRole::Error, "临时错误", true));
        tui.messages
            .push(chat_message(ChatRole::User, "仅回显消息", false));
        tui.messages
            .push(chat_message(ChatRole::User, "新问题", true));

        let history = tui.build_chat_history_for_llm("新问题");
        assert_eq!(
            history,
            vec![RigMessage::user("问题"), RigMessage::assistant("回答")]
        );
    }

    #[test]
    fn build_chat_history_should_limit_history_size() {
        let mut tui = OrderTui::default();
        for index in 0..130 {
            tui.messages
                .push(chat_message(ChatRole::User, &format!("u{index}"), true));
            tui.messages
                .push(chat_message(ChatRole::Llm, &format!("a{index}"), true));
        }
        tui.messages
            .push(chat_message(ChatRole::User, "当前问题", true));

        let history = tui.build_chat_history_for_llm("当前问题");
        assert_eq!(history.len(), 121);
        assert!(
            format!("{:?}", history[0]).contains("阶段摘要"),
            "历史裁剪后应注入中期摘要"
        );
        assert_eq!(history.get(1), Some(&RigMessage::user("u70")));
        assert_eq!(history.last(), Some(&RigMessage::assistant("a129")));
    }

    #[test]
    fn discard_pending_turn_should_remove_transient_round() {
        let mut tui = OrderTui::default();
        tui.messages
            .push(chat_message(ChatRole::User, "临时提问", false));
        tui.messages
            .push(chat_message(ChatRole::Llm, "正在生成...", false));
        tui.messages
            .push(chat_message(ChatRole::User, "保留消息", true));

        // 取消后必须只删除当前未完成轮次，避免污染后续会话。
        tui.discard_pending_turn(0, 1);

        assert_eq!(tui.messages.len(), 1);
        assert!(matches!(tui.messages[0].role, ChatRole::User));
        assert_eq!(tui.messages[0].content, "保留消息");
    }

    #[test]
    fn is_cancelled_completion_error_should_match_multi_language() {
        assert!(OrderTui::is_cancelled_completion_error("请求已取消"));
        assert!(OrderTui::is_cancelled_completion_error(
            "request canceled by user"
        ));
        assert!(OrderTui::is_cancelled_completion_error(
            "operation cancelled"
        ));
        assert!(!OrderTui::is_cancelled_completion_error("gateway timeout"));
    }

    #[test]
    fn is_retryable_stream_error_should_reject_cancel_and_allow_network_errors() {
        // 用户主动取消不应重试，否则会出现“取消后又自动重发”的违背预期行为。
        assert!(!OrderTui::is_retryable_stream_error(
            "request canceled by user"
        ));
        assert!(!OrderTui::is_retryable_stream_error("请求已取消"));
        assert!(OrderTui::is_retryable_stream_error(
            "503 Service Unavailable"
        ));
        assert!(OrderTui::is_retryable_stream_error(
            "transport error: connection reset by peer"
        ));
        assert!(OrderTui::is_retryable_stream_error(
            "CompletionError: ProviderError: Invalid status code 500 Internal Server Error"
        ));
        assert!(!OrderTui::is_retryable_stream_error("401 unauthorized"));
        assert!(!OrderTui::is_retryable_stream_error(
            "CompletionError: ProviderError: Invalid status code 400 Bad Request"
        ));
    }

    #[test]
    fn extract_retryable_http_status_should_only_accept_retryable_codes() {
        assert_eq!(
            OrderTui::extract_retryable_http_status(
                "completionerror: providererror: invalid status code 500 internal server error"
            ),
            Some(500)
        );
        assert_eq!(
            OrderTui::extract_retryable_http_status("http 429 too many requests"),
            Some(429)
        );
        assert_eq!(
            OrderTui::extract_retryable_http_status("invalid status code 400 bad request"),
            None
        );
    }

    #[test]
    fn prepare_request_prompt_should_append_enforcer_for_code_change_request() {
        let prompt = "请修复 connection.rs 里的重试逻辑";
        let prepared = OrderTui::prepare_request_prompt(prompt, true);
        assert!(prepared.starts_with(prompt));
        assert!(prepared.contains("[执行约束]"));
        assert!(prepared.contains("WriteTool"));
        assert!(prepared.contains("[写入硬约束]"));
    }

    #[test]
    fn prepare_request_prompt_should_keep_plain_prompt_for_non_code_request() {
        let prompt = "帮我总结今天的开发进度";
        let prepared = OrderTui::prepare_request_prompt(prompt, false);
        assert_eq!(prepared, prompt);
    }

    #[test]
    fn should_require_write_tool_should_match_patch_intent() {
        assert!(OrderTui::should_require_write_tool(
            "请直接写入补丁并修改 connection.rs"
        ));
        assert!(!OrderTui::should_require_write_tool(
            "请解释一下当前架构设计"
        ));
    }

    #[test]
    fn is_non_executing_commitment_response_should_detect_empty_commitment() {
        assert!(OrderTui::is_non_executing_commitment_response(
            "已收到同意。我下一步会继续执行，还没真正调用 WriteTool。"
        ));
    }

    #[test]
    fn is_non_executing_commitment_response_should_allow_real_execution_output() {
        assert!(!OrderTui::is_non_executing_commitment_response(
            "已写入 crates/core/src/observability.rs，trace_id=abc123，后续将执行 cargo test。"
        ));
    }

    #[test]
    fn retry_backoff_with_jitter_should_stay_in_expected_range() {
        let attempt1 = OrderTui::retry_backoff_with_jitter(1).as_millis();
        let attempt2 = OrderTui::retry_backoff_with_jitter(2).as_millis();
        let attempt3 = OrderTui::retry_backoff_with_jitter(3).as_millis();

        // 采用 base=600ms，jitter 上界约为 backoff 的 1/3。
        assert!((600..=800).contains(&attempt1));
        assert!((1200..=1600).contains(&attempt2));
        assert!((2400..=3200).contains(&attempt3));
    }

    #[test]
    fn write_approval_prompt_should_support_up_down_wrap_navigation() {
        let mut tui = OrderTui::default();
        tui.write_approval_prompt = Some(WriteApprovalPrompt {
            trace_id: "trace-1".to_string(),
            selected: 0,
            files: vec!["a.rs".to_string()],
        });

        tui.handle_key_event(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            tui.write_approval_prompt
                .as_ref()
                .expect("prompt should keep visible")
                .selected,
            WRITE_APPROVAL_OPTIONS.len() - 1
        );

        tui.handle_key_event(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            tui.write_approval_prompt
                .as_ref()
                .expect("prompt should keep visible")
                .selected,
            0
        );
    }

    #[test]
    fn write_approval_prompt_enter_on_reject_should_not_enable_approve_all() {
        let mut tui = OrderTui::default();
        tui.write_approval_prompt = Some(WriteApprovalPrompt {
            trace_id: "trace-2".to_string(),
            selected: 1,
            files: vec!["b.rs".to_string()],
        });

        tui.handle_key_event(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(tui.write_approval_prompt.is_none());
        assert!(!tui.approve_all_writes);
    }

    #[test]
    fn write_approval_prompt_enter_on_approve_all_should_enable_session_flag() {
        let mut tui = OrderTui::default();
        tui.write_approval_prompt = Some(WriteApprovalPrompt {
            trace_id: "trace-3".to_string(),
            selected: 2,
            files: vec!["c.rs".to_string()],
        });

        tui.handle_key_event(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(tui.write_approval_prompt.is_none());
        assert!(tui.approve_all_writes);
        assert!(
            tui.messages
                .iter()
                .any(|item| item.content.contains("同意之后一切修改")),
            "应给出已开启会话级自动同意的提示"
        );
    }
}

impl Widget for &OrderTui<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let input_height = self.input_state.required_height(area.width);
        let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(input_height)]);
        let [main_area, input_area] = layout.areas(area);
        let status_message = if let Some(active) = self.active_completion.as_ref() {
            let elapsed = active.started_at.elapsed().as_secs();
            let progress = active
                .last_tool_progress
                .clone()
                .unwrap_or_else(|| "流式响应中，Ctrl+C 可取消".to_string());
            Some(format!(
                "进行中({}s) {} {}",
                elapsed, active.trace_id, progress
            ))
        } else {
            self.last_failure
                .as_ref()
                .map(|item| format!("最近失败: {} {}", item.trace_id, item.reason))
        };

        // 历史选择界面优先渲染。
        if self.history_browser.is_some() {
            let history_block = Block::bordered()
                .title(" History ")
                .border_style(Style::default().fg(Color::DarkGray));
            let history_inner = history_block.inner(main_area);
            history_block.render(main_area, buf);

            if history_inner.width > 0 && history_inner.height > 0 {
                let mut lines = self.build_history_browser_lines(history_inner.width as usize);
                let max_visible = history_inner.height as usize;
                if lines.len() > max_visible {
                    lines = lines.into_iter().take(max_visible).collect();
                }
                Paragraph::new(Text::from(lines)).render(history_inner, buf);
            }

            let mut widget = InputWidget::new(&self.input_state);
            widget.set_context_remaining(self.context_remaining);
            if let Some(ref message) = status_message {
                widget.set_status_message(message.clone());
            }
            widget.clone().render(input_area, buf);
            self.render_write_approval_prompt(main_area, buf);
            return;
        }

        // 进入对话模式后：
        // - 隐藏欢迎信息；
        // - 左侧展示 LLM 与错误消息；
        // - 右侧展示用户消息。
        if !self.messages.is_empty() {
            let chat_block = Block::bordered()
                .title(" Conversation ")
                .border_style(Style::default().fg(Color::DarkGray));
            let chat_inner = chat_block.inner(main_area);
            chat_block.render(main_area, buf);

            if chat_inner.width > 0 && chat_inner.height > 0 {
                let conversation_lines = self.build_conversation_lines(chat_inner.width as usize);

                let max_visible_lines = chat_inner.height as usize;
                let total_lines = conversation_lines.len();

                if total_lines > max_visible_lines {
                    // 计算起始位置：从末尾往前推，再减去滚动偏移量
                    // conversation_scroll = 0 时显示最新消息
                    // conversation_scroll > 0 时向上滚动
                    let max_scroll = total_lines.saturating_sub(max_visible_lines);
                    let effective_scroll = self.conversation_scroll.min(max_scroll);
                    let start = max_scroll.saturating_sub(effective_scroll);

                    let visible_lines: Vec<_> = conversation_lines
                        .into_iter()
                        .skip(start)
                        .take(max_visible_lines)
                        .collect();
                    Paragraph::new(Text::from(visible_lines)).render(chat_inner, buf);
                } else {
                    Paragraph::new(Text::from(conversation_lines)).render(chat_inner, buf);
                }
            }

            let mut widget = InputWidget::new(&self.input_state);
            widget.set_context_remaining(self.context_remaining);
            if let Some(ref message) = status_message {
                widget.set_status_message(message.clone());
            }
            widget.clone().render(input_area, buf);
            self.render_write_approval_prompt(main_area, buf);
            return;
        }

        let main_layout = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
        ]);
        let [welcome_area, _, model_area, _, commands_area] = main_layout.areas(main_area);

        let welcome_text = Text::from(vec![Line::from(vec![Span::styled(
            format!("Welcome to Order   Version {}", env!("CARGO_PKG_VERSION")),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::DarkGray),
        )])]);
        Paragraph::new(welcome_text).render(welcome_area, buf);

        let model_label = if let Ok(Some(model_info)) = get_current_model_info() {
            // 显示 provider + model，便于用户快速确认当前走的是哪条连接链路。
            format!("{}/{}", model_info.provider_name, model_info.model_name)
        } else {
            "None".to_string()
        };
        let model_text = Text::from(vec![Line::from(vec![
            Span::styled(
                "Model: ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(model_label, Style::default().fg(Color::Green)),
        ])]);
        let model_block = Block::bordered().border_style(Style::default().fg(Color::DarkGray));
        Paragraph::new(model_text)
            .block(model_block)
            .render(model_area, buf);

        let commands = vec![
            ("/help", "Show help information"),
            ("/exit", "Exit the application"),
            ("/cancel", "Cancel latest operation"),
            ("/approve", "Approve pending writes by trace_id"),
            ("/reject", "Reject pending writes by trace_id"),
            ("/rollback", "Rollback snapshot by trace_id (or latest)"),
            (
                "/history",
                "Open history browser; /history N; /history clear",
            ),
            ("/skills", "Manage project skills"),
            ("/rules", "Edit project rules"),
            ("/settings", "Configure settings"),
            ("/status", "Check system status"),
            (
                "/capability",
                "Capability cache reset; usage: /capability reset ...",
            ),
            ("/editor", "Open Order-editor"),
        ];

        let mut lines = vec![Line::from(vec![Span::styled(
            "Available Commands:",
            Style::default().fg(Color::DarkGray),
        )])];
        for (cmd, desc) in commands {
            lines.push(Line::from(vec![
                Span::styled("     ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{:<10}", cmd), Style::default().fg(Color::Cyan)),
                Span::styled(" - ", Style::default().fg(Color::DarkGray)),
                Span::styled(desc, Style::default().fg(Color::Gray)),
            ]));
        }
        Paragraph::new(Text::from(lines)).render(commands_area, buf);

        let mut widget = InputWidget::new(&self.input_state);
        widget.set_context_remaining(self.context_remaining);
        if let Some(ref message) = status_message {
            widget.set_status_message(message.clone());
        }
        widget.clone().render(input_area, buf);
        self.render_write_approval_prompt(main_area, buf);
    }
}
