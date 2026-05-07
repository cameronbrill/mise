use std::path::{Path, PathBuf};

use eyre::Result;

pub mod mise_reader;
pub mod npm_reader;
pub mod nx_reader;
pub mod pnpm_reader;
pub mod turbo_reader;

/// Walk a monorepo root collecting directories that contain at least one of
/// `markers`. Used by every reader to find its files. Respects `.gitignore`,
/// skips hidden directories, and bounds depth to a sensible max.
pub(crate) fn walk_for_markers(
    monorepo_root: &Path,
    markers: &[&str],
    max_depth: usize,
) -> Result<Vec<PathBuf>> {
    let mut hits = Vec::new();
    let walker = ignore::WalkBuilder::new(monorepo_root)
        .max_depth(Some(max_depth))
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && markers.contains(&name)
            && let Some(parent) = path.parent()
        {
            hits.push(parent.to_path_buf());
        }
    }
    hits.sort();
    hits.dedup();
    Ok(hits)
}

/// Default depth cap for monorepo walks. Matches mise's existing
/// `monorepo_depth` default loosely (we don't depend on Settings here so
/// readers stay lean).
pub(crate) const DEFAULT_WALK_DEPTH: usize = 8;
