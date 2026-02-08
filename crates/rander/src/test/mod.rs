//! Rander crate 的单元测试模块
//!
//! 该模块包含所有单元测试，测试 widget 和其他组件的功能。

use crate::widget::input_widget::{InputState, AVAILABLE_COMMANDS, COMPLETION_VISIBLE_COUNT};

#[test]
fn test_input_state_default() {
    let state = InputState::default();
    assert_eq!(state.input, "");
    assert_eq!(state.cursor_position, 0);
    assert!(state.cursor_visible);
    assert!(!state.show_completion);
    assert!(state.filtered_commands.is_empty());
    assert_eq!(state.completion_selected, 0);
}

#[test]
fn test_insert_char() {
    let mut state = InputState::default();
    state.insert_char('h');
    state.insert_char('i');
    assert_eq!(state.input, "hi");
    assert_eq!(state.cursor_position, 2);
}

#[test]
fn test_insert_char_with_unicode() {
    let mut state = InputState::default();
    state.insert_char('中');
    state.insert_char('文');
    assert_eq!(state.input, "中文");
    assert_eq!(state.cursor_position, 2);
}

#[test]
fn test_delete_char() {
    let mut state = InputState::default();
    state.insert_char('h');
    state.insert_char('i');
    state.delete_char();
    assert_eq!(state.input, "h");
    assert_eq!(state.cursor_position, 1);
}

#[test]
fn test_move_cursor() {
    let mut state = InputState::default();
    state.insert_char('a');
    state.insert_char('b');
    state.insert_char('c');
    
    state.move_cursor_left();
    assert_eq!(state.cursor_position, 2);
    
    state.move_cursor_right();
    assert_eq!(state.cursor_position, 3);
    
    // 测试边界
    state.move_cursor_right();
    assert_eq!(state.cursor_position, 3);
    
    state.cursor_position = 0;
    state.move_cursor_left();
    assert_eq!(state.cursor_position, 0);
}

#[test]
fn test_clear() {
    let mut state = InputState::default();
    state.insert_char('h');
    state.insert_char('i');
    state.show_completion = true;
    state.filtered_commands.push(("/help".to_string(), "desc".to_string()));
    
    state.clear();
    
    assert_eq!(state.input, "");
    assert_eq!(state.cursor_position, 0);
    assert!(!state.show_completion);
    assert!(state.filtered_commands.is_empty());
}

#[test]
fn test_completion_trigger_with_slash() {
    let mut state = InputState::default();
    state.insert_char('/');
    
    assert!(state.show_completion);
    assert_eq!(state.filtered_commands.len(), AVAILABLE_COMMANDS.len());
}

#[test]
fn test_completion_filter() {
    let mut state = InputState::default();
    state.insert_char('/');
    state.insert_char('h');
    state.insert_char('e');
    
    assert!(state.show_completion);
    // 应该只匹配 /help
    assert!(state.filtered_commands.iter().any(|(cmd, _)| cmd == "/help"));
}

#[test]
fn test_completion_navigation() {
    let mut state = InputState::default();
    state.insert_char('/');
    
    let total_commands = state.filtered_commands.len();
    assert_eq!(state.completion_selected, 0);
    
    // 向下导航
    state.completion_down();
    assert_eq!(state.completion_selected, 1);
    
    state.completion_up();
    assert_eq!(state.completion_selected, 0);
    
    // 测试循环：在顶部按上键应该循环到底部
    state.completion_up();
    assert_eq!(state.completion_selected, total_commands - 1);
    
    // 测试循环：在底部按下键应该循环到顶部
    state.completion_down();
    assert_eq!(state.completion_selected, 0);
}

#[test]
fn test_confirm_completion() {
    let mut state = InputState::default();
    state.insert_char('/');
    state.insert_char('h');
    
    // 过滤后应该包含 /help
    let result = state.confirm_completion();
    
    assert!(result);
    assert_eq!(state.input, "/help");
    assert!(!state.show_completion);
}

#[test]
fn test_cancel_completion() {
    let mut state = InputState::default();
    state.insert_char('/');
    state.insert_char('h');
    
    state.cancel_completion();
    
    assert!(!state.show_completion);
    assert_eq!(state.input, "/h"); // 输入保持不变
}

#[test]
fn test_delete_char_updates_completion() {
    let mut state = InputState::default();
    state.insert_char('/');
    state.insert_char('h');
    state.insert_char('e');
    
    assert!(state.show_completion);
    
    // 删除 'e'，应该仍然显示补全
    state.delete_char();
    assert!(state.show_completion);
    
    // 删除所有字符，应该关闭补全
    state.delete_char();
    state.delete_char();
    assert!(!state.show_completion);
}

#[test]
fn test_completion_popup_height() {
    let mut state = InputState::default();
    assert_eq!(state.completion_popup_height(), 0);
    
    state.insert_char('/');
    let expected_height = (AVAILABLE_COMMANDS.len().min(8) as u16) + 2;
    assert_eq!(state.completion_popup_height(), expected_height);
}

#[test]
fn test_required_height() {
    let state = InputState::default();
    let height = state.required_height(80);
    assert_eq!(height, 4); // 1 行文本 + 3（边框等）
}

#[test]
fn test_toggle_cursor_visibility() {
    let mut state = InputState::default();
    assert!(state.cursor_visible);
    
    state.toggle_cursor_visibility();
    assert!(!state.cursor_visible);
    
    state.toggle_cursor_visibility();
    assert!(state.cursor_visible);
}

#[test]
fn test_set_cursor_visible() {
    let mut state = InputState::default();
    state.cursor_visible = false;
    
    state.set_cursor_visible(true);
    assert!(state.cursor_visible);
    
    state.set_cursor_visible(false);
    assert!(!state.cursor_visible);
}

#[test]
fn test_byte_index() {
    let mut state = InputState::default();
    state.insert_char('a');
    state.insert_char('中'); // 3 字节
    state.insert_char('b');
    
    state.cursor_position = 0;
    assert_eq!(state.byte_index(), 0);
    
    state.cursor_position = 1;
    assert_eq!(state.byte_index(), 1);
    
    state.cursor_position = 2;
    assert_eq!(state.byte_index(), 4); // 'a' (1) + '中' (3)
    
    state.cursor_position = 3;
    assert_eq!(state.byte_index(), 5);
}

#[test]
fn test_clamp_cursor() {
    let state = InputState::default();
    assert_eq!(state.clamp_cursor(5), 0); // 空字符串，限制为 0
    
    let mut state = InputState::default();
    state.insert_char('a');
    state.insert_char('b');
    
    assert_eq!(state.clamp_cursor(5), 2); // 限制为最大长度
    assert_eq!(state.clamp_cursor(1), 1);
}

#[test]
fn test_completion_case_insensitive() {
    let mut state = InputState::default();
    state.insert_char('/');
    state.insert_char('H'); // 大写
    state.insert_char('E'); // 大写
    
    // 应该匹配 /help（不区分大小写）
    assert!(state.filtered_commands.iter().any(|(cmd, _)| cmd == "/help"));
}

#[test]
fn test_completion_partial_match() {
    let mut state = InputState::default();
    state.insert_char('/');
    state.insert_char('s');
    
    // 应该匹配 /skills, /settings, /status
    assert!(state.filtered_commands.iter().any(|(cmd, _)| cmd == "/skills"));
    assert!(state.filtered_commands.iter().any(|(cmd, _)| cmd == "/settings"));
    assert!(state.filtered_commands.iter().any(|(cmd, _)| cmd == "/status"));
}

#[test]
fn test_completion_scroll_offset() {
    let mut state = InputState::default();
    state.insert_char('/');
    
    let total_commands = state.filtered_commands.len();
    assert!(total_commands > COMPLETION_VISIBLE_COUNT, "需要更多命令来测试滚动");
    
    // 初始滚动偏移量应为 0
    assert_eq!(state.completion_scroll_offset, 0);
    
    // 向下滚动到超出可见范围的位置
    // 需要按 COMPLETION_VISIBLE_COUNT 次，因为初始在 0
    for _ in 0..COMPLETION_VISIBLE_COUNT {
        state.completion_down();
    }
    
    // 现在选中索引为 COMPLETION_VISIBLE_COUNT（即8）
    let selected = state.completion_selected;
    let offset = state.completion_scroll_offset;
    assert_eq!(selected, COMPLETION_VISIBLE_COUNT, "选中位置应为 {}, 实际是 {}", COMPLETION_VISIBLE_COUNT, selected);
    // 此时偏移量应该为 1，因为第 8 个（索引8）超出了初始可见范围（0-7）
    assert_eq!(offset, 1, "偏移量应为 1, 实际是 {}", offset);
}

#[test]
fn test_visible_completion_range() {
    let mut state = InputState::default();
    state.insert_char('/');
    
    // 初始可见范围应该是前 COMPLETION_VISIBLE_COUNT 个
    let (start, end) = state.visible_completion_range();
    assert_eq!(start, 0);
    assert_eq!(end, AVAILABLE_COMMANDS.len().min(COMPLETION_VISIBLE_COUNT));
    
    // 滚动后可见范围应该更新
    for _ in 0..COMPLETION_VISIBLE_COUNT + 2 {
        state.completion_down();
    }
    
    let (start, end) = state.visible_completion_range();
    assert_eq!(start, state.completion_scroll_offset);
    assert!(end - start <= COMPLETION_VISIBLE_COUNT);
}

#[test]
fn test_scroll_progress() {
    let mut state = InputState::default();
    state.insert_char('/');
    
    // 初始进度为 0
    assert_eq!(state.scroll_progress(), 0.0);
    
    // 滚动到底部
    let max_index = state.filtered_commands.len().saturating_sub(1);
    state.completion_selected = max_index;
    state.completion_scroll_offset = max_index.saturating_sub(COMPLETION_VISIBLE_COUNT - 1);
    
    // 进度应该接近 1.0
    let progress = state.scroll_progress();
    assert!(progress > 0.0 && progress <= 1.0);
}

#[test]
fn test_completion_popup_height_constant() {
    // 测试弹窗高度始终为 COMPLETION_VISIBLE_COUNT + 2（边框）
    let mut state = InputState::default();
    state.insert_char('/');
    
    let expected_height = (COMPLETION_VISIBLE_COUNT as u16) + 2;
    assert_eq!(state.completion_popup_height(), expected_height);
    
    // 即使滚动，高度也应该保持不变
    for _ in 0..20 {
        state.completion_down();
    }
    assert_eq!(state.completion_popup_height(), expected_height);
}
