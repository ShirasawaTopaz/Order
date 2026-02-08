use std::{cmp::min, fs, path::PathBuf};

use super::{
    Editor, MAX_TREE_RATIO, MIN_TREE_RATIO, SESSION_FILE,
    types::{EditorBuffer, PaneFocus, SplitDirection, TabState, ThemeName},
    utils::{escape_text, pane_to_str, parse_pane, parse_split, split_to_str, unescape_text},
};

// 会话层：负责编辑器状态的持久化与恢复。
impl Editor {
    // 保存当前会话（布局、主题、tab 与 buffer 光标位置等）。
    pub(super) fn save_session(&mut self) {
        let mut lines = Vec::new();
        lines.push(format!("tree_ratio={}", self.tree_ratio));
        lines.push(format!("show_tree={}", self.show_tree as u8));
        lines.push(format!("theme={}", self.theme.as_str()));
        lines.push(format!("active_tab={}", self.active_tab));

        for tab in &self.tabs {
            lines.push(format!(
                "TAB\t{}\t{}\t{}\t{}",
                escape_text(&tab.title),
                tab.buffer_index,
                split_to_str(tab.split),
                pane_to_str(tab.focus)
            ));
        }

        for buffer in &self.buffers {
            let path = buffer
                .path
                .as_ref()
                .map(|item| item.to_string_lossy().to_string())
                .unwrap_or_default();
            lines.push(format!(
                "BUF\t{}\t{}\t{}\t{}",
                escape_text(&buffer.name),
                escape_text(&path),
                buffer.cursor_row,
                buffer.cursor_col
            ));
        }

        let session_path = self.root.join(SESSION_FILE);
        match fs::write(&session_path, lines.join("\n")) {
            Ok(_) => {
                self.status_message = format!("会话已保存: {}", session_path.display());
            }
            Err(error) => {
                self.status_message = format!("会话保存失败: {}", error);
            }
        }
    }

    // 加载会话并恢复编辑器状态。
    pub(super) fn load_session(&mut self) {
        let session_path = self.root.join(SESSION_FILE);
        let content = match fs::read_to_string(&session_path) {
            Ok(text) => text,
            Err(error) => {
                self.status_message = format!("会话读取失败: {}", error);
                return;
            }
        };

        let mut tree_ratio = self.tree_ratio;
        let mut show_tree = self.show_tree;
        let mut theme = self.theme;
        let mut active_tab = 0usize;
        let mut tabs = Vec::new();
        let mut buffers = Vec::new();

        for line in content.lines() {
            if let Some(value) = line.strip_prefix("tree_ratio=") {
                if let Ok(parsed) = value.parse::<u16>() {
                    tree_ratio = parsed.clamp(MIN_TREE_RATIO, MAX_TREE_RATIO);
                }
                continue;
            }
            if let Some(value) = line.strip_prefix("show_tree=") {
                show_tree = value == "1";
                continue;
            }
            if let Some(value) = line.strip_prefix("theme=") {
                theme = ThemeName::parse(value.trim());
                continue;
            }
            if let Some(value) = line.strip_prefix("active_tab=") {
                if let Ok(parsed) = value.parse::<usize>() {
                    active_tab = parsed;
                }
                continue;
            }

            let parts: Vec<&str> = line.split('\t').collect();
            if parts.is_empty() {
                continue;
            }

            match parts[0] {
                "TAB" if parts.len() >= 5 => {
                    tabs.push(TabState {
                        title: unescape_text(parts[1]),
                        buffer_index: parts[2].parse::<usize>().unwrap_or(0),
                        split: parse_split(parts[3]),
                        focus: parse_pane(parts[4]),
                    });
                }
                "BUF" if parts.len() >= 5 => {
                    let name = unescape_text(parts[1]);
                    let path_value = unescape_text(parts[2]);
                    let row = parts[3].parse::<usize>().unwrap_or(0);
                    let col = parts[4].parse::<usize>().unwrap_or(0);

                    let mut buffer = if path_value.is_empty() {
                        EditorBuffer::new_empty(name.clone())
                    } else {
                        let file_path = PathBuf::from(path_value.clone());
                        match EditorBuffer::from_file(&file_path) {
                            Ok(mut loaded) => {
                                loaded.name = name.clone();
                                loaded
                            }
                            Err(_) => EditorBuffer::new_empty(name.clone()),
                        }
                    };

                    if !path_value.is_empty() {
                        buffer.path = Some(PathBuf::from(path_value));
                    }

                    buffer.cursor_row = row;
                    buffer.cursor_col = col;
                    buffer.ensure_cursor_in_bounds();
                    buffers.push(buffer);
                }
                _ => {}
            }
        }

        if buffers.is_empty() {
            buffers.push(EditorBuffer::new_empty("untitled-1".to_string()));
        }
        if tabs.is_empty() {
            tabs.push(TabState {
                title: "Tab-1".to_string(),
                buffer_index: 0,
                split: SplitDirection::None,
                focus: PaneFocus::Primary,
            });
        }
        for tab in &mut tabs {
            if tab.buffer_index >= buffers.len() {
                tab.buffer_index = 0;
            }
        }

        self.tree_ratio = tree_ratio;
        self.show_tree = show_tree;
        self.theme = theme;
        self.buffers = buffers;
        self.tabs = tabs;
        self.active_tab = min(active_tab, self.tabs.len().saturating_sub(1));
        self.status_message = format!("会话已加载: {}", session_path.display());
    }

    // 读取当前活动 buffer（只读）。
    pub(super) fn active_buffer(&self) -> &EditorBuffer {
        let index = self.tabs[self.active_tab].buffer_index;
        &self.buffers[index]
    }

    // 读取当前活动 buffer（可写）。
    pub(super) fn active_buffer_mut(&mut self) -> &mut EditorBuffer {
        let index = self.tabs[self.active_tab].buffer_index;
        &mut self.buffers[index]
    }
}
