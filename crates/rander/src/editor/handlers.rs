use std::{cmp::min, collections::BTreeSet, path::PathBuf};

use core::commands::get_exit;
use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Direction, Layout};

use super::{
    Editor, MAX_TREE_RATIO, MIN_TREE_RATIO,
    types::{EditorBuffer, EditorMode, MainFocus, PaneFocus, SplitDirection, TabState},
    utils::{contains_point, file_name_or, is_normal_command_prefix, is_word_char},
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
            KeyCode::Char('j') if self.insert_j_pending => {
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
            KeyCode::Left => self.active_buffer_mut().move_left(),
            KeyCode::Right => self.active_buffer_mut().move_right(),
            KeyCode::Up => self.active_buffer_mut().move_up(),
            KeyCode::Down => self.active_buffer_mut().move_down(),
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
                if self.terminal_escape_pending && key.modifiers.contains(KeyModifiers::CONTROL) =>
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
                    self.tabs[self.active_tab].buffer_index = idx;
                    self.mode = EditorMode::Normal;
                    self.status_message = format!("已切换到缓冲区：{}", self.buffers[idx].name);
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
            let divider_hit =
                mouse.column == divider_x && mouse.row >= body.y && mouse.row < body.y + body.height;

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
        let relative = mouse_x.saturating_sub(body.x).clamp(1, body.width.saturating_sub(1));
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
                self.status_message = format!("TagBar {}", if self.show_tagbar { "ON" } else { "OFF" });
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
                    self.diagnostic_index = min(self.diagnostic_index + 1, self.diagnostics.len() - 1);
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
        let root = self.root.clone();
        match self.active_buffer_mut().save(&root) {
            Ok(path) => self.status_message = format!("保存成功：{}", path.display()),
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

        let mut words = BTreeSet::new();
        for line in &self.active_buffer().lines {
            let mut current = String::new();
            for ch in line.chars() {
                if is_word_char(ch) {
                    current.push(ch);
                } else if !current.is_empty() {
                    if current.starts_with(&prefix) && current != prefix {
                        words.insert(current.clone());
                    }
                    current.clear();
                }
            }
            if !current.is_empty() && current.starts_with(&prefix) && current != prefix {
                words.insert(current);
            }
        }
        self.completion_items = words.into_iter().take(20).collect();
        if self.completion_selected >= self.completion_items.len() {
            self.completion_selected = 0;
        }
    }

    // 应用当前选中的补全项。
    pub(super) fn accept_completion(&mut self) {
        if self.completion_items.is_empty() {
            return;
        }
        let choice = self.completion_items[self.completion_selected].clone();
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
        self.tabs.remove(self.active_tab);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        self.normalize_active_tab_focus();
        self.status_message = "已关闭 TAB".to_string();
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
            return;
        }

        match EditorBuffer::from_file(&path) {
            Ok(buffer) => {
                self.buffers.push(buffer);
                let idx = self.buffers.len().saturating_sub(1);
                self.tabs[self.active_tab].buffer_index = idx;
                self.tabs[self.active_tab].title = file_name_or(path.as_path(), "Tab").to_string();
                self.status_message = format!("已打开：{}", path.display());
            }
            Err(err) => {
                self.status_message = format!("打开失败：{}", err);
            }
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
            && let Some(idx) = self.tree_entries.iter().position(|entry| entry.path == path)
        {
            self.tree_selected = idx;
            return;
        }

        self.tree_selected = min(self.tree_selected, self.tree_entries.len() - 1);
    }
}
