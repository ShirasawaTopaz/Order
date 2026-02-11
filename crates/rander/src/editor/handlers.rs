use std::{
    cmp::min,
    path::{Path, PathBuf},
};

use core::commands::get_exit;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout};

use super::{
    Editor, MAX_TREE_RATIO, MIN_TREE_RATIO,
    types::{EditorBuffer, EditorMode, MainFocus, PaneFocus, SplitDirection, TabState},
    utils::{contains_point, file_name_or, is_normal_command_prefix},
};

impl Editor {
    pub(super) fn handle_key_event(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.should_exit = true;
            get_exit().store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }

        match self.mode {
            EditorMode::Normal => self.handle_normal_key_event(key),
            EditorMode::Insert => self.handle_insert_key_event(key),
            EditorMode::Terminal => self.handle_terminal_key_event(key),
            EditorMode::BufferPicker => self.handle_buffer_picker_key_event(key),
        }
    }

    // 功能说明：见下方实现。
    fn normalize_active_tab_focus(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        if self.tabs[self.active_tab].split == SplitDirection::None {
            self.tabs[self.active_tab].focus = PaneFocus::Primary;
        }
    }

    pub(super) fn handle_normal_key_event(&mut self, key: KeyEvent) {
        self.normalize_active_tab_focus();

        match key.code {
            KeyCode::Char('i') if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    return;
                }
                self.mode = EditorMode::Insert;
                self.status_message = "INSERT".to_string();
            }
            KeyCode::Char('h') if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    return;
                }
                self.active_buffer_mut().move_left();
            }
            KeyCode::Char('l') if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    self.open_selected_tree_entry();
                    return;
                }
                self.active_buffer_mut().move_right();
            }
            KeyCode::Char('j') if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    self.tree_select_next();
                    return;
                }
                self.active_buffer_mut().move_down();
            }
            KeyCode::Char('k') if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    self.tree_select_prev();
                    return;
                }
                self.active_buffer_mut().move_up();
            }
            KeyCode::Left if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    return;
                }
                self.active_buffer_mut().move_left();
            }
            KeyCode::Right if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    self.main_focus = MainFocus::Editor;
                    self.status_message = "焦点切换到编辑区".to_string();
                    return;
                }
                self.active_buffer_mut().move_right();
            }
            KeyCode::Down if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    self.tree_select_next();
                    return;
                }
                self.active_buffer_mut().move_down();
            }
            KeyCode::Up if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    self.tree_select_prev();
                    return;
                }
                self.active_buffer_mut().move_up();
            }
            KeyCode::Esc => {
                self.normal_pending.clear();
                self.status_message = "NORMAL".to_string();
            }
            KeyCode::Enter if self.normal_pending.is_empty() => {
                if self.main_focus == MainFocus::Tree {
                    self.open_selected_tree_entry();
                    return;
                }
                if !self.try_execute_enter_command() {
                    self.normal_pending.clear();
                }
            }
            KeyCode::Enter => {
                if !self.try_execute_enter_command() {
                    self.normal_pending.clear();
                }
            }
            KeyCode::Char(ch) => {
                if self.main_focus == MainFocus::Tree {
                    match ch {
                        'j' => self.tree_select_next(),
                        'k' => self.tree_select_prev(),
                        'l' => self.open_selected_tree_entry(),
                        'h' => {
                            if self.tree_entries.is_empty() {
                                return;
                            }
                            let path = self.tree_entries[self.tree_selected].path.clone();
                            self.toggle_expand_dir(path);
                        }
                        _ => {}
                    }
                    return;
                }

                self.normal_pending.push(ch);
                if self.try_execute_normal_command() {
                    self.normal_pending.clear();
                    return;
                }
                if !is_normal_command_prefix(&self.normal_pending) {
                    self.normal_pending.clear();
                }
            }
            _ => {}
        }
    }

    // 处理 INSERT 模式按键。
    pub(super) fn handle_insert_key_event(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = EditorMode::Normal;
                self.insert_j_pending = false;
                self.status_message = "NORMAL".to_string();
                self.refresh_completion();
            }
            KeyCode::Char('k') if self.insert_j_pending => {
                // `jk` 作为 INSERT 模式退出快捷键：
                // - 首个 `j` 已在上一拍被插入；
                // - 当前拍输入 `k` 时回退该 `j`，避免将 `jk` 残留到文本里。
                self.active_buffer_mut().backspace();
                self.mode = EditorMode::Normal;
                self.insert_j_pending = false;
                self.status_message = "NORMAL".to_string();
                self.refresh_completion();
            }
            KeyCode::Char(ch) => {
                self.insert_j_pending = ch == 'j';
                self.active_buffer_mut().insert_char(ch);
                self.refresh_completion();
            }
            KeyCode::Backspace => {
                self.insert_j_pending = false;
                self.active_buffer_mut().backspace();
                self.refresh_completion();
            }
            KeyCode::Enter => {
                self.insert_j_pending = false;
                self.active_buffer_mut().insert_newline();
                self.refresh_completion();
            }
            KeyCode::Tab => {
                if !self.completion_items.is_empty() {
                    self.accept_completion();
                    self.refresh_completion();
                } else {
                    for _ in 0..4 {
                        self.active_buffer_mut().insert_char(' ');
                    }
                    self.refresh_completion();
                }
            }
            KeyCode::Left => {
                self.active_buffer_mut().move_left();
                self.refresh_completion();
            }
            KeyCode::Right => {
                self.active_buffer_mut().move_right();
                self.refresh_completion();
            }
            KeyCode::Up => {
                self.active_buffer_mut().move_up();
                self.refresh_completion();
            }
            KeyCode::Down => {
                self.active_buffer_mut().move_down();
                self.refresh_completion();
            }
            _ => {
                self.insert_j_pending = false;
            }
        }
    }

    // 处理 TERMINAL 模式按键。
    pub(super) fn handle_terminal_key_event(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                if self.terminal_escape_pending {
                    self.mode = EditorMode::Normal;
                    self.terminal_escape_pending = false;
                    self.status_message = "NORMAL".to_string();
                } else {
                    self.terminal_escape_pending = true;
                }
            }
            KeyCode::Char('n')
                if self.terminal_escape_pending
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.mode = EditorMode::Normal;
                self.terminal_escape_pending = false;
                self.status_message = "NORMAL".to_string();
            }
            _ => {
                self.terminal_escape_pending = false;
            }
        }
    }

    // 处理缓冲区选择模式按键。
    pub(super) fn handle_buffer_picker_key_event(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = EditorMode::Normal;
                self.status_message = "NORMAL".to_string();
            }
            KeyCode::Char(ch) => {
                let idx = (ch as u8).wrapping_sub(b'a') as usize;
                if idx < self.buffers.len() {
                    // 切换前对当前 Rust 文件发送 didClose，避免语言服务端保留陈旧打开状态。
                    let current_idx = self.tabs[self.active_tab].buffer_index;
                    self.try_send_did_close_for_buffer_idx(current_idx);

                    self.tabs[self.active_tab].buffer_index = idx;
                    self.mode = EditorMode::Normal;
                    self.status_message = format!("已切换到缓冲区：{}", self.buffers[idx].name);

                    // 切换后对目标 Rust 缓冲区补发 didOpen，恢复语义上下文。
                    self.try_send_did_open_for_buffer_idx(idx);
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        let Some(area) = self.last_area else {
            return;
        };

        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area)[1];

        if self.show_tree {
            let tree_width = body.width.saturating_mul(self.tree_ratio) / 100;
            let divider_x = body.x + tree_width.saturating_sub(1);
            let divider_hit = mouse.column == divider_x
                && mouse.row >= body.y
                && mouse.row < body.y + body.height;

            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) if divider_hit => {
                    self.dragging_divider = true;
                    return;
                }
                MouseEventKind::Drag(MouseButton::Left) if self.dragging_divider => {
                    self.adjust_tree_ratio(body, mouse.column);
                    return;
                }
                MouseEventKind::Up(MouseButton::Left) if self.dragging_divider => {
                    self.dragging_divider = false;
                    return;
                }
                _ => {}
            }

            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(tree_width), Constraint::Min(1)])
                .split(body);

            if contains_point(panes[0], mouse.column, mouse.row)
                && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            {
                self.main_focus = MainFocus::Tree;
                self.select_tree_by_mouse(panes[0], mouse.row);
                return;
            }

            if contains_point(panes[1], mouse.column, mouse.row)
                && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            {
                self.main_focus = MainFocus::Editor;
                return;
            }
        }

        if matches!(mouse.kind, MouseEventKind::ScrollDown) {
            if self.main_focus == MainFocus::Tree {
                self.tree_select_next();
            } else {
                self.active_buffer_mut().move_down();
            }
        } else if matches!(mouse.kind, MouseEventKind::ScrollUp) {
            if self.main_focus == MainFocus::Tree {
                self.tree_select_prev();
            } else {
                self.active_buffer_mut().move_up();
            }
        }
    }

    // 根据鼠标位置选择目录树条目。
    pub(super) fn select_tree_by_mouse(&mut self, tree_area: ratatui::layout::Rect, row: u16) {
        if self.tree_entries.is_empty() {
            return;
        }
        let inner_top = tree_area.y.saturating_add(1);
        if row < inner_top {
            return;
        }
        let offset = row.saturating_sub(inner_top) as usize;
        let idx = self.tree_scroll + offset;
        if idx >= self.tree_entries.len() {
            return;
        }
        self.tree_selected = idx;
        self.open_selected_tree_entry();
    }

    // 目录树向下移动选中项。
    pub(super) fn tree_select_next(&mut self) {
        if self.tree_entries.is_empty() {
            return;
        }
        self.tree_selected = min(self.tree_selected + 1, self.tree_entries.len() - 1);
    }

    // 目录树向上移动选中项。
    pub(super) fn tree_select_prev(&mut self) {
        if self.tree_entries.is_empty() {
            return;
        }
        self.tree_selected = self.tree_selected.saturating_sub(1);
    }

    // 打开当前目录树选中项。
    pub(super) fn open_selected_tree_entry(&mut self) {
        if self.tree_entries.is_empty() {
            return;
        }
        let idx = self.tree_selected;
        if self.tree_entries[idx].is_dir {
            let path = self.tree_entries[idx].path.clone();
            self.toggle_expand_dir(path);
        } else {
            self.open_file_in_current_tab(self.tree_entries[idx].path.clone());
        }
    }

    // 切换目录展开/折叠状态。
    pub(super) fn toggle_expand_dir(&mut self, dir: PathBuf) {
        if self.expanded_dirs.contains(&dir) {
            self.expanded_dirs.remove(&dir);
        } else {
            self.expanded_dirs.insert(dir);
        }
        self.refresh_tree_entries();
    }

    pub(super) fn adjust_tree_ratio(&mut self, body: ratatui::layout::Rect, mouse_x: u16) {
        let relative = mouse_x
            .saturating_sub(body.x)
            .clamp(1, body.width.saturating_sub(1));
        let ratio = ((relative as f32 / body.width.max(1) as f32) * 100.0).round() as u16;
        self.tree_ratio = ratio.clamp(MIN_TREE_RATIO, MAX_TREE_RATIO);
    }

    // 处理 Enter 触发的简短命令。
    pub(super) fn try_execute_enter_command(&mut self) -> bool {
        if self.normal_pending.is_empty() {
            return false;
        }

        match self.normal_pending.as_str() {
            "w" => {
                self.save_current_file();
                true
            }
            "q" => {
                self.should_exit = true;
                get_exit().store(true, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    // 处理 NORMAL 模式命令。
    pub(super) fn try_execute_normal_command(&mut self) -> bool {
        match self.normal_pending.as_str() {
            "fs" => {
                self.save_session();
                true
            }
            "fl" => {
                self.load_session();
                self.refresh_tree_entries();
                true
            }
            "sv" => {
                self.tabs[self.active_tab].split = SplitDirection::Vertical;
                self.tabs[self.active_tab].focus = PaneFocus::Primary;
                self.status_message = "已切换到垂直分屏".to_string();
                true
            }
            "sp" => {
                self.tabs[self.active_tab].split = SplitDirection::Horizontal;
                self.tabs[self.active_tab].focus = PaneFocus::Primary;
                self.status_message = "已切换到水平分屏".to_string();
                true
            }
            "sh" => {
                if !self.show_tree {
                    self.show_tree = true;
                }
                self.main_focus = MainFocus::Tree;
                self.status_message = "焦点切换到左侧目录树".to_string();
                true
            }
            "sl" => {
                self.main_focus = MainFocus::Editor;
                if self.tabs[self.active_tab].split == SplitDirection::Vertical {
                    self.tabs[self.active_tab].focus = PaneFocus::Secondary;
                    self.status_message = "焦点切换到右侧窗格".to_string();
                } else {
                    self.tabs[self.active_tab].focus = PaneFocus::Primary;
                    self.status_message = "焦点切换到编辑区".to_string();
                }
                true
            }
            "sj" => {
                self.main_focus = MainFocus::Editor;
                if self.tabs[self.active_tab].split == SplitDirection::Horizontal {
                    self.tabs[self.active_tab].focus = PaneFocus::Secondary;
                    self.status_message = "焦点切换到下方窗格".to_string();
                } else {
                    self.tabs[self.active_tab].focus = PaneFocus::Primary;
                    self.status_message = "当前无下方窗格，已定位到编辑区".to_string();
                }
                true
            }
            "sk" => {
                self.main_focus = MainFocus::Editor;
                self.tabs[self.active_tab].focus = PaneFocus::Primary;
                self.status_message = "焦点切换到上方主窗格".to_string();
                true
            }
            "tn" => {
                self.new_tab();
                true
            }
            "tl" => {
                self.next_tab();
                true
            }
            "th" => {
                self.prev_tab();
                true
            }
            "tb" => {
                self.show_tree = !self.show_tree;
                self.status_message = format!("Tree {}", if self.show_tree { "ON" } else { "OFF" });
                true
            }
            "tc" => {
                self.close_tab();
                true
            }
            "tt" => {
                self.show_tagbar = !self.show_tagbar;
                self.status_message =
                    format!("TagBar {}", if self.show_tagbar { "ON" } else { "OFF" });
                true
            }
            "te" => {
                self.mode = EditorMode::Terminal;
                self.status_message = "TERMINAL".to_string();
                true
            }
            "e" => {
                self.mode = EditorMode::BufferPicker;
                self.status_message = "BUFFER PICKER".to_string();
                true
            }
            "pi" => {
                self.main_focus = MainFocus::Tree;
                self.status_message = "焦点切换到目录树".to_string();
                true
            }
            "pu" => {
                self.main_focus = MainFocus::Editor;
                self.status_message = "焦点切换到编辑区".to_string();
                true
            }
            "ci" => {
                self.completion_selected = self.completion_selected.saturating_sub(1);
                true
            }
            "cu" => {
                if !self.completion_items.is_empty() {
                    self.completion_selected = min(
                        self.completion_selected + 1,
                        self.completion_items.len().saturating_sub(1),
                    );
                }
                true
            }
            "w" => {
                self.save_current_file();
                true
            }
            "q" => {
                self.should_exit = true;
                get_exit().store(true, std::sync::atomic::Ordering::Relaxed);
                true
            }
            "fa" => {
                self.search_word_under_cursor();
                true
            }
            "ff" => {
                self.mode = EditorMode::BufferPicker;
                self.status_message = "BUFFER PICKER".to_string();
                true
            }
            "fh" => {
                if !self.command_history.is_empty() {
                    self.status_message = format!("历史命令：{}", self.command_history.join(" | "));
                }
                true
            }
            "fc" => {
                self.mode = EditorMode::Normal;
                self.status_message = "NORMAL".to_string();
                true
            }
            "lc" => {
                self.run_lsp_server_check();
                true
            }
            "fb" => {
                self.theme = self.theme.next();
                self.status_message = format!("theme => {}", self.theme.as_str());
                true
            }
            "[g" => {
                if !self.diagnostics.is_empty() {
                    self.diagnostic_index = self.diagnostic_index.saturating_sub(1);
                    self.status_message = self.diagnostics[self.diagnostic_index].clone();
                }
                true
            }
            "]g" => {
                if !self.diagnostics.is_empty() {
                    self.diagnostic_index =
                        min(self.diagnostic_index + 1, self.diagnostics.len() - 1);
                    self.status_message = self.diagnostics[self.diagnostic_index].clone();
                }
                true
            }
            "K" => {
                if !self.diagnostics.is_empty() {
                    self.status_message = self.diagnostics[self.diagnostic_index].clone();
                }
                true
            }
            _ => false,
        }
    }

    // 功能说明：见下方实现。
    pub(super) fn save_current_file(&mut self) {
        // 在本地落盘前先发送 willSave 系列通知/请求，
        // 尽量兼容语言服务端的保存前处理流程。
        self.try_send_will_save_for_active_buffer();

        let root = self.root.clone();
        match self.active_buffer_mut().save(&root) {
            Ok(path) => {
                self.status_message = format!("保存成功：{}", path.display());

                // 保存后发送 didSave，让 rust-analyzer 尽快更新语义/诊断。
                self.try_send_did_save_for_path(&path);
            }
            Err(err) => self.status_message = format!("保存失败：{}", err),
        }
    }

    // 搜索并跳转到当前单词。
    pub(super) fn search_word_under_cursor(&mut self) {
        let Some((_, _, word)) = self.active_buffer().word_prefix() else {
            self.status_message = "光标处没有可搜索的单词".to_string();
            return;
        };
        let row = self.active_buffer().cursor_row;

        let found = self
            .active_buffer()
            .lines
            .iter()
            .enumerate()
            .skip(row + 1)
            .find(|(_, line)| line.contains(&word))
            .map(|(idx, _)| idx)
            .or_else(|| {
                self.active_buffer()
                    .lines
                    .iter()
                    .enumerate()
                    .take(row)
                    .find(|(_, line)| line.contains(&word))
                    .map(|(idx, _)| idx)
            });

        if let Some(idx) = found {
            let buffer = self.active_buffer_mut();
            buffer.cursor_row = idx;
            buffer.cursor_col = 0;
            buffer.ensure_cursor_in_bounds();
            self.status_message = format!("已定位到：{}", word);
        } else {
            self.status_message = format!("未找到：{}", word);
        }
    }

    // 刷新自动补全候选列表。
    pub(super) fn refresh_completion(&mut self) {
        // 先使用最近一次 LSP 返回结果更新 UI，再异步请求下一轮补全。
        self.refresh_completion_from_lsp_cache();
        self.request_completion_for_active_buffer();
    }

    /// 基于 buffer 缓存中的 LSP 补全项刷新展示列表。
    ///
    /// 这里按“当前前缀 + 插入文本”做一次轻过滤，
    /// 是为了避免服务端返回候选过多时干扰编辑体验。
    pub(super) fn refresh_completion_from_lsp_cache(&mut self) {
        let Some((_, _, prefix)) = self.active_buffer().word_prefix() else {
            self.completion_items.clear();
            self.completion_selected = 0;
            return;
        };
        if prefix.is_empty() {
            self.completion_items.clear();
            self.completion_selected = 0;
            return;
        }

        let mut candidates = Vec::new();
        for item in &self.active_buffer().lsp_completion_items {
            let insert_text = item.insert_text.as_deref().unwrap_or(&item.label);
            if insert_text.starts_with(&prefix) && insert_text != prefix {
                candidates.push(insert_text.to_string());
                continue;
            }

            // 当服务端返回 label 与 insertText 不一致时，
            // 回退到 label 过滤，确保候选不会被意外漏掉。
            if item.label.starts_with(&prefix) && item.label != prefix {
                candidates.push(item.label.clone());
            }
        }

        candidates.sort();
        candidates.dedup();
        self.completion_items = candidates.into_iter().take(20).collect();
        if self.completion_selected >= self.completion_items.len() {
            self.completion_selected = 0;
        }
    }

    /// 向当前活动缓冲区发起一次 LSP 补全请求。
    ///
    /// 采用“异步请求 + 下一帧消费”策略，避免在按键路径中阻塞输入手感。
    fn request_completion_for_active_buffer(&mut self) {
        let buffer_idx = self.tabs[self.active_tab].buffer_index;
        let Some((path, cursor_row, cursor_col)) =
            self.buffers.get(buffer_idx).and_then(|buffer| {
                let path = buffer.path.as_ref()?.clone();
                Some((path, buffer.cursor_row, buffer.cursor_col))
            })
        else {
            // 未落盘缓冲区无法生成稳定 URI，暂不触发 LSP 补全。
            return;
        };

        if let Err(error) = self
            .lsp_client
            .request_completion(&path, cursor_row, cursor_col)
        {
            self.status_message = format!("LSP completion 请求失败: {error}");
        }
    }

    // 应用当前选中的补全项。
    pub(super) fn accept_completion(&mut self) {
        if self.completion_items.is_empty() {
            return;
        }

        let selected_label = self.completion_items[self.completion_selected].clone();
        let choice = self
            .active_buffer()
            .lsp_completion_items
            .iter()
            .find_map(|item| {
                let insert_text = item.insert_text.as_deref().unwrap_or(&item.label);
                if insert_text == selected_label || item.label == selected_label {
                    Some(insert_text.to_string())
                } else {
                    None
                }
            })
            .unwrap_or(selected_label);

        if let Some((start, end, _)) = self.active_buffer().word_prefix() {
            self.active_buffer_mut().replace_prefix(start, end, &choice);
        }
    }

    // 新建标签页。
    pub(super) fn new_tab(&mut self) {
        let name = format!("untitled-{}", self.buffers.len() + 1);
        self.buffers.push(EditorBuffer::new_empty(name));
        let idx = self.buffers.len().saturating_sub(1);
        self.tabs.push(TabState {
            title: format!("Tab-{}", self.tabs.len() + 1),
            buffer_index: idx,
            split: SplitDirection::None,
            focus: PaneFocus::Primary,
        });
        self.active_tab = self.tabs.len().saturating_sub(1);
        self.status_message = "已新建 TAB".to_string();
    }

    // 关闭当前标签页。
    pub(super) fn close_tab(&mut self) {
        if self.tabs.len() <= 1 {
            self.status_message = "至少保留一个 TAB".to_string();
            return;
        }

        let closing_idx = self.tabs[self.active_tab].buffer_index;
        self.try_send_did_close_for_buffer_idx(closing_idx);

        self.tabs.remove(self.active_tab);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        self.normalize_active_tab_focus();
        self.status_message = "已关闭 TAB".to_string();

        // 关闭后给新激活页补发 didOpen，保证 LSP 文档上下文一致。
        if !self.tabs.is_empty() {
            let active_idx = self.tabs[self.active_tab].buffer_index;
            self.try_send_did_open_for_buffer_idx(active_idx);
        }
    }

    // 切换到下一个标签页。
    pub(super) fn next_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
        self.normalize_active_tab_focus();
        self.status_message = "已切换到下一个 TAB".to_string();
    }

    // 切换到上一个标签页。
    pub(super) fn prev_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        if self.active_tab == 0 {
            self.active_tab = self.tabs.len().saturating_sub(1);
        } else {
            self.active_tab -= 1;
        }
        self.normalize_active_tab_focus();
        self.status_message = "已切换到上一个 TAB".to_string();
    }

    // 在当前标签页打开文件。
    pub(super) fn open_file_in_current_tab(&mut self, path: PathBuf) {
        self.main_focus = MainFocus::Editor;
        self.normalize_active_tab_focus();
        if let Some((idx, _)) = self
            .buffers
            .iter()
            .enumerate()
            .find(|(_, b)| b.path.as_ref().is_some_and(|p| p == &path))
        {
            self.tabs[self.active_tab].buffer_index = idx;
            self.tabs[self.active_tab].title = file_name_or(path.as_path(), "Tab").to_string();
            self.status_message = format!("已打开：{}", path.display());

            // 对已缓存的 Rust 文件同样发送 didOpen，确保 LSP 获取最新上下文。
            self.try_send_did_open_for_buffer_idx(idx);
            return;
        }

        match EditorBuffer::from_file(&path) {
            Ok(buffer) => {
                self.buffers.push(buffer);
                let idx = self.buffers.len().saturating_sub(1);
                self.tabs[self.active_tab].buffer_index = idx;
                self.tabs[self.active_tab].title = file_name_or(path.as_path(), "Tab").to_string();
                self.status_message = format!("已打开：{}", path.display());

                self.try_send_did_open_for_buffer_idx(idx);
            }
            Err(err) => {
                self.status_message = format!("打开失败：{}", err);
            }
        }
    }

    /// 若指定缓冲区是受支持语言文件，则发送 `textDocument/didOpen`。
    ///
    /// 该方法会在 `editor::mod` 的缓冲区切换逻辑中被复用，
    /// 因此需要对父模块可见，避免重复实现同一套 didOpen 触发流程。
    pub(super) fn try_send_did_open_for_buffer_idx(&mut self, buffer_idx: usize) {
        let Some((path, text, version)) = self.buffers.get(buffer_idx).and_then(|buffer| {
            let path = buffer.path.as_ref()?.clone();
            Some((path, buffer.lines.join("\n"), buffer.lsp_version))
        }) else {
            return;
        };

        match self
            .lsp_client
            .send_did_open(&self.root, &path, &text, version)
        {
            Ok(_) => {
                self.status_message = format!("已打开：{}（LSP didOpen 已发送）", path.display());
                if let Some(buffer_mut) = self.buffers.get_mut(buffer_idx) {
                    buffer_mut.lsp_dirty = false;
                    buffer_mut.lsp_last_synced_text = Some(text);
                }

                // `didOpen` 后主动拉取语义 token，确保首次渲染就有语义高亮。
                if let Err(error) = self.lsp_client.request_semantic_tokens(&path) {
                    self.status_message = format!(
                        "已打开：{}（LSP semanticTokens 失败: {}）",
                        path.display(),
                        error
                    );
                }
            }
            Err(error) => {
                self.status_message =
                    format!("已打开：{}（LSP didOpen 失败: {}）", path.display(), error);
            }
        }
    }

    /// 若指定缓冲区是受支持语言文件，则发送 `textDocument/didClose`。
    fn try_send_did_close_for_buffer_idx(&mut self, buffer_idx: usize) {
        let Some(path) = self
            .buffers
            .get(buffer_idx)
            .and_then(|buffer| buffer.path.as_ref().cloned())
        else {
            return;
        };

        match self.lsp_client.send_did_close(&path) {
            Ok(_) => {
                self.status_message = format!("LSP didClose：{}", path.display());
            }
            Err(error) => {
                self.status_message = format!("LSP didClose 失败：{}", error);
            }
        }
    }

    /// 若路径是受支持语言文件，则发送 `textDocument/didSave`。
    fn try_send_did_save_for_path(&mut self, path: &Path) {
        let text = self.active_buffer().lines.join("\n");
        match self.lsp_client.send_did_save(path, &text) {
            Ok(_) => {
                self.status_message = format!("保存成功：{}（LSP didSave 已发送）", path.display());

                // 保存后触发语义 token 刷新，确保格式化/导入变化能及时反映。
                if let Err(error) = self.lsp_client.request_semantic_tokens(path) {
                    self.status_message = format!(
                        "保存成功：{}（LSP semanticTokens 失败: {}）",
                        path.display(),
                        error
                    );
                }
            }
            Err(error) => {
                self.status_message = format!(
                    "保存成功：{}（LSP didSave 失败: {}）",
                    path.display(),
                    error
                );
            }
        }
    }

    /// 对当前活动缓冲区发送 willSave 与 willSaveWaitUntil。
    fn try_send_will_save_for_active_buffer(&mut self) {
        let buffer_idx = self.tabs[self.active_tab].buffer_index;
        let Some(path) = self
            .buffers
            .get(buffer_idx)
            .and_then(|buffer| buffer.path.as_ref().cloned())
        else {
            return;
        };

        if let Err(error) = self.lsp_client.send_will_save(&path) {
            self.status_message = format!("LSP willSave 失败：{}", error);
            return;
        }

        if let Err(error) = self.lsp_client.send_will_save_wait_until(&path) {
            self.status_message = format!("LSP willSaveWaitUntil 失败：{}", error);
        }
    }

    pub(super) fn refresh_tree_entries(&mut self) {
        let selected_path = self
            .tree_entries
            .get(self.tree_selected)
            .map(|entry| entry.path.clone());

        self.tree_entries = super::collect_tree_entries(&self.root, &self.expanded_dirs);

        if self.tree_entries.is_empty() {
            self.tree_selected = 0;
            self.tree_scroll = 0;
            return;
        }

        if let Some(path) = selected_path
            && let Some(idx) = self
                .tree_entries
                .iter()
                .position(|entry| entry.path == path)
        {
            self.tree_selected = idx;
            return;
        }

        self.tree_selected = min(self.tree_selected, self.tree_entries.len() - 1);
    }

    /// 执行 LSP 服务器可用性检查，并将结果汇总到状态栏。
    ///
    /// 结果展示策略：
    /// - 全部可用时给出成功摘要；
    /// - 存在缺失时显示缺失语言与安装建议（截断到可读长度）。
    fn run_lsp_server_check(&mut self) {
        let report = self.lsp_client.check_server_availability();
        let missing_items: Vec<_> = report
            .items
            .iter()
            .filter(|item| !item.available)
            .cloned()
            .collect();

        if missing_items.is_empty() {
            self.status_message = format!(
                "LSP 检查通过：{}/{} 可用",
                report.available_count(),
                report.items.len()
            );
            return;
        }

        let missing_languages = missing_items
            .iter()
            .map(|item| item.language.as_str())
            .collect::<Vec<_>>()
            .join(", ");

        let hint = missing_items
            .first()
            .map(|item| format!("{}（命令 `{}`）", item.install_hint, item.server_command))
            .unwrap_or_else(|| "请检查语言服务器安装与 PATH".to_string());

        // 状态栏空间有限，这里做一次长度保护，避免挤压其他关键信息。
        let mut message = format!(
            "LSP 缺失 {}/{}：{}。{}",
            report.missing_count(),
            report.items.len(),
            missing_languages,
            hint
        );
        if message.chars().count() > 180 {
            message = message.chars().take(180).collect::<String>() + "…";
        }
        self.status_message = message;
    }
}
