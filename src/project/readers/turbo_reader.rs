// MARKERS / parse / TurboJson are referenced in collect_projects/edges
// but rustc's dead-code analyzer doesn't track usage through serde.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use eyre::Result;
use serde::Deserialize;

use super::{DEFAULT_WALK_DEPTH, walk_for_markers};
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
    fn parse(path: &Path) -> Result<Option<TurboJson>> {
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        Ok(serde_json::from_str(&body).ok())
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
            if Self::parse(&path)?.is_none() {
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

    fn collect_edges(
        &self,
        monorepo_root: &Path,
        view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let mut out = Vec::new();
        let root_turbo = monorepo_root.join("turbo.json");
        let Some(turbo) = Self::parse(&root_turbo)? else {
            return Ok(out);
        };
        // For each pipeline entry, parse `<pkg>#<task>` dependencies. The
        // `^build` syntax (transitive deps via package.json) is intentionally
        // ignored here — those edges come from the npm/pnpm reader's
        // `dependencies` traversal.
        for entry in turbo.pipeline.values() {
            for dep in &entry.depends_on {
                if let Some(hash) = dep.find('#') {
                    let pkg = &dep[..hash];
                    if pkg.is_empty() || pkg.starts_with('^') {
                        continue;
                    }
                    // Edges flow in both directions until we know which
                    // package this pipeline entry belongs to. Resolve the
                    // referenced package against npm-workspace names.
                    if let Some(target) = view
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
                    {
                        // The "from" side is unknown without a project
                        // context; turbo cross-package references at the
                        // root pipeline level imply every package depends
                        // on the named one. Add an edge from each
                        // pnpm/npm-managed project to the target.
                        for project in view.projects_by_source(&ProjectSource::NpmWorkspace) {
                            if project.id != target {
                                out.push(ContributedEdge {
                                    from: EdgeEndpoint::Id(project.id.clone()),
                                    to: EdgeEndpoint::Id(target.clone()),
                                });
                            }
                        }
                        for project in view.projects_by_source(&ProjectSource::PnpmWorkspace) {
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
        }
        Ok(out)
    }
}
