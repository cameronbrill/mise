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
//
// Allow-dead because the consumer (`mise graph` CLI) ships in a follow-up PR.
#[allow(dead_code)]
pub const WIRE_SCHEMA_EXPERIMENTAL: &str = "mise-project-graph-experimental";

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub struct WireProjectGraph {
    pub schema: &'static str,
    pub version: u32,
    pub monorepo_root: PathBuf,
    pub projects: Vec<WireProject>,
    pub edges: Vec<WireEdge>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub struct WireProject {
    pub id: ProjectId,
    pub root: PathBuf,
    pub source: ProjectSource,
    pub sources: Vec<String>,
    pub tags: Vec<String>,
    /// Per-source foreign names (e.g., the nx `name` or the
    /// `package.json` `name`). Keys are the kebab-case source identifier
    /// (`nx`, `npm-workspace`, `pnpm-workspace`, etc.) so JSON consumers
    /// don't need to understand the `ProjectSource` enum tag form.
    pub foreign_names: BTreeMap<String, String>,
}

#[allow(dead_code)]
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
                foreign_names: p
                    .foreign_names
                    .iter()
                    .map(|(s, n)| (s.to_string(), n.clone()))
                    .collect(),
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

    /// Insta-snapshot of the wire format. Locks the JSON shape so any
    /// rename/removal of a field surfaces as a PR-time diff rather than
    /// silently breaking external consumers.
    #[test]
    fn wire_format_snapshot() {
        let mut g = ProjectGraph::new(PathBuf::from("/tmp/mono"));
        let mut web = Project::new(
            "//apps/web".into(),
            PathBuf::from("apps/web"),
            ProjectSource::Nx,
        );
        web.foreign_names
            .insert(ProjectSource::Nx, "web-shell".into());
        web.foreign_names
            .insert(ProjectSource::NpmWorkspace, "@org/web".into());
        web.sources = vec!["src/**/*.ts".into()];
        web.tags.insert("frontend".into());
        g.upsert_project(web);
        let mut shared = Project::new(
            "//libs/shared".into(),
            PathBuf::from("libs/shared"),
            ProjectSource::NpmWorkspace,
        );
        shared
            .foreign_names
            .insert(ProjectSource::NpmWorkspace, "@org/shared".into());
        g.upsert_project(shared);
        g.add_edge("//apps/web", "//libs/shared");
        let wire: WireProjectGraph = (&g).into();
        insta::assert_json_snapshot!(&wire);
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
        g.add_edge("//z", "//a");
        g.add_edge("//m", "//a");
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
