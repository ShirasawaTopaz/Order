use std::{
    collections::BTreeSet,
    path::Path,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyEventKind};
use lsp::{
    DiagnosticItem, LspClient, LspEvent, LspSemanticToken, LspTextEdit,
    detect_language_from_path_or_name,
};
use ratatui::DefaultTerminal;

// 输入事件与按键命令处理。
mod handlers;
// 编辑器界面渲染。
mod render;
// 会话保存与恢复。
mod session;
// 目录树数据构建。
mod tree;
// 编辑器核心类型定义。
mod types;
// 公共工具函数。
mod utils;

use self::{
    tree::collect_tree_entries,
    types::{
        EditorBuffer, EditorMode, MainFocus, PaneFocus, SplitDirection, TabState, ThemeName,
        TreeEntry,
    },
};

const SESSION_FILE: &str = ".order_editor.session";
const MIN_TREE_RATIO: u16 = 15;
const MAX_TREE_RATIO: u16 = 70;
const MAX_TREE_ENTRIES: usize = 1500;

// 编辑器主状态对象。
pub struct Editor {
    root: PathBuf,
    tree_entries: Vec<TreeEntry>,
    expanded_dirs: BTreeSet<PathBuf>,
    tree_selected: usize,
    tree_scroll: usize,
    tree_ratio: u16,
    show_tree: bool,
    main_focus: MainFocus,
    dragging_divider: bool,
    last_area: Option<ratatui::layout::Rect>,
    mode: EditorMode,
    normal_pending: String,
    insert_j_pending: bool,
    terminal_escape_pending: bool,
    buffers: Vec<EditorBuffer>,
    tabs: Vec<TabState>,
    active_tab: usize,
    show_tagbar: bool,
    completion_items: Vec<String>,
    completion_selected: usize,
    theme: ThemeName,
    diagnostics: Vec<String>,
    diagnostic_index: usize,
    status_message: String,
    command_history: Vec<String>,
    /// 多语言 LSP 客户端。
    ///
    /// 负责 Rust/Python/TS/JS/HTML/CSS/Vue/Java/Go/C/C++ 的语义高亮、补全与诊断。
    lsp_client: LspClient,
    /// 最近一次 LSP 动作摘要。
    ///
    /// 该字段用于状态栏简略展示（例如 `didOpen`、`publishDiagnostics`），
    /// 帮助用户快速了解 editor 当前正在执行的 LSP 操作。
    lsp_last_action: String,
    rust_analyzer_status: String,
    should_exit: bool,
    last_tick: Instant,
}

impl Default for Editor {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(cwd)
    }
}

impl Editor {
    // 创建编辑器并初始化默认状态。
    pub fn new(root: PathBuf) -> Self {
        let buffer = EditorBuffer::new_empty("untitled-1".to_string());
        let expanded_dirs = BTreeSet::new();
        let lsp_client = LspClient::new();
        let lsp_start_message = "LSP: 按需启动语言服务".to_string();

        Self {
            root: root.clone(),
            tree_entries: collect_tree_entries(&root, &expanded_dirs),
            expanded_dirs,
            tree_selected: 0,
            tree_scroll: 0,
            tree_ratio: 30,
            show_tree: true,
            main_focus: MainFocus::Editor,
            dragging_divider: false,
            last_area: None,
            mode: EditorMode::Normal,
            normal_pending: String::new(),
            insert_j_pending: false,
            terminal_escape_pending: false,
            buffers: vec![buffer],
            tabs: vec![TabState {
                title: "Tab-1".to_string(),
                buffer_index: 0,
                split: SplitDirection::None,
                focus: PaneFocus::Primary,
            }],
            active_tab: 0,
            show_tagbar: false,
            completion_items: Vec::new(),
            completion_selected: 0,
            theme: ThemeName::MaterialOcean,
            diagnostics: vec![
                "warning: unused variable".to_string(),
                "error: mismatched types".to_string(),
            ],
            diagnostic_index: 0,
            status_message: lsp_start_message,
            command_history: Vec::new(),
            lsp_client,
            lsp_last_action: "idle".to_string(),
            rust_analyzer_status: "rust-analyzer: 未激活".to_string(),
            should_exit: false,
            last_tick: Instant::now(),
        }
    }

    // 编辑器主循环。
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let tick_rate = Duration::from_millis(200);
        while !self.should_exit {
            // 每轮主循环刷新一次 LSP 进程状态，避免僵尸状态长期滞留。
            if let Err(error) = self.lsp_client.sync_running_state() {
                self.status_message = format!("LSP 状态检查失败: {error}");
            }

            self.auto_activate_rust_analyzer();
            self.handle_lsp_events();
            self.lsp_last_action = self.lsp_client.last_action().to_string();
            self.sync_lsp_did_change();

            terminal.draw(|frame| self.draw(frame))?;
            let timeout = tick_rate
                .checked_sub(self.last_tick.elapsed())
                .unwrap_or(Duration::ZERO);
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key_event(key),
                    Event::Mouse(mouse) => self.handle_mouse_event(mouse),
                    _ => {}
                }
            }
            if self.last_tick.elapsed() >= tick_rate {
                self.last_tick = Instant::now();
            }
        }
        Ok(())
    }

    /// 将缓冲区中的未同步变更通过 `didChange` 推送到 LSP。
    ///
    /// 当前策略：
    /// - 每轮事件循环进行一次轻量扫描；
    /// - 仅同步标记为 `lsp_dirty` 且属于受支持语言的缓冲区；
    /// - 使用增量变更同步，成功后清除脏标记并递增版本。
    fn sync_lsp_did_change(&mut self) {
        if !self.lsp_client.is_running() {
            return;
        }

        for buffer in &mut self.buffers {
            if !buffer.lsp_dirty {
                continue;
            }

            let Some(path) = buffer.path.as_ref() else {
                continue;
            };
            if detect_language_from_path_or_name(Some(path), &buffer.name).is_none() {
                continue;
            }

            let next_version = buffer.lsp_version.saturating_add(1);
            let text = buffer.lines.join("\n");
            let old_text = buffer
                .lsp_last_synced_text
                .as_deref()
                .unwrap_or("");
            match self
                .lsp_client
                .send_did_change(path, old_text, &text, next_version)
            {
                Ok(_) => {
                    buffer.lsp_version = next_version;
                    buffer.lsp_dirty = false;
                    buffer.lsp_last_synced_text = Some(text);
                }
                Err(error) => {
                    self.status_message = format!("LSP didChange 失败: {error}");
                }
            }

            // `didChange` 成功后立刻请求语义高亮，
            // 可以确保高亮结果与当前文本尽量同步。
            if let Err(error) = self.lsp_client.request_semantic_tokens(path) {
                self.status_message = format!("LSP semanticTokens 请求失败: {error}");
            }
        }
    }

    /// 处理 LSP 事件并同步到 editor 状态。
    fn handle_lsp_events(&mut self) {
        for event in self.lsp_client.poll_events() {
            match event {
                LspEvent::Status(text) => {
                    self.status_message = text;
                }
                LspEvent::PublishDiagnostics { file_path: _, items } => {
                    self.apply_lsp_diagnostics(items);
                }
                LspEvent::WillSaveWaitUntilEdits { file_path, edits } => {
                    self.apply_will_save_wait_until_edits(&file_path, edits);
                }
                LspEvent::CompletionItems { file_path, items } => {
                    self.apply_lsp_completion_items(&file_path, items);
                }
                LspEvent::SemanticTokens { file_path, tokens } => {
                    self.apply_lsp_semantic_tokens(&file_path, tokens);
                }
                LspEvent::RustAnalyzerStatus { message, done } => {
                    self.rust_analyzer_status = if done {
                        format!("rust-analyzer: 已就绪（{}）", message)
                    } else {
                        format!("rust-analyzer: {}", message)
                    };
                    self.status_message = self.rust_analyzer_status.clone();

                    if done {
                        let tab_idx = self.active_tab;
                        let buffer_idx = self.tabs[tab_idx].buffer_index;
                        if let Some(path) = self.buffers[buffer_idx].path.clone()
                            && detect_language_from_path_or_name(Some(&path), "")
                                .is_some_and(|language| language == lsp::LspLanguage::Rust)
                            && let Err(error) = self.lsp_client.request_semantic_tokens(&path)
                        {
                            self.status_message =
                                format!("rust-analyzer 已就绪，但语义高亮请求失败: {}", error);
                        }
                    }
                }
            }
        }
    }

    /// 自动激活 rust-analyzer。
    ///
    /// 每轮主循环仅在“当前活跃 buffer 是 Rust 且会话未运行”时触发一次 didOpen，
    /// 这样既能实现开箱即用自动激活，也避免重复请求造成噪音。
    fn auto_activate_rust_analyzer(&mut self) {
        if self.tabs.is_empty() {
            return;
        }

        let buffer_idx = self.tabs[self.active_tab].buffer_index;
        let Some(path) = self
            .buffers
            .get(buffer_idx)
            .and_then(|buffer| buffer.path.as_ref().cloned())
        else {
            return;
        };

        let is_rust = detect_language_from_path_or_name(Some(&path), "")
            .is_some_and(|language| language == lsp::LspLanguage::Rust);
        if !is_rust {
            return;
        }

        if self.lsp_client.is_language_running(lsp::LspLanguage::Rust) {
            return;
        }

        self.try_send_did_open_for_buffer_idx(buffer_idx);
        self.rust_analyzer_status = "rust-analyzer: 自动激活中".to_string();
    }

    /// 将 LSP 补全候选写回目标缓冲区。
    ///
    /// 通过“路径定位 -> 全量替换”策略，避免跨 buffer 残留旧补全数据。
    fn apply_lsp_completion_items(&mut self, file_path: &Path, items: Vec<lsp::LspCompletionItem>) {
        let Some(buffer_idx) = self
            .buffers
            .iter()
            .position(|buffer| buffer.path.as_deref() == Some(file_path))
        else {
            return;
        };

        if let Some(buffer) = self.buffers.get_mut(buffer_idx) {
            buffer.lsp_completion_items = items;
        }

        if buffer_idx == self.tabs[self.active_tab].buffer_index {
            self.refresh_completion_from_lsp_cache();
        }
    }

    /// 将 LSP 语义 token 写回目标缓冲区，并构建按行索引缓存。
    fn apply_lsp_semantic_tokens(&mut self, file_path: &Path, tokens: Vec<LspSemanticToken>) {
        let Some(buffer_idx) = self
            .buffers
            .iter()
            .position(|buffer| buffer.path.as_deref() == Some(file_path))
        else {
            return;
        };

        if let Some(buffer) = self.buffers.get_mut(buffer_idx) {
            let mut tokens_by_line: std::collections::HashMap<usize, Vec<LspSemanticToken>> =
                std::collections::HashMap::new();
            for token in &tokens {
                tokens_by_line
                    .entry(token.line)
                    .or_default()
                    .push(token.clone());
            }
            for grouped_tokens in tokens_by_line.values_mut() {
                grouped_tokens.sort_by_key(|item| item.start);
            }

            buffer.lsp_semantic_tokens = tokens;
            buffer.lsp_tokens_by_line = tokens_by_line;
        }
    }

    /// 将 LSP 诊断列表映射到现有 diagnostics 面板。
    fn apply_lsp_diagnostics(&mut self, items: Vec<DiagnosticItem>) {
        let mut rendered = Vec::new();
        for item in items {
            let file = item
                .file_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("<unknown>");
            rendered.push(format!(
                "{}:{}:{} [{}] {}",
                file,
                item.line,
                item.column,
                item.severity.as_str(),
                item.message
            ));
        }

        self.diagnostics = rendered;
        self.diagnostic_index = 0;
        if self.diagnostics.is_empty() {
            self.status_message = "LSP: 无诊断问题".to_string();
        } else {
            self.status_message = format!("LSP: 收到 {} 条诊断", self.diagnostics.len());
        }
    }

    /// 将 `willSaveWaitUntil` 返回的 TextEdit 应用到目标缓冲区。
    fn apply_will_save_wait_until_edits(&mut self, file_path: &std::path::Path, mut edits: Vec<LspTextEdit>) {
        if edits.is_empty() {
            return;
        }

        let Some(buffer_idx) = self
            .buffers
            .iter()
            .position(|buffer| buffer.path.as_deref() == Some(file_path))
        else {
            return;
        };

        // 为避免前面编辑影响后面区间坐标，按起始位置从后向前应用。
        edits.sort_by(|left, right| {
            left.start_line
                .cmp(&right.start_line)
                .then(left.start_character.cmp(&right.start_character))
        });
        edits.reverse();
        let applied_count = edits.len();

        let mut text = self.buffers[buffer_idx].lines.join("\n");
        for edit in edits {
            let Some(start_byte) = line_col_to_byte_index(&text, edit.start_line, edit.start_character)
            else {
                continue;
            };
            let Some(end_byte) = line_col_to_byte_index(&text, edit.end_line, edit.end_character) else {
                continue;
            };
            if start_byte > end_byte || end_byte > text.len() {
                continue;
            }
            text.replace_range(start_byte..end_byte, &edit.new_text);
        }

        let mut new_lines: Vec<String> = text.split('\n').map(ToOwned::to_owned).collect();
        if new_lines.is_empty() {
            new_lines.push(String::new());
        }

        let buffer = &mut self.buffers[buffer_idx];
        buffer.lines = new_lines;
        buffer.modified = true;
        buffer.lsp_dirty = true;
        buffer.ensure_cursor_in_bounds();
        self.lsp_last_action = format!("willSaveWaitUntil({} edits)", applied_count);
        self.status_message = format!("LSP: 已应用 {} 条 TextEdit", applied_count);
    }
}

/// 将 `(line, column)`（0-based，按字符计数）转换为字符串字节索引。
fn line_col_to_byte_index(text: &str, line: usize, column: usize) -> Option<usize> {
    let mut current_line = 0usize;
    let mut current_column = 0usize;

    for (byte_index, ch) in text.char_indices() {
        if current_line == line && current_column == column {
            return Some(byte_index);
        }

        if ch == '\n' {
            current_line += 1;
            current_column = 0;
        } else {
            current_column += 1;
        }
    }

    if current_line == line && current_column == column {
        Some(text.len())
    } else {
        None
    }
}

/// 计算当前行中“第 N 个字符”对应的字节偏移。
///
/// 该函数用于把 LSP 的字符坐标映射到 Rust 字符串字节索引，
/// 以便在渲染阶段安全切分 UTF-8 文本。
fn char_to_byte_index_in_line(line: &str, char_index: usize) -> usize {
    line.char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or_else(|| line.len())
}
