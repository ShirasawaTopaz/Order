use std::{
    collections::BTreeMap,
    fs,
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

        // 对外只返回相对路径列表，避免泄露绝对路径细节。
        touched_files.sort();
        touched_files.dedup();

        Ok(ApplyPendingResult {
            trace_id: trace_id.to_string(),
            files: touched_files,
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
            return Err(anyhow!("未找到快照目录: trace_id={trace_id}"));
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
