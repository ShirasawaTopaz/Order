use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use super::{MAX_TREE_ENTRIES, types::TreeEntry, utils::file_name_or};

// 根据展开状态收集目录树节点。
pub(super) fn collect_tree_entries(
    root: &Path,
    expanded_dirs: &BTreeSet<PathBuf>,
) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    collect_tree_entries_recursive(root, 0, expanded_dirs, &mut entries);
    entries
}

// 递归构建目录树。
fn collect_tree_entries_recursive(
    path: &Path,
    depth: usize,
    expanded_dirs: &BTreeSet<PathBuf>,
    output: &mut Vec<TreeEntry>,
) {
    if output.len() >= MAX_TREE_ENTRIES {
        return;
    }

    let read_dir = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        let entry_path = entry.path();
        let is_dir = entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false);
        let name = file_name_or(entry_path.as_path(), "").to_string();
        if name.is_empty() {
            continue;
        }
        entries.push((entry_path, is_dir, name));
    }

    entries.sort_by(|left, right| match (left.1, right.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => left.2.cmp(&right.2),
    });

    for (entry_path, is_dir, name) in entries {
        if output.len() >= MAX_TREE_ENTRIES {
            break;
        }

        output.push(TreeEntry {
            path: entry_path.clone(),
            depth,
            is_dir,
            name,
        });

        if is_dir && expanded_dirs.contains(&entry_path) {
            collect_tree_entries_recursive(entry_path.as_path(), depth + 1, expanded_dirs, output);
        }
    }
}
