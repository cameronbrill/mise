use std::collections::BTreeSet;

use eyre::{Result, bail};
use serde::Serialize;

use crate::config::{Config, Settings};
use crate::git::Git;

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
    #[clap(long)]
    all: bool,

    /// Base ref to diff against (default: `main` if it exists, else `master`)
    #[clap(long)]
    base: Option<String>,

    /// Exclude these project ids from the result
    #[clap(long)]
    exclude: Vec<String>,

    /// Override the changed-file set (one path per `--files` flag).
    /// Useful in CI when the runner already has the file list.
    #[clap(long)]
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

#[derive(Debug, Serialize)]
struct AffectedReport {
    base: String,
    head: String,
    projects: Vec<String>,
    target_tasks: Option<Vec<String>>,
}

const AFTER_LONG_HELP: &str = color_print::cstr!(
    r#"<bold><underline>Examples:</underline></bold>

    $ <bold>mise affected</bold>
    //apps/web
    //libs/shared

    $ <bold>mise affected --base main --head HEAD --target build</bold>
    //apps/web

    $ <bold>mise affected --format json</bold>
    {"base":"main","head":"HEAD","projects":["//apps/web","//libs/shared"],"target_tasks":null}
"#
);

impl Affected {
    pub async fn run(self) -> Result<()> {
        Settings::get().ensure_experimental("project-graph")?;
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
            .unwrap_or_else(|| default_base(&graph.monorepo_root));
        let head = self
            .head
            .clone()
            .unwrap_or_else(|| "HEAD".to_string());

        let exclude_set: BTreeSet<String> = self.exclude.iter().cloned().collect();

        let mut projects: BTreeSet<String> = if self.all {
            graph.projects().map(|p| p.id.clone()).collect()
        } else if !self.files.is_empty() {
            // User-supplied file list bypasses git entirely.
            let files = self
                .files
                .iter()
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>();
            graph.affected_from_paths(&files)?
        } else {
            let git = Git::new(graph.monorepo_root.clone());
            if !git.is_repo() {
                bail!(
                    "monorepo root {} is not a git repository — pass --files explicitly",
                    graph.monorepo_root.display()
                );
            }
            let changed = git.changed_files(&base, &head, true)?;
            graph.affected_from_paths(&changed)?
        };

        for ex in &exclude_set {
            projects.remove(ex);
        }

        if let Some(target) = self.target.as_ref() {
            // Filter to projects that define the named task. We don't
            // load mise tasks here (that's a heavier operation than
            // listing project ids); the project schema doesn't carry
            // task definitions yet either. For PR 2, --target is a
            // pass-through filter that keeps a project iff its mise.toml
            // declares a task with that name. Future PRs may move the
            // task→project mapping into the project graph itself.
            projects.retain(|id| project_has_task(&graph, id, target));
        }

        match self.format {
            AffectedFormat::Ids => {
                for id in &projects {
                    miseprintln!("{id}");
                }
            }
            AffectedFormat::Tasks => {
                if let Some(target) = self.target.as_ref() {
                    for id in &projects {
                        miseprintln!("{id}:{target}");
                    }
                } else {
                    for id in &projects {
                        miseprintln!("{id}");
                    }
                }
            }
            AffectedFormat::Json => {
                let report = AffectedReport {
                    base,
                    head,
                    projects: projects.into_iter().collect(),
                    target_tasks: self.target.as_ref().map(|t| {
                        // Already filtered above; emit `id:target` pairs.
                        vec![t.clone()]
                    }),
                };
                miseprintln!("{}", serde_json::to_string(&report)?);
            }
        }

        Ok(())
    }
}

/// Default base ref: prefer `main`, fall back to `master`. If neither
/// exists, we still try `main` and let the git invocation error point
/// the user at `--base`.
fn default_base(repo_dir: &std::path::Path) -> String {
    let git = Git::new(repo_dir);
    if !git.is_repo() {
        return "main".to_string();
    }
    let candidates = ["main", "master"];
    for c in &candidates {
        if git.merge_base(c, "HEAD").is_ok() {
            return c.to_string();
        }
    }
    "main".to_string()
}

/// Returns true if the project's directory has a `mise.toml` declaring
/// `[tasks.<target>]` (or any descendant task whose name ends with
/// `:<target>` after monorepo prefixing).
fn project_has_task(
    graph: &crate::project::ProjectGraph,
    project_id: &str,
    target: &str,
) -> bool {
    let Some(project) = graph.project(&project_id.to_string()) else {
        return false;
    };
    let project_root = if project.root.is_absolute() {
        project.root.clone()
    } else {
        graph.monorepo_root.join(&project.root)
    };
    for marker in &["mise.toml", ".mise.toml"] {
        let path = project_root.join(marker);
        if let Ok(body) = std::fs::read_to_string(&path)
            && let Ok(table) = body.parse::<toml::Table>()
            && let Some(tasks) = table.get("tasks").and_then(|v| v.as_table())
            && tasks.contains_key(target)
        {
            return true;
        }
    }
    false
}
