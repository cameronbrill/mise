use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, bail, eyre};
use serde::Serialize;

use crate::config::{Config, Settings};
use crate::git::Git;
use crate::project::{ProjectGraph, ProjectId};

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
        Settings::get().ensure_experimental("project-graph")?;

        validate_ref(self.base.as_deref(), "--base")?;
        validate_ref(self.head.as_deref(), "--head")?;

        let config = Config::get().await?;
        let Some(graph) = config.project_graph()? else {
            bail!(
                "no monorepo found — set `experimental_monorepo_root = true` in your root mise.toml"
            );
        };

        let base = match self.base.clone() {
            Some(b) => b,
            None => match std::env::var("MISE_AFFECTED_BASE") {
                Ok(b) => b,
                Err(_) => default_base(&graph.monorepo_root)?,
            },
        };
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
            // thing. (F-20)
            if git.rev_parse_verify(&head).is_err() {
                bail!(
                    "{} has no commit named '{head}' — make an initial commit or pass --files",
                    graph.monorepo_root.display()
                );
            }
            let changed = git.changed_files(&base, &head, true).map_err(|e| {
                e.wrap_err(format!(
                    "could not compute changed files for {base}...{head} \
                     (if this is a shallow clone, run `git fetch --unshallow` or pass --files)"
                ))
            })?;
            graph.affected_from_paths(&changed)?
        };

        for ex in &exclude_set {
            projects.remove(ex);
        }

        // --target filter: keep only projects that declare the named
        // task. We delegate to mise's existing task loader so file-based
        // tasks, included tasks, and templated tasks are all visible
        // (matches `mise tasks ls` semantics). (F-5)
        if let Some(target) = self.target.as_ref() {
            let task_index = build_task_index(&config).await?;
            projects.retain(|id| {
                task_index
                    .get(id)
                    .is_some_and(|tasks| tasks.contains(target))
            });
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
/// `git` itself parses leading `--` as flags (F-1). git's
/// `check-ref-format` rules already exclude leading `-`, so legitimate
/// refs pass through.
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
/// of monorepo-relative paths. Warns about paths outside the monorepo
/// instead of silently dropping them. (F-12)
fn resolve_user_files(graph: &Arc<ProjectGraph>, files: &[String]) -> Result<Vec<PathBuf>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| graph.monorepo_root.clone());
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let raw = PathBuf::from(f);
        let abs = if raw.is_absolute() { raw } else { cwd.join(&raw) };
        let canonical = abs.canonicalize().unwrap_or(abs);
        if let Ok(rel) = canonical.strip_prefix(&graph.monorepo_root) {
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
/// hit a confusing `git diff main...HEAD` error downstream. (F-10, F-3)
fn default_base(repo_dir: &Path) -> Result<String> {
    let git = Git::new(repo_dir);
    if !git.is_repo() {
        return Ok("main".to_string());
    }
    for c in &["main", "master"] {
        if git.rev_parse_verify(c).is_ok() {
            return Ok((*c).to_string());
        }
    }
    bail!(
        "could not determine default base ref (tried 'main' and 'master'); \
         pass --base explicitly"
    )
}

/// Build a `ProjectId → task names` index by consulting mise's task
/// loader. Task names like `//apps/web:build` are split into
/// `(//apps/web, build)`; non-monorepo tasks are dropped from the
/// result. Loads tasks across the whole monorepo (`TaskLoadContext::all()`)
/// so subdir-defined tasks are visible. (F-5)
async fn build_task_index(
    config: &Arc<Config>,
) -> Result<std::collections::HashMap<ProjectId, BTreeSet<String>>> {
    let ctx = crate::task::TaskLoadContext::all();
    let tasks = config.tasks_with_context(Some(&ctx)).await?;
    let mut by_project: std::collections::HashMap<ProjectId, BTreeSet<String>> =
        std::collections::HashMap::new();
    for task in tasks.values() {
        let Some((project, name)) = split_monorepo_task_name(&task.name) else {
            continue;
        };
        by_project.entry(project).or_default().insert(name);
    }
    Ok(by_project)
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
}
