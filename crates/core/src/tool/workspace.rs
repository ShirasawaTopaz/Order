use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

/// 读取文件的最大字节数上限（默认 512 KiB）。
///
/// 这样做是为了避免模型误读大文件导致：
/// - 运行时内存/延迟飙升；
/// - 上下文被无意义内容淹没，影响对话质量。
pub const MAX_READ_BYTES: u64 = 512 * 1024;

/// 写入文件的最大字节数上限（默认 1 MiB）。
///
/// 这样做是为了避免一次性写入过大内容造成卡顿或误写。
pub const MAX_WRITE_BYTES: usize = 1024 * 1024;

/// 搜索单个文件时允许读取的最大字节数上限（默认 1 MiB）。
///
/// 搜索往往会遍历多个文件，因此单文件上限略高但仍需限制。
pub const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;

/// 搜索命中的最大条数（默认 200）。
///
/// 这能避免结果过多导致模型与 UI 处理压力过大。
pub const MAX_SEARCH_RESULTS: usize = 200;

/// 递归搜索的最大文件数（默认 2000）。
///
/// 这是一个安全阈值：当目录过大时，鼓励用户缩小搜索范围。
pub const MAX_SEARCH_FILES: usize = 2000;

/// 获取当前工作区根目录。
///
/// 这里选择使用“进程启动目录”作为工作区根目录：
/// - 对 CLI/TUI 工具来说最直观：通常从仓库根目录启动；
/// - 不额外引入配置依赖，保持默认可用。
pub fn workspace_root() -> Result<PathBuf, String> {
    env::current_dir().map_err(|error| format!("获取当前工作目录失败: {error}"))
}

/// 将用户传入的路径解析为“工作区内”的绝对路径。
///
/// 安全策略（核心目的：避免模型读写工作区以外的文件）：
/// - 只允许相对路径；
/// - 禁止出现盘符/UNC/根路径等绝对语义；
/// - 处理 `.` / `..`，并在 `..` 试图越界时直接拒绝。
pub fn resolve_workspace_relative_path(root: &Path, user_path: &str) -> Result<PathBuf, String> {
    let trimmed = user_path.trim();
    if trimmed.is_empty() {
        return Err("path 不能为空".to_string());
    }

    let rel = Path::new(trimmed);

    // Windows 上存在 `C:foo` 这类“带盘符但非绝对路径”的写法，
    // 也可能出现 `\\server\\share` 等 UNC 路径；这些都必须拒绝，
    // 否则模型可以绕过工作区限制访问任意位置。
    if rel
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return Err(
            "不允许使用绝对路径、盘符路径或 UNC 路径；请传入工作区内的相对路径".to_string(),
        );
    }

    let mut resolved = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 当解析回退到根目录时仍出现 `..`，说明企图越界。
                if resolved == root {
                    return Err("路径越界：不允许使用 `..` 访问工作区之外".to_string());
                }
                resolved.pop();
            }
            Component::Normal(segment) => resolved.push(segment),
            Component::Prefix(_) | Component::RootDir => {
                // 上面已整体拦截；这里保留分支用于兜底。
                return Err("不允许使用绝对路径语义".to_string());
            }
        }
    }

    Ok(resolved)
}

/// 校验从工作区根目录到目标路径的“已存在节点”是否包含符号链接。
///
/// 这样做是为了防止通过工作区内的符号链接跳转到工作区外部文件系统。
pub fn ensure_no_symlink_in_existing_path(root: &Path, resolved: &Path) -> Result<(), String> {
    let relative = resolved
        .strip_prefix(root)
        .map_err(|_| format!("路径不在工作区内: {}", resolved.display()))?;

    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            // `resolved` 已经是解析后的绝对路径，此处理论上只会出现 Normal。
            continue;
        };

        cursor.push(segment);
        if !cursor.exists() {
            continue;
        }

        let metadata = fs::symlink_metadata(&cursor)
            .map_err(|error| format!("读取路径元信息失败: {} ({error})", cursor.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "检测到符号链接，已拒绝访问以避免越权: {}",
                cursor.display()
            ));
        }
    }

    Ok(())
}
