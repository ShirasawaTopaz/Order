use std::path::Path;

use ratatui::layout::Rect;

use super::types::{PaneFocus, SplitDirection};

// 统计字符数（按 Unicode 字符而非字节）。
pub(super) fn char_count(input: &str) -> usize {
    input.chars().count()
}

// 字符索引转字节索引。
pub(super) fn char_to_byte_index(input: &str, char_idx: usize) -> usize {
    input
        .char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(input.len())
}

// 判断是否为单词字符。
pub(super) fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

// 判断是否允许触发补全请求的字符。
//
// 这里仅接受 ASCII 字母与下划线，目的是把补全请求限制在常见标识符输入场景，
// 避免数字或符号输入时触发无效请求，减少 LSP 往返与弹窗干扰。
pub(super) fn is_completion_trigger_char(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

// 判断坐标是否位于矩形内。
pub(super) fn contains_point(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height
}

// 判断当前输入是否为已知命令前缀。
pub(super) fn is_normal_command_prefix(prefix: &str) -> bool {
    const COMMANDS: &[&str] = &[
        "fs", "fl", "sv", "sp", "sh", "sl", "sj", "sk", "tn", "tl", "th", "tb", "tc", "tt", "te",
        "e", "pi", "pu", "ci", "cu", "w", "q", "fa", "ff", "fh", "fc", "lc", "lr", "lf", "lq",
        "fb", "[g", "]g", "K",
    ];
    COMMANDS.iter().any(|cmd| cmd.starts_with(prefix))
}

// 获取文件名（失败时使用回退值）。
pub(super) fn file_name_or<'a>(path: &'a Path, fallback: &'a str) -> &'a str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback)
}

// 分屏方向转字符串。
pub(super) fn split_to_str(split: SplitDirection) -> &'static str {
    match split {
        SplitDirection::None => "none",
        SplitDirection::Vertical => "vertical",
        SplitDirection::Horizontal => "horizontal",
    }
}

// 字符串转分屏方向。
pub(super) fn parse_split(value: &str) -> SplitDirection {
    match value {
        "vertical" => SplitDirection::Vertical,
        "horizontal" => SplitDirection::Horizontal,
        _ => SplitDirection::None,
    }
}

// 焦点窗格转字符串。
pub(super) fn pane_to_str(pane: PaneFocus) -> &'static str {
    match pane {
        PaneFocus::Primary => "primary",
        PaneFocus::Secondary => "secondary",
    }
}

// 字符串转焦点窗格。
pub(super) fn parse_pane(value: &str) -> PaneFocus {
    match value {
        "secondary" => PaneFocus::Secondary,
        _ => PaneFocus::Primary,
    }
}

// 会话文本转义。
pub(super) fn escape_text(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

// 会话文本反转义。
pub(super) fn unescape_text(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::is_completion_trigger_char;

    #[test]
    fn test_is_completion_trigger_char() {
        assert!(is_completion_trigger_char('a'));
        assert!(is_completion_trigger_char('Z'));
        assert!(is_completion_trigger_char('_'));

        assert!(!is_completion_trigger_char('0'));
        assert!(!is_completion_trigger_char('-'));
        assert!(!is_completion_trigger_char('中'));
    }
}
