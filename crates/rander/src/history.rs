use anyhow::{Context, Result};
use chrono::Local;
use core::encoding::{read_utf8_text_with_report, write_utf8_text_with_report};
use rig::completion::Message as RigMessage;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

/// 默认任务 ID。
///
/// 当用户未显式提供 `ORDER_TASK_ID` 时，长期记忆会归档到该任务下。
const DEFAULT_TASK_ID: &str = "default";
/// 长期记忆每个分类最多保留的条目数。
const MAX_MEMORY_ITEMS: usize = 40;
/// 每次从近期消息中抽取长期记忆时，最多回看多少条消息。
const MEMORY_SCAN_LIMIT: usize = 24;
/// 为防止预算过小导致上下文几乎为空，输入预算会有一个下限。
const MIN_CONTEXT_BUDGET: u32 = 512;
/// 需要从长期记忆中剔除的“低信号元话术”前缀。
///
/// 这些文本通常是助手的阶段性承诺或流程反馈，不是稳定规则/偏好/决策，
/// 若反复注入会诱导模型持续“先口头确认、再承诺后续动作”。
const LOW_SIGNAL_MEMORY_PREFIXES: [&str; 11] = [
    "收到",
    "好的",
    "已确认",
    "我已确认",
    "我已经",
    "我会",
    "我将",
    "我建议",
    "我刚刚",
    "我先",
    "下一步",
];

/// 上下文消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRole {
    /// 用户消息。
    User,
    /// 助手消息。
    Assistant,
    /// 错误消息。
    Error,
}

/// 上下文消息结构。
#[derive(Debug, Clone)]
pub struct ContextMessage {
    /// 消息角色。
    pub role: ContextRole,
    /// 消息正文。
    pub content: String,
    /// 是否允许参与上下文与长期记忆提取。
    pub persist_to_history: bool,
}

/// 模型上下文相关限制参数。
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextModelLimits {
    /// 模型最大上下文长度。
    pub model_max_context: u32,
    /// 模型最大 token 总预算。
    pub model_max_tokens: u32,
    /// 模型期望的最大输出长度。
    pub model_max_output: u32,
}

impl ContextModelLimits {
    /// 计算当前请求可用的“输入 token 预算”。
    ///
    /// 预算优先使用模型声明值；若模型未声明，则回退到压缩器的保守默认值。
    fn input_budget(self, fallback_input_budget: u32, reserved_output_tokens: u32) -> u32 {
        let declared_total = match (self.model_max_context, self.model_max_tokens) {
            (0, 0) => 0,
            (context, 0) => context,
            (0, tokens) => tokens,
            (context, tokens) => context.min(tokens),
        };

        let mut budget = if declared_total == 0 {
            fallback_input_budget
        } else {
            declared_total
        };

        let output_reserve = self.model_max_output.max(reserved_output_tokens);
        budget = budget.saturating_sub(output_reserve);
        budget.max(MIN_CONTEXT_BUDGET)
    }
}

/// 压缩结果。
#[derive(Debug, Clone)]
pub struct ContextBuildResult {
    /// 发送给模型的历史消息。
    pub history: Vec<RigMessage>,
    /// 估算后的剩余上下文百分比。
    pub context_remaining: u32,
}

/// 上下文压缩器。
///
/// 该组件负责在 token 预算下组织“短期上下文 + 中期摘要 + 长期记忆”。
#[derive(Debug, Clone)]
pub struct ContextCompressor {
    /// 短期上下文保留的用户轮数上限。
    pub short_term_rounds: usize,
    /// 短期上下文最大消息条数。
    pub max_short_term_messages: usize,
    /// 模型未声明上下文预算时的保守输入预算。
    pub fallback_input_budget: u32,
    /// 预留给模型输出的 token。
    pub reserved_output_tokens: u32,
    /// 中期摘要最大字符数。
    pub max_summary_chars: usize,
    /// 长期记忆注入文本最大字符数。
    pub max_long_memory_chars: usize,
}

impl Default for ContextCompressor {
    fn default() -> Self {
        Self {
            short_term_rounds: 60,
            max_short_term_messages: 120,
            fallback_input_budget: 8192,
            reserved_output_tokens: 1024,
            max_summary_chars: 1200,
            max_long_memory_chars: 900,
        }
    }
}

impl ContextCompressor {
    /// 在预算范围内构建最终历史消息。
    ///
    /// 参数说明：
    /// - `current_prompt`：当前轮用户输入（会单独发送，避免在历史中重复注入）。
    /// - `messages`：当前会话累计消息。
    /// - `task_id`：长期记忆归档任务 ID，用于标注记忆来源。
    /// - `task_memory`：当前任务下持久化的长期记忆内容。
    /// - `limits`：模型上下文限制。
    fn compress(
        &self,
        current_prompt: &str,
        messages: &[ContextMessage],
        task_id: &str,
        task_memory: &TaskMemory,
        limits: ContextModelLimits,
    ) -> ContextBuildResult {
        let filtered_entries = filter_messages_for_llm(messages, current_prompt);
        let (older_entries, mut short_entries) = split_short_term_entries(
            &filtered_entries,
            self.short_term_rounds,
            self.max_short_term_messages,
        );

        // 仅在确实发生历史裁剪时才注入中期摘要，避免短会话被冗余提示干扰。
        let mut mid_summary = if older_entries.is_empty() {
            None
        } else {
            build_mid_term_summary(&older_entries, self.max_summary_chars)
        };
        let mut long_memory =
            build_long_term_memory_prompt(task_id, task_memory, self.max_long_memory_chars);

        let input_budget =
            limits.input_budget(self.fallback_input_budget, self.reserved_output_tokens);
        let mut used_tokens = estimate_total_tokens(
            &short_entries,
            mid_summary.as_deref(),
            long_memory.as_deref(),
            current_prompt,
        );

        // 超预算时按“短期上下文 -> 中期摘要 -> 长期记忆”的顺序收缩。
        while used_tokens > input_budget {
            if short_entries.len() > 2 {
                short_entries.remove(0);
            } else if let Some(summary) = mid_summary.as_mut()
                && summary.chars().count() > 120
            {
                *summary = truncate_text(summary, summary.chars().count() * 3 / 4);
            } else if let Some(memory) = long_memory.as_mut()
                && memory.chars().count() > 120
            {
                *memory = truncate_text(memory, memory.chars().count() * 3 / 4);
            } else {
                break;
            }

            used_tokens = estimate_total_tokens(
                &short_entries,
                mid_summary.as_deref(),
                long_memory.as_deref(),
                current_prompt,
            );
        }

        let mut history_entries = Vec::new();
        if let Some(memory) = long_memory {
            history_entries.push(ContextEntry::assistant(memory));
        }
        if let Some(summary) = mid_summary {
            history_entries.push(ContextEntry::assistant(summary));
        }
        history_entries.extend(short_entries);

        let history = history_entries
            .into_iter()
            .map(ContextEntry::into_rig_message)
            .collect::<Vec<_>>();
        let context_remaining = calc_remaining_percentage(input_budget, used_tokens);

        ContextBuildResult {
            history,
            context_remaining,
        }
    }
}

/// 上下文管理器。
///
/// 负责：
/// - 组织三层上下文（短期/中期/长期）；
/// - 管理长期记忆文件 `.order/context/memory.json` 的读写；
/// - 按任务 ID 归档长期记忆。
#[derive(Debug, Clone)]
pub struct ContextManager {
    /// 当前任务 ID。
    task_id: String,
    /// 长期记忆文件路径。
    memory_path: PathBuf,
    /// 全量长期记忆数据。
    memory_file: ContextMemoryFile,
    /// 上下文压缩器。
    compressor: ContextCompressor,
}

impl Default for ContextManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextManager {
    /// 使用默认路径与默认参数创建上下文管理器。
    ///
    /// 初始化阶段若读文件失败，会回退为空记忆，保证主流程不被阻断。
    pub fn new() -> Self {
        let task_id = resolve_task_id();
        let memory_path = resolve_memory_path();
        let mut memory_file = read_memory_file(&memory_path).unwrap_or_else(|error| {
            eprintln!("failed to load context memory, fallback to empty: {error}");
            ContextMemoryFile::default()
        });
        // 启动时做一次轻量清理，自动剔除历史遗留的低信号元话术，
        // 防止旧版本写入的“承诺式文本”持续污染后续上下文。
        if sanitize_memory_file(&mut memory_file)
            && let Err(error) = write_memory_file(&memory_path, &memory_file)
        {
            eprintln!("failed to sanitize context memory, keep in-memory result only: {error}");
        }

        Self {
            task_id,
            memory_path,
            memory_file,
            compressor: ContextCompressor::default(),
        }
    }

    /// 使用当前管理器构建历史上下文。
    pub fn build_history(
        &self,
        current_prompt: &str,
        messages: &[ContextMessage],
        limits: ContextModelLimits,
    ) -> ContextBuildResult {
        let task_memory = self
            .memory_file
            .tasks
            .get(&self.task_id)
            .cloned()
            .unwrap_or_default();

        self.compressor.compress(
            current_prompt,
            messages,
            self.task_id.as_str(),
            &task_memory,
            limits,
        )
    }

    /// 从近期会话中抽取长期记忆并落盘。
    ///
    /// 副作用：
    /// - 可能更新内存中的 `memory_file`；
    /// - 可能写入 `.order/context/memory.json`。
    pub fn update_long_term_memory(&mut self, messages: &[ContextMessage]) -> Result<()> {
        let candidates = extract_memory_candidates(messages);
        if candidates.is_empty() {
            return Ok(());
        }

        let task_memory = self
            .memory_file
            .tasks
            .entry(self.task_id.clone())
            .or_default();
        let mut changed = false;

        for (category, content) in candidates {
            let target = match category {
                MemoryCategory::Rule => &mut task_memory.project_rules,
                MemoryCategory::Preference => &mut task_memory.preferences,
                MemoryCategory::Decision => &mut task_memory.key_decisions,
            };

            if push_unique_limited(target, &content, MAX_MEMORY_ITEMS) {
                changed = true;
            }
        }

        if !changed {
            return Ok(());
        }

        task_memory.updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        write_memory_file(&self.memory_path, &self.memory_file)
    }

    #[cfg(test)]
    fn new_for_test(task_id: &str, memory_path: PathBuf, compressor: ContextCompressor) -> Self {
        Self {
            task_id: task_id.to_string(),
            memory_path,
            memory_file: ContextMemoryFile::default(),
            compressor,
        }
    }
}

/// 长期记忆文件结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ContextMemoryFile {
    /// 任务 ID -> 任务长期记忆。
    #[serde(default)]
    tasks: HashMap<String, TaskMemory>,
}

/// 单个任务下的长期记忆。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TaskMemory {
    /// 项目规则。
    #[serde(default)]
    project_rules: Vec<String>,
    /// 用户偏好。
    #[serde(default)]
    preferences: Vec<String>,
    /// 关键决策。
    #[serde(default)]
    key_decisions: Vec<String>,
    /// 最近更新时间。
    #[serde(default)]
    updated_at: String,
}

/// 压缩阶段使用的内部消息结构。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryRole {
    User,
    Assistant,
}

/// 压缩阶段使用的内部消息实体。
#[derive(Debug, Clone)]
struct ContextEntry {
    role: EntryRole,
    content: String,
}

impl ContextEntry {
    /// 创建用户消息条目。
    fn user(content: String) -> Self {
        Self {
            role: EntryRole::User,
            content,
        }
    }

    /// 创建助手消息条目。
    fn assistant(content: String) -> Self {
        Self {
            role: EntryRole::Assistant,
            content,
        }
    }

    /// 转换为 `rig` 所需消息类型。
    fn into_rig_message(self) -> RigMessage {
        match self.role {
            EntryRole::User => RigMessage::user(self.content),
            EntryRole::Assistant => RigMessage::assistant(self.content),
        }
    }
}

/// 长期记忆分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryCategory {
    /// 项目规则。
    Rule,
    /// 用户偏好。
    Preference,
    /// 关键决策。
    Decision,
}

/// 过滤并转换会话消息，得到可发送给模型的上下文条目。
fn filter_messages_for_llm(messages: &[ContextMessage], current_prompt: &str) -> Vec<ContextEntry> {
    let latest_user_index = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| {
            message.persist_to_history
                && matches!(message.role, ContextRole::User)
                && message.content == current_prompt
        })
        .map(|(index, _)| index);

    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.persist_to_history)
        .filter_map(|(index, message)| {
            if Some(index) == latest_user_index {
                return None;
            }

            let content = message.content.trim();
            if content.is_empty() {
                return None;
            }

            match message.role {
                ContextRole::User => Some(ContextEntry::user(content.to_string())),
                ContextRole::Assistant => {
                    // 对“只承诺下一步、无实际产出”的助手回复做降噪，
                    // 避免该类文本在短期上下文中被反复强化，形成“只说不做”回环。
                    if is_non_executing_commitment_message(content) {
                        None
                    } else {
                        Some(ContextEntry::assistant(content.to_string()))
                    }
                }
                ContextRole::Error => None,
            }
        })
        .collect()
}

/// 将全部历史拆分为“中期摘要候选（older）+ 短期上下文（short）”。
fn split_short_term_entries(
    entries: &[ContextEntry],
    short_term_rounds: usize,
    max_short_term_messages: usize,
) -> (Vec<ContextEntry>, Vec<ContextEntry>) {
    if entries.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut user_round_count = 0usize;
    let mut start_index = 0usize;

    for (index, entry) in entries.iter().enumerate().rev() {
        if entry.role == EntryRole::User {
            user_round_count += 1;
            start_index = index;
            if user_round_count >= short_term_rounds {
                break;
            }
        }
    }

    let mut older_entries = if user_round_count < short_term_rounds {
        Vec::new()
    } else {
        entries[..start_index].to_vec()
    };
    let mut short_entries = if user_round_count < short_term_rounds {
        entries.to_vec()
    } else {
        entries[start_index..].to_vec()
    };

    // 双重保险：即使用户轮数很大，也不会让短期层无限膨胀。
    if short_entries.len() > max_short_term_messages {
        let overflow = short_entries.len() - max_short_term_messages;
        older_entries.extend_from_slice(&short_entries[..overflow]);
        short_entries = short_entries[overflow..].to_vec();
    }

    (older_entries, short_entries)
}

/// 构建中期摘要文本。
fn build_mid_term_summary(entries: &[ContextEntry], max_chars: usize) -> Option<String> {
    let mut goals = collect_category_snippets(
        entries,
        &["目标", "需要", "请", "希望", "实现", "支持", "完成", "修复"],
        3,
    );
    let completed = collect_category_snippets(
        entries,
        &[
            "已",
            "完成",
            "支持",
            "修复",
            "增加",
            "新增",
            "通过",
            "实现了",
        ],
        3,
    );
    let blockers = collect_category_snippets(
        entries,
        &[
            "失败", "报错", "错误", "无法", "阻塞", "异常", "超时", "问题",
        ],
        3,
    );

    // 若关键词未命中，回退到最近片段，避免摘要为空。
    if goals.is_empty() && completed.is_empty() && blockers.is_empty() {
        let fallback = entries
            .iter()
            .rev()
            .filter_map(|entry| first_meaningful_line(&entry.content))
            .take(3)
            .collect::<Vec<_>>();
        if !fallback.is_empty() {
            goals.push(format!("延续近期对话：{}", fallback.join(" / ")));
        }
    }

    if goals.is_empty() && completed.is_empty() && blockers.is_empty() {
        return None;
    }

    let summary = format!(
        "阶段摘要：\n- 目标：{}\n- 已完成：{}\n- 阻塞点：{}",
        join_or_default(&goals),
        join_or_default(&completed),
        join_or_default(&blockers),
    );

    Some(truncate_text(&summary, max_chars))
}

/// 构建长期记忆提示文本。
fn build_long_term_memory_prompt(
    task_id: &str,
    task_memory: &TaskMemory,
    max_chars: usize,
) -> Option<String> {
    let mut lines = Vec::new();

    append_memory_items(&mut lines, "规则", &task_memory.project_rules, 4);
    append_memory_items(&mut lines, "偏好", &task_memory.preferences, 4);
    append_memory_items(&mut lines, "决策", &task_memory.key_decisions, 4);

    if lines.is_empty() {
        return None;
    }

    let text = format!(
        "长期记忆（任务ID: {}，仅供本轮参考）：\n{}",
        task_id,
        lines.join("\n")
    );
    Some(truncate_text(&text, max_chars))
}

/// 估算总 token 使用量。
fn estimate_total_tokens(
    short_entries: &[ContextEntry],
    mid_summary: Option<&str>,
    long_memory: Option<&str>,
    current_prompt: &str,
) -> u32 {
    let short_tokens = short_entries.iter().map(estimate_entry_tokens).sum::<u32>();
    let summary_tokens = mid_summary.map(estimate_text_tokens).unwrap_or(0);
    let memory_tokens = long_memory.map(estimate_text_tokens).unwrap_or(0);
    let prompt_tokens = estimate_text_tokens(current_prompt);

    short_tokens
        .saturating_add(summary_tokens)
        .saturating_add(memory_tokens)
        .saturating_add(prompt_tokens)
        .saturating_add(16)
}

/// 估算单条消息 token 使用量。
fn estimate_entry_tokens(entry: &ContextEntry) -> u32 {
    // 每条消息除了正文还包含 role/结构开销，这里统一给 8 token 保守值。
    estimate_text_tokens(&entry.content).saturating_add(8)
}

/// 估算文本 token 数。
fn estimate_text_tokens(text: &str) -> u32 {
    if text.trim().is_empty() {
        return 0;
    }

    // 使用“4 字节约 1 token”的经验值做快速估算。
    let len = text.len() as u32;
    (len.saturating_add(3) / 4).max(1)
}

/// 计算剩余上下文百分比。
fn calc_remaining_percentage(input_budget: u32, used_tokens: u32) -> u32 {
    if input_budget == 0 {
        return 0;
    }

    let remaining = input_budget.saturating_sub(used_tokens);
    ((u64::from(remaining) * 100) / u64::from(input_budget)).min(100) as u32
}

/// 从历史条目中抽取某一类别的片段。
fn collect_category_snippets(
    entries: &[ContextEntry],
    keywords: &[&str],
    limit: usize,
) -> Vec<String> {
    let mut collected = Vec::new();

    for entry in entries.iter().rev() {
        if collected.len() >= limit {
            break;
        }

        let Some(line) = first_meaningful_line(&entry.content) else {
            continue;
        };
        if !contains_any_keyword(&line, keywords) {
            continue;
        }
        if push_unique_limited(&mut collected, &line, limit) {
            continue;
        }
    }

    collected
}

/// 从消息中抽取长期记忆候选。
fn extract_memory_candidates(messages: &[ContextMessage]) -> Vec<(MemoryCategory, String)> {
    if messages.is_empty() {
        return Vec::new();
    }

    let start = messages.len().saturating_sub(MEMORY_SCAN_LIMIT);
    let mut result = Vec::new();

    for message in &messages[start..] {
        // 长期记忆只采集用户输入，避免把助手的“阶段性承诺/流程话术”写回并自我强化。
        if !message.persist_to_history || !matches!(message.role, ContextRole::User) {
            continue;
        }

        for raw_line in message.content.lines() {
            let line = normalize_text(raw_line);
            if line.chars().count() < 6 {
                continue;
            }
            if is_low_signal_memory_item(&line) {
                continue;
            }

            let category = detect_memory_category(&line);
            if let Some(category) = category {
                result.push((category, truncate_text(&line, 180)));
            }
        }
    }

    deduplicate_candidates(result)
}

/// 对候选长期记忆做去重。
fn deduplicate_candidates(
    candidates: Vec<(MemoryCategory, String)>,
) -> Vec<(MemoryCategory, String)> {
    // 显式标注元素类型，避免在首次只读迭代时被错误推断为 `str`。
    let mut deduped: Vec<(MemoryCategory, String)> = Vec::new();

    for (category, content) in candidates {
        let existed = deduped.iter().any(|(existing_category, existing_content)| {
            *existing_category == category
                && normalize_text(existing_content) == normalize_text(&content)
        });
        if !existed {
            deduped.push((category, content));
        }
    }

    deduped
}

/// 清理长期记忆中的低信号条目。
///
/// 返回值表示是否发生了修改，便于调用方决定是否回写到磁盘。
fn sanitize_memory_file(file: &mut ContextMemoryFile) -> bool {
    let mut changed = false;
    for task_memory in file.tasks.values_mut() {
        changed |= retain_high_signal_items(&mut task_memory.project_rules);
        changed |= retain_high_signal_items(&mut task_memory.preferences);
        changed |= retain_high_signal_items(&mut task_memory.key_decisions);
    }
    changed
}

/// 原地保留高信号条目。
fn retain_high_signal_items(items: &mut Vec<String>) -> bool {
    let before = items.len();
    items.retain(|item| !is_low_signal_memory_item(item));
    before != items.len()
}

/// 向长期记忆提示中追加高信号条目。
fn append_memory_items(lines: &mut Vec<String>, label: &str, items: &[String], max_items: usize) {
    for item in items
        .iter()
        .rev()
        .filter(|item| !is_low_signal_memory_item(item))
        .take(max_items)
    {
        lines.push(format!("- {}：{}", label, item));
    }
}

/// 识别文本对应的长期记忆分类。
fn detect_memory_category(text: &str) -> Option<MemoryCategory> {
    if contains_any_keyword(
        text,
        &[
            "必须", "禁止", "不要", "务必", "严禁", "约束", "规则", "统一",
        ],
    ) {
        return Some(MemoryCategory::Rule);
    }
    if contains_any_keyword(text, &["偏好", "喜欢", "习惯", "建议", "优先", "尽量"]) {
        return Some(MemoryCategory::Preference);
    }
    if contains_any_keyword(text, &["决定", "采用", "方案", "改为", "结论", "选择"]) {
        return Some(MemoryCategory::Decision);
    }
    None
}

/// 将字符串集合拼接；空集合返回“暂无记录”。
fn join_or_default(items: &[String]) -> String {
    if items.is_empty() {
        "暂无记录".to_string()
    } else {
        items.join("；")
    }
}

/// 提取首行有效文本。
fn first_meaningful_line(content: &str) -> Option<String> {
    content
        .lines()
        .map(normalize_text)
        .find(|line| !line.is_empty())
        .map(|line| truncate_text(&line, 120))
}

/// 判断文本是否包含任意关键词。
fn contains_any_keyword(text: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|keyword| text.contains(keyword))
}

/// 规范化文本，减少去重误判。
fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 判断是否属于不应进入长期记忆的低信号文本。
///
/// 过滤目标是“过程状态”而非“稳定事实”，例如：
/// - 助手承诺下一步动作；
/// - 对方案/流程的礼貌确认；
/// - 一次性阶段汇报。
fn is_low_signal_memory_item(text: &str) -> bool {
    let normalized = normalize_text(text);
    if normalized.is_empty() {
        return true;
    }

    if LOW_SIGNAL_MEMORY_PREFIXES
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
    {
        return true;
    }

    contains_any_keyword(
        &normalized,
        &[
            "提交第一批补丁",
            "进入实现阶段",
            "完成入口定位",
            "按既定方案继续",
            "我会继续",
            "关键改动点都在仓库里可定位",
        ],
    )
}

/// 判断助手消息是否属于“仅承诺下一步、没有实际执行结果”的低价值上下文。
///
/// 该规则只用于上下文降噪，不用于业务正确性判断：
/// - 若消息包含明确执行结果（如“已修改/已完成/测试通过”），则保留；
/// - 若主要是阶段计划与承诺语句，则在后续轮次中剔除，降低复读风险。
fn is_non_executing_commitment_message(text: &str) -> bool {
    let normalized = normalize_text(text);
    if normalized.is_empty() {
        return false;
    }

    let has_commitment_phrase = contains_any_keyword(
        &normalized,
        &[
            "我会按",
            "我会继续",
            "接下来我将",
            "我现在就开始",
            "开始进入实现阶段",
            "第一步",
            "第二步",
            "提交第一批补丁",
            "确认关键改动点",
        ],
    );
    if !has_commitment_phrase {
        return false;
    }

    !contains_any_keyword(
        &normalized,
        &[
            "已修改",
            "已完成",
            "已实现",
            "测试通过",
            "测试失败",
            ".rs",
            ".toml",
            "trace_id=",
        ],
    )
}

/// 按字符上限截断文本。
fn truncate_text(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut truncated = text
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

/// 向列表追加唯一项，并限制最大长度。
fn push_unique_limited(list: &mut Vec<String>, content: &str, max_items: usize) -> bool {
    let normalized = normalize_text(content);
    let exists = list
        .iter()
        .any(|existing| normalize_text(existing) == normalized);
    if exists {
        return false;
    }

    list.push(content.to_string());
    if list.len() > max_items {
        let overflow = list.len() - max_items;
        list.drain(0..overflow);
    }
    true
}

/// 读取长期记忆文件。
fn read_memory_file(path: &Path) -> Result<ContextMemoryFile> {
    if !path.exists() {
        return Ok(ContextMemoryFile::default());
    }

    let (content, report) = read_utf8_text_with_report(path)
        .with_context(|| format!("读取上下文记忆失败: {}", path.display()))?;
    if report.has_warning() {
        for warning in report.warnings_for(path) {
            // 记忆文件在启动阶段读取，无法安全回显到 TUI，这里使用标准错误输出提示排障信息。
            eprintln!("context memory encoding warning: {warning}");
        }
    }
    if content.trim().is_empty() {
        return Ok(ContextMemoryFile::default());
    }

    // 优先解析新结构；若失败则尝试兼容旧结构（直接 tasks map）。
    let parsed = serde_json::from_str::<ContextMemoryFile>(&content).or_else(|_| {
        serde_json::from_str::<HashMap<String, TaskMemory>>(&content)
            .map(|tasks| ContextMemoryFile { tasks })
    });

    parsed.with_context(|| format!("解析上下文记忆失败: {}", path.display()))
}

/// 写入长期记忆文件。
fn write_memory_file(path: &Path, file: &ContextMemoryFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建上下文目录失败: {}", parent.display()))?;
    }

    let mut content = serde_json::to_string_pretty(file).context("序列化上下文记忆失败")?;
    content.push('\n');
    let report = write_utf8_text_with_report(path, &content)
        .with_context(|| format!("写入上下文记忆失败: {}", path.display()))?;
    if report.has_warning() {
        for warning in report.warnings_for(path) {
            eprintln!("context memory encoding warning: {warning}");
        }
    }
    Ok(())
}

/// 解析当前任务 ID。
fn resolve_task_id() -> String {
    first_non_empty_env(&["ORDER_TASK_ID", "TASK_ID"])
        .unwrap_or_else(|| DEFAULT_TASK_ID.to_string())
}

/// 解析长期记忆文件路径。
fn resolve_memory_path() -> PathBuf {
    if let Some(explicit) = first_non_empty_env(&["ORDER_CONTEXT_MEMORY_FILE"]) {
        return PathBuf::from(explicit);
    }

    if let Ok(current_dir) = env::current_dir() {
        return current_dir
            .join(".order")
            .join("context")
            .join("memory.json");
    }

    PathBuf::from(".order").join("context").join("memory.json")
}

/// 从环境变量列表中读取首个非空值。
fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn context_message(
        role: ContextRole,
        content: &str,
        persist_to_history: bool,
    ) -> ContextMessage {
        ContextMessage {
            role,
            content: content.to_string(),
            persist_to_history,
        }
    }

    fn temp_memory_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("order-context-test-{nonce}"))
            .join("memory.json")
    }

    #[test]
    fn build_history_should_skip_current_prompt_and_error_message() {
        let manager = ContextManager::new_for_test(
            "task-a",
            temp_memory_path(),
            ContextCompressor::default(),
        );
        let messages = vec![
            context_message(ContextRole::User, "第一问", true),
            context_message(ContextRole::Assistant, "第一答", true),
            context_message(ContextRole::Error, "临时报错", true),
            context_message(ContextRole::User, "第二问", true),
        ];

        let result = manager.build_history("第二问", &messages, ContextModelLimits::default());
        assert_eq!(
            result.history,
            vec![RigMessage::user("第一问"), RigMessage::assistant("第一答")]
        );
        assert!(result.context_remaining <= 100);
    }

    #[test]
    fn build_history_should_include_mid_summary_when_history_is_trimmed() {
        let compressor = ContextCompressor {
            short_term_rounds: 2,
            max_short_term_messages: 4,
            fallback_input_budget: 2048,
            reserved_output_tokens: 256,
            max_summary_chars: 200,
            max_long_memory_chars: 200,
        };
        let manager = ContextManager::new_for_test("task-b", temp_memory_path(), compressor);

        let mut messages = Vec::new();
        for index in 0..6 {
            messages.push(context_message(
                ContextRole::User,
                &format!("请修复第{index}个模块"),
                true,
            ));
            messages.push(context_message(
                ContextRole::Assistant,
                &format!("已完成第{index}个模块"),
                true,
            ));
        }
        messages.push(context_message(ContextRole::User, "继续执行", true));

        let result = manager.build_history("继续执行", &messages, ContextModelLimits::default());
        let first = result.history.first().expect("history should not be empty");
        assert!(
            format!("{first:?}").contains("阶段摘要"),
            "历史被裁剪后应注入中期摘要"
        );
    }

    #[test]
    fn update_long_term_memory_should_persist_and_deduplicate() {
        let path = temp_memory_path();
        let mut manager =
            ContextManager::new_for_test("task-c", path.clone(), ContextCompressor::default());
        let messages = vec![
            context_message(ContextRole::User, "必须使用 UTF-8 编码", true),
            context_message(ContextRole::User, "用户偏好：优先修复根因", true),
            context_message(ContextRole::User, "最终决定采用最小改动方案", true),
            context_message(ContextRole::User, "最终决定采用最小改动方案", true),
            context_message(
                ContextRole::Assistant,
                "已确认，我会继续按既定方案提交第一批补丁",
                true,
            ),
        ];

        manager
            .update_long_term_memory(&messages)
            .expect("memory should be persisted");

        let content = fs::read_to_string(&path).expect("memory file should exist");
        let parsed: ContextMemoryFile =
            serde_json::from_str(&content).expect("memory file should be valid json");
        let task_memory = parsed
            .tasks
            .get("task-c")
            .expect("task memory should exist");

        assert_eq!(task_memory.project_rules.len(), 1);
        assert_eq!(task_memory.preferences.len(), 1);
        assert_eq!(task_memory.key_decisions.len(), 1);
    }

    #[test]
    fn update_long_term_memory_should_ignore_assistant_meta_talk() {
        let mut manager = ContextManager::new_for_test(
            "task-d",
            temp_memory_path(),
            ContextCompressor::default(),
        );
        let messages = vec![
            context_message(
                ContextRole::Assistant,
                "我会继续按既定方案提交第一批补丁",
                true,
            ),
            context_message(
                ContextRole::Assistant,
                "好的，收到“同意”，下一步进入实现阶段",
                true,
            ),
        ];

        manager
            .update_long_term_memory(&messages)
            .expect("assistant meta talk should be ignored without error");

        assert!(
            manager.memory_file.tasks.get("task-d").is_none(),
            "仅有助手元话术时不应写入长期记忆"
        );
    }

    #[test]
    fn build_long_term_memory_prompt_should_skip_low_signal_items() {
        let task_memory = TaskMemory {
            project_rules: vec![
                "必须使用 UTF-8 编码".to_string(),
                "已确认，我会继续按既定方案提交第一批补丁".to_string(),
            ],
            preferences: vec![
                "用户偏好：优先修复根因".to_string(),
                "我建议下一步进入实现阶段".to_string(),
            ],
            key_decisions: vec![
                "最终决定采用最小改动方案".to_string(),
                "我会继续按既定方案提交第一批补丁".to_string(),
            ],
            updated_at: String::new(),
        };

        let prompt = build_long_term_memory_prompt("task-e", &task_memory, 1200)
            .expect("high-signal memory should still generate prompt");

        assert!(prompt.contains("必须使用 UTF-8 编码"));
        assert!(prompt.contains("用户偏好：优先修复根因"));
        assert!(prompt.contains("最终决定采用最小改动方案"));
        assert!(!prompt.contains("提交第一批补丁"));
        assert!(!prompt.contains("进入实现阶段"));
    }

    #[test]
    fn sanitize_memory_file_should_remove_low_signal_items() {
        let mut file = ContextMemoryFile {
            tasks: HashMap::from([(
                "task-f".to_string(),
                TaskMemory {
                    project_rules: vec![
                        "必须使用 UTF-8 编码".to_string(),
                        "收到，我会继续按既定方案提交第一批补丁".to_string(),
                    ],
                    preferences: vec!["用户偏好：优先修复根因".to_string()],
                    key_decisions: vec![
                        "最终决定采用最小改动方案".to_string(),
                        "下一步进入实现阶段".to_string(),
                    ],
                    updated_at: String::new(),
                },
            )]),
        };

        let changed = sanitize_memory_file(&mut file);
        assert!(changed);

        let task = file
            .tasks
            .get("task-f")
            .expect("task-f should exist after sanitize");
        assert_eq!(task.project_rules, vec!["必须使用 UTF-8 编码".to_string()]);
        assert_eq!(task.preferences, vec!["用户偏好：优先修复根因".to_string()]);
        assert_eq!(
            task.key_decisions,
            vec!["最终决定采用最小改动方案".to_string()]
        );
    }

    #[test]
    fn build_history_should_skip_non_executing_commitment_assistant_message() {
        let manager = ContextManager::new_for_test(
            "task-g",
            temp_memory_path(),
            ContextCompressor::default(),
        );
        let messages = vec![
            context_message(ContextRole::User, "请实现 usage 成本统计", true),
            context_message(
                ContextRole::Assistant,
                "收到，我会按你这版方案直接落地。接下来我将分两步推进，第一步先提交第一批补丁。",
                true,
            ),
            context_message(ContextRole::User, "继续", true),
        ];

        let result = manager.build_history("继续", &messages, ContextModelLimits::default());
        assert_eq!(result.history.len(), 1, "应剔除仅承诺型助手消息");
        assert_eq!(result.history[0], RigMessage::user("请实现 usage 成本统计"));
    }
}
