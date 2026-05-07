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
///
/// Walk errors are logged at `warn!` and skipped instead of being silently
/// swallowed (F-8 from review).
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
            Err(e) => {
                warn!("project walk: skipping unreadable entry: {e}");
                continue;
            }
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
/// `task.monorepo_depth` default; the builder may override via config.
pub(crate) const DEFAULT_WALK_DEPTH: usize = 8;

/// Read a file, distinguishing `NotFound` (legitimate skip — return
/// `Ok(None)`) from other errors (warn and skip). Without this, every
/// reader's `Err(_) => Ok(None)` masks permission errors and races,
/// silently under-reporting projects in CI gating. (F-3)
pub(crate) fn read_optional_file(path: &Path, reader_id: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(body) => Some(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!(
                "project reader '{reader_id}': could not read {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Reject globs that escape the monorepo root: absolute paths and
/// patterns containing `..` components. Without this, a malicious
/// `pnpm-workspace.yaml` or root `package.json` can enumerate paths
/// outside the monorepo. (F-14)
pub(crate) fn is_safe_glob_pattern(pat: &str) -> bool {
    if pat.is_empty() {
        return false;
    }
    let p = Path::new(pat);
    if p.is_absolute() {
        return false;
    }
    !p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}
