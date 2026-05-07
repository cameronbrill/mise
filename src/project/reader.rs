use std::collections::BTreeMap;
use std::path::PathBuf;

use eyre::Result;

use super::{ProjectId, ProjectSource};

/// Context passed to every `MonorepoConfigReader` call.
///
/// `config_roots` is the expansion of `[monorepo].config_roots` from
/// the root mise.toml. When present, readers must restrict their
/// filesystem walks to these subtrees so the project graph mirrors
/// mise's task discovery boundary. When `None`, readers walk the
/// entire monorepo (the deprecated implicit-discovery path).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReaderContext {
    pub monorepo_root: PathBuf,
    pub config_roots: Option<Vec<PathBuf>>,
}

impl ReaderContext {
    pub fn new(monorepo_root: PathBuf, config_roots: Option<Vec<PathBuf>>) -> Self {
        Self {
            monorepo_root,
            config_roots,
        }
    }
}

/// Project information contributed by a single reader during pass 1.
///
/// `path_root` is the absolute filesystem location; the builder converts
/// it to the canonical id via `project_id_from_path`. `foreign_name` is the
/// per-source name (e.g., the value of `name` in `project.json` for Nx,
/// or the `package.json` `name` field for npm/pnpm) used by other readers
/// to refer to this project.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ContributedProject {
    pub source: ProjectSource,
    pub path_root: PathBuf,
    pub foreign_name: Option<String>,
    pub sources: Vec<String>,
    pub tags: Vec<String>,
}

/// An edge contributed by a reader during pass 2.
///
/// References to other projects are by `(ProjectSource, name)` pairs,
/// resolved against the graph's `name_index` populated in pass 1. If a
/// reference cannot be resolved (e.g., the target project doesn't exist
/// in the graph), the edge is dropped with a `trace!` log — readers
/// shouldn't fail the whole build because one cross-reference is wrong,
/// but the user needs a way to see typo'd dependencies during debugging.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ContributedEdge {
    pub from: EdgeEndpoint,
    pub to: EdgeEndpoint,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EdgeEndpoint {
    /// The project at this absolute path. Resolved by exact-path match.
    Path(PathBuf),
    /// Look up the project by its name in a particular source's namespace.
    ForeignName { source: ProjectSource, name: String },
    /// Already-canonical project id; used by the mise reader.
    Id(ProjectId),
}

/// Two-pass reader contract for a monorepo configuration format.
///
/// The builder calls `collect_projects` for every reader first, populates
/// the `name_index`, then calls `collect_edges` for every reader so that
/// cross-format references can be resolved. Readers should be read-only —
/// no `fs::write` or `File::create` calls — to honor R4.
pub trait MonorepoConfigReader: Send + Sync {
    fn id(&self) -> &'static str;

    /// Pass 1: discover projects within `ctx`. Each contribution becomes
    /// a node in the graph (or is merged into an existing node if the
    /// canonical id already exists). Readers honor `ctx.config_roots`
    /// when set so the graph reflects mise's task-discovery boundary.
    fn collect_projects(&self, ctx: &ReaderContext) -> Result<Vec<ContributedProject>>;

    /// Pass 2: emit edges using the graph populated by all readers. The
    /// builder hands in a lookup function that resolves an `EdgeEndpoint`
    /// to a canonical `ProjectId`, returning `None` if the endpoint cannot
    /// be resolved.
    fn collect_edges(
        &self,
        ctx: &ReaderContext,
        graph: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>>;
}

/// Read-only view of the partial graph passed to `collect_edges`.
///
/// Wraps the in-progress `ProjectGraph` so readers can resolve foreign
/// names and check for project existence without holding a full graph
/// reference (and without being able to mutate it).
pub struct ResolutionView<'a> {
    pub(crate) graph: &'a super::ProjectGraph,
}

impl<'a> ResolutionView<'a> {
    pub fn new(graph: &'a super::ProjectGraph) -> Self {
        Self { graph }
    }

    pub fn resolve(&self, endpoint: &EdgeEndpoint) -> Option<ProjectId> {
        match endpoint {
            EdgeEndpoint::Id(id) => self
                .graph
                .by_id
                .contains_key(id)
                .then(|| id.clone()),
            EdgeEndpoint::Path(p) => {
                let monorepo_root = &self.graph.monorepo_root;
                let id = super::project_id_from_path(monorepo_root, p);
                self.graph.by_id.contains_key(&id).then_some(id)
            }
            EdgeEndpoint::ForeignName { source, name } => self
                .graph
                .resolve_by_foreign_name(source, name)
                .cloned(),
        }
    }

    /// Iterate projects whose primary source matches the given format.
    /// Used by readers that need to enumerate by source kind (currently
    /// none in-tree; left in for future plugin readers).
    #[allow(dead_code)]
    pub fn projects_by_source(
        &self,
        source: &ProjectSource,
    ) -> impl Iterator<Item = &super::Project> {
        self.graph
            .projects()
            .filter(move |p| &p.source == source)
    }

    /// Look up a project by canonical id. Used by future plugin readers.
    #[allow(dead_code)]
    pub fn project(&self, id: &ProjectId) -> Option<&super::Project> {
        self.graph.project(id)
    }

    /// All foreign-name → id mappings for a single source. Used by future
    /// plugin readers that want to enumerate all known names.
    #[allow(dead_code)]
    pub fn names_for(&self, source: &ProjectSource) -> BTreeMap<String, ProjectId> {
        let mut map = BTreeMap::new();
        for p in self.graph.projects() {
            if let Some(name) = p.foreign_names.get(source) {
                map.insert(name.clone(), p.id.clone());
            }
        }
        map
    }
}
