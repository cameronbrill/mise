use std::path::Path;

use eyre::Result;
use serde::Deserialize;

use super::{DEFAULT_WALK_DEPTH, walk_for_markers};
use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ResolutionView,
};

/// Reads `[project]` tables from `mise.toml` files across the monorepo.
///
/// `id` (if provided) is informational; the canonical project id is
/// always path-derived. `name` is exposed as the foreign-name in the
/// `Mise` source's namespace so other tooling can reference projects by
/// their mise-declared name. `depends`/`implicit_dependencies` become
/// edges in pass 2.
pub struct MiseReader;

#[derive(Debug, Default, Deserialize)]
struct MiseTomlSurface {
    #[serde(default)]
    project: Option<ProjectSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    depends: Vec<String>,
    #[serde(default)]
    implicit_dependencies: Vec<String>,
}

const MARKERS: &[&str] = &["mise.toml", ".mise.toml"];

impl MiseReader {
    fn parse(path: &Path) -> Result<Option<ProjectSection>> {
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let parsed: MiseTomlSurface = match toml::from_str(&body) {
            Ok(p) => p,
            // mise.tomls may use template syntax we can't render here.
            // Skip silently — the main mise.toml parser will surface the
            // real error elsewhere.
            Err(_) => return Ok(None),
        };
        Ok(parsed.project)
    }
}

impl MonorepoConfigReader for MiseReader {
    fn id(&self) -> &'static str {
        "mise"
    }

    fn collect_projects(&self, monorepo_root: &Path) -> Result<Vec<ContributedProject>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(monorepo_root, MARKERS, DEFAULT_WALK_DEPTH)? {
            let mut section: Option<ProjectSection> = None;
            for marker in MARKERS {
                let path = dir.join(marker);
                if path.exists()
                    && let Some(s) = Self::parse(&path)?
                {
                    section = Some(s);
                    break;
                }
            }
            let Some(section) = section else { continue };
            out.push(ContributedProject {
                source: ProjectSource::Mise,
                path_root: dir,
                foreign_name: section.name.clone(),
                sources: section.sources,
                tags: section.tags,
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
        for dir in walk_for_markers(monorepo_root, MARKERS, DEFAULT_WALK_DEPTH)? {
            let mut section: Option<ProjectSection> = None;
            for marker in MARKERS {
                let path = dir.join(marker);
                if path.exists()
                    && let Some(s) = Self::parse(&path)?
                {
                    section = Some(s);
                    break;
                }
            }
            let Some(section) = section else { continue };
            let from = EdgeEndpoint::Path(dir.clone());
            for dep in section.depends.iter().chain(section.implicit_dependencies.iter()) {
                let endpoint = if dep.starts_with("//") {
                    EdgeEndpoint::Id(dep.clone())
                } else {
                    // Try mise foreign-name first; fall back to path
                    // relative to monorepo root.
                    if view
                        .resolve(&EdgeEndpoint::ForeignName {
                            source: ProjectSource::Mise,
                            name: dep.clone(),
                        })
                        .is_some()
                    {
                        EdgeEndpoint::ForeignName {
                            source: ProjectSource::Mise,
                            name: dep.clone(),
                        }
                    } else {
                        EdgeEndpoint::Path(monorepo_root.join(dep))
                    }
                };
                out.push(ContributedEdge {
                    from: from.clone(),
                    to: endpoint,
                });
            }
        }
        Ok(out)
    }
}
