use std::collections::BTreeMap;
use std::path::Path;

use eyre::Result;
use serde::Deserialize;

use super::{DEFAULT_WALK_DEPTH, read_optional_file, walk_for_markers};
use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ResolutionView,
};

/// Reads `turbo.json` files (turborepo). The root `turbo.json` declares
/// the pipeline; per-package `turbo.json` files (turbo 2.x extensions)
/// can override pipeline entries.
///
/// Turborepo references projects by **package name** from `package.json`,
/// using the `<package-name>#<task>` syntax in `dependsOn`. We resolve
/// those references against the npm/pnpm namespaces during pass 2.
pub struct TurboReader;

#[derive(Debug, Default, Deserialize)]
struct TurboJson {
    #[serde(default, alias = "tasks")]
    pipeline: BTreeMap<String, PipelineEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct PipelineEntry {
    #[serde(default, rename = "dependsOn")]
    depends_on: Vec<String>,
}

const MARKERS: &[&str] = &["turbo.json"];

impl TurboReader {
    fn parse(path: &Path) -> Option<TurboJson> {
        let body = read_optional_file(path, "turbo")?;
        match serde_json::from_str::<TurboJson>(&body) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!(
                    "project reader 'turbo': could not parse {}: {e}",
                    path.display()
                );
                None
            }
        }
    }
}

impl MonorepoConfigReader for TurboReader {
    fn id(&self) -> &'static str {
        "turbo"
    }

    fn collect_projects(&self, monorepo_root: &Path) -> Result<Vec<ContributedProject>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(monorepo_root, MARKERS, DEFAULT_WALK_DEPTH)? {
            // The root turbo.json represents the workspace, not a project.
            // Skip it; per-package turbo.json files (in subdirs) become
            // projects only if no other reader has already claimed them.
            if dir == monorepo_root {
                continue;
            }
            let path = dir.join("turbo.json");
            if Self::parse(&path).is_none() {
                continue;
            }
            // Use the directory name as the foreign-name fallback when
            // we can't read a package.json sibling for the real name.
            let foreign_name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            out.push(ContributedProject {
                source: ProjectSource::Turbo,
                path_root: dir,
                foreign_name,
                sources: Vec::new(),
                tags: Vec::new(),
            });
        }
        Ok(out)
    }

    /// Per pipeline entry, emit edges for `<pkg>#<task>` references that
    /// resolve against npm/pnpm workspace names. The `^build` form (which
    /// means "transitive deps' build") is intentionally ignored — those
    /// edges come from the workspace dependency traversal in the
    /// npm/pnpm readers.
    ///
    /// Note: at the root pipeline level we don't know which project
    /// owns the entry, so the edge attribution is approximate. This is
    /// documented in the PR description and tracked as future work.
    fn collect_edges(
        &self,
        monorepo_root: &Path,
        view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let mut out = Vec::new();
        let root_turbo = monorepo_root.join("turbo.json");
        let Some(turbo) = Self::parse(&root_turbo) else {
            return Ok(out);
        };
        for entry in turbo.pipeline.values() {
            for dep in &entry.depends_on {
                let Some(hash) = dep.find('#') else { continue };
                let pkg = &dep[..hash];
                if pkg.is_empty() || pkg.starts_with('^') {
                    continue;
                }
                let Some(target) = view
                    .resolve(&EdgeEndpoint::ForeignName {
                        source: ProjectSource::NpmWorkspace,
                        name: pkg.to_string(),
                    })
                    .or_else(|| {
                        view.resolve(&EdgeEndpoint::ForeignName {
                            source: ProjectSource::PnpmWorkspace,
                            name: pkg.to_string(),
                        })
                    })
                else {
                    continue;
                };
                for source in
                    [ProjectSource::NpmWorkspace, ProjectSource::PnpmWorkspace].iter()
                {
                    for project in view.projects_by_source(source) {
                        if project.id != target {
                            out.push(ContributedEdge {
                                from: EdgeEndpoint::Id(project.id.clone()),
                                to: EdgeEndpoint::Id(target.clone()),
                            });
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_pipeline_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("turbo.json");
        std::fs::write(
            &path,
            r#"{"pipeline":{"build":{"dependsOn":["^build","shared#build"]}}}"#,
        )
        .unwrap();
        let parsed = TurboReader::parse(&path).unwrap();
        let entry = parsed.pipeline.get("build").unwrap();
        assert_eq!(entry.depends_on, vec!["^build", "shared#build"]);
    }

    #[test]
    fn parse_supports_tasks_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("turbo.json");
        // Turbo 2.x uses `tasks` instead of `pipeline`.
        std::fs::write(
            &path,
            r#"{"tasks":{"build":{"dependsOn":["shared#build"]}}}"#,
        )
        .unwrap();
        let parsed = TurboReader::parse(&path).unwrap();
        assert!(parsed.pipeline.contains_key("build"));
    }

    #[test]
    fn collect_projects_skips_root_turbo_json() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("turbo.json"), r#"{"pipeline":{}}"#).unwrap();
        std::fs::create_dir_all(tmp.path().join("apps/web")).unwrap();
        std::fs::write(
            tmp.path().join("apps/web/turbo.json"),
            r#"{"pipeline":{}}"#,
        )
        .unwrap();
        let projects = TurboReader.collect_projects(tmp.path()).unwrap();
        // Root turbo.json is skipped; only apps/web becomes a project.
        assert_eq!(projects.len(), 1);
        assert!(projects[0].path_root.ends_with("apps/web"));
    }
}
