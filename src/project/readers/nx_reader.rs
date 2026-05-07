use std::path::Path;

use eyre::Result;
use serde::Deserialize;

use super::{DEFAULT_WALK_DEPTH, walk_for_markers};
use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ResolutionView,
};

/// Reads nx workspace projects: each `project.json` becomes one project,
/// keyed in the Nx namespace by its `name` field. `implicitDependencies`
/// references resolve against other projects' nx names (or paths).
///
/// `nx.json` at the monorepo root is read for workspace metadata but is
/// not itself a project.
pub struct NxReader;

#[derive(Debug, Default, Deserialize)]
struct ProjectJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default, rename = "implicitDependencies")]
    implicit_dependencies: Vec<String>,
    /// nx allows a project to declare a different sourceRoot than its
    /// project root; we store both as sources hints for affected matching.
    #[serde(default, rename = "sourceRoot")]
    source_root: Option<String>,
}

const MARKERS: &[&str] = &["project.json"];

impl NxReader {
    fn parse(path: &Path) -> Result<Option<ProjectJson>> {
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        Ok(serde_json::from_str(&body).ok())
    }
}

impl MonorepoConfigReader for NxReader {
    fn id(&self) -> &'static str {
        "nx"
    }

    fn collect_projects(&self, monorepo_root: &Path) -> Result<Vec<ContributedProject>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(monorepo_root, MARKERS, DEFAULT_WALK_DEPTH)? {
            let path = dir.join("project.json");
            let Some(parsed) = Self::parse(&path)? else {
                continue;
            };
            let mut sources = Vec::new();
            if let Some(sr) = &parsed.source_root {
                sources.push(format!("{sr}/**/*"));
            }
            out.push(ContributedProject {
                source: ProjectSource::Nx,
                path_root: dir,
                foreign_name: parsed.name.clone(),
                sources,
                tags: parsed.tags,
            });
        }
        Ok(out)
    }

    fn collect_edges(
        &self,
        monorepo_root: &Path,
        _view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(monorepo_root, MARKERS, DEFAULT_WALK_DEPTH)? {
            let path = dir.join("project.json");
            let Some(parsed) = Self::parse(&path)? else {
                continue;
            };
            let from = EdgeEndpoint::Path(dir);
            for dep in &parsed.implicit_dependencies {
                // Skip nx's "negate" syntax (`!project-name`) — we don't
                // model exclusions yet.
                if dep.starts_with('!') {
                    continue;
                }
                out.push(ContributedEdge {
                    from: from.clone(),
                    to: EdgeEndpoint::ForeignName {
                        source: ProjectSource::Nx,
                        name: dep.clone(),
                    },
                });
            }
        }
        Ok(out)
    }
}
