use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::Path,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyEventKind};
use lsp::{
    DiagnosticItem, LspClient, LspCodeAction, LspEvent, LspSemanticToken, LspTextEdit,
    LspWorkspaceEdit, detect_language_from_path_or_name,
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
        CompletionDisplayItem, EditorBuffer, EditorMode, MainFocus, PaneFocus, SplitDirection,
        TabState, ThemeName, TreeEntry,
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
    last_editor_inner_area: Option<ratatui::layout::Rect>,
    mode: EditorMode,
    normal_pending: String,
    /// `RenameInput` 模式下的临时输入缓冲。
    ///
    /// 独立存储输入内容可以避免污染 NORMAL 命令串，
    /// 同时为后续扩展“更多带参数的 LSP 命令”预留统一入口。
    rename_input: String,
    insert_j_pending: bool,
    terminal_escape_pending: bool,
    buffers: Vec<EditorBuffer>,
    tabs: Vec<TabState>,
    active_tab: usize,
    show_tagbar: bool,
    completion_items: Vec<CompletionDisplayItem>,
    completion_selected: usize,
    completion_scroll_offset: usize,
    /// 补全确认后的弹窗抑制开关。
    ///
    /// 当用户确认补全后，异步 LSP 响应可能会在短时间内返回旧候选。
    /// 该开关用于在“下一次真实输入”前屏蔽这类回流，避免弹窗立即二次打开。
    suppress_completion_until_input: bool,
    theme: ThemeName,
    diagnostics: Vec<String>,
    diagnostic_index: usize,
    /// 最近一次由 LSP 发布的诊断，按文件路径分组缓存。
    ///
    /// quick fix 请求需要把诊断上下文回传给服务端，
    /// 因此不能只保留渲染后的字符串列表。
    lsp_diagnostics_by_file: HashMap<PathBuf, Vec<DiagnosticItem>>,
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
    /// LSP 项目加载状态提示。
    ///
    /// 用于显示"项目加载中..."或"项目加载完成"等状态。
    lsp_loading_status: String,
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
            last_editor_inner_area: None,
            mode: EditorMode::Normal,
            normal_pending: String::new(),
            rename_input: String::new(),
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
            completion_scroll_offset: 0,
            suppress_completion_until_input: false,
            theme: ThemeName::MaterialOcean,
            diagnostics: vec![
                "warning: unused variable".to_string(),
                "error: mismatched types".to_string(),
            ],
            diagnostic_index: 0,
            lsp_diagnostics_by_file: HashMap::new(),
            status_message: lsp_start_message,
            command_history: Vec::new(),
            lsp_client,
            lsp_last_action: "idle".to_string(),
            rust_analyzer_status: "rust-analyzer: 未激活".to_string(),
            lsp_loading_status: String::new(),
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

            self.auto_activate_lsp();
            self.handle_lsp_events();
            self.lsp_last_action = self.lsp_client.last_action().to_string();
            self.sync_lsp_did_change();

            terminal.draw(|frame| self.draw(frame))?;
            let timeout = tick_rate
                .checked_sub(self.last_tick.elapsed())
                .unwrap_or(Duration::ZERO);
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key_event(key)
                    }
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

    /// 标记指定语言已进入“项目加载中”阶段，并同步到状态栏提示。
    ///
    /// 这样做的原因是：语言服务刚启动时到首个进度事件之间存在空窗期，
    /// 若不主动提示，用户会误以为 LSP 没有响应。
    pub(super) fn mark_lsp_project_loading(&mut self, language: lsp::LspLanguage) {
        self.lsp_loading_status = "项目加载中...".to_string();
        self.status_message = format!("{} LSP 正在加载项目，请稍候...", language.display_name());
        if language == lsp::LspLanguage::Rust {
            self.rust_analyzer_status = "rust-analyzer: 项目加载中".to_string();
        }
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
            let old_text = buffer.lsp_last_synced_text.as_deref().unwrap_or("");
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
                LspEvent::PublishDiagnostics { file_path, items } => {
                    self.apply_lsp_diagnostics(file_path, items);
                }
                LspEvent::WillSaveWaitUntilEdits { file_path, edits } => {
                    self.apply_will_save_wait_until_edits(&file_path, edits);
                }
                LspEvent::CompletionItems { file_path, items } => {
                    self.apply_lsp_completion_items(&file_path, items);
                }
                LspEvent::SemanticTokens { file_path, tokens } => {
                    let token_count = tokens.len();
                    self.apply_lsp_semantic_tokens(&file_path, tokens);
                    if token_count > 0 {
                        self.lsp_loading_status = "项目加载完成".to_string();
                    }
                }
                LspEvent::FormattingEdits { file_path, edits } => {
                    self.apply_formatting_edits(&file_path, edits);
                }
                LspEvent::RenameWorkspaceEdit {
                    file_path,
                    new_name,
                    edit,
                } => {
                    self.apply_rename_workspace_edit(&file_path, &new_name, edit);
                }
                LspEvent::CodeActions { file_path, actions } => {
                    self.apply_quick_fix_code_actions(&file_path, actions);
                }
                LspEvent::WorkspaceApplyEditRequest {
                    language,
                    request_id,
                    label,
                    edit,
                } => {
                    let summary = self.apply_workspace_edit(edit);
                    let applied = summary.failed_files == 0;
                    let failure_reason =
                        (!applied).then(|| format!("{} 个文件应用失败", summary.failed_files));

                    if let Err(error) = self.lsp_client.respond_workspace_apply_edit(
                        language,
                        request_id,
                        applied,
                        failure_reason.as_deref(),
                    ) {
                        self.status_message = format!("workspace/applyEdit 回包失败: {error}");
                    } else if let Some(label) = label {
                        self.status_message = format!(
                            "workspace/applyEdit：{}（{} 文件，{} 编辑）",
                            label, summary.touched_files, summary.applied_edits
                        );
                    } else {
                        self.status_message = format!(
                            "workspace/applyEdit：{} 文件，{} 编辑",
                            summary.touched_files, summary.applied_edits
                        );
                    }
                }
                LspEvent::RustAnalyzerStatus { message, done } => {
                    self.rust_analyzer_status = if done {
                        format!("rust-analyzer: 已就绪（{}）", message)
                    } else {
                        format!("rust-analyzer: {}", message)
                    };
                    self.status_message = self.rust_analyzer_status.clone();
                    self.lsp_loading_status = if done {
                        "项目加载完成".to_string()
                    } else {
                        "项目加载中...".to_string()
                    };

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

    /// 自动激活 LSP 语言服务。
    ///
    /// 每轮主循环检查：
    /// - 如果当前活跃 buffer 是某语言文件且会话未运行，触发 didOpen；
    /// - 如果项目根目录存在该语言的项目标识文件且会话未运行，直接启动 LSP。
    /// 这样既能实现开箱即用自动激活，也避免重复请求造成噪音。
    fn auto_activate_lsp(&mut self) {
        if self.tabs.is_empty() {
            return;
        }

        let buffer_idx = self.tabs[self.active_tab].buffer_index;
        let buffer_path = self
            .buffers
            .get(buffer_idx)
            .and_then(|buffer| buffer.path.as_ref().cloned());

        for language in lsp::all_languages() {
            if self.lsp_client.is_language_running(*language) {
                continue;
            }

            let has_project_marker = language
                .project_markers()
                .iter()
                .any(|marker| self.root.join(marker).exists());

            if !has_project_marker {
                continue;
            }

            if let Some(ref path) = buffer_path {
                let buffer_language = detect_language_from_path_or_name(Some(path), "");
                if buffer_language == Some(*language) {
                    self.try_send_did_open_for_buffer_idx(buffer_idx);
                    if self.lsp_client.is_language_running(*language) {
                        self.mark_lsp_project_loading(*language);
                    }
                } else if let Err(error) = self
                    .lsp_client
                    .ensure_started_for_language(&self.root, *language)
                {
                    self.status_message =
                        format!("{} LSP 启动失败: {error}", language.display_name());
                    continue;
                } else {
                    self.mark_lsp_project_loading(*language);
                }
            } else if let Err(error) = self
                .lsp_client
                .ensure_started_for_language(&self.root, *language)
            {
                self.status_message = format!("{} LSP 启动失败: {error}", language.display_name());
                continue;
            } else {
                self.mark_lsp_project_loading(*language);
            }

            if *language == lsp::LspLanguage::Rust {
                self.rust_analyzer_status = "rust-analyzer: 自动激活中".to_string();
            }
        }
    }

    /// 将 LSP 补全候选写回目标缓冲区。
    ///
    /// 通过“路径定位 -> 全量替换”策略，避免跨 buffer 残留旧补全数据。
    fn apply_lsp_completion_items(&mut self, file_path: &Path, items: Vec<lsp::LspCompletionItem>) {
        let Some(buffer_idx) = self.buffers.iter().position(|buffer| {
            buffer.path.as_ref().is_some_and(|p| {
                p == file_path || p.canonicalize().ok() == file_path.canonicalize().ok()
            })
        }) else {
            return;
        };

        if let Some(buffer) = self.buffers.get_mut(buffer_idx) {
            buffer.lsp_completion_items = items;
        }

        let is_active_buffer = buffer_idx == self.tabs[self.active_tab].buffer_index;
        if is_active_buffer {
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

    /// 将 LSP 诊断按文件缓存，并同步到 diagnostics 面板。
    fn apply_lsp_diagnostics(&mut self, file_path: PathBuf, items: Vec<DiagnosticItem>) {
        if items.is_empty() {
            self.lsp_diagnostics_by_file.remove(&file_path);
        } else {
            self.lsp_diagnostics_by_file.insert(file_path, items);
        }

        let mut flattened = self
            .lsp_diagnostics_by_file
            .values()
            .flat_map(|items| items.iter().cloned())
            .collect::<Vec<_>>();
        flattened.sort_by(|left, right| {
            left.file_path
                .cmp(&right.file_path)
                .then(left.line.cmp(&right.line))
                .then(left.column.cmp(&right.column))
        });

        self.diagnostics = flattened
            .iter()
            .map(|item| {
                let file = item
                    .file_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("<unknown>");
                format!(
                    "{}:{}:{} [{}] {}",
                    file,
                    item.line,
                    item.column,
                    item.severity.as_str(),
                    item.message
                )
            })
            .collect();
        self.diagnostic_index = self
            .diagnostic_index
            .min(self.diagnostics.len().saturating_sub(1));
        if self.diagnostics.is_empty() {
            self.status_message = "LSP: 无诊断问题".to_string();
        } else {
            self.status_message = format!("LSP: 收到 {} 条诊断", self.diagnostics.len());
        }
    }

    /// 应用 `textDocument/formatting` 返回的编辑。
    fn apply_formatting_edits(&mut self, file_path: &Path, edits: Vec<LspTextEdit>) {
        if edits.is_empty() {
            self.status_message = "LSP format：无需要应用的编辑".to_string();
            return;
        }

        match self.apply_text_edits_to_file(file_path, edits) {
            Ok(applied_edits) => {
                self.status_message = format!("LSP format：已应用 {} 条编辑", applied_edits);
            }
            Err(error) => {
                self.status_message = format!("LSP format 应用失败：{error}");
            }
        }
    }

    /// 应用 `textDocument/rename` 返回的工作区编辑。
    fn apply_rename_workspace_edit(
        &mut self,
        _file_path: &Path,
        new_name: &str,
        edit: Option<LspWorkspaceEdit>,
    ) {
        let Some(edit) = edit else {
            self.status_message = format!("LSP rename：`{new_name}` 未返回可应用编辑");
            return;
        };

        if edit.is_empty() {
            self.status_message = format!("LSP rename：`{new_name}` 无需修改");
            return;
        }

        let summary = self.apply_workspace_edit(edit);
        if summary.failed_files == 0 {
            self.status_message = format!(
                "LSP rename：`{new_name}` 已完成（{} 文件，{} 编辑）",
                summary.touched_files, summary.applied_edits
            );
        } else {
            self.status_message = format!(
                "LSP rename：部分失败（成功 {} 文件，失败 {} 文件）",
                summary.touched_files, summary.failed_files
            );
        }
    }

    /// 选择并执行最合适的 quick fix。
    fn apply_quick_fix_code_actions(&mut self, file_path: &Path, actions: Vec<LspCodeAction>) {
        let Some(selected) = pick_preferred_quick_fix(actions) else {
            self.status_message = "LSP quick fix：当前无可用修复".to_string();
            return;
        };

        let mut status_parts = Vec::new();
        if let Some(edit) = selected.edit.clone() {
            let summary = self.apply_workspace_edit(edit);
            if summary.failed_files == 0 {
                status_parts.push(format!(
                    "已应用 {} 文件 / {} 编辑",
                    summary.touched_files, summary.applied_edits
                ));
            } else {
                status_parts.push(format!(
                    "应用部分失败（成功 {}，失败 {}）",
                    summary.touched_files, summary.failed_files
                ));
            }
        }

        if let Some(command) = selected.command.as_ref() {
            match self.lsp_client.execute_command(file_path, command) {
                Ok(()) => {
                    status_parts.push(format!("已触发命令 `{}`", command.title));
                }
                Err(error) => {
                    status_parts.push(format!("命令触发失败：{error}"));
                }
            }
        }

        if status_parts.is_empty() {
            self.status_message = format!("LSP quick fix：{}（无可应用内容）", selected.title);
        } else {
            self.status_message = format!(
                "LSP quick fix：{}，{}",
                selected.title,
                status_parts.join("，")
            );
        }
    }

    /// 将 `willSaveWaitUntil` 返回的 TextEdit 应用到目标缓冲区。
    fn apply_will_save_wait_until_edits(
        &mut self,
        file_path: &std::path::Path,
        edits: Vec<LspTextEdit>,
    ) {
        if edits.is_empty() {
            return;
        }

        match self.apply_text_edits_to_file(file_path, edits) {
            Ok(applied_count) => {
                self.lsp_last_action = format!("willSaveWaitUntil({} edits)", applied_count);
                self.status_message = format!("LSP: 已应用 {} 条 TextEdit", applied_count);
            }
            Err(error) => {
                self.status_message = format!("LSP TextEdit 应用失败：{error}");
            }
        }
    }

    /// 应用 `WorkspaceEdit` 并汇总结果。
    ///
    /// rename/quick fix 可能跨多个文件，且部分文件并不在当前编辑器缓冲区打开。
    /// 这里统一处理“已打开缓冲区 + 未打开落盘文件”两类目标，避免编辑半生效。
    fn apply_workspace_edit(&mut self, edit: LspWorkspaceEdit) -> WorkspaceEditApplySummary {
        let mut summary = WorkspaceEditApplySummary::default();
        for file_edit in edit.document_edits {
            if file_edit.edits.is_empty() {
                continue;
            }

            match self.apply_text_edits_to_file(&file_edit.file_path, file_edit.edits) {
                Ok(applied) if applied > 0 => {
                    summary.touched_files += 1;
                    summary.applied_edits += applied;
                }
                Ok(_) => {
                    summary.failed_files += 1;
                }
                Err(_) => {
                    summary.failed_files += 1;
                }
            }
        }
        summary
    }

    /// 获取指定文件当前可用于 quick fix 的诊断。
    pub(super) fn diagnostics_for_file(&self, file_path: &Path) -> Vec<DiagnosticItem> {
        if let Some(items) = self.lsp_diagnostics_by_file.get(file_path) {
            return items.clone();
        }

        // 某些平台上路径大小写或符号链接可能导致键不命中，这里做一次温和兜底。
        self.lsp_diagnostics_by_file
            .iter()
            .find(|(path, _)| {
                *path == file_path || path.canonicalize().ok() == file_path.canonicalize().ok()
            })
            .map(|(_, items)| items.clone())
            .unwrap_or_default()
    }

    /// 将 text edits 应用到打开中的 buffer 或磁盘文件。
    fn apply_text_edits_to_file(
        &mut self,
        file_path: &Path,
        edits: Vec<LspTextEdit>,
    ) -> Result<usize, String> {
        if edits.is_empty() {
            return Ok(0);
        }

        if let Some(buffer_idx) = self.buffers.iter().position(|buffer| {
            buffer.path.as_ref().is_some_and(|path| {
                path == file_path || path.canonicalize().ok() == file_path.canonicalize().ok()
            })
        }) {
            let original = self.buffers[buffer_idx].lines.join("\n");
            let (updated, applied_count) = apply_text_edits_to_text(original, edits);
            let mut new_lines: Vec<String> = updated.split('\n').map(ToOwned::to_owned).collect();
            if new_lines.is_empty() {
                new_lines.push(String::new());
            }

            let buffer = &mut self.buffers[buffer_idx];
            buffer.lines = new_lines;
            buffer.modified = true;
            buffer.lsp_dirty = true;
            buffer.ensure_cursor_in_bounds();
            return Ok(applied_count);
        }

        // 文件未在当前 buffer 打开时，直接在磁盘落地，保证 rename/quick fix 全局一致生效。
        let original =
            fs::read_to_string(file_path).map_err(|error| format!("读取失败: {}", error))?;
        let (updated, applied_count) = apply_text_edits_to_text(original, edits);
        fs::write(file_path, updated).map_err(|error| format!("写入失败: {}", error))?;
        Ok(applied_count)
    }
}

/// 工作区编辑应用结果统计。
#[derive(Default)]
struct WorkspaceEditApplySummary {
    touched_files: usize,
    applied_edits: usize,
    failed_files: usize,
}

/// 从 code action 列表中挑选最合适的 quick fix。
///
/// 优先级策略：
/// 1. `kind=quickfix` 且 `isPreferred=true`
/// 2. `kind=quickfix`
/// 3. 任意首个 action（兜底）
fn pick_preferred_quick_fix(actions: Vec<LspCodeAction>) -> Option<LspCodeAction> {
    actions
        .iter()
        .position(|item| {
            item.kind
                .as_deref()
                .is_some_and(|kind| kind.starts_with("quickfix"))
                && item.is_preferred
        })
        .and_then(|idx| actions.get(idx).cloned())
        .or_else(|| {
            actions
                .iter()
                .position(|item| {
                    item.kind
                        .as_deref()
                        .is_some_and(|kind| kind.starts_with("quickfix"))
                })
                .and_then(|idx| actions.get(idx).cloned())
        })
        .or_else(|| actions.into_iter().next())
}

/// 按 LSP 坐标把一组 text edits 应用到文本。
///
/// 按“从后向前”应用的原因是：前面的替换会改变后续偏移，
/// 倒序可以保证所有 edit 坐标都基于原始文本解释。
fn apply_text_edits_to_text(mut text: String, mut edits: Vec<LspTextEdit>) -> (String, usize) {
    edits.sort_by(|left, right| {
        left.start_line
            .cmp(&right.start_line)
            .then(left.start_character.cmp(&right.start_character))
    });
    edits.reverse();

    let mut applied_count = 0usize;
    for edit in edits {
        let Some(start_byte) = line_col_to_byte_index(&text, edit.start_line, edit.start_character)
        else {
            continue;
        };
        let Some(end_byte) = line_col_to_byte_index(&text, edit.end_line, edit.end_character)
        else {
            continue;
        };
        if start_byte > end_byte || end_byte > text.len() {
            continue;
        }

        text.replace_range(start_byte..end_byte, &edit.new_text);
        applied_count += 1;
    }

    (text, applied_count)
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
