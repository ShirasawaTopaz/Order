use std::cmp::min;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Widget},
};

use super::{
    Editor,
    types::{EditorMode, MainFocus, PaneFocus, SplitDirection, ThemePalette},
};

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
        for row in buffer.scroll_row..end {
            let mut spans = vec![Span::styled(
                format!("{:>4} ", row + 1),
                Style::default().fg(palette.dim),
            )];
            let line = &buffer.lines[row];
            if focused && row == buffer.cursor_row {
                let chars: Vec<char> = line.chars().collect();
                let left: String = chars.iter().take(buffer.cursor_col).collect();
                let cursor = chars.get(buffer.cursor_col).copied().unwrap_or(' ');
                let right: String = chars.iter().skip(buffer.cursor_col + 1).collect();
                spans.push(Span::styled(left, Style::default().fg(palette.fg)));
                spans.push(Span::styled(
                    cursor.to_string(),
                    Style::default().bg(palette.accent).fg(Color::Black),
                ));
                spans.push(Span::styled(right, Style::default().fg(palette.fg)));
            } else {
                spans.push(Span::styled(line.clone(), Style::default().fg(palette.fg)));
            }
            lines.push(Line::from(spans));
        }
        Paragraph::new(lines).render(inner, frame.buffer_mut());
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
            "{} | theme:{}{} | {}",
            mode,
            self.theme.as_str(),
            pending,
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
}
