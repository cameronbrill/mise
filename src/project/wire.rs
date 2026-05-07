// Some items are consumed only by future PRs (mise graph CLI command).
// Mark as allowed-dead so PR 1 builds cleanly.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use super::{ProjectGraph, ProjectId, ProjectSource};

/// Schema marker for the project-graph JSON wire format.
///
/// Ships as `mise-project-graph-experimental` until the data shape
/// stabilizes after the vfox plugin hook lands and real plugin
/// contributions are exercised. Graduates to `mise-project-graph-v1` in a
/// later PR (see workpad "Wire format graduation timing"). Consumers
/// should pin against `mise --version` while the schema is experimental.
pub const WIRE_SCHEMA_EXPERIMENTAL: &str = "mise-project-graph-experimental";

#[derive(Debug, Clone, Serialize)]
pub struct WireProjectGraph {
    pub schema: &'static str,
    pub version: u32,
    pub monorepo_root: PathBuf,
    pub projects: Vec<WireProject>,
    pub edges: Vec<WireEdge>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WireProject {
    pub id: ProjectId,
    pub root: PathBuf,
    pub source: ProjectSource,
    pub sources: Vec<String>,
    pub tags: Vec<String>,
    pub foreign_names: BTreeMap<ProjectSource, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WireEdge {
    pub from: ProjectId,
    pub to: ProjectId,
}

impl From<&ProjectGraph> for WireProjectGraph {
    fn from(g: &ProjectGraph) -> Self {
        let mut projects: Vec<WireProject> = g
            .projects()
            .map(|p| WireProject {
                id: p.id.clone(),
                root: p.root.clone(),
                source: p.source.clone(),
                sources: p.sources.clone(),
                tags: p.tags.iter().cloned().collect(),
                foreign_names: p.foreign_names.clone(),
            })
            .collect();
        projects.sort_by(|a, b| a.id.cmp(&b.id));
        let mut edges: Vec<WireEdge> = g
            .graph
            .edge_indices()
            .filter_map(|e| g.graph.edge_endpoints(e))
            .map(|(from, to)| WireEdge {
                from: g.graph[from].id.clone(),
                to: g.graph[to].id.clone(),
            })
            .collect();
        edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
        WireProjectGraph {
            schema: WIRE_SCHEMA_EXPERIMENTAL,
            version: 1,
            monorepo_root: g.monorepo_root.clone(),
            projects,
            edges,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::project::Project;

    #[test]
    fn schema_marker_is_experimental_not_v1() {
        let g = ProjectGraph::new(PathBuf::from("/tmp/mono"));
        let wire: WireProjectGraph = (&g).into();
        assert_eq!(wire.schema, "mise-project-graph-experimental");
    }

    #[test]
    fn projects_and_edges_are_sorted_for_stable_output() {
        let mut g = ProjectGraph::new(PathBuf::from("/tmp/mono"));
        g.upsert_project(Project::new(
            "//z".into(),
            PathBuf::from("z"),
            ProjectSource::Mise,
        ));
        g.upsert_project(Project::new(
            "//a".into(),
            PathBuf::from("a"),
            ProjectSource::Mise,
        ));
        g.upsert_project(Project::new(
            "//m".into(),
            PathBuf::from("m"),
            ProjectSource::Mise,
        ));
        g.add_edge(&"//z".into(), &"//a".into());
        g.add_edge(&"//m".into(), &"//a".into());
        let wire: WireProjectGraph = (&g).into();
        let ids: Vec<&str> = wire.projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["//a", "//m", "//z"]);
        let edges: Vec<(&str, &str)> = wire
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        assert_eq!(edges, vec![("//m", "//a"), ("//z", "//a")]);
    }
}
