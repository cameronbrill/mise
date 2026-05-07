use std::path::Path;

use eyre::Result;
use serde::Deserialize;

use super::{DEFAULT_WALK_DEPTH, read_optional_file, walk_for_markers};
use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ReaderContext,
    ResolutionView,
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
    fn parse(path: &Path) -> Option<ProjectJson> {
        let body = read_optional_file(path, "nx")?;
        match serde_json::from_str::<ProjectJson>(&body) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!(
                    "project reader 'nx': could not parse {}: {e}",
                    path.display()
                );
                None
            }
        }
    }
}

impl MonorepoConfigReader for NxReader {
    fn id(&self) -> &'static str {
        "nx"
    }

    fn collect_projects(&self, ctx: &ReaderContext) -> Result<Vec<ContributedProject>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(ctx, MARKERS, DEFAULT_WALK_DEPTH)? {
            let path = dir.join("project.json");
            let Some(parsed) = Self::parse(&path) else {
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
        ctx: &ReaderContext,
        _view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(ctx, MARKERS, DEFAULT_WALK_DEPTH)? {
            let path = dir.join("project.json");
            let Some(parsed) = Self::parse(&path) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_name_and_implicit_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("project.json");
        std::fs::write(
            &path,
            r#"{"name":"web","tags":["frontend"],"implicitDependencies":["shared","!ignored"],"sourceRoot":"apps/web/src"}"#,
        )
        .unwrap();
        let parsed = NxReader::parse(&path).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("web"));
        assert_eq!(parsed.tags, vec!["frontend"]);
        assert_eq!(parsed.implicit_dependencies, vec!["shared", "!ignored"]);
        assert_eq!(parsed.source_root.as_deref(), Some("apps/web/src"));
    }

    #[test]
    fn parse_handles_missing_file() {
        assert!(NxReader::parse(Path::new("/nonexistent/project.json")).is_none());
    }

    #[test]
    fn parse_tolerates_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("project.json");
        std::fs::write(&path, "{not json}").unwrap();
        assert!(NxReader::parse(&path).is_none());
    }

    #[test]
    fn collect_edges_skips_negate_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("apps/web")).unwrap();
        std::fs::write(
            tmp.path().join("apps/web/project.json"),
            r#"{"name":"web","implicitDependencies":["shared","!ignored"]}"#,
        )
        .unwrap();
        let graph = crate::project::ProjectGraph::new(tmp.path().to_path_buf());
        let view = ResolutionView::new(&graph);
        let ctx = ReaderContext::new(tmp.path().to_path_buf(), None);
        let edges = NxReader.collect_edges(&ctx, &view).unwrap();
        // Only one edge — the negate-prefixed one is skipped.
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeEndpoint::ForeignName { name, .. } => assert_eq!(name, "shared"),
            other => panic!("expected ForeignName, got {other:?}"),
        }
    }
}
