use eyre::{Result, WrapErr};
use petgraph::algo::tarjan_scc;

use super::reader::{MonorepoConfigReader, ReaderContext, ResolutionView};
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
    ctx: &ReaderContext,
    readers: &[Box<dyn MonorepoConfigReader>],
) -> Result<ProjectGraph> {
    let monorepo_root = &ctx.monorepo_root;
    let mut graph = ProjectGraph::new(monorepo_root.clone());

    // Pass 1 — collect projects.
    for reader in readers {
        let reader_id = reader.id();
        let contributions = reader
            .collect_projects(ctx)
            .with_context(|| format!("project reader '{reader_id}'"))?;
        for contrib in contributions {
            // Reject contributions whose path escapes the monorepo —
            // produces nonsense ids and pollutes the name_index.
            if contrib.path_root.strip_prefix(monorepo_root).is_err() {
                warn!(
                    "project reader '{reader_id}' contributed path {} outside monorepo root — skipping",
                    contrib.path_root.display()
                );
                continue;
            }
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

    // Pass 2 — collect edges into a buffer, then apply. Avoids cloning
    // the entire graph for an immutable view: edges are accumulated
    // while `&graph` is borrowed read-only via `ResolutionView`, then
    // applied via `&mut graph` after the view is dropped.
    let pending: Vec<(super::ProjectId, super::ProjectId)> = {
        let view = ResolutionView::new(&graph);
        let mut acc: Vec<(super::ProjectId, super::ProjectId)> = Vec::new();
        for reader in readers {
            let reader_id = reader.id();
            let edges = reader
                .collect_edges(ctx, &view)
                .with_context(|| format!("project reader '{reader_id}' edges"))?;
            for edge in edges {
                match (view.resolve(&edge.from), view.resolve(&edge.to)) {
                    (Some(from), Some(to)) => acc.push((from, to)),
                    _ => trace!(
                        "project reader '{reader_id}': dropped unresolvable edge from={:?} to={:?}",
                        edge.from, edge.to
                    ),
                }
            }
        }
        acc
    };
    for (from, to) in pending {
        graph.add_edge(&from, &to);
    }

    // Cycles are user-fixable but should be visible. tarjan_scc itself
    // detects them — no separate has_cycle call needed.
    for component in tarjan_scc(&graph.graph).into_iter().filter(|c| c.len() > 1) {
        let ids: Vec<&str> = component
            .iter()
            .map(|idx| graph.graph[*idx].id.as_str())
            .collect();
        warn!("project graph cycle detected among {ids:?}");
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
        fn collect_projects(&self, _ctx: &ReaderContext) -> Result<Vec<ContributedProject>> {
            Ok(self.projects.clone())
        }
        fn collect_edges(
            &self,
            _ctx: &ReaderContext,
            _view: &ResolutionView,
        ) -> Result<Vec<ContributedEdge>> {
            Ok(self.edges.clone())
        }
    }

    fn ctx_for(root: &std::path::Path) -> ReaderContext {
        ReaderContext::new(root.to_path_buf(), None)
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
        let graph = build_project_graph(&ctx_for(&monorepo_root), &readers).unwrap();
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
        let graph = build_project_graph(&ctx_for(&monorepo_root), &readers).unwrap();
        // No edges should have been added.
        assert_eq!(graph.graph.edge_count(), 0);
    }

    /// Adversarial test for the load-bearing AC11 mitigation: a real
    /// on-disk monorepo with `nx.json`, `project.json`, `package.json`
    /// workspaces, **and** `turbo.json`, with cross-format references.
    /// Verifies the two-pass builder + `name_index` resolves edges that
    /// reference projects by name (not path) across formats. Asserts
    /// exact set + edge_count so a fan-out bug in any reader (e.g.,
    /// turbo's coarse cross-package edges) would fail the test.
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
        // foreign_names[Nx] = "shared-lib", foreign_names[NpmWorkspace] = "@org/shared".
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

        // Per-package `turbo.json` files. The turbo reader emits one
        // edge per `<pkg>#<task>` reference, attributed to the package
        // that owns the entry. This exercises the post-fan-out edge
        // path: api.turbo.json declares `pipeline.build.dependsOn =
        // ["@org/shared#build"]`, so api → shared.
        std::fs::write(
            root.join("turbo.json"),
            r#"{"pipeline":{}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("apps/api/turbo.json"),
            r#"{"pipeline":{"build":{"dependsOn":["@org/shared#build"]}}}"#,
        )
        .unwrap();

        let graph = build_project_graph(&ctx_for(root), &default_readers()).unwrap();

        // All three projects should be present.
        let ids: BTreeSet<String> = graph.projects().map(|p| p.id.clone()).collect();
        let expected_ids: BTreeSet<String> = ["//apps/web", "//apps/api", "//libs/shared"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ids, expected_ids, "unexpected project set");

        // /libs/shared should carry foreign names from nx AND npm AND
        // implicitly via turbo's pipeline reference.
        let shared = graph.project("//libs/shared").unwrap();
        assert_eq!(
            shared.foreign_names.get(&ProjectSource::Nx).map(String::as_str),
            Some("shared-lib")
        );
        assert_eq!(
            shared
                .foreign_names
                .get(&ProjectSource::NpmWorkspace)
                .map(String::as_str),
            Some("@org/shared")
        );

        // Cross-format edges: api (nx) → shared, web (npm) → shared.
        let affected_by_shared = graph.transitive_dependents(&["//libs/shared".into()]);
        assert!(
            affected_by_shared.contains("//apps/api"),
            "api should be a dependent of shared via the nx implicit edge; got {affected_by_shared:?}"
        );
        assert!(
            affected_by_shared.contains("//apps/web"),
            "web should be a dependent of shared via the npm dependency; got {affected_by_shared:?}"
        );

        // Exact set check — fan-out bugs (extra edges to non-dependents)
        // would fail this.
        assert_eq!(affected_by_shared, expected_ids);

        // Edge-count check — locks in the exact graph shape so a bug
        // that adds duplicate or spurious edges (e.g., a regression of
        // turbo's fan-out behavior) trips this even if the reachability
        // sets stay correct. Expected edges:
        //   - //apps/web → //libs/shared (npm `dependencies`)
        //   - //apps/api → //libs/shared (nx `implicitDependencies`)
        //   - //apps/api → //libs/shared (turbo `pipeline.build.dependsOn`)
        // npm-deps and turbo both target shared from api/web; petgraph
        // dedupes via update_edge so duplicates collapse to a single
        // edge per (from, to) pair → 2 distinct edges.
        assert_eq!(graph.graph.edge_count(), 2);

        // No incoming edges to api: changing api shouldn't affect anything else.
        let affected_by_api = graph.transitive_dependents(&["//apps/api".into()]);
        let just_api: BTreeSet<String> = ["//apps/api".to_string()].into_iter().collect();
        assert_eq!(affected_by_api, just_api);
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
        let graph = build_project_graph(&ctx_for(&monorepo_root), &readers).unwrap();
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
