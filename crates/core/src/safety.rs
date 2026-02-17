use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::tool::workspace::{
    ensure_no_symlink_in_existing_path, resolve_workspace_relative_path, workspace_root,
};

/// 风险分级。
///
/// 当前实现以“可回滚 + 需要确认”为核心目标：
/// - `ReadOnly`：无副作用操作（例如读文件、搜索）。
/// - `LowRisk`：轻微副作用（预留扩展）。
/// - `HighRisk`：会改变磁盘状态（写文件/执行命令），默认必须二次确认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    ReadOnly,
    LowRisk,
    HighRisk,
}

/// 写入操作的 diff 摘要（用于预览）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    /// 写入前文件是否存在。
    pub existed: bool,
    /// 写入前行数。
    pub old_lines: usize,
    /// 写入后行数。
    pub new_lines: usize,
    /// 估算新增行数。
    pub added_lines: usize,
    /// 估算删除行数。
    pub removed_lines: usize,
}

/// 待确认的写入操作记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingWrite {
    pub trace_id: String,
    /// 用户传入的相对路径（用于 UI 展示）。
    pub path: String,
    pub append: bool,
    /// 写入内容（已做 CRLF -> LF 规范化）。
    pub content: String,
    pub created_at_unix_ms: u128,
    pub diff: DiffSummary,
}

/// 申请写入后返回给 UI/模型的摘要信息。
#[derive(Debug, Clone)]
pub struct PendingWriteSummary {
    pub trace_id: String,
    pub path: String,
    pub append: bool,
    pub diff: DiffSummary,
}

/// 应用待确认写入后的结果。
#[derive(Debug, Clone)]
pub struct ApplyPendingResult {
    pub trace_id: String,
    /// 实际写入的文件列表（工作区相对路径）。
    pub files: Vec<String>,
    /// 是否保留了本次写入前快照。
    ///
    /// - `true`：快照仍在 `.order/snapshots/<trace_id>`；
    /// - `false`：快照已按策略清理。
    pub snapshot_retained: bool,
    /// 自动清理快照失败时的错误信息（仅作为提示，不影响写入成功结果）。
    pub snapshot_cleanup_error: Option<String>,
}

/// 回滚结果。
#[derive(Debug, Clone)]
pub struct RollbackResult {
    pub trace_id: String,
    /// 已回滚的文件列表（工作区相对路径）。
    pub files: Vec<String>,
}

static OP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 安全执行闸门：负责 staging/确认/快照/回滚。
#[derive(Debug, Default, Clone)]
pub struct ExecutionGuard;

impl ExecutionGuard {
    /// 当前是否启用“写入后保留快照”。
    ///
    /// 默认策略是“写入成功后自动删除 `.order/snapshots/<trace_id>` 中的代码副本”，
    /// 避免旧快照持续干扰检索与阅读；若需要保留以支持 `/rollback`，
    /// 可设置环境变量 `ORDER_KEEP_SNAPSHOTS=1`（或 true/yes/on）。
    pub fn keep_snapshots_enabled(&self) -> bool {
        keep_snapshots_enabled()
    }

    /// 当前仓库内：写文件视为高风险操作。
    ///
    /// 保守策略的原因：
    /// - 工具调用来自模型，容易在“误理解需求”时产生不可逆副作用；
    /// - 通过二次确认把风险显式暴露给用户，体验成本可接受但安全收益很大。
    pub fn classify_write(&self, _path: &Path, _append: bool, _content: &str) -> RiskLevel {
        RiskLevel::HighRisk
    }

    /// Stage 一次写入：生成 diff 摘要并写入 pending 目录，但不落盘修改目标文件。
    pub fn stage_write(
        &self,
        trace_id: &str,
        relative_path: &str,
        content: &str,
        append: bool,
    ) -> Result<PendingWriteSummary> {
        let root = workspace_root().map_err(|e| anyhow!(e))?;
        let resolved =
            resolve_workspace_relative_path(&root, relative_path).map_err(|e| anyhow!(e))?;
        ensure_no_symlink_in_existing_path(&root, &resolved).map_err(|e| anyhow!(e))?;

        // 统一将 CRLF 规范为 LF，减少跨平台 diff 噪音。
        let normalized_content = content.replace("\r\n", "\n");
        // 拦截明显未替换的占位内容，避免把整文件覆盖成 `<same>` 这类无效文本。
        validate_write_content(relative_path, &normalized_content, append)?;

        let risk = self.classify_write(&resolved, append, &normalized_content);
        if risk != RiskLevel::HighRisk {
            // 预留：未来可对低风险写入直接放行。
        }

        let diff = compute_diff_summary(&resolved, &normalized_content, append)?;
        let created_at_unix_ms = unix_ms();

        let record = PendingWrite {
            trace_id: trace_id.to_string(),
            path: relative_path.trim().to_string(),
            append,
            content: normalized_content,
            created_at_unix_ms,
            diff: diff.clone(),
        };

        let pending_path = pending_write_record_path(&root, trace_id, next_op_id());
        if let Some(parent) = pending_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建 pending 目录失败: {}", parent.display()))?;
        }

        // pretty JSON 便于用户在必要时手动检查 pending 内容。
        let mut text =
            serde_json::to_string_pretty(&record).context("序列化 pending write 失败")?;
        text.push('\n');
        fs::write(&pending_path, text)
            .with_context(|| format!("写入 pending write 失败: {}", pending_path.display()))?;

        Ok(PendingWriteSummary {
            trace_id: trace_id.to_string(),
            path: record.path,
            append,
            diff,
        })
    }

    /// 查询指定 trace_id 的待确认写入摘要。
    ///
    /// 返回空列表而非报错的原因：
    /// - TUI 会在每轮请求结束后探测是否有待确认写入；
    /// - “无待确认写入”属于正常路径，不应强迫调用方处理错误分支。
    pub fn list_pending_writes(&self, trace_id: &str) -> Result<Vec<PendingWriteSummary>> {
        let root = workspace_root().map_err(|e| anyhow!(e))?;
        let pending_dir = pending_trace_dir(&root, trace_id);
        if !pending_dir.exists() {
            return Ok(Vec::new());
        }

        let records = read_pending_write_records(&pending_dir)?;
        Ok(records
            .into_iter()
            .map(|record| PendingWriteSummary {
                trace_id: record.trace_id,
                path: record.path,
                append: record.append,
                diff: record.diff,
            })
            .collect())
    }

    /// 应用 trace_id 对应的所有 pending write：先建快照，再写入，然后清理 pending。
    pub fn apply_pending_writes(&self, trace_id: &str) -> Result<ApplyPendingResult> {
        let root = workspace_root().map_err(|e| anyhow!(e))?;
        let pending_dir = pending_trace_dir(&root, trace_id);
        if !pending_dir.exists() {
            return Err(anyhow!("未找到待确认写入记录: trace_id={trace_id}"));
        }

        let records = read_pending_write_records(&pending_dir)?;
        if records.is_empty() {
            return Err(anyhow!("待确认写入目录为空: {}", pending_dir.display()));
        }

        // 二次校验 pending 记录，防止历史脏数据或手工篡改绕过 stage 阶段检查。
        for record in &records {
            validate_write_content(&record.path, &record.content, record.append)?;
        }

        // 为避免“先写后快照”导致快照失效，先对所有目标文件建立快照。
        let snapshot_dir = snapshot_trace_dir(&root, trace_id);
        create_snapshot(&root, &snapshot_dir, &records)?;

        // 然后再执行写入。
        let mut touched_files = Vec::new();
        for record in &records {
            let resolved =
                resolve_workspace_relative_path(&root, &record.path).map_err(|e| anyhow!(e))?;
            ensure_no_symlink_in_existing_path(&root, &resolved).map_err(|e| anyhow!(e))?;

            if let Some(parent) = resolved.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录失败: {}", parent.display()))?;
            }

            if record.append {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&resolved)
                    .with_context(|| format!("追加写入失败: {}", resolved.display()))?;
                file.write_all(record.content.as_bytes())
                    .with_context(|| format!("写入失败: {}", resolved.display()))?;
                file.flush()
                    .with_context(|| format!("刷新写入失败: {}", resolved.display()))?;
            } else {
                fs::write(&resolved, record.content.as_bytes())
                    .with_context(|| format!("写入失败: {}", resolved.display()))?;
            }

            touched_files.push(record.path.clone());
        }

        // 清理 pending：写入已完成，不应保留旧请求以免重复应用。
        fs::remove_dir_all(&pending_dir)
            .with_context(|| format!("清理 pending 目录失败: {}", pending_dir.display()))?;

        // 按策略清理快照副本：默认清理，避免 `.order/snapshots` 长期堆积历史代码副本。
        // 若用户显式启用保留（ORDER_KEEP_SNAPSHOTS=1），则继续保留供 /rollback 使用。
        let (snapshot_retained, snapshot_cleanup_error) =
            cleanup_snapshot_after_apply(&snapshot_dir, self.keep_snapshots_enabled());

        // 对外只返回相对路径列表，避免泄露绝对路径细节。
        touched_files.sort();
        touched_files.dedup();

        Ok(ApplyPendingResult {
            trace_id: trace_id.to_string(),
            files: touched_files,
            snapshot_retained,
            snapshot_cleanup_error,
        })
    }

    /// 取消指定 trace_id 的 pending write（仅清理待确认记录，不影响磁盘文件）。
    pub fn reject_pending_writes(&self, trace_id: &str) -> Result<()> {
        let root = workspace_root().map_err(|e| anyhow!(e))?;
        let pending_dir = pending_trace_dir(&root, trace_id);
        if !pending_dir.exists() {
            return Err(anyhow!("未找到待确认写入记录: trace_id={trace_id}"));
        }
        fs::remove_dir_all(&pending_dir)
            .with_context(|| format!("清理 pending 目录失败: {}", pending_dir.display()))?;
        Ok(())
    }

    /// 回滚指定 trace_id 的快照。
    pub fn rollback(&self, trace_id: &str) -> Result<RollbackResult> {
        let root = workspace_root().map_err(|e| anyhow!(e))?;
        let snapshot_dir = snapshot_trace_dir(&root, trace_id);
        if !snapshot_dir.exists() {
            let hint = if self.keep_snapshots_enabled() {
                String::new()
            } else {
                "（当前默认写入后自动清理快照；如需保留请设置 ORDER_KEEP_SNAPSHOTS=1）".to_string()
            };
            return Err(anyhow!("未找到快照目录: trace_id={trace_id}{hint}"));
        }

        let manifest_path = snapshot_dir.join("manifest.json");
        let manifest_text = fs::read_to_string(&manifest_path)
            .with_context(|| format!("读取快照 manifest 失败: {}", manifest_path.display()))?;
        let manifest: SnapshotManifest = serde_json::from_str(&manifest_text)
            .with_context(|| format!("解析快照 manifest 失败: {}", manifest_path.display()))?;

        let mut rolled_back = Vec::new();
        for item in manifest.files {
            let resolved =
                resolve_workspace_relative_path(&root, &item.path).map_err(|e| anyhow!(e))?;
            ensure_no_symlink_in_existing_path(&root, &resolved).map_err(|e| anyhow!(e))?;

            if item.existed {
                let backup_path = snapshot_dir.join("files").join(&item.path);
                let backup = fs::read(&backup_path)
                    .with_context(|| format!("读取快照文件失败: {}", backup_path.display()))?;
                if let Some(parent) = resolved.parent()
                    && !parent.as_os_str().is_empty()
                {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("创建目录失败: {}", parent.display()))?;
                }
                fs::write(&resolved, backup)
                    .with_context(|| format!("回滚写入失败: {}", resolved.display()))?;
            } else {
                // 原本不存在的文件：回滚时删除，恢复到“未创建”状态。
                if resolved.exists() {
                    fs::remove_file(&resolved)
                        .with_context(|| format!("删除新建文件失败: {}", resolved.display()))?;
                }
            }

            rolled_back.push(item.path);
        }

        rolled_back.sort();
        rolled_back.dedup();
        Ok(RollbackResult {
            trace_id: trace_id.to_string(),
            files: rolled_back,
        })
    }

    /// 回滚最近一次快照（按目录修改时间倒序）。
    pub fn rollback_last(&self) -> Result<Option<RollbackResult>> {
        let root = workspace_root().map_err(|e| anyhow!(e))?;
        let snapshots_dir = root.join(".order").join("snapshots");
        if !snapshots_dir.exists() {
            return Ok(None);
        }

        let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        for entry in fs::read_dir(&snapshots_dir)
            .with_context(|| format!("读取 snapshots 目录失败: {}", snapshots_dir.display()))?
        {
            let entry = entry.with_context(|| "读取 snapshots 条目失败")?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let metadata = entry
                .metadata()
                .with_context(|| format!("读取快照目录元信息失败: {}", path.display()))?;
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((path, modified));
        }

        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        let Some((latest_dir, _)) = candidates.first() else {
            return Ok(None);
        };
        let trace_id = latest_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        if trace_id.is_empty() {
            return Ok(None);
        }

        Ok(Some(self.rollback(&trace_id)?))
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn next_op_id() -> String {
    let counter = OP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", unix_ms(), counter)
}

fn pending_trace_dir(workspace_root: &Path, trace_id: &str) -> PathBuf {
    workspace_root
        .join(".order")
        .join("pending")
        .join("writes")
        .join(trace_id)
}

fn pending_write_record_path(workspace_root: &Path, trace_id: &str, op_id: String) -> PathBuf {
    pending_trace_dir(workspace_root, trace_id).join(format!("{op_id}.json"))
}

fn snapshot_trace_dir(workspace_root: &Path, trace_id: &str) -> PathBuf {
    workspace_root
        .join(".order")
        .join("snapshots")
        .join(trace_id)
}

/// 当前进程是否启用“保留写入快照”。
fn keep_snapshots_enabled() -> bool {
    match env::var("ORDER_KEEP_SNAPSHOTS") {
        Ok(value) => parse_env_truthy(&value),
        Err(_) => false,
    }
}

/// 将环境变量文本解析为布尔值（真值集合）。
fn parse_env_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotManifest {
    trace_id: String,
    created_at_unix_ms: u128,
    files: Vec<SnapshotFileItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotFileItem {
    path: String,
    existed: bool,
}

fn read_pending_write_records(pending_dir: &Path) -> Result<Vec<PendingWrite>> {
    let mut records = Vec::new();
    for entry in fs::read_dir(pending_dir)
        .with_context(|| format!("读取 pending 目录失败: {}", pending_dir.display()))?
    {
        let entry = entry.with_context(|| "读取 pending 条目失败")?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("读取 pending write 失败: {}", path.display()))?;
        let record: PendingWrite = serde_json::from_str(&text)
            .with_context(|| format!("解析 pending write 失败: {}", path.display()))?;
        records.push(record);
    }

    // 按创建时间排序，保证应用顺序更接近模型的调用顺序。
    records.sort_by_key(|r| r.created_at_unix_ms);
    Ok(records)
}

fn create_snapshot(
    workspace_root: &Path,
    snapshot_dir: &Path,
    records: &[PendingWrite],
) -> Result<()> {
    if snapshot_dir.exists() {
        // 避免覆盖既有快照；同 trace_id 多次写入应使用新的 trace_id。
        return Err(anyhow!(
            "快照目录已存在，拒绝覆盖: {}",
            snapshot_dir.display()
        ));
    }

    let files_root = snapshot_dir.join("files");
    fs::create_dir_all(&files_root)
        .with_context(|| format!("创建快照目录失败: {}", files_root.display()))?;

    let mut manifest_files = Vec::new();
    for record in records {
        let resolved = resolve_workspace_relative_path(workspace_root, &record.path)
            .map_err(|e| anyhow!(e))?;
        ensure_no_symlink_in_existing_path(workspace_root, &resolved).map_err(|e| anyhow!(e))?;

        let existed = resolved.exists();
        if existed {
            let backup_path = files_root.join(&record.path);
            if let Some(parent) = backup_path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)
                    .with_context(|| format!("创建快照子目录失败: {}", parent.display()))?;
            }
            let bytes = fs::read(&resolved)
                .with_context(|| format!("读取待快照文件失败: {}", resolved.display()))?;
            fs::write(&backup_path, bytes)
                .with_context(|| format!("写入快照文件失败: {}", backup_path.display()))?;
        }

        manifest_files.push(SnapshotFileItem {
            path: record.path.clone(),
            existed,
        });
    }

    // 快照的文件列表要去重，避免同一文件多次写入导致 manifest 膨胀。
    manifest_files = dedup_snapshot_items(manifest_files);

    let manifest = SnapshotManifest {
        trace_id: snapshot_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string(),
        created_at_unix_ms: unix_ms(),
        files: manifest_files,
    };

    let mut text = serde_json::to_string_pretty(&manifest).context("序列化快照 manifest 失败")?;
    text.push('\n');
    let manifest_path = snapshot_dir.join("manifest.json");
    fs::write(&manifest_path, text)
        .with_context(|| format!("写入快照 manifest 失败: {}", manifest_path.display()))
}

/// 写入成功后的快照清理策略。
///
/// 返回 `(snapshot_retained, cleanup_error)`：
/// - `snapshot_retained=true`：快照仍保留（策略保留或清理失败）；
/// - `snapshot_retained=false`：快照已删除（或原本不存在）；
/// - `cleanup_error` 仅在“尝试删除失败”时给出说明。
fn cleanup_snapshot_after_apply(
    snapshot_dir: &Path,
    keep_snapshot: bool,
) -> (bool, Option<String>) {
    if keep_snapshot {
        return (true, None);
    }
    if !snapshot_dir.exists() {
        return (false, None);
    }

    match fs::remove_dir_all(snapshot_dir) {
        Ok(()) => (false, None),
        Err(error) => (
            true,
            Some(format!(
                "自动清理快照失败: {} ({error})",
                snapshot_dir.display()
            )),
        ),
    }
}

fn dedup_snapshot_items(items: Vec<SnapshotFileItem>) -> Vec<SnapshotFileItem> {
    let mut map: BTreeMap<String, SnapshotFileItem> = BTreeMap::new();
    for item in items {
        map.entry(item.path.clone()).or_insert(item);
    }
    map.into_values().collect()
}

fn compute_diff_summary(path: &Path, new_text: &str, append: bool) -> Result<DiffSummary> {
    let existed = path.exists();
    let old_text = if existed {
        fs::read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    };

    // 追加模式：diff 的“新文本”应基于 old + new。
    let final_text = if append && existed {
        format!("{}{}", old_text, new_text)
    } else if append {
        new_text.to_string()
    } else {
        new_text.to_string()
    };

    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = final_text.lines().collect();

    // 采用“行频次差”做近似统计，避免引入复杂 diff 算法/依赖。
    // 对用户来说，确认阶段最重要的是看清“变化规模”，而不是完全精确的逐行对齐。
    let (added, removed) = estimate_line_delta(&old_lines, &new_lines);

    Ok(DiffSummary {
        existed,
        old_lines: old_lines.len(),
        new_lines: new_lines.len(),
        added_lines: added,
        removed_lines: removed,
    })
}

fn estimate_line_delta(old_lines: &[&str], new_lines: &[&str]) -> (usize, usize) {
    let mut old_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for line in old_lines {
        *old_counts.entry(*line).or_insert(0) += 1;
    }

    let mut new_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for line in new_lines {
        *new_counts.entry(*line).or_insert(0) += 1;
    }

    let mut added = 0usize;
    let mut removed = 0usize;

    for (line, new_count) in &new_counts {
        let old_count = old_counts.get(line).copied().unwrap_or(0);
        if *new_count > old_count {
            added += *new_count - old_count;
        }
    }
    for (line, old_count) in &old_counts {
        let new_count = new_counts.get(line).copied().unwrap_or(0);
        if *old_count > new_count {
            removed += *old_count - new_count;
        }
    }

    (added, removed)
}

/// 校验写入内容是否包含明显未替换的占位符。
///
/// 只拦截“整文件替换且正文仅为 `<same>`”这一高风险场景：
/// - 该内容几乎不可能是用户真实意图；
/// - 一旦落盘会直接破坏源码可编译性；
/// - 仅对覆盖写入生效，避免误伤追加文本这类合法用例。
fn validate_write_content(path: &str, content: &str, append: bool) -> Result<()> {
    if append {
        return Ok(());
    }

    let trimmed = content.trim();
    if is_suspicious_placeholder_text(trimmed) {
        return Err(anyhow!(
            "检测到未替换占位符 `{}`，拒绝覆盖写入: {}",
            trimmed,
            path
        ));
    }

    Ok(())
}

/// 判断文本是否是“整文件占位符”。
///
/// 拦截目标是 `<same>`、`<omitted>`、`<redacted>` 这类模型占位输出，
/// 防止在审批通过后把真实源码覆盖成一行无效文本。
fn is_suspicious_placeholder_text(trimmed: &str) -> bool {
    if trimmed.len() < 3 || trimmed.len() > 80 {
        return false;
    }
    if !trimmed.starts_with('<') || !trimmed.ends_with('>') {
        return false;
    }

    let body = &trimmed[1..trimmed.len() - 1];
    if body.is_empty() || body.len() > 64 {
        return false;
    }
    // 仅拦截“单标签式占位符”，避免误伤带嵌套符号的真实内容。
    if body.contains('<') || body.contains('>') {
        return false;
    }

    let normalized = body.to_ascii_lowercase();
    [
        "same",
        "omitted",
        "placeholder",
        "redacted",
        "truncated",
        "elided",
        "unchanged",
    ]
    .iter()
    .any(|keyword| normalized.contains(keyword))
}

#[cfg(test)]
mod tests {
    use super::{
        cleanup_snapshot_after_apply, is_suspicious_placeholder_text, parse_env_truthy,
        validate_write_content,
    };
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn new_temp_snapshot_dir() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("order-snapshot-cleanup-{stamp}-{seq}"))
    }

    #[test]
    fn validate_write_content_should_reject_same_placeholder_on_replace() {
        let result = validate_write_content("crates/rander/src/history.rs", "  <same>\n", false);
        assert!(result.is_err());
    }

    #[test]
    fn validate_write_content_should_allow_normal_replace_content() {
        let result = validate_write_content(
            "crates/rander/src/history.rs",
            "use anyhow::{Context, Result};\n",
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_write_content_should_allow_append_even_when_text_matches_placeholder() {
        let result = validate_write_content("notes.txt", "<same>", true);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_write_content_should_reject_omitted_placeholder_on_replace() {
        let result = validate_write_content("crates/rander/src/history.rs", "\n<omitted>\n", false);
        assert!(result.is_err());
    }

    #[test]
    fn is_suspicious_placeholder_text_should_allow_normal_markup() {
        assert!(!is_suspicious_placeholder_text("<div>"));
        assert!(!is_suspicious_placeholder_text("<details>内容</details>"));
    }

    #[test]
    fn parse_env_truthy_should_match_expected_values() {
        assert!(parse_env_truthy("1"));
        assert!(parse_env_truthy("true"));
        assert!(parse_env_truthy(" YES "));
        assert!(parse_env_truthy("On"));
        assert!(!parse_env_truthy("0"));
        assert!(!parse_env_truthy("false"));
        assert!(!parse_env_truthy("off"));
        assert!(!parse_env_truthy("random"));
    }

    #[test]
    fn cleanup_snapshot_after_apply_should_remove_dir_when_keep_disabled() {
        let snapshot_dir = new_temp_snapshot_dir();
        let files_dir = snapshot_dir.join("files");
        fs::create_dir_all(&files_dir).expect("snapshot files dir should be created");
        fs::write(snapshot_dir.join("manifest.json"), "{}\n").expect("manifest should be created");
        fs::write(files_dir.join("history.rs"), "content")
            .expect("snapshot file should be created");

        let (retained, cleanup_error) = cleanup_snapshot_after_apply(&snapshot_dir, false);
        assert!(!retained);
        assert!(cleanup_error.is_none());
        assert!(!snapshot_dir.exists());
    }

    #[test]
    fn cleanup_snapshot_after_apply_should_keep_dir_when_keep_enabled() {
        let snapshot_dir = new_temp_snapshot_dir();
        let files_dir = snapshot_dir.join("files");
        fs::create_dir_all(&files_dir).expect("snapshot files dir should be created");

        let (retained, cleanup_error) = cleanup_snapshot_after_apply(&snapshot_dir, true);
        assert!(retained);
        assert!(cleanup_error.is_none());
        assert!(snapshot_dir.exists());

        fs::remove_dir_all(&snapshot_dir).expect("temp snapshot dir should be removed");
    }
}
