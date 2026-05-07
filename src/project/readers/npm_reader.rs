use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::Result;
use serde::Deserialize;

use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ResolutionView,
};

/// Reads npm/yarn-style workspaces: the root `package.json` `workspaces`
/// field declares member globs. Each member is a project in the
/// `NpmWorkspace` namespace.
pub struct NpmReader;

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum WorkspacesField {
    Array(Vec<String>),
    Object {
        #[serde(default)]
        packages: Vec<String>,
    },
    #[default]
    None,
}

#[derive(Debug, Default, Deserialize)]
struct RootPackageJson {
    #[serde(default)]
    workspaces: WorkspacesField,
}

#[derive(Debug, Default, Deserialize)]
struct MemberPackageJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
}

impl NpmReader {
    fn read_root(monorepo_root: &Path) -> Option<RootPackageJson> {
        let body = std::fs::read_to_string(monorepo_root.join("package.json")).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn read_member(path: &Path) -> Option<MemberPackageJson> {
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn workspace_globs(field: &WorkspacesField) -> &[String] {
        match field {
            WorkspacesField::Array(arr) => arr.as_slice(),
            WorkspacesField::Object { packages } => packages.as_slice(),
            WorkspacesField::None => &[],
        }
    }

    fn expand_globs(monorepo_root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for pat in patterns {
            if pat.starts_with('!') {
                continue;
            }
            let abs = monorepo_root.join(pat);
            for entry in glob::glob(abs.to_string_lossy().as_ref())? {
                if let Ok(p) = entry
                    && p.is_dir()
                    && p.join("package.json").is_file()
                {
                    out.push(p);
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

impl MonorepoConfigReader for NpmReader {
    fn id(&self) -> &'static str {
        "npm-workspace"
    }

    fn collect_projects(&self, monorepo_root: &Path) -> Result<Vec<ContributedProject>> {
        let Some(root_pkg) = Self::read_root(monorepo_root) else {
            return Ok(Vec::new());
        };
        let members =
            Self::expand_globs(monorepo_root, Self::workspace_globs(&root_pkg.workspaces))?;
        let mut out = Vec::new();
        for member in members {
            let pkg = Self::read_member(&member.join("package.json"));
            let foreign_name = pkg.as_ref().and_then(|p| p.name.clone());
            out.push(ContributedProject {
                source: ProjectSource::NpmWorkspace,
                path_root: member,
                foreign_name,
                sources: Vec::new(),
                tags: Vec::new(),
            });
        }
        Ok(out)
    }

    fn collect_edges(
        &self,
        monorepo_root: &Path,
        view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let Some(root_pkg) = Self::read_root(monorepo_root) else {
            return Ok(Vec::new());
        };
        let members =
            Self::expand_globs(monorepo_root, Self::workspace_globs(&root_pkg.workspaces))?;
        let mut out = Vec::new();
        for member in members {
            let Some(pkg) = Self::read_member(&member.join("package.json")) else {
                continue;
            };
            let from = EdgeEndpoint::Path(member);
            for deps in [&pkg.dependencies, &pkg.dev_dependencies, &pkg.peer_dependencies] {
                for name in deps.keys() {
                    if view
                        .resolve(&EdgeEndpoint::ForeignName {
                            source: ProjectSource::NpmWorkspace,
                            name: name.clone(),
                        })
                        .is_some()
                    {
                        out.push(ContributedEdge {
                            from: from.clone(),
                            to: EdgeEndpoint::ForeignName {
                                source: ProjectSource::NpmWorkspace,
                                name: name.clone(),
                            },
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}
