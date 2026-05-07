use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use eyre::Result;
use globset::{Glob, GlobSetBuilder};
use petgraph::Direction;
use petgraph::algo::is_cyclic_directed;
use petgraph::graph::{DiGraph, NodeIndex};

use super::{Project, ProjectId, ProjectSource};

/// Project dependency graph for a monorepo.
///
/// Mirrors the shape of `Deps` in `src/task/deps.rs`: a concrete
/// `DiGraph<Project, ()>` with an `id → NodeIndex` map. Adds a
/// `name_index` keyed by `(ProjectSource, foreign-name)` so cross-format
/// edges (e.g., an `nx.json` `implicitDependencies: ["web-shell"]` reference
/// inside a `package.json` workspace) can resolve to the canonical id.
#[derive(Debug, Clone, Default)]
pub struct ProjectGraph {
    pub graph: DiGraph<Project, ()>,
    pub by_id: HashMap<ProjectId, NodeIndex>,
    pub name_index: HashMap<(ProjectSource, String), ProjectId>,
    pub monorepo_root: PathBuf,
}

impl ProjectGraph {
    pub fn new(monorepo_root: PathBuf) -> Self {
        Self {
            graph: DiGraph::new(),
            by_id: HashMap::new(),
            name_index: HashMap::new(),
            monorepo_root,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.graph.node_count() == 0
    }

    /// Adds a project to the graph if not already present, returning its
    /// `NodeIndex`. If the id already exists, foreign names from `project`
    /// are merged into the existing node.
    pub fn upsert_project(&mut self, project: Project) -> NodeIndex {
        if let Some(&idx) = self.by_id.get(&project.id) {
            for (src, name) in &project.foreign_names {
                self.name_index
                    .entry((src.clone(), name.clone()))
                    .or_insert_with(|| project.id.clone());
            }
            let existing = &mut self.graph[idx];
            for (src, name) in project.foreign_names {
                existing.foreign_names.entry(src).or_insert(name);
            }
            for tag in project.tags {
                existing.tags.insert(tag);
            }
            for src in project.sources {
                if !existing.sources.contains(&src) {
                    existing.sources.push(src);
                }
            }
            return idx;
        }
        let id = project.id.clone();
        for (src, name) in &project.foreign_names {
            self.name_index
                .entry((src.clone(), name.clone()))
                .or_insert_with(|| id.clone());
        }
        let idx = self.graph.add_node(project);
        self.by_id.insert(id, idx);
        idx
    }

    /// Adds an edge `from -> to` (meaning "from depends on to"). Both ids
    /// must already be present in the graph; missing ids are a soft error
    /// (the edge is dropped) so a partial graph can still render.
    pub fn add_edge(&mut self, from: &ProjectId, to: &ProjectId) {
        let (Some(&from_idx), Some(&to_idx)) = (self.by_id.get(from), self.by_id.get(to)) else {
            return;
        };
        if from_idx != to_idx {
            self.graph.update_edge(from_idx, to_idx, ());
        }
    }

    pub fn resolve_by_foreign_name(&self, source: &ProjectSource, name: &str) -> Option<&ProjectId> {
        self.name_index.get(&(source.clone(), name.to_string()))
    }

    pub fn projects(&self) -> impl Iterator<Item = &Project> {
        self.graph.node_weights()
    }

    pub fn project(&self, id: &ProjectId) -> Option<&Project> {
        self.by_id.get(id).map(|&idx| &self.graph[idx])
    }

    pub fn has_cycle(&self) -> bool {
        is_cyclic_directed(&self.graph)
    }

    /// Returns ids of all projects that depend on `seed` (transitively).
    /// Includes `seed` itself in the result.
    pub fn transitive_dependents(&self, seeds: &[ProjectId]) -> BTreeSet<ProjectId> {
        let mut out = BTreeSet::new();
        let mut queue: VecDeque<NodeIndex> = VecDeque::new();
        let mut visited: HashSet<NodeIndex> = HashSet::new();
        for s in seeds {
            if let Some(&idx) = self.by_id.get(s) {
                queue.push_back(idx);
                visited.insert(idx);
            }
        }
        while let Some(idx) = queue.pop_front() {
            out.insert(self.graph[idx].id.clone());
            // Reverse edges: in the graph, an edge `A -> B` means
            // "A depends on B". So dependents of B are nodes with
            // outgoing edges *to* B — petgraph::Direction::Incoming
            // from B's perspective.
            for nbr in self.graph.neighbors_directed(idx, Direction::Incoming) {
                if visited.insert(nbr) {
                    queue.push_back(nbr);
                }
            }
        }
        out
    }

    /// Maps changed file paths to the projects that contain them.
    /// A file is mapped to a project P if:
    ///   - the file lives under `P.root`, AND
    ///   - either `P.sources` is empty, or at least one glob in
    ///     `P.sources` matches the file path relative to `P.root`.
    pub fn projects_for_changed_files(&self, files: &[PathBuf]) -> Result<BTreeSet<ProjectId>> {
        let mut out = BTreeSet::new();
        let mut compiled: HashMap<ProjectId, globset::GlobSet> = HashMap::new();
        for project in self.projects() {
            if project.sources.is_empty() {
                continue;
            }
            let mut builder = GlobSetBuilder::new();
            for pat in &project.sources {
                builder.add(Glob::new(pat)?);
            }
            compiled.insert(project.id.clone(), builder.build()?);
        }
        for file in files {
            let abs = if file.is_absolute() {
                file.clone()
            } else {
                self.monorepo_root.join(file)
            };
            let mut best: Option<(usize, &Project)> = None;
            for project in self.projects() {
                let project_root = if project.root.is_absolute() {
                    project.root.clone()
                } else {
                    self.monorepo_root.join(&project.root)
                };
                if let Ok(rel) = abs.strip_prefix(&project_root) {
                    let depth = project_root.components().count();
                    let glob_ok = match compiled.get(&project.id) {
                        Some(set) => set.is_match(rel),
                        None => true, // empty sources = match all under root
                    };
                    if glob_ok && best.is_none_or(|(d, _)| depth > d) {
                        best = Some((depth, project));
                    }
                }
            }
            if let Some((_, p)) = best {
                out.insert(p.id.clone());
            }
        }
        Ok(out)
    }

    /// Convenience: full set of affected projects (seeds + dependents).
    pub fn affected_from_paths(&self, files: &[PathBuf]) -> Result<BTreeSet<ProjectId>> {
        let seeds: Vec<ProjectId> = self
            .projects_for_changed_files(files)?
            .into_iter()
            .collect();
        Ok(self.transitive_dependents(&seeds))
    }

    /// DOT representation, mirroring the pattern in `src/cli/tasks/deps.rs`.
    pub fn to_dot(&self) -> String {
        use petgraph::dot::{Config, Dot};
        let dot = Dot::with_attr_getters(
            &self.graph,
            &[Config::NodeNoLabel, Config::EdgeNoLabel],
            &|_, _| String::new(),
            &|_, nr| format!("label = \"{}\"", nr.1.id),
        );
        format!("{dot:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn proj(id: &str, source: ProjectSource) -> Project {
        Project::new(id.to_string(), PathBuf::from(id.trim_start_matches("//")), source)
    }

    #[test]
    fn upsert_dedups_by_id_and_merges_foreign_names() {
        let mut g = ProjectGraph::new(PathBuf::from("/tmp"));
        let mut a1 = proj("//apps/web", ProjectSource::Mise);
        a1.foreign_names.insert(ProjectSource::Mise, "web".into());
        g.upsert_project(a1);
        let mut a2 = proj("//apps/web", ProjectSource::Nx);
        a2.foreign_names
            .insert(ProjectSource::Nx, "web-shell".into());
        g.upsert_project(a2);
        assert_eq!(g.graph.node_count(), 1);
        let p = g.project(&"//apps/web".to_string()).unwrap();
        assert_eq!(p.foreign_names.get(&ProjectSource::Mise).unwrap(), "web");
        assert_eq!(
            p.foreign_names.get(&ProjectSource::Nx).unwrap(),
            "web-shell"
        );
    }

    #[test]
    fn transitive_dependents_includes_seed_and_walks_reverse_edges() {
        let mut g = ProjectGraph::new(PathBuf::from("/tmp"));
        g.upsert_project(proj("//a", ProjectSource::Mise));
        g.upsert_project(proj("//b", ProjectSource::Mise));
        g.upsert_project(proj("//c", ProjectSource::Mise));
        // a depends on b; c depends on a → modifying b should affect a, b, c
        g.add_edge(&"//a".into(), &"//b".into());
        g.add_edge(&"//c".into(), &"//a".into());
        let affected = g.transitive_dependents(&["//b".into()]);
        let expected: BTreeSet<String> =
            ["//a".into(), "//b".into(), "//c".into()].into_iter().collect();
        assert_eq!(affected, expected);
    }

    #[test]
    fn cycle_detected() {
        let mut g = ProjectGraph::new(PathBuf::from("/tmp"));
        g.upsert_project(proj("//a", ProjectSource::Mise));
        g.upsert_project(proj("//b", ProjectSource::Mise));
        g.add_edge(&"//a".into(), &"//b".into());
        g.add_edge(&"//b".into(), &"//a".into());
        assert!(g.has_cycle());
    }

    #[test]
    fn resolve_by_foreign_name() {
        let mut g = ProjectGraph::new(PathBuf::from("/tmp"));
        let mut p = proj("//apps/web", ProjectSource::Nx);
        p.foreign_names
            .insert(ProjectSource::Nx, "web-shell".into());
        g.upsert_project(p);
        let resolved = g.resolve_by_foreign_name(&ProjectSource::Nx, "web-shell");
        assert_eq!(resolved.cloned(), Some("//apps/web".to_string()));
    }

    #[test]
    fn projects_for_changed_files_picks_deepest_match() {
        let monorepo = PathBuf::from("/tmp/mono");
        let mut g = ProjectGraph::new(monorepo.clone());
        let mut shared = Project::new("//".into(), PathBuf::from(""), ProjectSource::Mise);
        shared.sources = vec!["**".into()];
        g.upsert_project(shared);
        let nested = Project::new(
            "//apps/web".into(),
            PathBuf::from("apps/web"),
            ProjectSource::Mise,
        );
        g.upsert_project(nested);
        let changed = vec![PathBuf::from("/tmp/mono/apps/web/src/main.ts")];
        let seeds = g.projects_for_changed_files(&changed).unwrap();
        // Deepest (apps/web) wins over the root-level catch-all.
        assert!(seeds.contains("//apps/web"));
        assert!(!seeds.contains("//"));
    }
}
