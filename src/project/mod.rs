use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub mod builder;
pub mod graph;
pub mod reader;
pub mod readers;
pub mod wire;

pub use graph::ProjectGraph;
pub use reader::MonorepoConfigReader;

// Internal-only re-exports. ContributedEdge/ContributedProject and the
// wire format types are stable surface for downstream PRs (project-graph
// plugin protocol, `mise graph`) but no consumer references them through
// `crate::project::*` yet, so re-exporting `pub use` would generate
// unused-import warnings.
pub(crate) use builder::{build_project_graph, default_readers};
#[allow(unused_imports)]
pub(crate) use reader::{ContributedEdge, ContributedProject};
#[allow(unused_imports)]
pub(crate) use wire::{WIRE_SCHEMA_EXPERIMENTAL, WireProjectGraph};

/// Canonical identifier for a project.
///
/// Form: `//<rel-path-from-monorepo-root>` matching mise's existing
/// `//path:task` task-naming convention (see `extract_monorepo_path` in
/// `src/task/mod.rs`). The path uses forward slashes regardless of OS.
pub type ProjectId = String;

/// Source format that contributed a project to the graph.
///
/// Cross-format edge resolution uses `(ProjectSource, name)` keys via the
/// `name_index` on `ProjectGraph`, since most foreign formats reference
/// projects by name rather than path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProjectSource {
    Mise,
    Nx,
    Turbo,
    PnpmWorkspace,
    NpmWorkspace,
    Plugin(String),
}

impl std::fmt::Display for ProjectSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectSource::Mise => f.write_str("mise"),
            ProjectSource::Nx => f.write_str("nx"),
            ProjectSource::Turbo => f.write_str("turbo"),
            ProjectSource::PnpmWorkspace => f.write_str("pnpm-workspace"),
            ProjectSource::NpmWorkspace => f.write_str("npm-workspace"),
            ProjectSource::Plugin(name) => write!(f, "plugin:{name}"),
        }
    }
}

/// A single project node in the monorepo graph.
///
/// `id` is path-derived (`//apps/web`); `foreign_names` carries the
/// per-source aliases (e.g., `(Nx, "web-shell")`, `(NpmWorkspace, "@org/web")`)
/// that other readers use to point at this project. The two-pass builder
/// populates `foreign_names` during pass 1 and consults the graph's
/// `name_index` during pass 2 to resolve edges.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Project {
    pub id: ProjectId,
    pub root: PathBuf,
    pub source: ProjectSource,
    pub sources: Vec<String>,
    pub tags: BTreeSet<String>,
    pub foreign_names: BTreeMap<ProjectSource, String>,
}

impl Project {
    pub fn new(id: ProjectId, root: PathBuf, source: ProjectSource) -> Self {
        Self {
            id,
            root,
            source,
            sources: Vec::new(),
            tags: BTreeSet::new(),
            foreign_names: BTreeMap::new(),
        }
    }
}

/// Compute the canonical project id (`//rel/path`) from a project root and
/// the monorepo root. Always uses forward slashes.
pub fn project_id_from_path(monorepo_root: &std::path::Path, project_root: &std::path::Path) -> ProjectId {
    let rel = project_root
        .strip_prefix(monorepo_root)
        .unwrap_or(project_root);
    let normalized = rel
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    // Strip leading separators so we don't end up with `///abs/path` when
    // `project_root` is absolute and outside the monorepo root (the
    // strip_prefix fallback case).
    let normalized = normalized.trim_start_matches('/');
    if normalized.is_empty() || normalized == "." {
        "//".to_string()
    } else {
        format!("//{normalized}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_id_from_path_basic() {
        let root = PathBuf::from("/tmp/mono");
        let proj = PathBuf::from("/tmp/mono/apps/web");
        assert_eq!(project_id_from_path(&root, &proj), "//apps/web");
    }

    #[test]
    fn project_id_from_path_root_dir() {
        let root = PathBuf::from("/tmp/mono");
        assert_eq!(project_id_from_path(&root, &root), "//");
    }

    #[test]
    fn project_id_from_path_outside_root() {
        let root = PathBuf::from("/tmp/mono");
        let outside = PathBuf::from("/elsewhere/foo");
        assert_eq!(project_id_from_path(&root, &outside), "//elsewhere/foo");
    }

    #[test]
    fn project_source_display() {
        assert_eq!(ProjectSource::Mise.to_string(), "mise");
        assert_eq!(ProjectSource::Nx.to_string(), "nx");
        assert_eq!(
            ProjectSource::Plugin("vfox".into()).to_string(),
            "plugin:vfox"
        );
    }
}
