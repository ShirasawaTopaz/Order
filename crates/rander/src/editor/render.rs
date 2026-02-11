use std::cmp::min;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::sync::OnceLock;

use lsp::LspSemanticToken;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Widget},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Style as SyntectStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};

use super::{
    Editor,
    types::{EditorBuffer, EditorMode, MainFocus, PaneFocus, SplitDirection, ThemePalette},
};

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static SYNTAX_THEME: OnceLock<Theme> = OnceLock::new();

impl Editor {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
        self.last_area = Some(frame.area());
        let area = frame.area();
        let palette = ThemePalette::from_theme(self.theme);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        self.render_tabs(frame, chunks[0], palette);

        if self.show_tree {
            let tree_width = chunks[1].width.saturating_mul(self.tree_ratio) / 100;
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(tree_width), Constraint::Min(1)])
                .split(chunks[1]);
            self.render_tree(frame, panes[0], palette);
            self.render_editor(frame, panes[1], palette);
            self.render_divider(frame, chunks[1], tree_width, palette);
        } else {
            self.render_editor(frame, chunks[1], palette);
        }

        self.render_status(frame, chunks[2], palette);
        if self.mode == EditorMode::BufferPicker {
            self.render_buffer_picker(frame, area, palette);
        }
    }

    pub(super) fn render_tabs(&self, frame: &mut Frame, area: Rect, palette: ThemePalette) {
        let mut spans = Vec::new();
        for (idx, tab) in self.tabs.iter().enumerate() {
            let style = if idx == self.active_tab {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.dim)
            };
            spans.push(Span::styled(format!(" [{}:{}] ", idx + 1, tab.title), style));
        }
        Paragraph::new(Line::from(spans)).render(area, frame.buffer_mut());
    }

    pub(super) fn render_tree(&mut self, frame: &mut Frame, area: Rect, palette: ThemePalette) {
        let focused = self.main_focus == MainFocus::Tree;
        let block = Block::bordered().title(" Tree ").style(
            Style::default()
                .fg(palette.fg)
                .bg(palette.bg)
                .add_modifier(Modifier::BOLD),
        );
        let inner = block.inner(area);
        block
            .border_style(Style::default().fg(if focused {
                palette.accent
            } else {
                palette.dim
            }))
            .render(area, frame.buffer_mut());
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let visible = inner.height as usize;
        if self.tree_selected < self.tree_scroll {
            self.tree_scroll = self.tree_selected;
        }
        if self.tree_selected >= self.tree_scroll + visible {
            self.tree_scroll = self.tree_selected.saturating_sub(visible.saturating_sub(1));
        }

        let end = min(self.tree_entries.len(), self.tree_scroll + visible);
        let mut lines = Vec::new();
        for idx in self.tree_scroll..end {
            let item = &self.tree_entries[idx];
            let indent = "  ".repeat(item.depth);
            let icon = if item.is_dir { "[D]" } else { "[F]" };
            let mut style = Style::default().fg(if item.is_dir { palette.warn } else { palette.fg });
            if idx == self.tree_selected {
                style = style
                    .bg(if focused { palette.accent } else { palette.dim })
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD);
            }
            lines.push(Line::from(Span::styled(
                format!("{}{} {}", indent, icon, item.name),
                style,
            )));
        }

        Paragraph::new(lines).render(inner, frame.buffer_mut());
    }

    pub(super) fn render_divider(&self, frame: &mut Frame, main: Rect, tree_width: u16, palette: ThemePalette) {
        if tree_width == 0 || tree_width >= main.width {
            return;
        }
        let x = main.x + tree_width.saturating_sub(1);
        for y in main.y..(main.y + main.height) {
            frame
                .buffer_mut()[(x, y)]
                .set_char('|')
                .set_fg(palette.dim)
                .set_bg(palette.bg);
        }
    }

    pub(super) fn render_editor(&mut self, frame: &mut Frame, area: Rect, palette: ThemePalette) {
        let mut editor_area = area;
        if self.show_tagbar && area.width > 30 {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(26)])
                .split(area);
            editor_area = panes[0];
            self.render_tagbar(frame, panes[1], palette);
        }

        if self.mode == EditorMode::Terminal {
            Paragraph::new(vec![
                Line::from(Span::styled("Terminal mode", Style::default().fg(palette.ok))),
                Line::from(Span::styled(
                    "Use <C-\\><C-n> back to NORMAL",
                    Style::default().fg(palette.dim),
                )),
            ])
            .block(
                Block::bordered()
                    .title(" Terminal ")
                    .border_style(Style::default().fg(palette.ok)),
            )
            .render(editor_area, frame.buffer_mut());
            return;
        }

        let split = self.tabs[self.active_tab].split;
        if split == SplitDirection::None {
            self.tabs[self.active_tab].focus = PaneFocus::Primary;
        }
        let active_focus = self.tabs[self.active_tab].focus;

        match split {
            SplitDirection::None => self.render_editor_pane(
                frame,
                editor_area,
                PaneFocus::Primary,
                active_focus,
                palette,
            ),
            SplitDirection::Vertical => {
                let panes = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(editor_area);
                self.render_editor_pane(
                    frame,
                    panes[0],
                    PaneFocus::Primary,
                    active_focus,
                    palette,
                );
                self.render_editor_pane(
                    frame,
                    panes[1],
                    PaneFocus::Secondary,
                    active_focus,
                    palette,
                );
            }
            SplitDirection::Horizontal => {
                let panes = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(editor_area);
                self.render_editor_pane(
                    frame,
                    panes[0],
                    PaneFocus::Primary,
                    active_focus,
                    palette,
                );
                self.render_editor_pane(
                    frame,
                    panes[1],
                    PaneFocus::Secondary,
                    active_focus,
                    palette,
                );
            }
        }
    }

    pub(super) fn render_editor_pane(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        pane: PaneFocus,
        active_focus: PaneFocus,
        palette: ThemePalette,
    ) {
        let buffer_idx = self.tabs[self.active_tab].buffer_index;
        let focused = self.main_focus == MainFocus::Editor && pane == active_focus;

        let buffer = &mut self.buffers[buffer_idx];
        buffer.ensure_cursor_in_bounds();

        let mode_text = match self.mode {
            EditorMode::Normal => "NORMAL",
            EditorMode::Insert => "INSERT",
            EditorMode::Terminal => "TERMINAL",
            EditorMode::BufferPicker => "BUFFER",
        };
        let mut title = format!(" {} [{}] ", buffer.name, mode_text);
        if buffer.modified {
            title.push('*');
        }

        let block = Block::bordered()
            .title(title)
            .border_style(Style::default().fg(if focused { palette.accent } else { palette.dim }));
        let inner = block.inner(area);
        block.render(area, frame.buffer_mut());
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let visible = inner.height as usize;
        if buffer.cursor_row < buffer.scroll_row {
            buffer.scroll_row = buffer.cursor_row;
        }
        if buffer.cursor_row >= buffer.scroll_row + visible {
            buffer.scroll_row = buffer.cursor_row.saturating_sub(visible.saturating_sub(1));
        }

        let mut lines = Vec::new();
        let end = min(buffer.lines.len(), buffer.scroll_row + visible);
        let is_markdown = Self::is_markdown_buffer(buffer);
        let is_rust = Self::is_rust_buffer(buffer);
        let use_semantic_highlight = Self::can_use_lsp_semantic_highlight(buffer);
        let mut markdown_fence_language = if is_markdown {
            Self::markdown_fence_language_before(buffer, buffer.scroll_row)
        } else {
            None
        };

        for row in buffer.scroll_row..end {
            let mut spans = vec![Span::styled(
                format!("{:>4} ", row + 1),
                Style::default().fg(palette.dim),
            )];

            let line = &buffer.lines[row];

            if is_markdown {
                let (mut highlighted, next_state) =
                    Self::highlight_markdown_line(line, palette, markdown_fence_language.as_deref());
                markdown_fence_language = next_state;
                spans.append(&mut highlighted);
            } else if is_rust {
                if use_semantic_highlight {
                    let semantic_tokens = buffer
                        .lsp_tokens_by_line
                        .get(&row)
                        .cloned()
                        .unwrap_or_default();
                    let mut highlighted =
                        Self::highlight_line_with_lsp_tokens(line, &semantic_tokens, palette);
                    spans.append(&mut highlighted);
                } else {
                    // Rust 语义 token 未就绪时，回退为普通文本，
                    // 避免与“由 rust-analyzer 提供高亮”的要求相冲突。
                    spans.push(Span::styled(line.clone(), Style::default().fg(palette.fg)));
                }
            } else {
                spans.push(Span::styled(line.clone(), Style::default().fg(palette.fg)));
            }

            lines.push(Line::from(spans));
        }

        Paragraph::new(lines).render(inner, frame.buffer_mut());

        if focused {
            let cursor_visible_row = buffer.cursor_row.saturating_sub(buffer.scroll_row);
            if cursor_visible_row < visible {
                // 5 列偏移：4 位行号 + 1 个空格。
                let cursor_x = inner
                    .x
                    .saturating_add(5)
                    .saturating_add(buffer.cursor_col as u16);
                let cursor_y = inner.y.saturating_add(cursor_visible_row as u16);

                if cursor_x < inner.x.saturating_add(inner.width)
                    && cursor_y < inner.y.saturating_add(inner.height)
                {
                    let cursor_char = buffer.lines[buffer.cursor_row]
                        .chars()
                        .nth(buffer.cursor_col)
                        .unwrap_or(' ');
                    frame
                        .buffer_mut()[(cursor_x, cursor_y)]
                        .set_char(cursor_char)
                        .set_bg(palette.accent)
                        .set_fg(Color::Black);
                }
            }
        }
    }

    /// 判断当前缓冲区是否为 Markdown 文件。
    ///
    /// 同时兼容以下来源：
    /// - 已打开文件路径扩展名（`.md` / `.markdown` / `.mdx`）
    /// - 尚未落盘时的缓冲区名称后缀
    fn is_markdown_buffer(buffer: &EditorBuffer) -> bool {
        let by_path = buffer.path.as_ref().and_then(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdx"))
        });
        if by_path == Some(true) {
            return true;
        }

        let name = buffer.name.to_ascii_lowercase();
        name.ends_with(".md") || name.ends_with(".markdown") || name.ends_with(".mdx")
    }

    /// 判断当前缓冲区是否为 Rust 文件。
    ///
    /// 识别规则：
    /// - 已打开文件扩展名为 `.rs`；
    /// - 或未落盘缓冲区名称后缀为 `.rs`。
    fn is_rust_buffer(buffer: &EditorBuffer) -> bool {
        let by_path = buffer.path.as_ref().and_then(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("rs"))
        });
        if by_path == Some(true) {
            return true;
        }

        buffer.name.to_ascii_lowercase().ends_with(".rs")
    }

    /// 判断当前缓冲区是否适合使用 LSP 语义高亮。
    ///
    /// 这里限定“Rust + 已有 token”两个条件：
    /// - Rust 语义高亮由 rust-analyzer 提供，符合用户要求；
    /// - token 未返回时回退普通文本，避免闪烁和错色。
    fn can_use_lsp_semantic_highlight(buffer: &EditorBuffer) -> bool {
        let is_supported_language = lsp::detect_language_from_path_or_name(
            buffer.path.as_deref(),
            &buffer.name,
        )
        .is_some();
        is_supported_language && !buffer.lsp_tokens_by_line.is_empty()
    }

    /// 将单行文本按 LSP 语义 token 进行着色。
    ///
    /// 关键策略：
    /// - 严格按 token 起止切片，保留未覆盖片段为默认前景色；
    /// - token 越界时自动截断，避免服务端异常数据导致 panic；
    /// - token 重叠时以后到先用为主，保证视觉连续性。
    fn highlight_line_with_lsp_tokens(
        line: &str,
        tokens: &[LspSemanticToken],
        palette: ThemePalette,
    ) -> Vec<Span<'static>> {
        if tokens.is_empty() {
            return vec![Span::styled(line.to_string(), Style::default().fg(palette.fg))];
        }

        let mut spans = Vec::new();
        let line_char_count = line.chars().count();
        let mut current_char = 0usize;

        for token in tokens {
            let token_start = token.start.min(line_char_count);
            let token_end = token_start.saturating_add(token.length).min(line_char_count);
            if token_start > current_char {
                let plain_start = super::char_to_byte_index_in_line(line, current_char);
                let plain_end = super::char_to_byte_index_in_line(line, token_start);
                if plain_start <= plain_end && plain_end <= line.len() {
                    spans.push(Span::styled(
                        line[plain_start..plain_end].to_string(),
                        Style::default().fg(palette.fg),
                    ));
                }
            }

            if token_end > token_start {
                let token_start_byte = super::char_to_byte_index_in_line(line, token_start);
                let token_end_byte = super::char_to_byte_index_in_line(line, token_end);
                if token_start_byte <= token_end_byte && token_end_byte <= line.len() {
                    spans.push(Span::styled(
                        line[token_start_byte..token_end_byte].to_string(),
                        Self::semantic_token_style(token, palette),
                    ));
                }
                current_char = current_char.max(token_end);
            }
        }

        if current_char < line_char_count {
            let tail_start = super::char_to_byte_index_in_line(line, current_char);
            if tail_start <= line.len() {
                spans.push(Span::styled(
                    line[tail_start..].to_string(),
                    Style::default().fg(palette.fg),
                ));
            }
        }

        if spans.is_empty() {
            spans.push(Span::styled(line.to_string(), Style::default().fg(palette.fg)));
        }
        spans
    }

    /// 将语义 token 类型映射到主题色。
    ///
    /// 映射优先考虑“结构可读性”：关键字/函数/类型/变量保持稳定对比。
    fn semantic_token_style(token: &LspSemanticToken, palette: ThemePalette) -> Style {
        let mut style = match token.token_type.as_str() {
            "keyword" => Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
            "string" => Style::default().fg(palette.ok),
            "number" => Style::default().fg(palette.warn),
            "function" | "method" => Style::default().fg(Color::Rgb(130, 170, 255)),
            "type" | "struct" | "class" | "enum" | "interface" => {
                Style::default().fg(Color::Rgb(255, 203, 107)).add_modifier(Modifier::BOLD)
            }
            "parameter" => Style::default().fg(Color::Rgb(199, 146, 234)),
            "property" | "field" => Style::default().fg(Color::Rgb(128, 203, 196)),
            "macro" => Style::default().fg(Color::Rgb(255, 158, 128)).add_modifier(Modifier::BOLD),
            "comment" => Style::default().fg(palette.dim).add_modifier(Modifier::ITALIC),
            "operator" => Style::default().fg(palette.accent),
            _ => Style::default().fg(palette.fg),
        };

        if token.token_modifiers.iter().any(|item| item == "deprecated") {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if token.token_modifiers.iter().any(|item| item == "readonly") {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    /// 计算指定起始行之前的 fenced code 状态。
    ///
    /// 返回值含义：
    /// - `Some(language)`：当前在 fenced code 块内，且语言标签为 `language`（可能为空字符串）；
    /// - `None`：当前不在 fenced code 块内。
    fn markdown_fence_language_before(buffer: &EditorBuffer, row_start: usize) -> Option<String> {
        let mut language: Option<String> = None;
        let end = min(row_start, buffer.lines.len());
        for line in buffer.lines.iter().take(end) {
            if let Some(fence_language) = Self::parse_markdown_fence_language(line) {
                if language.is_some() {
                    language = None;
                } else {
                    language = Some(fence_language);
                }
            }
        }
        language
    }

    /// 解析 fenced code 行语言标签。
    ///
    /// - `None`：不是 fenced code 行。
    /// - `Some(lang)`：是 fenced code 行，`lang` 可能为空。
    fn parse_markdown_fence_language(line: &str) -> Option<String> {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("```") {
            return None;
        }
        let language = trimmed[3..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        Some(language)
    }

    /// 判断行是否为 Markdown 水平线。
    fn is_markdown_horizontal_rule(trimmed: &str) -> bool {
        let compact: String = trimmed.chars().filter(|ch| !ch.is_whitespace()).collect();
        if compact.len() < 3 {
            return false;
        }
        compact.chars().all(|ch| ch == '-')
            || compact.chars().all(|ch| ch == '*')
            || compact.chars().all(|ch| ch == '_')
    }

    /// 判断行是否是 Markdown 表格分隔行。
    ///
    /// 示例：`| --- | :---: | ---: |`
    fn is_markdown_table_separator(trimmed: &str) -> bool {
        if !trimmed.contains('|') {
            return false;
        }

        trimmed
            .split('|')
            .filter(|cell| !cell.trim().is_empty())
            .all(|cell| {
                let token = cell.trim();
                if token.is_empty() {
                    return false;
                }
                token
                    .chars()
                    .all(|ch| ch == '-' || ch == ':' || ch.is_whitespace())
                    && token.chars().any(|ch| ch == '-')
            })
    }

    /// 判断行是否是 Markdown 表格普通行。
    fn is_markdown_table_row(trimmed: &str) -> bool {
        trimmed.contains('|')
    }

    /// 判断行是否是任务列表项，并返回 marker 与是否完成。
    ///
    /// 支持：`- [ ]` / `- [x]` / `* [ ]` / `+ [x]`。
    fn parse_markdown_task_list_marker(trimmed: &str) -> Option<(char, bool)> {
        let chars: Vec<char> = trimmed.chars().collect();
        if chars.len() < 6 {
            return None;
        }

        let bullet = chars[0];
        if !matches!(bullet, '-' | '*' | '+') {
            return None;
        }
        if chars.get(1) != Some(&' ')
            || chars.get(2) != Some(&'[')
            || chars.get(4) != Some(&']')
            || chars.get(5) != Some(&' ')
        {
            return None;
        }

        let checked = matches!(chars[3], 'x' | 'X');
        if !checked && chars[3] != ' ' {
            return None;
        }

        Some((bullet, checked))
    }

    /// 对单行 Markdown 做高亮，并返回更新后的 fenced code 语言状态。
    fn highlight_markdown_line(
        line: &str,
        palette: ThemePalette,
        fence_language: Option<&str>,
    ) -> (Vec<Span<'static>>, Option<String>) {
        let trimmed = line.trim_start();

        // 代码围栏内：根据语言标签区分颜色。
        if let Some(language) = fence_language {
            if let Some(parsed) = Self::parse_markdown_fence_language(line) {
                let mut spans = vec![Span::styled(
                    "```".to_string(),
                    Style::default()
                        .fg(palette.warn)
                        .add_modifier(Modifier::BOLD),
                )];
                if !parsed.is_empty() {
                    spans.push(Span::styled(
                        parsed,
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                    ));
                }
                return (spans, None);
            }

            return (
                Self::highlight_fenced_code_line_with_syntect(line, language, palette),
                Some(language.to_string()),
            );
        }

        // 代码围栏起始行 + 语言标签。
        if let Some(language) = Self::parse_markdown_fence_language(line) {
            let mut spans = vec![Span::styled(
                "```".to_string(),
                Style::default()
                    .fg(palette.warn)
                    .add_modifier(Modifier::BOLD),
            )];
            if !language.is_empty() {
                spans.push(Span::styled(
                    language.clone(),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ));
            }
            return (spans, Some(language));
        }

        // 水平线。
        if Self::is_markdown_horizontal_rule(trimmed) {
            return (
                vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(palette.dim).add_modifier(Modifier::BOLD),
                )],
                None,
            );
        }

        // 标题。
        let heading_level = trimmed.chars().take_while(|ch| *ch == '#').count();
        if heading_level > 0 && trimmed.chars().nth(heading_level) == Some(' ') {
            let heading_color = match heading_level {
                1 => palette.accent,
                2 => palette.warn,
                _ => palette.ok,
            };
            return (
                vec![Span::styled(
                    line.to_string(),
                    Style::default()
                        .fg(heading_color)
                        .add_modifier(Modifier::BOLD),
                )],
                None,
            );
        }

        // 引用。
        if trimmed.starts_with('>') {
            return (
                vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(palette.dim).add_modifier(Modifier::ITALIC),
                )],
                None,
            );
        }

        // 表格分隔行。
        if Self::is_markdown_table_separator(trimmed) {
            return (
                vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(palette.warn).add_modifier(Modifier::BOLD),
                )],
                None,
            );
        }

        // 表格行。
        if Self::is_markdown_table_row(trimmed) {
            let mut spans = Vec::new();
            for ch in line.chars() {
                if ch == '|' {
                    spans.push(Span::styled(
                        "|".to_string(),
                        Style::default().fg(palette.warn),
                    ));
                } else {
                    spans.push(Span::styled(ch.to_string(), Style::default().fg(palette.fg)));
                }
            }
            return (spans, None);
        }

        // 任务列表。
        if let Some((bullet, checked)) = Self::parse_markdown_task_list_marker(trimmed) {
            let content = &trimmed[6..];
            let marker_style = if checked {
                Style::default()
                    .fg(palette.ok)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(palette.warn)
                    .add_modifier(Modifier::BOLD)
            };
            let content_style = if checked {
                Style::default().fg(palette.dim)
            } else {
                Style::default().fg(palette.fg)
            };
            return (
                vec![
                    Span::styled(format!("{} [{}] ", bullet, if checked { "x" } else { " " }), marker_style),
                    Span::styled(content.to_string(), content_style),
                ],
                None,
            );
        }

        // 普通无序列表。
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
            let marker = trimmed.chars().next().unwrap_or('-');
            let content = trimmed[2..].to_string();
            return (
                vec![
                    Span::styled(
                        format!("{} ", marker),
                        Style::default()
                            .fg(palette.warn)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(content, Style::default().fg(palette.fg)),
                ],
                None,
            );
        }

        // 有序列表。
        let mut digit_count = 0usize;
        for ch in trimmed.chars() {
            if ch.is_ascii_digit() {
                digit_count += 1;
            } else {
                break;
            }
        }
        if digit_count > 0 {
            let chars: Vec<char> = trimmed.chars().collect();
            if chars.get(digit_count) == Some(&'.') && chars.get(digit_count + 1) == Some(&' ') {
                let marker: String = chars.iter().take(digit_count + 2).collect();
                let content: String = chars.iter().skip(digit_count + 2).collect();
                return (
                    vec![
                        Span::styled(
                            marker,
                            Style::default()
                                .fg(palette.warn)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(content, Style::default().fg(palette.fg)),
                    ],
                    None,
                );
            }
        }

        (Self::highlight_markdown_line_with_parser(line, palette), None)
    }

    pub(super) fn render_tagbar(&self, frame: &mut Frame, area: Rect, palette: ThemePalette) {
        let buffer = self.active_buffer();
        let mut tags = Vec::new();
        for (idx, line) in buffer.lines.iter().enumerate() {
            let t = line.trim_start();
            if t.starts_with("fn ")
                || t.starts_with("pub fn ")
                || t.starts_with("struct ")
                || t.starts_with("enum ")
                || t.starts_with("impl ")
            {
                tags.push(format!("L{} {}", idx + 1, t));
            }
        }
        if tags.is_empty() {
            tags.push("No tags".to_string());
        }
        let lines: Vec<Line> = tags
            .into_iter()
            .take(area.height.saturating_sub(2) as usize)
            .map(|tag| Line::from(Span::styled(tag, Style::default().fg(palette.fg))))
            .collect();

        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(" TagBar ")
                    .border_style(Style::default().fg(palette.dim)),
            )
            .render(area, frame.buffer_mut());
    }

    pub(super) fn render_status(&self, frame: &mut Frame, area: Rect, palette: ThemePalette) {
        let mode = match self.mode {
            EditorMode::Normal => "NORMAL",
            EditorMode::Insert => "INSERT",
            EditorMode::Terminal => "TERMINAL",
            EditorMode::BufferPicker => "BUFFER",
        };
        let pending = if self.normal_pending.is_empty() {
            String::new()
        } else {
            format!(" | cmd:{}", self.normal_pending)
        };
        let text = format!(
            "{} | theme:{}{} | lsp:{} | ra:{} | op:{} | {}",
            mode,
            self.theme.as_str(),
            pending,
            if self.lsp_client.is_running() {
                "running"
            } else {
                "stopped"
            },
            self.rust_analyzer_status,
            self.lsp_last_action,
            self.status_message
        );
        Paragraph::new(text)
            .style(Style::default().bg(palette.bg).fg(palette.ok))
            .render(area, frame.buffer_mut());
    }

    pub(super) fn render_buffer_picker(&self, frame: &mut Frame, area: Rect, palette: ThemePalette) {
        let width = min(60, area.width.saturating_sub(4));
        let height = min(16, area.height.saturating_sub(4));
        let popup = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        Clear.render(popup, frame.buffer_mut());

        let mut lines = vec![Line::from(Span::styled(
            "Select buffer by letter",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))];
        for (idx, b) in self.buffers.iter().enumerate().take(26) {
            let letter = (b'a' + idx as u8) as char;
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", letter), Style::default().fg(palette.warn)),
                Span::styled(b.name.clone(), Style::default().fg(palette.fg)),
            ]));
        }

        Paragraph::new(lines)
            .block(Block::bordered().title(" Buffers "))
            .render(popup, frame.buffer_mut());
    }

    /// 使用正式 Markdown tokenizer（pulldown-cmark）对源码进行规范级高亮。
    ///
    /// 关键点：
    /// - 使用 `into_offset_iter` 获取事件对应的源码字节区间；
    /// - 在“源码文本”上着色，保证语法标记字符（如 `**`、`[]()`）不丢失；
    /// - 支持复杂转义、嵌套边界、脚注、表格、HTML block 等标准解析能力。
    fn highlight_markdown_line_with_parser(line: &str, palette: ThemePalette) -> Vec<Span<'static>> {
        if line.is_empty() {
            return vec![Span::styled(String::new(), Style::default().fg(palette.fg))];
        }

        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_FOOTNOTES);
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_SMART_PUNCTUATION);
        options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
        options.insert(Options::ENABLE_MATH);

        let default_style = Style::default().fg(palette.fg);
        let mut byte_styles = vec![default_style; line.len()];
        let mut style_stack = vec![default_style];

        for (event, range) in Parser::new_ext(line, options).into_offset_iter() {
            match event {
                Event::Start(tag) => {
                    let parent = *style_stack.last().unwrap_or(&default_style);
                    let marker_style = Self::markdown_marker_style_for_tag(&tag, palette, parent);
                    Self::apply_style_to_range(&mut byte_styles, range.clone(), marker_style);

                    let content_style = Self::markdown_content_style_for_tag(&tag, palette, parent);
                    style_stack.push(content_style);
                }
                Event::End(tag_end) => {
                    let parent = *style_stack.last().unwrap_or(&default_style);
                    let marker_style = Self::markdown_marker_style_for_tag_end(tag_end, palette, parent);
                    Self::apply_style_to_range(&mut byte_styles, range, marker_style);
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                }
                Event::Text(_) => {
                    let current = *style_stack.last().unwrap_or(&default_style);
                    Self::apply_style_to_range(&mut byte_styles, range, current);
                }
                Event::Code(_) => {
                    let style = Style::default().fg(palette.warn).add_modifier(Modifier::BOLD);
                    Self::apply_style_to_range(&mut byte_styles, range, style);
                }
                Event::Html(_) | Event::InlineHtml(_) => {
                    let style = Style::default().fg(palette.dim).add_modifier(Modifier::ITALIC);
                    Self::apply_style_to_range(&mut byte_styles, range, style);
                }
                Event::FootnoteReference(_) => {
                    let style = Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::UNDERLINED);
                    Self::apply_style_to_range(&mut byte_styles, range, style);
                }
                Event::Rule => {
                    let style = Style::default().fg(palette.dim).add_modifier(Modifier::BOLD);
                    Self::apply_style_to_range(&mut byte_styles, range, style);
                }
                Event::TaskListMarker(done) => {
                    let style = Style::default()
                        .fg(if done { palette.ok } else { palette.warn })
                        .add_modifier(Modifier::BOLD);
                    Self::apply_style_to_range(&mut byte_styles, range, style);
                }
                Event::InlineMath(_) | Event::DisplayMath(_) => {
                    let style = Style::default().fg(palette.accent);
                    Self::apply_style_to_range(&mut byte_styles, range, style);
                }
                Event::SoftBreak | Event::HardBreak => {
                    let current = *style_stack.last().unwrap_or(&default_style);
                    Self::apply_style_to_range(&mut byte_styles, range, current);
                }
            }
        }

        Self::byte_styles_to_spans(line, &byte_styles)
    }

    /// 将样式应用到指定源码字节区间。
    fn apply_style_to_range(byte_styles: &mut [Style], range: std::ops::Range<usize>, style: Style) {
        if range.start >= range.end || range.start >= byte_styles.len() {
            return;
        }
        let end = range.end.min(byte_styles.len());
        for item in byte_styles.iter_mut().take(end).skip(range.start) {
            *item = style;
        }
    }

    /// 将按字节样式数组转换为可渲染的 span 序列。
    fn byte_styles_to_spans(line: &str, byte_styles: &[Style]) -> Vec<Span<'static>> {
        if line.is_empty() {
            return vec![Span::raw(String::new())];
        }
        if byte_styles.is_empty() {
            return vec![Span::styled(line.to_string(), Style::default())];
        }

        let mut spans = Vec::new();
        let mut start = 0usize;
        let mut current_style = byte_styles[0];

        for (index, style) in byte_styles.iter().enumerate().skip(1).take(line.len().saturating_sub(1)) {
            if *style != current_style {
                if let Some(segment) = line.get(start..index)
                    && !segment.is_empty()
                {
                    spans.push(Span::styled(segment.to_string(), current_style));
                }
                start = index;
                current_style = *style;
            }
        }

        if let Some(segment) = line.get(start..line.len())
            && !segment.is_empty()
        {
            spans.push(Span::styled(segment.to_string(), current_style));
        }

        if spans.is_empty() {
            spans.push(Span::styled(line.to_string(), Style::default()));
        }
        spans
    }

    /// `Tag::Start` 对应的“标记字符”样式。
    fn markdown_marker_style_for_tag(tag: &Tag<'_>, palette: ThemePalette, base: Style) -> Style {
        match tag {
            Tag::Heading { .. } => base.fg(palette.accent).add_modifier(Modifier::BOLD),
            Tag::CodeBlock(_) => base.fg(palette.warn).add_modifier(Modifier::BOLD),
            Tag::Emphasis => base.fg(palette.ok).add_modifier(Modifier::ITALIC),
            Tag::Strong => base.fg(palette.ok).add_modifier(Modifier::BOLD),
            Tag::Strikethrough => base.fg(palette.dim).add_modifier(Modifier::CROSSED_OUT),
            Tag::Link { .. } | Tag::Image { .. } => {
                base.fg(palette.accent).add_modifier(Modifier::UNDERLINED)
            }
            Tag::BlockQuote(_) => base.fg(palette.dim).add_modifier(Modifier::ITALIC),
            Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell => base.fg(palette.warn),
            Tag::FootnoteDefinition(_) => base.fg(palette.accent),
            _ => base,
        }
    }

    /// `Tag::Start` 对应的“内容”样式。
    fn markdown_content_style_for_tag(tag: &Tag<'_>, palette: ThemePalette, base: Style) -> Style {
        match tag {
            Tag::Heading { level, .. } => {
                let color = match *level as u8 {
                    1 => palette.accent,
                    2 => palette.warn,
                    _ => palette.ok,
                };
                base.fg(color).add_modifier(Modifier::BOLD)
            }
            Tag::CodeBlock(kind) => {
                let color = match kind {
                    CodeBlockKind::Fenced(info) => match info.to_ascii_lowercase().as_str() {
                        "rust" | "rs" => Color::Rgb(250, 130, 90),
                        "python" | "py" => Color::Rgb(120, 200, 255),
                        "javascript" | "js" | "typescript" | "ts" => Color::Rgb(255, 210, 120),
                        "json" | "yaml" | "yml" | "toml" => Color::Rgb(180, 220, 160),
                        "bash" | "sh" | "shell" => Color::Rgb(190, 210, 190),
                        _ => palette.ok,
                    },
                    CodeBlockKind::Indented => palette.ok,
                };
                base.fg(color)
            }
            Tag::Emphasis => base.fg(palette.ok).add_modifier(Modifier::ITALIC),
            Tag::Strong => base.fg(palette.ok).add_modifier(Modifier::BOLD),
            Tag::Strikethrough => base.fg(palette.dim).add_modifier(Modifier::CROSSED_OUT),
            Tag::Link { .. } => base.fg(palette.accent).add_modifier(Modifier::UNDERLINED),
            Tag::Image { .. } => base.fg(palette.warn).add_modifier(Modifier::BOLD),
            Tag::BlockQuote(_) => base.fg(palette.dim).add_modifier(Modifier::ITALIC),
            Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell => base.fg(palette.fg),
            Tag::FootnoteDefinition(_) => base.fg(palette.accent),
            _ => base,
        }
    }

    /// `Tag::End` 对应的“标记字符”样式。
    fn markdown_marker_style_for_tag_end(
        tag_end: TagEnd,
        palette: ThemePalette,
        base: Style,
    ) -> Style {
        match tag_end {
            TagEnd::Heading(_) => base.fg(palette.accent).add_modifier(Modifier::BOLD),
            TagEnd::CodeBlock => base.fg(palette.warn).add_modifier(Modifier::BOLD),
            TagEnd::Emphasis => base.fg(palette.ok).add_modifier(Modifier::ITALIC),
            TagEnd::Strong => base.fg(palette.ok).add_modifier(Modifier::BOLD),
            TagEnd::Strikethrough => base.fg(palette.dim).add_modifier(Modifier::CROSSED_OUT),
            TagEnd::Link | TagEnd::Image => base.fg(palette.accent).add_modifier(Modifier::UNDERLINED),
            TagEnd::BlockQuote(_) => base.fg(palette.dim).add_modifier(Modifier::ITALIC),
            TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell => {
                base.fg(palette.warn)
            }
            _ => base,
        }
    }

    /// 使用 `syntect` 对 fenced code 单行进行真正的语言级语法高亮。
    ///
    /// 若无法匹配语言或高亮失败，则自动降级为普通代码色，保证编辑体验稳定。
    fn highlight_fenced_code_line_with_syntect(
        line: &str,
        language: &str,
        palette: ThemePalette,
    ) -> Vec<Span<'static>> {
        let syntax_set = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines);
        let theme = SYNTAX_THEME.get_or_init(|| {
            let themes = ThemeSet::load_defaults();
            themes
                .themes
                .get("base16-ocean.dark")
                .cloned()
                .or_else(|| themes.themes.values().next().cloned())
                .unwrap_or_default()
        });

        let syntax = syntax_set
            .find_syntax_by_token(language)
            .or_else(|| syntax_set.find_syntax_by_extension(language))
            .unwrap_or_else(|| syntax_set.find_syntax_plain_text());

        let mut highlighter = HighlightLines::new(syntax, theme);
        match highlighter.highlight_line(line, syntax_set) {
            Ok(parts) => {
                let mut spans = Vec::new();
                for (style, segment) in parts {
                    if segment.is_empty() {
                        continue;
                    }
                    spans.push(Span::styled(
                        segment.to_string(),
                        Self::syntect_style_to_ratatui(style),
                    ));
                }
                if spans.is_empty() {
                    spans.push(Span::styled(line.to_string(), Style::default().fg(palette.ok)));
                }
                spans
            }
            Err(_) => vec![Span::styled(line.to_string(), Style::default().fg(palette.ok))],
        }
    }

    /// 将 `syntect` 样式转换为 `ratatui` 样式。
    fn syntect_style_to_ratatui(style: SyntectStyle) -> Style {
        let foreground = style.foreground;
        let mut result = Style::default().fg(Color::Rgb(foreground.r, foreground.g, foreground.b));

        if style.font_style.contains(syntect::highlighting::FontStyle::BOLD) {
            result = result.add_modifier(Modifier::BOLD);
        }
        if style.font_style.contains(syntect::highlighting::FontStyle::ITALIC) {
            result = result.add_modifier(Modifier::ITALIC);
        }
        if style
            .font_style
            .contains(syntect::highlighting::FontStyle::UNDERLINE)
        {
            result = result.add_modifier(Modifier::UNDERLINED);
        }

        result
    }
}
