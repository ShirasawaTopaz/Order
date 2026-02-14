use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::encoding::{read_utf8_text_with_report, write_utf8_text_with_report};
use crate::observability::{AgentEvent, log_event_best_effort, ts, workspace_root_best_effort};

/// 自动验证配置（可选），读取自 `.order/validation.toml`。
///
/// 之所以允许覆盖：
/// - 不同项目的“最小验证”差异很大；
/// - 让用户可以把最常用的验证命令固化下来，减少反复手动输入。
#[derive(Debug, Clone, Default, Deserialize)]
struct ValidationConfig {
    minimal: Option<Vec<String>>,
    extended: Option<Vec<String>>,
}

/// 单条验证命令的执行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReport {
    pub command: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

/// 验证阶段的执行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageReport {
    pub name: String,
    pub commands: Vec<CommandReport>,
}

/// 验证总报告（写入 `.order/reports/<trace_id>/validation.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub trace_id: String,
    pub started_at_unix_ms: u128,
    pub duration_ms: u128,
    pub ok: bool,
    pub stages: Vec<StageReport>,
    pub failed_command: Option<String>,
    /// 给用户的下一步建议（尽量可执行/可回退）。
    pub suggestion: Option<String>,
}

/// 自动验证管线。
#[derive(Debug, Default, Clone)]
pub struct ValidationPipeline;

impl ValidationPipeline {
    /// 基于改动文件执行“最小验证 -> 扩展验证”，并写入报告。
    pub fn run(&self, trace_id: &str, changed_files: &[String]) -> Result<ValidationReport> {
        let workspace_root = workspace_root_best_effort();
        let started_at = unix_ms();
        let start_clock = Instant::now();

        let config = load_validation_config(&workspace_root).unwrap_or_default();
        let minimal_commands = config
            .minimal
            .unwrap_or_else(|| default_minimal_commands(changed_files));
        let extended_commands = config
            .extended
            .unwrap_or_else(|| vec!["cargo check --workspace".to_string()]);

        let commands_for_event = minimal_commands
            .iter()
            .chain(extended_commands.iter())
            .cloned()
            .collect::<Vec<_>>();
        log_event_best_effort(
            &workspace_root,
            AgentEvent::ValidationStart {
                ts: ts(),
                trace_id: trace_id.to_string(),
                commands: commands_for_event.clone(),
            },
        );

        let mut stages = Vec::new();
        let mut failed_command: Option<String> = None;

        let minimal_stage =
            run_stage(&workspace_root, "minimal", &minimal_commands).context("最小验证执行失败")?;
        let minimal_ok = minimal_stage.commands.iter().all(|command| command.ok);
        if !minimal_ok {
            failed_command = minimal_stage
                .commands
                .iter()
                .find(|command| !command.ok)
                .map(|command| command.command.clone());
        }
        stages.push(minimal_stage);

        if minimal_ok {
            let extended_stage = run_stage(&workspace_root, "extended", &extended_commands)
                .context("扩展验证执行失败")?;
            let extended_ok = extended_stage.commands.iter().all(|command| command.ok);
            if !extended_ok {
                failed_command = extended_stage
                    .commands
                    .iter()
                    .find(|command| !command.ok)
                    .map(|command| command.command.clone());
            }
            stages.push(extended_stage);
        }

        let ok = failed_command.is_none();
        let duration_ms = start_clock.elapsed().as_millis();

        log_event_best_effort(
            &workspace_root,
            AgentEvent::ValidationEnd {
                ts: ts(),
                trace_id: trace_id.to_string(),
                ok,
                duration_ms,
                failed_command: failed_command.clone(),
            },
        );

        let suggestion = if let Some(ref cmd) = failed_command {
            Some(format!(
                "验证失败：可直接复制执行复现命令：`{}`；如需快速回退请使用 `/rollback {}`",
                cmd, trace_id
            ))
        } else {
            Some("验证通过".to_string())
        };

        let report = ValidationReport {
            trace_id: trace_id.to_string(),
            started_at_unix_ms: started_at,
            duration_ms,
            ok,
            stages,
            failed_command,
            suggestion,
        };

        write_report(&workspace_root, trace_id, &report)?;
        Ok(report)
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn load_validation_config(workspace_root: &Path) -> Result<ValidationConfig> {
    let path = workspace_root.join(".order").join("validation.toml");
    if !path.exists() {
        return Ok(ValidationConfig::default());
    }
    let (text, report) = read_utf8_text_with_report(&path)
        .with_context(|| format!("读取验证配置失败: {}", path.display()))?;
    if report.has_warning() {
        for warning in report.warnings_for(&path) {
            eprintln!("validation config encoding warning: {warning}");
        }
    }
    if text.trim().is_empty() {
        return Ok(ValidationConfig::default());
    }
    toml::from_str(&text).with_context(|| format!("解析验证配置失败: {}", path.display()))
}

fn default_minimal_commands(changed_files: &[String]) -> Vec<String> {
    let mut crates = BTreeSet::new();
    for path in changed_files {
        if let Some(crate_name) = crate_name_from_path(path) {
            crates.insert(crate_name);
        }
    }

    if crates.is_empty() {
        // 兜底：无法归因到某个 crate 时，至少跑一次 workspace 级别的测试。
        return vec!["cargo test --workspace".to_string()];
    }

    crates
        .into_iter()
        .map(|crate_name| format!("cargo test -p {}", crate_name))
        .collect()
}

fn crate_name_from_path(path: &str) -> Option<String> {
    // 约定：`crates/<crate>/...`
    let normalized = path.replace('\\', "/");
    let mut segments = normalized.split('/');
    if segments.next()? != "crates" {
        return None;
    }
    let name = segments.next()?;
    if name.trim().is_empty() {
        None
    } else {
        Some(name.trim().to_string())
    }
}

fn run_stage(workspace_root: &Path, name: &str, commands: &[String]) -> Result<StageReport> {
    let mut reports = Vec::new();
    for command_line in commands {
        let report = run_command(workspace_root, command_line)?;
        let ok = report.ok;
        reports.push(report);
        if !ok {
            // 阶段内遇到失败就立刻停止，减少无意义消耗。
            break;
        }
    }
    Ok(StageReport {
        name: name.to_string(),
        commands: reports,
    })
}

fn run_command(workspace_root: &Path, command_line: &str) -> Result<CommandReport> {
    let (program, args) = parse_command_line(command_line)
        .ok_or_else(|| anyhow!("命令解析失败: {}", command_line))?;

    // 安全策略：默认只允许执行 cargo（验证闭环常用，且可控）。
    // 如果未来需要放开更多命令，应在这里增加 allowlist/前缀规则。
    if program.to_ascii_lowercase() != "cargo" {
        return Err(anyhow!(
            "验证命令不在 allowlist 中（仅允许 cargo）: {}",
            program
        ));
    }

    let start = Instant::now();
    let output = Command::new(&program)
        .args(&args)
        .current_dir(workspace_root)
        .output()
        .with_context(|| format!("执行命令失败: {}", command_line))?;
    let duration_ms = start.elapsed().as_millis();

    let ok = output.status.success();
    let exit_code = output.status.code();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(CommandReport {
        command: command_line.trim().to_string(),
        ok,
        exit_code,
        duration_ms,
        stdout_tail: tail_text(&stdout, 4000),
        stderr_tail: tail_text(&stderr, 4000),
    })
}

fn parse_command_line(line: &str) -> Option<(String, Vec<String>)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 简易解析：
    // - 支持双引号包裹参数（例如 `--config \"a b\"`）；
    // - 不支持复杂转义，这里优先满足常见的 cargo 命令。
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in trimmed.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    args.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }

    if args.is_empty() {
        return None;
    }

    let program = args.remove(0);
    Some((program, args))
}

fn tail_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.trim().to_string();
    }
    let tail = text.chars().rev().take(max_chars).collect::<Vec<_>>();
    tail.into_iter()
        .rev()
        .collect::<String>()
        .trim()
        .to_string()
}

fn write_report(workspace_root: &Path, trace_id: &str, report: &ValidationReport) -> Result<()> {
    let dir = workspace_root.join(".order").join("reports").join(trace_id);
    fs::create_dir_all(&dir).with_context(|| format!("创建报告目录失败: {}", dir.display()))?;

    let path = dir.join("validation.json");
    let mut text = serde_json::to_string_pretty(report).context("序列化验证报告失败")?;
    text.push('\n');
    let report = write_utf8_text_with_report(&path, &text)
        .with_context(|| format!("写入验证报告失败: {}", path.display()))?;
    if report.has_warning() {
        for warning in report.warnings_for(&path) {
            eprintln!("validation report encoding warning: {warning}");
        }
    }
    Ok(())
}
