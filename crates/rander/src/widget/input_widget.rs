use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph, Widget},
};

/// 可用的命令列表，包含命令名称和描述。
pub const AVAILABLE_COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show help information"),
    ("/exit", "Exit the application"),
    ("/cancel", "Cancel latest operation"),
    ("/approve", "Approve pending writes by trace_id"),
    ("/reject", "Reject pending writes by trace_id"),
    ("/rollback", "Rollback snapshot by trace_id (or latest)"),
    (
        "/history",
        "Open history browser; support /history N, /history clear",
    ),
    ("/skills", "Manage project skills"),
    ("/rules", "Edit project rules"),
    ("/settings", "Configure settings"),
    ("/status", "Check system status"),
    ("/editor", "Open Order-editor"),
];

/// 补全弹窗一次最多显示的命令数量。
pub const COMPLETION_VISIBLE_COUNT: usize = 8;

/// 表示输入组件的状态。
///
/// 此结构体保存当前的输入文本、光标位置（以字符为单位）、光标的可见状态（用于闪烁效果）
/// 以及命令补全相关的状态。
#[derive(Debug, Clone)]
pub struct InputState {
    /// 当前的输入文本。
    pub input: String,
    /// 当前光标位置（字符索引，非字节索引）。
    pub cursor_position: usize,
    /// 光标当前是否可见（用于闪烁）。
    pub cursor_visible: bool,
    /// 是否显示命令补全弹窗。
    pub show_completion: bool,
    /// 当前选中的补全命令索引。
    pub completion_selected: usize,
    /// 过滤后的补全命令列表。
    pub filtered_commands: Vec<(String, String)>,
    /// 补全列表的滚动偏移量（可见区域的起始索引）。
    pub completion_scroll_offset: usize,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            input: String::new(),
            cursor_position: 0,
            cursor_visible: true,
            show_completion: false,
            completion_selected: 0,
            filtered_commands: Vec::new(),
            completion_scroll_offset: 0,
        }
    }
}

impl InputState {
    /// 将光标向左移动一个字符。
    ///
    /// 光标位置被限制为不低于 0。
    pub fn move_cursor_left(&mut self) {
        let cursor_moved_left = self.cursor_position.saturating_sub(1);
        self.cursor_position = self.clamp_cursor(cursor_moved_left);
    }

    /// 将光标向右移动一个字符。
    ///
    /// 光标位置被限制为不超过输入字符串的长度。
    pub fn move_cursor_right(&mut self) {
        let cursor_moved_right = self.cursor_position.saturating_add(1);
        self.cursor_position = self.clamp_cursor(cursor_moved_right);
    }

    /// 更新命令补全列表。
    ///
    /// 根据当前输入过滤可用的命令，并在输入以 '/' 开头时显示补全弹窗。
    fn update_completion(&mut self) {
        if self.input.starts_with('/') && self.input.len() > 1 {
            let filter = &self.input[1..].to_lowercase();
            self.filtered_commands = AVAILABLE_COMMANDS
                .iter()
                .filter(|(cmd, _)| cmd.to_lowercase().contains(filter))
                .map(|(cmd, desc)| (cmd.to_string(), desc.to_string()))
                .collect();
            self.show_completion = !self.filtered_commands.is_empty();
            self.completion_selected = 0;
            self.completion_scroll_offset = 0;
        } else if self.input == "/" {
            // 当只输入 '/' 时显示所有命令
            self.filtered_commands = AVAILABLE_COMMANDS
                .iter()
                .map(|(cmd, desc)| (cmd.to_string(), desc.to_string()))
                .collect();
            self.show_completion = true;
            self.completion_selected = 0;
            self.completion_scroll_offset = 0;
        } else {
            self.show_completion = false;
            self.filtered_commands.clear();
            self.completion_selected = 0;
            self.completion_scroll_offset = 0;
        }
    }

    /// 在当前光标位置插入一个字符。
    ///
    /// 插入后，光标向右移动一个位置。
    /// 如果输入以 '/' 开头，会触发命令补全弹窗。
    pub fn insert_char(&mut self, new_char: char) {
        let index = self.byte_index();
        self.input.insert(index, new_char);
        self.move_cursor_right();

        // 更新命令补全状态
        self.update_completion();
    }

    /// 计算当前光标位置的字节索引。
    ///
    /// Rust 字符串是 UTF-8 编码的，所以字符索引 != 字节索引。
    /// 这个辅助函数将基于字符的光标位置转换为适用于字符串切片和突变的字节索引。
    pub fn byte_index(&self) -> usize {
        self.input
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.cursor_position)
            .unwrap_or(self.input.len())
    }

    /// 删除光标前的一个字符。
    ///
    /// 如果光标位于字符串开头，则不执行任何操作。
    /// 删除后，光标向左移动一个位置。
    pub fn delete_char(&mut self) {
        if self.cursor_position != 0 {
            let current_index = self.cursor_position;
            let from_left_to_current_index = current_index - 1;

            // Getting all characters before the char to be deleted
            let before_char_to_delete = self.input.chars().take(from_left_to_current_index);
            // Getting all characters after the char to be deleted
            let after_char_to_delete = self.input.chars().skip(current_index);

            // Concatenating both strings
            self.input = before_char_to_delete.chain(after_char_to_delete).collect();
            self.move_cursor_left();

            // 更新命令补全状态
            self.update_completion();
        }
    }

    /// 限制光标位置以确保其保持在有效范围内 [0, 字符数]。
    pub fn clamp_cursor(&self, new_cursor_pos: usize) -> usize {
        new_cursor_pos.clamp(0, self.input.chars().count())
    }

    /// 切换光标的可见性。
    ///
    /// 通常在定时器循环中使用，以创建光标闪烁效果。
    pub fn toggle_cursor_visibility(&mut self) {
        self.cursor_visible = !self.cursor_visible;
    }

    /// 显式设置光标可见性。
    ///
    /// 用于确保用户输入时光标立即可见。
    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    /// 清除输入文本并将光标位置重置为 0。
    pub fn clear(&mut self) {
        self.input.clear();
        self.cursor_position = 0;
        self.show_completion = false;
        self.filtered_commands.clear();
        self.completion_selected = 0;
        self.completion_scroll_offset = 0;
    }

    /// 在补全列表中向上移动选择。
    ///
    /// 如果已经到达列表顶部，则循环到底部。
    /// 同时更新滚动偏移量以确保选中项在可见区域内。
    pub fn completion_up(&mut self) {
        if self.show_completion && !self.filtered_commands.is_empty() {
            let max_index = self.filtered_commands.len().saturating_sub(1);

            if self.completion_selected == 0 {
                // 循环到底部
                self.completion_selected = max_index;
                // 调整滚动偏移量到底部
                self.completion_scroll_offset =
                    max_index.saturating_sub(COMPLETION_VISIBLE_COUNT - 1);
            } else {
                self.completion_selected -= 1;
                // 如果选中项在可见区域上方，向上滚动
                if self.completion_selected < self.completion_scroll_offset {
                    self.completion_scroll_offset = self.completion_selected;
                }
            }
        }
    }

    /// 在补全列表中向下移动选择。
    ///
    /// 如果已经到达列表底部，则循环到顶部。
    /// 同时更新滚动偏移量以确保选中项在可见区域内。
    pub fn completion_down(&mut self) {
        if self.show_completion && !self.filtered_commands.is_empty() {
            let max_index = self.filtered_commands.len().saturating_sub(1);

            if self.completion_selected >= max_index {
                // 循环到顶部
                self.completion_selected = 0;
                self.completion_scroll_offset = 0;
            } else {
                self.completion_selected += 1;
                // 如果选中项在可见区域下方，向下滚动
                let visible_end = self.completion_scroll_offset + COMPLETION_VISIBLE_COUNT - 1;
                if self.completion_selected > visible_end {
                    self.completion_scroll_offset = self
                        .completion_selected
                        .saturating_sub(COMPLETION_VISIBLE_COUNT - 1);
                }
            }
        }
    }

    /// 确认选择当前高亮的补全命令。
    ///
    /// 将输入文本替换为选中的命令，并关闭补全弹窗。
    /// 返回是否成功选择了命令。
    pub fn confirm_completion(&mut self) -> bool {
        if self.show_completion
            && !self.filtered_commands.is_empty()
            && let Some((cmd, _)) = self.filtered_commands.get(self.completion_selected)
        {
            self.input = cmd.clone();
            self.cursor_position = self.input.chars().count();
            self.show_completion = false;
            return true;
        }
        false
    }

    /// 取消命令补全。
    ///
    /// 关闭补全弹窗但不修改输入文本。
    pub fn cancel_completion(&mut self) {
        self.show_completion = false;
    }

    /// 计算给定宽度下所需的组件高度。
    ///
    /// 考虑到边框、提示符 ">>> " 以及文本自动换行。
    pub fn required_height(&self, width: u16) -> u16 {
        let available_width = width.saturating_sub(2); // 减去边框
        if available_width == 0 {
            return 3;
        }

        let prompt_width = 4; // ">>> "
        let input_width: usize = self
            .input
            .chars()
            .map(|c| if c.is_ascii() { 1 } else { 2 })
            .sum();
        let cursor_extra_width = if self.cursor_position >= self.input.chars().count() {
            1
        } else {
            0
        };
        let total_width = prompt_width + input_width + cursor_extra_width;

        let lines = total_width.div_ceil(available_width as usize);
        let lines = lines.max(1) as u16;

        lines + 3
    }

    /// 计算补全弹窗的高度。
    ///
    /// 根据过滤后的命令数量计算所需高度，最多显示 COMPLETION_VISIBLE_COUNT 个命令。
    pub fn completion_popup_height(&self) -> u16 {
        if !self.show_completion || self.filtered_commands.is_empty() {
            return 0;
        }
        // 边框(2) + 命令数量，最多显示 COMPLETION_VISIBLE_COUNT 个命令
        (self.filtered_commands.len().min(COMPLETION_VISIBLE_COUNT) as u16) + 2
    }

    /// 获取当前可见的补全命令范围。
    ///
    /// 返回 (起始索引, 结束索引)，用于渲染时只显示可见区域的命令。
    pub fn visible_completion_range(&self) -> (usize, usize) {
        if !self.show_completion || self.filtered_commands.is_empty() {
            return (0, 0);
        }
        let start = self.completion_scroll_offset;
        let end = (start + COMPLETION_VISIBLE_COUNT).min(self.filtered_commands.len());
        (start, end)
    }

    /// 获取滚动进度百分比（用于滚动条显示）。
    ///
    /// 返回值范围：0.0 ~ 1.0
    pub fn scroll_progress(&self) -> f32 {
        if !self.show_completion || self.filtered_commands.len() <= COMPLETION_VISIBLE_COUNT {
            return 0.0;
        }
        let max_scroll = self
            .filtered_commands
            .len()
            .saturating_sub(COMPLETION_VISIBLE_COUNT);
        if max_scroll == 0 {
            return 0.0;
        }
        self.completion_scroll_offset as f32 / max_scroll as f32
    }
}

/// 一个 Ratatui 组件，用于渲染带有自定义块状光标的文本输入框。
///
/// 该组件可视化 `InputState` 中包含的状态。它支持：
/// - 显示提示符 ">>> "
/// - 渲染输入文本
/// - 渲染可闪烁的块状光标（基于 `InputState::cursor_visible`）
#[derive(Clone)]
pub struct InputWidget<'a> {
    state: &'a InputState,
    context_remaining: Option<u32>,
    status_message: Option<String>,
}

impl<'a> InputWidget<'a> {
    /// 使用给定状态创建一个新的 `InputWidget`。
    pub fn new(state: &'a InputState) -> Self {
        Self {
            state,
            context_remaining: None,
            status_message: None,
        }
    }

    /// 设置剩余上下文百分比显示。
    pub fn set_context_remaining(&mut self, percentage: u32) -> &mut Self {
        self.context_remaining = Some(percentage);
        self
    }

    /// 设置状态栏消息（显示在输入框标题左侧）。
    pub fn set_status_message(&mut self, message: impl Into<String>) -> &mut Self {
        self.status_message = Some(message.into());
        self
    }

    /// 计算补全弹窗的布局区域。
    ///
    /// 弹窗显示在输入框上方，宽度根据内容自适应。
    fn calculate_completion_area(&self, input_area: Rect) -> Rect {
        let popup_height = self.state.completion_popup_height();
        if popup_height == 0 {
            return Rect::default();
        }

        // 计算弹窗宽度（基于最长命令的长度）
        let max_cmd_len = self
            .state
            .filtered_commands
            .iter()
            .map(|(cmd, desc)| cmd.len() + desc.len() + 5) // 5 = " - " + 间距
            .max()
            .unwrap_or(30)
            .min(input_area.width as usize - 4)
            .max(20) as u16;

        let popup_width = max_cmd_len + 4; // 加上边框和间距

        // 弹窗显示在输入框上方
        let x = input_area.x + 4; // 偏移以对齐提示符后的输入区域
        let y = input_area.y.saturating_sub(popup_height);

        Rect::new(x, y, popup_width.min(input_area.width), popup_height)
    }
}

impl<'a> Widget for InputWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut input_block = Block::bordered().border_style(Style::default().fg(Color::DarkGray));

        if let Some(status) = self.status_message.as_deref() {
            // 状态栏是“快速定位问题”的入口，因此使用醒目颜色，但保持文案尽量简短。
            input_block = input_block.title(
                Line::from(vec![Span::styled(
                    format!(" {status} "),
                    Style::default().fg(Color::Red).bold(),
                )])
                .left_aligned(),
            );
        }

        if let Some(remaining) = self.context_remaining {
            input_block = input_block.title(
                Line::from(vec![Span::styled(
                    format!(" Context: {}% ", remaining),
                    Style::default().fg(Color::Yellow).bold(),
                )])
                .right_aligned(),
            );
        }

        let mut input_spans = vec![Span::styled(
            ">>> ",
            Style::default().fg(Color::Green).bold(),
        )];

        let byte_index = self.state.byte_index();
        let (left, right) = self.state.input.split_at(byte_index);

        input_spans.push(Span::raw(left));

        if self.state.cursor_visible {
            let (cursor_char, right_rest) = if let Some(c) = right.chars().next() {
                (c.to_string(), &right[c.len_utf8()..])
            } else {
                (" ".to_string(), "")
            };
            input_spans.push(Span::styled(
                cursor_char,
                Style::default().bg(Color::Green).fg(Color::Black),
            ));
            input_spans.push(Span::raw(right_rest));
        } else {
            input_spans.push(Span::raw(right));
        }

        let input_text = Line::from(input_spans);

        Paragraph::new(input_text)
            .block(input_block)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .render(area, buf);

        // 渲染命令补全弹窗
        if self.state.show_completion && !self.state.filtered_commands.is_empty() {
            let popup_area = self.calculate_completion_area(area);
            if popup_area.height > 0 && popup_area.width > 0 {
                // 清空弹窗区域
                Clear.render(popup_area, buf);

                // 获取可见范围
                let (start_idx, end_idx) = self.state.visible_completion_range();
                let visible_count = end_idx.saturating_sub(start_idx);
                let total_count = self.state.filtered_commands.len();
                let needs_scrollbar = total_count > COMPLETION_VISIBLE_COUNT;

                // 计算内容区域（是否需要为滚动条预留空间）
                let content_width = if needs_scrollbar {
                    popup_area.width.saturating_sub(1) // 预留 1 列给滚动条
                } else {
                    popup_area.width
                };

                // 构建列表项（只渲染可见区域的命令）
                let items: Vec<ListItem> = self
                    .state
                    .filtered_commands
                    .iter()
                    .enumerate()
                    .skip(start_idx)
                    .take(visible_count)
                    .map(|(idx, (cmd, desc))| {
                        let is_selected = idx == self.state.completion_selected;
                        let style = if is_selected {
                            Style::default().bg(Color::Cyan).fg(Color::Black)
                        } else {
                            Style::default().fg(Color::White)
                        };

                        // 截断内容以适应宽度
                        let full_content = format!("{} - {}", cmd, desc);
                        let content = if full_content.len() > content_width as usize - 2 {
                            format!("{}..", &full_content[..content_width as usize - 4])
                        } else {
                            full_content
                        };
                        ListItem::new(content).style(style)
                    })
                    .collect();

                // 渲染列表
                let list = List::new(items)
                    .block(Block::bordered().border_style(Style::default().fg(Color::Cyan)))
                    .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black));

                list.render(popup_area, buf);

                // 渲染滚动条
                if needs_scrollbar && popup_area.width > 1 {
                    self.render_scrollbar(popup_area, buf, visible_count, total_count);
                }
            }
        }
    }
}

impl<'a> InputWidget<'a> {
    /// 渲染滚动条。
    ///
    /// 在弹窗右侧显示一个简单的文本滚动条，指示当前滚动位置。
    fn render_scrollbar(
        &self,
        area: Rect,
        buf: &mut Buffer,
        visible_count: usize,
        total_count: usize,
    ) {
        if area.height <= 2 || area.width <= 1 {
            return;
        }

        let scrollbar_x = area.x + area.width - 2;
        let content_height = area.height.saturating_sub(2) as usize; // 减去边框
        let start_y = area.y + 1; // 从边框内开始

        // 计算滚动条滑块位置和大小
        let progress = self.state.scroll_progress();
        let thumb_size = ((visible_count as f32 / total_count as f32) * content_height as f32)
            .max(1.0)
            .min(content_height as f32) as usize;

        let max_thumb_start = content_height.saturating_sub(thumb_size);
        let thumb_start = if max_thumb_start == 0 {
            0
        } else {
            (progress * max_thumb_start as f32).round() as usize
        };

        // 渲染滚动条轨道和滑块
        for i in 0..content_height {
            let y = start_y + i as u16;
            let ch = if i >= thumb_start && i < thumb_start + thumb_size {
                '█' // 滑块
            } else {
                '░' // 轨道
            };

            if y < area.y + area.height - 1 {
                buf[(scrollbar_x, y)].set_char(ch).set_fg(Color::DarkGray);
            }
        }
    }
}
