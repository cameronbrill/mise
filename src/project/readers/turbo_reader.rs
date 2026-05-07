use std::collections::BTreeMap;
use std::path::Path;

use eyre::Result;
use serde::Deserialize;

use super::{DEFAULT_WALK_DEPTH, read_optional_file, walk_for_markers};
use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ReaderContext,
    ResolutionView,
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

    fn collect_projects(&self, ctx: &ReaderContext) -> Result<Vec<ContributedProject>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(ctx, MARKERS, DEFAULT_WALK_DEPTH)? {
            // The root turbo.json represents the workspace, not a project.
            // Skip it; per-package turbo.json files (in subdirs) become
            // projects only if no other reader has already claimed them.
            if dir == ctx.monorepo_root {
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

    /// Walk per-package `turbo.json` files (turbo 2.x extensions) for
    /// `<pkg>#<task>` references and emit a single edge from THIS
    /// package to the referenced one. Root-level pipeline entries are
    /// no longer used for edge inference — they describe task ordering
    /// at the workspace level, not per-project dependencies, and the
    /// previous fan-out emitted M×N spurious edges. Real package-to-
    /// package dependencies come from the npm/pnpm reader's
    /// `dependencies` traversal.
    fn collect_edges(
        &self,
        ctx: &ReaderContext,
        view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(ctx, MARKERS, DEFAULT_WALK_DEPTH)? {
            if dir == ctx.monorepo_root {
                continue;
            }
            let Some(parsed) = Self::parse(&dir.join("turbo.json")) else {
                continue;
            };
            let from = EdgeEndpoint::Path(dir);
            for entry in parsed.pipeline.values() {
                for dep in &entry.depends_on {
                    let Some(hash) = dep.find('#') else { continue };
                    let pkg = &dep[..hash];
                    if pkg.is_empty() || pkg.starts_with('^') {
                        continue;
                    }
                    // Reuse npm/pnpm workspace name resolution; turbo
                    // identifies packages by their package.json name.
                    let to_endpoint = if view
                        .resolve(&EdgeEndpoint::ForeignName {
                            source: ProjectSource::NpmWorkspace,
                            name: pkg.to_string(),
                        })
                        .is_some()
                    {
                        EdgeEndpoint::ForeignName {
                            source: ProjectSource::NpmWorkspace,
                            name: pkg.to_string(),
                        }
                    } else if view
                        .resolve(&EdgeEndpoint::ForeignName {
                            source: ProjectSource::PnpmWorkspace,
                            name: pkg.to_string(),
                        })
                        .is_some()
                    {
                        EdgeEndpoint::ForeignName {
                            source: ProjectSource::PnpmWorkspace,
                            name: pkg.to_string(),
                        }
                    } else {
                        continue;
                    };
                    out.push(ContributedEdge {
                        from: from.clone(),
                        to: to_endpoint,
                    });
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
        let ctx = ReaderContext::new(tmp.path().to_path_buf(), None);
        let projects = TurboReader.collect_projects(&ctx).unwrap();
        // Root turbo.json is skipped; only apps/web becomes a project.
        assert_eq!(projects.len(), 1);
        assert!(projects[0].path_root.ends_with("apps/web"));
    }
}
