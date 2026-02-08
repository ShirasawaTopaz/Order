use std::{
    collections::BTreeSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyEventKind};
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
            status_message: "NORMAL".to_string(),
            command_history: Vec::new(),
            should_exit: false,
            last_tick: Instant::now(),
        }
    }

    // 编辑器主循环。
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let tick_rate = Duration::from_millis(200);
        while !self.should_exit {
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
}
