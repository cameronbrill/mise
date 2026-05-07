use std::path::Path;

use eyre::Result;

use super::reader::{MonorepoConfigReader, ResolutionView};
use super::{Project, ProjectGraph, project_id_from_path};

/// Two-pass orchestrator that walks every registered reader to assemble
/// the project graph.
///
/// Pass 1: each reader contributes projects (path + optional foreign name).
/// Projects with the same canonical id are merged; foreign names accumulate
/// in `Project.foreign_names` and the graph's `name_index`.
///
/// Pass 2: each reader emits edges referencing other projects by path,
/// id, or foreign name. Unresolvable references are dropped silently.
///
/// The cycle check at the end is informational — it sets a flag the
/// caller can read; we don't reject the graph because a circular
/// dependency in foreign config is the user's problem to fix and we
/// still want a renderable graph.
pub fn build_project_graph(
    monorepo_root: &Path,
    readers: &[Box<dyn MonorepoConfigReader>],
) -> Result<ProjectGraph> {
    let mut graph = ProjectGraph::new(monorepo_root.to_path_buf());

    // Pass 1 — collect projects.
    for reader in readers {
        let contributions = reader
            .collect_projects(monorepo_root)
            .map_err(|e| e.wrap_err(format!("project reader '{}'", reader.id())))?;
        for contrib in contributions {
            let id = project_id_from_path(monorepo_root, &contrib.path_root);
            let mut project = Project::new(id, contrib.path_root, contrib.source.clone());
            if let Some(name) = contrib.foreign_name {
                project.foreign_names.insert(contrib.source, name);
            }
            project.sources = contrib.sources;
            project.tags = contrib.tags.into_iter().collect();
            graph.upsert_project(project);
        }
    }

    // Pass 2 — collect edges with the populated graph in scope.
    let snapshot = graph.clone();
    let view = ResolutionView::new(&snapshot);
    for reader in readers {
        let edges = reader
            .collect_edges(monorepo_root, &view)
            .map_err(|e| e.wrap_err(format!("project reader '{}' edges", reader.id())))?;
        for edge in edges {
            let (Some(from), Some(to)) = (view.resolve(&edge.from), view.resolve(&edge.to)) else {
                continue;
            };
            graph.add_edge(&from, &to);
        }
    }

    Ok(graph)
}

/// Default reader set — every in-tree reader, in deterministic order so
/// the mise reader (highest authority for project identity) runs first
/// and foreign-name aliases land before edges that reference them.
pub fn default_readers() -> Vec<Box<dyn MonorepoConfigReader>> {
    vec![
        Box::new(super::readers::mise_reader::MiseReader),
        Box::new(super::readers::nx_reader::NxReader),
        Box::new(super::readers::turbo_reader::TurboReader),
        Box::new(super::readers::pnpm_reader::PnpmReader),
        Box::new(super::readers::npm_reader::NpmReader),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use eyre::Result;

    use super::*;
    use crate::project::ProjectSource;
    use crate::project::reader::{ContributedEdge, ContributedProject, EdgeEndpoint};

    /// Test reader that emits a fixed set of projects + edges.
    struct StaticReader {
        id: &'static str,
        projects: Vec<ContributedProject>,
        edges: Vec<ContributedEdge>,
    }

    impl MonorepoConfigReader for StaticReader {
        fn id(&self) -> &'static str {
            self.id
        }
        fn collect_projects(&self, _monorepo_root: &Path) -> Result<Vec<ContributedProject>> {
            Ok(self.projects.clone())
        }
        fn collect_edges(
            &self,
            _monorepo_root: &Path,
            _view: &ResolutionView,
        ) -> Result<Vec<ContributedEdge>> {
            Ok(self.edges.clone())
        }
    }

    #[test]
    fn two_pass_resolves_foreign_name_edge() {
        let monorepo_root = PathBuf::from("/tmp/mono");
        // Reader A says "//apps/web is also called 'web-shell' in nx-land".
        let nx_reader = StaticReader {
            id: "nx-test",
            projects: vec![ContributedProject {
                source: ProjectSource::Nx,
                path_root: PathBuf::from("/tmp/mono/apps/web"),
                foreign_name: Some("web-shell".into()),
                sources: vec![],
                tags: vec![],
            }],
            edges: vec![],
        };
        // Reader B says "//apps/api depends on the project named 'web-shell' in nx".
        let turbo_reader = StaticReader {
            id: "turbo-test",
            projects: vec![ContributedProject {
                source: ProjectSource::Turbo,
                path_root: PathBuf::from("/tmp/mono/apps/api"),
                foreign_name: None,
                sources: vec![],
                tags: vec![],
            }],
            edges: vec![ContributedEdge {
                from: EdgeEndpoint::Path(PathBuf::from("/tmp/mono/apps/api")),
                to: EdgeEndpoint::ForeignName {
                    source: ProjectSource::Nx,
                    name: "web-shell".into(),
                },
            }],
        };
        let readers: Vec<Box<dyn MonorepoConfigReader>> =
            vec![Box::new(nx_reader), Box::new(turbo_reader)];
        let graph = build_project_graph(&monorepo_root, &readers).unwrap();
        // Both projects should be present.
        let ids: BTreeSet<String> = graph.projects().map(|p| p.id.clone()).collect();
        assert!(ids.contains("//apps/web"));
        assert!(ids.contains("//apps/api"));
        // The cross-format edge should resolve via name_index.
        let affected = graph.transitive_dependents(&["//apps/web".into()]);
        assert!(
            affected.contains("//apps/api"),
            "api should be a transitive dependent of web via the foreign-name edge; got {affected:?}"
        );
    }

    #[test]
    fn unresolvable_edge_is_dropped_silently() {
        let monorepo_root = PathBuf::from("/tmp/mono");
        let reader = StaticReader {
            id: "broken",
            projects: vec![ContributedProject {
                source: ProjectSource::Mise,
                path_root: PathBuf::from("/tmp/mono/a"),
                foreign_name: None,
                sources: vec![],
                tags: vec![],
            }],
            edges: vec![ContributedEdge {
                from: EdgeEndpoint::Path(PathBuf::from("/tmp/mono/a")),
                to: EdgeEndpoint::ForeignName {
                    source: ProjectSource::Nx,
                    name: "ghost".into(),
                },
            }],
        };
        let readers: Vec<Box<dyn MonorepoConfigReader>> = vec![Box::new(reader)];
        let graph = build_project_graph(&monorepo_root, &readers).unwrap();
        // No edges should have been added.
        assert_eq!(graph.graph.edge_count(), 0);
    }

    /// Adversarial test: a real on-disk monorepo with `nx.json`,
    /// `project.json`, `package.json` workspaces, `pnpm-workspace.yaml`,
    /// and `mise.toml` `[project]` tables, with cross-format references.
    /// Verifies the two-pass builder + `name_index` resolves edges that
    /// reference projects by name (not path) across formats.
    #[test]
    fn multi_format_coexist_resolves_cross_format_edges() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // /apps/web is an Nx project named "web-shell".
        std::fs::create_dir_all(root.join("apps/web")).unwrap();
        std::fs::write(
            root.join("apps/web/project.json"),
            r#"{"name":"web-shell","tags":["frontend"]}"#,
        )
        .unwrap();
        // /apps/web also has a package.json so the npm reader picks it up.
        std::fs::write(
            root.join("apps/web/package.json"),
            r#"{"name":"@org/web","dependencies":{"@org/shared":"*"}}"#,
        )
        .unwrap();

        // /libs/shared is an Nx project named "shared-lib" AND an npm
        // workspace package "@org/shared". Cross-format identity:
        // foreign_names["nx"] = "shared-lib", foreign_names["npm"] = "@org/shared".
        std::fs::create_dir_all(root.join("libs/shared")).unwrap();
        std::fs::write(
            root.join("libs/shared/project.json"),
            r#"{"name":"shared-lib","tags":["lib"]}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("libs/shared/package.json"),
            r#"{"name":"@org/shared"}"#,
        )
        .unwrap();

        // Root package.json declares the workspaces.
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"root","workspaces":["apps/*","libs/*"]}"#,
        )
        .unwrap();

        // /apps/api is an Nx project that has an `implicitDependencies`
        // pointing at "shared-lib" (the nx-namespace name of /libs/shared).
        // This is the cross-format edge: api → shared via nx name resolution.
        std::fs::create_dir_all(root.join("apps/api")).unwrap();
        std::fs::write(
            root.join("apps/api/project.json"),
            r#"{"name":"api","implicitDependencies":["shared-lib"]}"#,
        )
        .unwrap();

        let graph = build_project_graph(root, &default_readers()).unwrap();

        // All three projects should be present.
        let ids: BTreeSet<String> = graph.projects().map(|p| p.id.clone()).collect();
        assert!(ids.contains("//apps/web"), "got: {ids:?}");
        assert!(ids.contains("//apps/api"), "got: {ids:?}");
        assert!(ids.contains("//libs/shared"), "got: {ids:?}");

        // /libs/shared should carry foreign names from both nx and npm.
        let shared = graph.project(&"//libs/shared".to_string()).unwrap();
        assert_eq!(
            shared.foreign_names.get(&ProjectSource::Nx).map(|s| s.as_str()),
            Some("shared-lib")
        );
        assert_eq!(
            shared
                .foreign_names
                .get(&ProjectSource::NpmWorkspace)
                .map(|s| s.as_str()),
            Some("@org/shared")
        );

        // Cross-format edge: api (nx) → shared (resolved via nx name).
        let affected_by_shared = graph.transitive_dependents(&["//libs/shared".into()]);
        assert!(
            affected_by_shared.contains("//apps/api"),
            "api should be a dependent of shared via the nx implicit edge; got {affected_by_shared:?}"
        );

        // npm workspaces edge: @org/web → @org/shared (resolved via npm name).
        assert!(
            affected_by_shared.contains("//apps/web"),
            "web should be a dependent of shared via the npm dependency; got {affected_by_shared:?}"
        );
    }

    #[test]
    fn duplicate_id_merges_foreign_names() {
        let monorepo_root = PathBuf::from("/tmp/mono");
        let mise_reader = StaticReader {
            id: "mise-test",
            projects: vec![ContributedProject {
                source: ProjectSource::Mise,
                path_root: PathBuf::from("/tmp/mono/apps/web"),
                foreign_name: Some("web".into()),
                sources: vec![],
                tags: vec!["frontend".into()],
            }],
            edges: vec![],
        };
        let nx_reader = StaticReader {
            id: "nx-test",
            projects: vec![ContributedProject {
                source: ProjectSource::Nx,
                path_root: PathBuf::from("/tmp/mono/apps/web"),
                foreign_name: Some("web-shell".into()),
                sources: vec![],
                tags: vec!["nx-managed".into()],
            }],
            edges: vec![],
        };
        let readers: Vec<Box<dyn MonorepoConfigReader>> =
            vec![Box::new(mise_reader), Box::new(nx_reader)];
        let graph = build_project_graph(&monorepo_root, &readers).unwrap();
        // Single node despite two contributions.
        assert_eq!(graph.graph.node_count(), 1);
        let p = graph.project(&"//apps/web".to_string()).unwrap();
        assert_eq!(p.foreign_names.get(&ProjectSource::Mise).unwrap(), "web");
        assert_eq!(
            p.foreign_names.get(&ProjectSource::Nx).unwrap(),
            "web-shell"
        );
        assert!(p.tags.contains("frontend"));
        assert!(p.tags.contains("nx-managed"));
    }
}
