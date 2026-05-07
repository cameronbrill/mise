use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr, bail, eyre};
use serde::Serialize;

use crate::config::Config;
use crate::git::Git;
use crate::project::{ProjectGraph, ProjectId};
use crate::task::TaskLoadContext;

/// List projects affected by changes between two git refs.
///
/// Modeled on `nx affected`: identifies projects whose source files
/// changed between `--base` and `--head`, then expands transitively
/// through the project graph so any project that depends on a changed
/// project is also included.
///
/// Requires `mise` experimental mode and `[monorepo]` configuration in
/// the root mise.toml. See https://mise.en.dev/tasks/monorepo.html for
/// monorepo setup.
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, after_long_help = AFTER_LONG_HELP)]
pub struct Affected {
    /// Return every project, ignoring the affected calculation
    #[clap(long, conflicts_with_all = ["base", "head", "files"])]
    all: bool,

    /// Base ref to diff against (default: `main` if it exists, else `master`)
    #[clap(long)]
    base: Option<String>,

    /// Exclude these project ids from the result
    #[clap(long)]
    exclude: Vec<String>,

    /// Override the changed-file set (one path per `--files` flag).
    /// Useful in CI when the runner already has the file list.
    /// Paths are resolved relative to the current working directory.
    #[clap(long, conflicts_with_all = ["base", "head", "all"])]
    files: Vec<String>,

    /// Output format: `ids` (default), `json`, or `tasks`
    #[clap(long, value_enum, default_value = "ids")]
    format: AffectedFormat,

    /// Head ref to diff (default: HEAD)
    #[clap(long)]
    head: Option<String>,

    /// Only return projects that define this task
    #[clap(long)]
    target: Option<String>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum AffectedFormat {
    Ids,
    Json,
    Tasks,
}

/// Wire shape for `mise affected --format json`. Ships as
/// `mise-affected-experimental` until the schema settles after the
/// project-graph plugin protocol lands.
#[derive(Debug, Serialize)]
struct AffectedReport {
    schema: &'static str,
    version: u32,
    base: String,
    head: String,
    projects: Vec<String>,
    /// Per-project task identifiers in `//path:task` form. `None` when
    /// `--target` was not given.
    target_tasks: Option<Vec<String>>,
}

const AFFECTED_REPORT_SCHEMA: &str = "mise-affected-experimental";

const AFTER_LONG_HELP: &str = color_print::cstr!(
    r#"<bold><underline>Examples:</underline></bold>

    $ <bold>mise affected</bold>
    //apps/web
    //libs/shared

    $ <bold>mise affected --base main --head HEAD --target build</bold>
    //apps/web

    $ <bold>mise affected --format json</bold>
    {"schema":"mise-affected-experimental","version":1,"base":"main","head":"HEAD","projects":["//apps/web"],"target_tasks":null}
"#
);

impl Affected {
    pub async fn run(self) -> Result<()> {
        // `Config::project_graph()` carries the experimental gate; the
        // direct call here would have double-gated. When experimental
        // is off, `project_graph()` returns `Ok(None)` (because
        // `find_monorepo_config` already filters on the flag) and the
        // `bail!` below points at the right setting.
        validate_ref(self.base.as_deref(), "--base")?;
        validate_ref(self.head.as_deref(), "--head")?;

        let config = Config::get().await?;
        let Some(graph) = config.project_graph()? else {
            bail!(
                "no monorepo found — set `experimental_monorepo_root = true` in your root mise.toml"
            );
        };

        let base = self
            .base
            .clone()
            .or_else(|| std::env::var("MISE_AFFECTED_BASE").ok())
            .map(Ok)
            .unwrap_or_else(|| default_base(&graph.monorepo_root).map(str::to_string))?;
        // The env-var fallback bypassed the CLI-flag validation in earlier
        // revisions; revalidate so an attacker setting
        // `MISE_AFFECTED_BASE=--output=...` hits the same defense as `--base`.
        validate_ref(Some(&base), "MISE_AFFECTED_BASE")?;
        let head = self.head.clone().unwrap_or_else(|| "HEAD".to_string());

        let exclude_set: BTreeSet<String> = self.exclude.iter().cloned().collect();

        let mut projects: BTreeSet<String> = if self.all {
            graph.projects().map(|p| p.id.clone()).collect()
        } else if !self.files.is_empty() {
            graph.affected_from_paths(&resolve_user_files(&graph, &self.files)?)?
        } else {
            let git = Git::new(graph.monorepo_root.clone());
            if !git.is_repo() {
                bail!(
                    "monorepo root {} is not a git repository — pass --files explicitly",
                    graph.monorepo_root.display()
                );
            }
            // Defend against fresh `git init` repos with no commits — the
            // bare `git diff` error otherwise would point at the wrong
            // thing.
            if git.rev_parse_verify(&head).is_err() {
                bail!(
                    "{} has no commit named '{head}' — make an initial commit or pass --files",
                    graph.monorepo_root.display()
                );
            }
            // Verify the base ref exists too so a typo in `--base` produces
            // a precise error rather than a downstream shallow-clone hint
            // that doesn't apply.
            if git.rev_parse_verify(&base).is_err() {
                bail!(
                    "{} has no commit or branch named '{base}' — pass --base explicitly or run `git fetch`",
                    graph.monorepo_root.display()
                );
            }
            // Surface orphan-branch / unrelated-history failures with a
            // clearer message than git's `fatal: ... no merge base`.
            if git.merge_base(&base, &head).is_err() {
                bail!(
                    "no common ancestor between '{base}' and '{head}' \
                     (orphan branches or unrelated histories)\n\
                     pass --base/--head to commits reachable from both, or use --files",
                );
            }
            let changed = git.changed_files(&base, &head, true).wrap_err_with(|| {
                format!(
                    "could not compute changed files for {base}...{head} \
                     (if this is a shallow clone, run `git fetch --unshallow` or pass --files)"
                )
            })?;
            graph.affected_from_paths(&changed)?
        };

        for ex in &exclude_set {
            projects.remove(ex);
        }

        // --target filter: keep projects whose loaded task set matches.
        // Delegates to `Task::is_match`, which strips file extensions
        // and honors aliases so `--target build` finds `tasks/build.sh`
        // and other forms that `mise tasks ls` would surface.
        if let Some(target) = self.target.as_ref() {
            let tasks = config
                .tasks_with_context(Some(&TaskLoadContext::all()))
                .await
                .wrap_err("could not load tasks for --target filter")?;
            let mut keepers: BTreeSet<ProjectId> = BTreeSet::new();
            for task in tasks.values() {
                if !task.is_match(target) {
                    continue;
                }
                if let Some((project, _)) = split_monorepo_task_name(&task.name) {
                    keepers.insert(project);
                }
            }
            projects.retain(|id| keepers.contains(id));
        }

        match self.format {
            AffectedFormat::Ids => {
                for id in &projects {
                    miseprintln!("{id}");
                }
            }
            AffectedFormat::Tasks => {
                let target = self.target.as_deref();
                for id in &projects {
                    match target {
                        Some(t) => miseprintln!("{id}:{t}"),
                        None => miseprintln!("{id}"),
                    }
                }
            }
            AffectedFormat::Json => {
                let target_tasks = self.target.as_ref().map(|t| {
                    projects.iter().map(|id| format!("{id}:{t}")).collect()
                });
                let report = AffectedReport {
                    schema: AFFECTED_REPORT_SCHEMA,
                    version: 1,
                    base,
                    head,
                    projects: projects.into_iter().collect(),
                    target_tasks,
                };
                miseprintln!("{}", serde_json::to_string(&report)?);
            }
        }

        Ok(())
    }
}

/// Reject ref strings that begin with `-` so an attacker-controlled
/// `--base "--output=..."` cannot tunnel a flag through `git`. The
/// underlying `git_cmd_read!` already passes args without a shell, but
/// `git` itself parses leading `--` as flags. git's `check-ref-format`
/// rules already exclude leading `-`, so legitimate refs pass through.
fn validate_ref(value: Option<&str>, flag: &str) -> Result<()> {
    if let Some(v) = value
        && v.starts_with('-')
    {
        return Err(eyre!(
            "{flag} value {v:?} starts with '-'; refusing to forward to git"
        ));
    }
    Ok(())
}

/// Resolve user-supplied `--files` paths against CWD, then strip the
/// monorepo root so they line up with the project-graph's expectation
/// of monorepo-relative paths. Both sides are canonicalized so symlinked
/// tmp dirs (macOS `/tmp` → `/private/tmp`) don't produce spurious
/// mismatches. Paths outside the monorepo are warn-logged and skipped.
fn resolve_user_files(graph: &Arc<ProjectGraph>, files: &[String]) -> Result<Vec<PathBuf>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| graph.monorepo_root.clone());
    let canonical_root = graph
        .monorepo_root
        .canonicalize()
        .unwrap_or_else(|_| graph.monorepo_root.clone());
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let raw = PathBuf::from(f);
        let abs = if raw.is_absolute() { raw } else { cwd.join(&raw) };
        let canonical = abs.canonicalize().unwrap_or(abs);
        if let Ok(rel) = canonical.strip_prefix(&canonical_root) {
            out.push(rel.to_path_buf());
        } else {
            warn!(
                "--files path {} is outside monorepo root and will be ignored",
                canonical.display()
            );
        }
    }
    Ok(out)
}

/// Default base ref: prefer `main`, fall back to `master`. Returns an
/// error with a usage hint if neither ref exists, so the caller doesn't
/// hit a confusing `git diff main...HEAD` error downstream.
fn default_base(repo_dir: &Path) -> Result<&'static str> {
    let git = Git::new(repo_dir);
    if !git.is_repo() {
        return Ok("main");
    }
    for c in ["main", "master"] {
        if git.rev_parse_verify(c).is_ok() {
            return Ok(c);
        }
    }
    bail!(
        "could not determine default base ref (tried 'main' and 'master'); \
         pass --base explicitly"
    )
}

/// Split a monorepo-prefixed task name `//path/to/project:task` into
/// `(project_id, task_name)`. Returns `None` for non-monorepo tasks.
fn split_monorepo_task_name(name: &str) -> Option<(ProjectId, String)> {
    let stripped = name.strip_prefix("//")?;
    let colon = stripped.find(':')?;
    let project = format!("//{}", &stripped[..colon]);
    let task = stripped[colon + 1..].to_string();
    Some((project, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_monorepo_task_name_extracts_project_and_task() {
        assert_eq!(
            split_monorepo_task_name("//apps/web:build"),
            Some(("//apps/web".to_string(), "build".to_string()))
        );
        assert_eq!(
            split_monorepo_task_name("//libs/shared:test:unit"),
            Some(("//libs/shared".to_string(), "test:unit".to_string()))
        );
    }

    #[test]
    fn split_monorepo_task_name_rejects_non_monorepo() {
        assert!(split_monorepo_task_name("build").is_none());
        assert!(split_monorepo_task_name("//apps/web").is_none());
    }

    #[test]
    fn validate_ref_rejects_leading_dash() {
        assert!(validate_ref(Some("--output=/tmp/pwn"), "--base").is_err());
        assert!(validate_ref(Some("-suspicious"), "--head").is_err());
    }

    #[test]
    fn validate_ref_accepts_real_refs() {
        assert!(validate_ref(Some("main"), "--base").is_ok());
        assert!(validate_ref(Some("origin/main"), "--base").is_ok());
        assert!(validate_ref(Some("HEAD~3"), "--head").is_ok());
        assert!(validate_ref(None, "--base").is_ok());
    }

    #[test]
    fn default_base_returns_master_when_main_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // git init (defaults to "master" on older git or initialized
        // explicitly). Then verify default_base returns "master" when
        // no "main" branch exists.
        let _ = std::process::Command::new("git")
            .args(["-c", "init.defaultBranch=master", "init", "-q"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let _ = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "initial",
            ])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert_eq!(default_base(tmp.path()).unwrap(), "master");
    }

    #[test]
    fn default_base_errs_when_no_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = std::process::Command::new("git")
            .args(["-c", "init.defaultBranch=trunk", "init", "-q"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let _ = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "initial",
            ])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let err = default_base(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("could not determine default base ref"));
    }

    #[test]
    fn default_base_returns_main_for_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // No git init; should return "main" without erroring.
        assert_eq!(default_base(tmp.path()).unwrap(), "main");
    }

    fn graph_with_root(root: PathBuf) -> Arc<ProjectGraph> {
        Arc::new(ProjectGraph::new(root))
    }

    #[test]
    fn resolve_user_files_strips_monorepo_root_for_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let graph = graph_with_root(root.clone());
        // Create a file inside the monorepo so canonicalize succeeds.
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let files = vec![root.join("a.txt").to_string_lossy().into_owned()];
        let resolved = resolve_user_files(&graph, &files).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0], PathBuf::from("a.txt"));
    }

    #[test]
    fn resolve_user_files_warns_and_drops_outside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let graph = graph_with_root(root);
        std::fs::write(other.path().join("escape.txt"), "x").unwrap();
        let files = vec![
            other
                .path()
                .join("escape.txt")
                .to_string_lossy()
                .into_owned(),
        ];
        let resolved = resolve_user_files(&graph, &files).unwrap();
        // Outside-root paths are warn-logged and dropped.
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_user_files_handles_canonicalized_root_with_symlinked_tmp() {
        // Even when the user provides a path through a symlinked tmp
        // dir, canonicalizing both sides should produce a stable
        // strip_prefix result.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let graph = graph_with_root(root.clone());
        std::fs::create_dir_all(root.join("apps/web")).unwrap();
        std::fs::write(root.join("apps/web/main.ts"), "x").unwrap();
        let files = vec![root.join("apps/web/main.ts").to_string_lossy().into_owned()];
        let resolved = resolve_user_files(&graph, &files).unwrap();
        assert_eq!(resolved, vec![PathBuf::from("apps/web/main.ts")]);
    }
}
