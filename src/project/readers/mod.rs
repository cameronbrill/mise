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

/// Expand a list of workspace glob patterns relative to `monorepo_root`,
/// returning every matched directory that contains a `package.json`.
/// Used by the npm and pnpm readers — both share the identical
/// expand-then-canonicalize-then-confine logic.
///
/// `reader_id` is used only for warn-log context. Skips negate (`!`)
/// patterns; rejects unsafe (absolute / `..`) patterns; canonicalizes
/// each match and discards anything that escapes the monorepo root.
pub(crate) fn expand_workspace_globs(
    monorepo_root: &Path,
    patterns: &[String],
    reader_id: &str,
) -> Result<Vec<PathBuf>> {
    let canonical_root = monorepo_root
        .canonicalize()
        .unwrap_or_else(|_| monorepo_root.to_path_buf());
    let mut out = Vec::new();
    for pat in patterns {
        if pat.starts_with('!') {
            continue; // exclusion patterns not modeled yet
        }
        if !is_safe_glob_pattern(pat) {
            warn!("project reader '{reader_id}': rejecting unsafe workspace glob {pat:?}");
            continue;
        }
        let abs = monorepo_root.join(pat);
        for entry in glob::glob(abs.to_string_lossy().as_ref())? {
            let p = match entry {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        "project reader '{reader_id}': skipping unreadable workspace member: {e}"
                    );
                    continue;
                }
            };
            if !p.is_dir() || !p.join("package.json").is_file() {
                continue;
            }
            let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
            if !canonical.starts_with(&canonical_root) {
                warn!(
                    "project reader '{reader_id}': rejecting workspace member {} outside monorepo root",
                    canonical.display()
                );
                continue;
            }
            out.push(p);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Drop workspace members that don't sit under any of `ctx.config_roots`.
/// When `config_roots` is unset, all members are kept (the deprecated
/// implicit-discovery default).
pub(crate) fn filter_members_by_config_roots(
    members: Vec<PathBuf>,
    ctx: &ReaderContext,
) -> Vec<PathBuf> {
    let Some(roots) = ctx.config_roots.as_ref() else {
        return members;
    };
    if roots.is_empty() {
        return members;
    }
    members
        .into_iter()
        .filter(|m| roots.iter().any(|r| m.starts_with(r) || m == r))
        .collect()
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

    #[test]
    fn expand_workspace_globs_rejects_absolute_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = vec!["/etc/*".to_string()];
        let result = expand_workspace_globs(tmp.path(), &bad, "test").unwrap();
        assert!(result.is_empty(), "absolute glob should be rejected");
    }

    #[test]
    fn expand_workspace_globs_rejects_parent_dir_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = vec!["../../etc/*".to_string()];
        let result = expand_workspace_globs(tmp.path(), &bad, "test").unwrap();
        assert!(result.is_empty(), "parent-dir glob should be rejected");
    }

    #[test]
    fn expand_workspace_globs_finds_workspace_members() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("packages/foo")).unwrap();
        std::fs::write(
            tmp.path().join("packages/foo/package.json"),
            r#"{"name":"foo"}"#,
        )
        .unwrap();
        let patterns = vec!["packages/*".to_string()];
        let members = expand_workspace_globs(tmp.path(), &patterns, "test").unwrap();
        assert_eq!(members.len(), 1);
        assert!(members[0].ends_with("packages/foo"));
    }

    #[test]
    fn filter_members_by_config_roots_passes_through_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ReaderContext::new(tmp.path().to_path_buf(), None);
        let members = vec![tmp.path().join("a"), tmp.path().join("b")];
        assert_eq!(filter_members_by_config_roots(members.clone(), &ctx), members);
    }

    #[test]
    fn filter_members_by_config_roots_drops_outside_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ReaderContext::new(
            tmp.path().to_path_buf(),
            Some(vec![tmp.path().join("apps")]),
        );
        let members = vec![
            tmp.path().join("apps/web"),
            tmp.path().join("vendor/legacy"),
        ];
        let kept = filter_members_by_config_roots(members, &ctx);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].ends_with("apps/web"));
    }
}
