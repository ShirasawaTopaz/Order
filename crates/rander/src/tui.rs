use crate::{
    editor::Editor,
    focus_status::{CURRENT_FOCUS, FocusStatus},
    widget::input_widget::{InputState, InputWidget},
};
use anyhow::{Context, anyhow};
use chrono::Local;
use core::{
    commands::{EXIT, get_exit},
    model::{
        connection::{Connection, Provider},
        info::get_current_model_info,
    },
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Widget},
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

pub struct OrderTui<'a> {
    /// 全局退出标记。
    exit: &'a AtomicBool,
    /// 输入组件状态。
    input_state: InputState,
    /// 输入光标闪烁时钟。
    last_tick: Instant,
    /// 预留上下文剩余量。
    context_remaining: u32,
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
}

impl Default for OrderTui<'_> {
    fn default() -> Self {
        let now = Local::now();
        Self {
            exit: &EXIT,
            input_state: InputState::default(),
            last_tick: Instant::now(),
            context_remaining: 100,
            pending_command: None,
            connection: None,
            messages: Vec::new(),
            session_timestamp: now.format("%Y-%-m-%-d %H:%M:%S").to_string(),
            history_browser: None,
        }
    }
}

impl OrderTui<'_> {
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        // 启用鼠标捕获，主界面和 editor 会共享鼠标事件能力。
        execute!(std::io::stdout(), EnableMouseCapture)?;

        let tick_rate = Duration::from_millis(500);
        while !get_exit().load(Ordering::Relaxed) {
            terminal.draw(|frame| self.draw(frame))?;

            let timeout = tick_rate
                .checked_sub(self.last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0));

            if event::poll(timeout)?
                && let Event::Key(key) = event::read()?
            {
                self.handle_key_event(&key);
                self.process_pending_command(terminal)?;
                self.input_state.set_cursor_visible(true);
                self.last_tick = Instant::now();
            }

            if self.last_tick.elapsed() >= tick_rate {
                self.input_state.toggle_cursor_visibility();
                self.last_tick = Instant::now();
            }
        }

        execute!(std::io::stdout(), DisableMouseCapture)?;
        terminal.clear()?;
        Ok(())
    }

    fn draw(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_key_event(&mut self, key: &KeyEvent) {
        if key.kind != KeyEventKind::Press {
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
                self.exit.store(true, Ordering::Relaxed);
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
        // 用户输入以对话形式展示在右侧。
        self.push_chat_message(ChatRole::User, input.clone(), true);

        // 发送结果以对话形式展示：
        // - 成功：LLM 回复显示在左侧；
        // - 失败：错误显示在左侧；
        // 并且都不会导致程序退出。
        match self.send_prompt_to_llm(input) {
            Ok(response) => self.push_chat_message(ChatRole::Llm, response, true),
            Err(error) => {
                self.push_chat_message(ChatRole::Error, format!("发送失败：{error}"), true);
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

        match command {
            "/editor" => self.launch_editor(terminal)?,
            "/exit" => self.exit.store(true, Ordering::Relaxed),
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

    /// 将用户输入发送给当前配置的大模型。
    ///
    /// 设计说明：
    /// - 复用 `Connection` 实例，保证 `Connection` 内部懒加载的 `client` 能被持续复用；
    /// - 当前事件循环是同步的，因此这里用一个轻量 `tokio` 运行时阻塞等待异步响应；
    /// - 成功时返回响应文本，供对话区展示。
    fn send_prompt_to_llm(&mut self, prompt: String) -> anyhow::Result<String> {
        self.ensure_connection()?;

        let connection = self
            .connection
            .as_mut()
            .context("LLM 连接初始化后仍不可用")?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("创建异步运行时失败")?;

        runtime
            .block_on(connection.response(prompt))
            .context("向 LLM 发送消息失败")
    }

    /// 向对话流追加一条消息。
    ///
    /// 为防止内存无限增长，仅保留最近 200 条消息。
    fn push_chat_message(&mut self, role: ChatRole, content: String, persist_to_history: bool) {
        const MAX_MESSAGES: usize = 200;

        let normalized_content = content.trim().to_string();
        if normalized_content.is_empty() {
            return;
        }

        self.messages.push(ChatMessage {
            role,
            content: normalized_content,
            persist_to_history,
        });

        if self.messages.len() > MAX_MESSAGES {
            let overflow = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(0..overflow);
        }

        // 消息入队后立即尝试持久化到运行目录。
        // 历史写入失败不影响主流程，仅通过标准错误输出告警。
        if persist_to_history && let Err(error) = self.persist_history() {
            eprintln!("failed to persist history: {error}");
        }
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

        let content = fs::read_to_string(path)
            .with_context(|| format!("读取历史文件失败: {}", path.display()))?;

        if content.trim().is_empty() {
            return Ok(HistoryFile::default());
        }

        let records: Vec<HistoryRecord> = serde_json::from_str(&content)
            .with_context(|| format!("解析历史文件失败: {}", path.display()))?;
        Ok(HistoryFile { records })
    }

    /// 将历史结构以 UTF-8 JSON（pretty）写回磁盘。
    fn write_history_file(path: &PathBuf, file: &HistoryFile) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(&file.records).context("序列化历史记录失败")?;
        fs::write(path, content).with_context(|| format!("写入历史文件失败: {}", path.display()))
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

    /// 根据给定宽度将消息按字符分段换行。
    ///
    /// 这里按字符数近似宽度，足以满足当前终端对话显示需求。
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
            let mut count = 0usize;
            for ch in raw_line.chars() {
                current.push(ch);
                count += 1;
                if count >= width {
                    lines.push(current);
                    current = String::new();
                    count = 0;
                }
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
                    let padding = width.saturating_sub(content.chars().count());
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

    /// 进入 editor 子界面，退出后回到主界面。
    fn launch_editor(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let mut editor = Editor::default();
        editor.run(terminal)?;
        terminal.clear()?;
        self.last_tick = Instant::now();
        Ok(())
    }
}

impl Widget for &OrderTui<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let input_height = self.input_state.required_height(area.width);
        let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(input_height)]);
        let [main_area, input_area] = layout.areas(area);

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

            InputWidget::new(&self.input_state)
                .set_context_remaining(self.context_remaining)
                .clone()
                .render(input_area, buf);
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
                let mut conversation_lines =
                    self.build_conversation_lines(chat_inner.width as usize);

                let max_visible_lines = chat_inner.height as usize;
                if conversation_lines.len() > max_visible_lines {
                    let start = conversation_lines.len().saturating_sub(max_visible_lines);
                    conversation_lines = conversation_lines.into_iter().skip(start).collect();
                }

                Paragraph::new(Text::from(conversation_lines)).render(chat_inner, buf);
            }

            InputWidget::new(&self.input_state)
                .set_context_remaining(self.context_remaining)
                .clone()
                .render(input_area, buf);
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

        let model_name = if let Ok(Some(model_info)) = get_current_model_info() {
            model_info.model_name
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
            Span::styled(model_name, Style::default().fg(Color::Green)),
        ])]);
        let model_block = Block::bordered().border_style(Style::default().fg(Color::DarkGray));
        Paragraph::new(model_text)
            .block(model_block)
            .render(model_area, buf);

        let commands = vec![
            ("/help", "Show help information"),
            ("/exit", "Exit the application"),
            ("/cancel", "Cancel latest operation"),
            (
                "/history",
                "Open history browser; /history N; /history clear",
            ),
            ("/skills", "Manage project skills"),
            ("/rules", "Edit project rules"),
            ("/settings", "Configure settings"),
            ("/status", "Check system status"),
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

        InputWidget::new(&self.input_state)
            .set_context_remaining(self.context_remaining)
            .clone()
            .render(input_area, buf);
    }
}
