use crate::{
    editor::Editor,
    focus_status::{CURRENT_FOCUS, FocusStatus},
    widget::input_widget::{InputState, InputWidget},
};
use core::{
    commands::{get_exit, EXIT},
    model::info::get_current_model_info,
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Widget},
    DefaultTerminal, Frame,
};

pub struct OrderTui<'a> {
    /// 全局退出标记。
    exit: &'a AtomicBool,
    /// 输入组件状态。
    input_state: InputState,
    /// 输入光标闪烁时钟。
    last_tick: Instant,
    /// 预留上下文剩余量。
    context_remaining: u32,
    /// 回车后待处理的命令。
    pending_command: Option<String>,
}

impl Default for OrderTui<'_> {
    fn default() -> Self {
        Self {
            exit: &EXIT,
            input_state: InputState::default(),
            last_tick: Instant::now(),
            context_remaining: 100,
            pending_command: None,
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
                        // 回车提交输入命令，由统一入口处理。
                        let command = self.input_state.input.trim().to_string();
                        if !command.is_empty() {
                            self.pending_command = Some(command);
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

    /// 统一消费输入框提交命令。
    fn process_pending_command(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let Some(command) = self.pending_command.take() else {
            return Ok(());
        };

        match command.as_str() {
            "/editor" => self.launch_editor(terminal)?,
            "/exit" => self.exit.store(true, Ordering::Relaxed),
            _ => {}
        }
        Ok(())
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
            ("/history", "View project chat history"),
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

