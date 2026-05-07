use std::path::Path;

use eyre::Result;
use serde::Deserialize;

use super::{DEFAULT_WALK_DEPTH, read_optional_file, walk_for_markers};
use crate::config::config_file::mise_toml::MiseProjectSection;
use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ReaderContext,
    ResolutionView,
};

/// Reads `[project]` tables from `mise.toml` files across the monorepo.
///
/// The `name` field, if provided, is exposed as the foreign-name in the
/// `Mise` source's namespace so other tooling can reference projects by
/// their mise-declared name. The canonical project id is always
/// path-derived. `depends`/`implicit_dependencies` become edges in
/// pass 2.
pub struct MiseReader;

/// Tolerant subset of `MiseToml` used by the project reader. We deserialize
/// only the `[project]` table (and ignore everything else) so a `mise.toml`
/// using template syntax in unrelated fields still parses cleanly.
#[derive(Debug, Default, Deserialize)]
struct MiseTomlSurface {
    #[serde(default)]
    project: Option<MiseProjectSection>,
}

const MARKERS: &[&str] = &["mise.toml", ".mise.toml"];

impl MiseReader {
    /// Parse the `[project]` section out of a single mise.toml file.
    /// Returns `Ok(None)` for a file without a `[project]` section. TOML
    /// parse failures are logged at `warn!` rather than surfaced — the
    /// main `MiseToml` parser handles structural errors with full
    /// context, and the project reader is best-effort.
    fn parse(path: &Path) -> Option<MiseProjectSection> {
        let body = read_optional_file(path, "mise")?;
        match toml::from_str::<MiseTomlSurface>(&body) {
            Ok(parsed) => parsed.project,
            Err(e) => {
                warn!(
                    "project reader 'mise': could not parse {}: {e} \
                     (the [project] section in this file will not contribute to the graph)",
                    path.display()
                );
                None
            }
        }
    }

    fn read_section(dir: &Path) -> Option<MiseProjectSection> {
        for marker in MARKERS {
            let path = dir.join(marker);
            if let Some(section) = Self::parse(&path) {
                return Some(section);
            }
        }
        None
    }
}

impl MonorepoConfigReader for MiseReader {
    fn id(&self) -> &'static str {
        "mise"
    }

    fn collect_projects(&self, ctx: &ReaderContext) -> Result<Vec<ContributedProject>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(ctx, MARKERS, DEFAULT_WALK_DEPTH)? {
            let Some(section) = Self::read_section(&dir) else {
                continue;
            };
            out.push(ContributedProject {
                source: ProjectSource::Mise,
                path_root: dir,
                foreign_name: section.name,
                sources: section.sources,
                tags: section.tags,
            });
        }
        Ok(out)
    }

    fn collect_edges(
        &self,
        ctx: &ReaderContext,
        view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let mut out = Vec::new();
        for dir in walk_for_markers(ctx, MARKERS, DEFAULT_WALK_DEPTH)? {
            let Some(section) = Self::read_section(&dir) else {
                continue;
            };
            let from = EdgeEndpoint::Path(dir.clone());
            for dep in section
                .depends
                .iter()
                .chain(section.implicit_dependencies.iter())
            {
                let endpoint = if dep.starts_with("//") {
                    EdgeEndpoint::Id(dep.clone())
                } else if view
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
                    EdgeEndpoint::Path(ctx.monorepo_root.join(dep))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn parse_handles_missing_file() {
        assert!(MiseReader::parse(Path::new("/nonexistent/mise.toml")).is_none());
    }

    #[test]
    fn parse_extracts_project_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mise.toml");
        std::fs::write(
            &path,
            r#"
[project]
name = "web"
sources = ["src/**/*.ts"]
tags = ["frontend"]
depends = ["//libs/shared"]
"#,
        )
        .unwrap();
        let section = MiseReader::parse(&path).unwrap();
        assert_eq!(section.name.as_deref(), Some("web"));
        assert_eq!(section.sources, vec!["src/**/*.ts"]);
        let tags: BTreeSet<&str> = section.tags.iter().map(String::as_str).collect();
        assert!(tags.contains("frontend"));
        assert_eq!(section.depends, vec!["//libs/shared"]);
    }

    #[test]
    fn parse_returns_none_for_mise_toml_without_project_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mise.toml");
        std::fs::write(
            &path,
            r#"
[tools]
node = "20"
"#,
        )
        .unwrap();
        assert!(MiseReader::parse(&path).is_none());
    }

    #[test]
    fn parse_tolerates_malformed_toml() {
        // Malformed TOML is logged but doesn't propagate as an error.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mise.toml");
        std::fs::write(&path, "this is :: not toml ===").unwrap();
        assert!(MiseReader::parse(&path).is_none());
    }
}
