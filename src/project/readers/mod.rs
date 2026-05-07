use std::path::{Path, PathBuf};

use eyre::Result;

use crate::project::reader::ReaderContext;

pub mod mise_reader;
pub mod npm_reader;
pub mod nx_reader;
pub mod pnpm_reader;
pub mod turbo_reader;

/// Walk a monorepo collecting directories that contain at least one of
/// `markers`. When `ctx.config_roots` is set, the walk is restricted to
/// those subtrees so the project graph mirrors mise's task-discovery
/// boundary. Otherwise the entire monorepo is walked (the deprecated
/// implicit-discovery default).
///
/// Respects `.gitignore` and bounds depth to a sensible max.
/// `.hidden(false)` so dot-prefixed marker files like `.mise.toml` are
/// visible. Walk errors are logged at `warn!` and skipped.
pub(crate) fn walk_for_markers(
    ctx: &ReaderContext,
    markers: &[&str],
    max_depth: usize,
) -> Result<Vec<PathBuf>> {
    let starts: Vec<PathBuf> = match &ctx.config_roots {
        Some(roots) if !roots.is_empty() => roots.clone(),
        _ => vec![ctx.monorepo_root.clone()],
    };
    let mut hits = Vec::new();
    for start in &starts {
        let walker = ignore::WalkBuilder::new(start)
            .max_depth(Some(max_depth))
            .hidden(false)
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
/// silently under-reporting projects in CI gating.
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
/// outside the monorepo.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_for_markers_finds_dotfile_mise_toml() {
        // Regression: previous version used `.hidden(true)` which made the
        // walker skip every `.mise.toml` file even though the marker list
        // included it. Verifies the dotfile form is now reachable.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("apps/web");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".mise.toml"), "[project]\nname = \"web\"").unwrap();
        let ctx = ReaderContext::new(tmp.path().to_path_buf(), None);
        let hits = walk_for_markers(&ctx, &["mise.toml", ".mise.toml"], 8).unwrap();
        assert!(
            hits.contains(&dir),
            "expected walk to find apps/web (with .mise.toml); got {hits:?}"
        );
    }

    #[test]
    fn walk_for_markers_finds_plain_mise_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("libs/shared");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mise.toml"), "[project]\nname = \"shared\"").unwrap();
        let ctx = ReaderContext::new(tmp.path().to_path_buf(), None);
        let hits = walk_for_markers(&ctx, &["mise.toml", ".mise.toml"], 8).unwrap();
        assert!(hits.contains(&dir));
    }

    #[test]
    fn walk_for_markers_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("ignored")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(tmp.path().join("ignored/mise.toml"), "[project]").unwrap();
        std::fs::create_dir_all(tmp.path().join("kept")).unwrap();
        std::fs::write(tmp.path().join("kept/mise.toml"), "[project]").unwrap();
        let ctx = ReaderContext::new(tmp.path().to_path_buf(), None);
        let hits = walk_for_markers(&ctx, &["mise.toml"], 8).unwrap();
        assert!(hits.iter().any(|p| p.ends_with("kept")));
        assert!(
            !hits.iter().any(|p| p.ends_with("ignored")),
            "gitignored dir should not be walked: {hits:?}"
        );
    }

    #[test]
    fn walk_for_markers_restricts_to_config_roots() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("apps/web")).unwrap();
        std::fs::write(tmp.path().join("apps/web/mise.toml"), "[project]").unwrap();
        std::fs::create_dir_all(tmp.path().join("vendor/legacy")).unwrap();
        std::fs::write(tmp.path().join("vendor/legacy/mise.toml"), "[project]").unwrap();
        let ctx = ReaderContext::new(
            tmp.path().to_path_buf(),
            Some(vec![tmp.path().join("apps/web")]),
        );
        let hits = walk_for_markers(&ctx, &["mise.toml"], 8).unwrap();
        assert!(hits.iter().any(|p| p.ends_with("apps/web")));
        assert!(
            !hits.iter().any(|p| p.ends_with("vendor/legacy")),
            "config_roots restriction should hide vendor/legacy: {hits:?}"
        );
    }

    #[test]
    fn is_safe_glob_pattern_rejects_unsafe() {
        assert!(!is_safe_glob_pattern(""));
        assert!(!is_safe_glob_pattern("/etc/*"));
        assert!(!is_safe_glob_pattern("../../etc"));
        assert!(!is_safe_glob_pattern("apps/../../etc"));
        assert!(is_safe_glob_pattern("apps/*"));
        assert!(is_safe_glob_pattern("packages/**"));
    }
}
