use std::{
    cmp::min,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use ratatui::style::Color;

use lsp::{LspCompletionItem, LspSemanticToken};

use super::utils::{char_count, char_to_byte_index, file_name_or, is_word_char};

// 功能说明：见下方实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
    Normal,
    Insert,
    /// 视觉模式：先提供 Vim 风格的进入/退出体验，避免在未实现选区时误触普通命令。
    Visual,
    Terminal,
    BufferPicker,
}

// 功能说明：见下方实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SplitDirection {
    None,
    Vertical,
    Horizontal,
}

// 功能说明：见下方实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneFocus {
    Primary,
    Secondary,
}

// 功能说明：见下方实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MainFocus {
    Tree,
    Editor,
}

// 功能说明：见下方实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThemeName {
    MaterialOcean,
    Gruvbox,
    One,
}

impl ThemeName {
    // 返回主题名称字符串。
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::MaterialOcean => "material ocean",
            Self::Gruvbox => "gruvbox",
            Self::One => "one",
        }
    }

    // 切换到下一个主题。
    pub(super) fn next(self) -> Self {
        match self {
            Self::MaterialOcean => Self::Gruvbox,
            Self::Gruvbox => Self::One,
            Self::One => Self::MaterialOcean,
        }
    }

    // 从字符串解析主题。
    pub(super) fn parse(input: &str) -> Self {
        match input {
            "gruvbox" => Self::Gruvbox,
            "one" => Self::One,
            _ => Self::MaterialOcean,
        }
    }
}

// 功能说明：见下方实现。
#[derive(Debug, Clone, Copy)]
pub(super) struct ThemePalette {
    pub(super) bg: Color,
    pub(super) fg: Color,
    pub(super) accent: Color,
    pub(super) dim: Color,
    pub(super) warn: Color,
    pub(super) ok: Color,
}

impl ThemePalette {
    // 根据主题生成配色。
    pub(super) fn from_theme(theme: ThemeName) -> Self {
        match theme {
            ThemeName::MaterialOcean => Self {
                bg: Color::Rgb(38, 50, 56),
                fg: Color::Rgb(238, 255, 255),
                accent: Color::Rgb(130, 170, 255),
                dim: Color::Rgb(144, 164, 174),
                warn: Color::Rgb(255, 203, 107),
                ok: Color::Rgb(195, 232, 141),
            },
            ThemeName::Gruvbox => Self {
                bg: Color::Rgb(40, 40, 40),
                fg: Color::Rgb(235, 219, 178),
                accent: Color::Rgb(131, 165, 152),
                dim: Color::Rgb(146, 131, 116),
                warn: Color::Rgb(250, 189, 47),
                ok: Color::Rgb(184, 187, 38),
            },
            ThemeName::One => Self {
                bg: Color::Rgb(40, 44, 52),
                fg: Color::Rgb(171, 178, 191),
                accent: Color::Rgb(97, 175, 239),
                dim: Color::Rgb(92, 99, 112),
                warn: Color::Rgb(229, 192, 123),
                ok: Color::Rgb(152, 195, 121),
            },
        }
    }
}

// 功能说明：见下方实现。
#[derive(Debug, Clone)]
pub(super) struct TreeEntry {
    pub(super) path: PathBuf,
    pub(super) depth: usize,
    pub(super) is_dir: bool,
    pub(super) name: String,
}

/// editor 展示层使用的补全候选。
///
/// 设计为结构体而不是字符串，目的是同时保留：
/// - `label`：用于 popover 展示；
/// - `insert_text`：用于真正插入到缓冲区；
/// - `detail`：用于展示更完整的 LSP 上下文提示。
#[derive(Debug, Clone)]
pub(super) struct CompletionDisplayItem {
    pub(super) label: String,
    pub(super) insert_text: String,
    pub(super) detail: Option<String>,
}

// 功能说明：见下方实现。
#[derive(Debug, Clone)]
pub(super) struct EditorBuffer {
    pub(super) name: String,
    pub(super) path: Option<PathBuf>,
    pub(super) lines: Vec<String>,
    pub(super) cursor_row: usize,
    pub(super) cursor_col: usize,
    pub(super) scroll_row: usize,
    pub(super) modified: bool,
    /// LSP 文档版本号。
    ///
    /// 按 LSP 规范，`didOpen/didChange` 需携带单调递增版本号。
    pub(super) lsp_version: i32,
    /// 当前缓冲区是否存在尚未同步到 LSP 的变更。
    ///
    /// 当编辑内容变化后置为 `true`，同步成功后置为 `false`。
    pub(super) lsp_dirty: bool,
    /// 最近一次成功同步到 LSP 的文本快照。
    ///
    /// 用于增量 `didChange` 计算 old/new 差异。
    pub(super) lsp_last_synced_text: Option<String>,
    /// 当前缓冲区最近一次 LSP 返回的补全候选。
    ///
    /// 使用结构化补全项而不是纯字符串，
    /// 是为了后续可扩展 `insert_text/detail` 等上下文信息。
    pub(super) lsp_completion_items: Vec<LspCompletionItem>,
    /// 当前缓冲区最近一次 LSP 返回的语义高亮 token。
    ///
    /// 语义 token 由 LSP 异步返回，渲染阶段按行读取，
    /// 因此将其缓存在 buffer 内可避免重复请求。
    pub(super) lsp_semantic_tokens: Vec<LspSemanticToken>,
    /// 按行索引后的语义 token 映射。
    ///
    /// 将 token 预先分组到行级，可以把渲染时复杂度降到 O(当前行 token 数)，
    /// 避免每一帧都全量扫描 token 列表。
    pub(super) lsp_tokens_by_line: HashMap<usize, Vec<LspSemanticToken>>,
}

impl EditorBuffer {
    // 创建空白缓冲区。
    pub(super) fn new_empty(name: String) -> Self {
        Self {
            name,
            path: None,
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_row: 0,
            modified: false,
            lsp_version: 1,
            lsp_dirty: false,
            lsp_last_synced_text: None,
            lsp_completion_items: Vec::new(),
            lsp_semantic_tokens: Vec::new(),
            lsp_tokens_by_line: HashMap::new(),
        }
    }

    // 从文件加载缓冲区。
    pub(super) fn from_file(path: &Path) -> std::io::Result<Self> {
        let content = fs::read_to_string(path)?;
        let mut lines: Vec<String> = content.lines().map(ToString::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Ok(Self {
            name: file_name_or(path, "untitled").to_string(),
            path: Some(path.to_path_buf()),
            lines,
            cursor_row: 0,
            cursor_col: 0,
            scroll_row: 0,
            modified: false,
            lsp_version: 1,
            lsp_dirty: false,
            lsp_last_synced_text: None,
            lsp_completion_items: Vec::new(),
            lsp_semantic_tokens: Vec::new(),
            lsp_tokens_by_line: HashMap::new(),
        })
    }

    // 修正光标越界问题。
    pub(super) fn ensure_cursor_in_bounds(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = min(self.cursor_row, self.lines.len().saturating_sub(1));
        self.cursor_col = min(self.cursor_col, char_count(&self.lines[self.cursor_row]));
    }

    // 光标左移。
    pub(super) fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = char_count(&self.lines[self.cursor_row]);
        }
    }

    // 光标右移。
    pub(super) fn move_right(&mut self) {
        let max_col = char_count(&self.lines[self.cursor_row]);
        if self.cursor_col < max_col {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    // 光标上移。
    pub(super) fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = min(self.cursor_col, char_count(&self.lines[self.cursor_row]));
        }
    }

    // 光标下移。
    pub(super) fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = min(self.cursor_col, char_count(&self.lines[self.cursor_row]));
        }
    }

    // 在光标位置插入字符。
    pub(super) fn insert_char(&mut self, ch: char) {
        let row = self.cursor_row;
        let col = self.cursor_col;
        let line = &mut self.lines[row];
        let byte_idx = char_to_byte_index(line, col);
        line.insert(byte_idx, ch);
        self.cursor_col += 1;
        self.modified = true;
        self.lsp_dirty = true;
    }

    // 删除光标前字符。
    pub(super) fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let start = char_to_byte_index(line, self.cursor_col - 1);
            let end = char_to_byte_index(line, self.cursor_col);
            line.replace_range(start..end, "");
            self.cursor_col -= 1;
            self.modified = true;
            self.lsp_dirty = true;
        } else if self.cursor_row > 0 {
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            let prev = &mut self.lines[self.cursor_row];
            let old_len = char_count(prev);
            prev.push_str(&current);
            self.cursor_col = old_len;
            self.modified = true;
            self.lsp_dirty = true;
        }
    }

    // 在光标处换行。
    pub(super) fn insert_newline(&mut self) {
        let line = &mut self.lines[self.cursor_row];
        let split = char_to_byte_index(line, self.cursor_col);
        let rest = line.split_off(split);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.lines.insert(self.cursor_row, rest);
        self.modified = true;
        self.lsp_dirty = true;
    }

    // 获取当前单词前缀。
    pub(super) fn word_prefix(&self) -> Option<(usize, usize, String)> {
        let chars: Vec<char> = self.lines.get(self.cursor_row)?.chars().collect();
        let mut start = self.cursor_col;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        if start == self.cursor_col {
            return None;
        }
        Some((
            start,
            self.cursor_col,
            chars[start..self.cursor_col].iter().collect(),
        ))
    }

    // 用补全内容替换前缀。
    pub(super) fn replace_prefix(&mut self, start: usize, end: usize, replacement: &str) {
        let line = &mut self.lines[self.cursor_row];
        let start_byte = char_to_byte_index(line, start);
        let end_byte = char_to_byte_index(line, end);
        line.replace_range(start_byte..end_byte, replacement);
        self.cursor_col = start + replacement.chars().count();
        self.modified = true;
        self.lsp_dirty = true;
    }

    // 保存缓冲区内容到文件。
    pub(super) fn save(&mut self, cwd: &Path) -> std::io::Result<PathBuf> {
        let path = match &self.path {
            Some(path) => path.clone(),
            None => {
                let generated = cwd.join(format!("{}.txt", self.name));
                self.path = Some(generated.clone());
                generated
            }
        };
        fs::write(
            &path,
            self.lines.join(
                "
",
            ),
        )?;
        self.modified = false;
        Ok(path)
    }
}

// 功能说明：见下方实现。
#[derive(Debug, Clone)]
pub(super) struct TabState {
    pub(super) title: String,
    pub(super) buffer_index: usize,
    pub(super) split: SplitDirection,
    pub(super) focus: PaneFocus,
}
