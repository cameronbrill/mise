use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::Result;
use serde::Deserialize;

use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ResolutionView,
};

/// Reads `pnpm-workspace.yaml` and the `package.json` files of its
/// member directories. Each member becomes a project in the
/// `PnpmWorkspace` namespace, keyed by its `package.json` `name`.
/// Cross-package edges come from `dependencies`, `devDependencies`, and
/// `peerDependencies` whose value names another workspace member.
pub struct PnpmReader;

#[derive(Debug, Default, Deserialize)]
struct PnpmWorkspaceYaml {
    #[serde(default)]
    packages: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PackageJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
}

impl PnpmReader {
    fn read_yaml(path: &Path) -> Result<Option<PnpmWorkspaceYaml>> {
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        Ok(serde_yaml::from_str(&body).ok())
    }

    fn read_package_json(path: &Path) -> Option<PackageJson> {
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn expand_globs(monorepo_root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for pat in patterns {
            // Skip negate patterns; we don't model exclusion here.
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

impl MonorepoConfigReader for PnpmReader {
    fn id(&self) -> &'static str {
        "pnpm-workspace"
    }

    fn collect_projects(&self, monorepo_root: &Path) -> Result<Vec<ContributedProject>> {
        let yaml = monorepo_root.join("pnpm-workspace.yaml");
        let Some(workspace) = Self::read_yaml(&yaml)? else {
            return Ok(Vec::new());
        };
        let members = Self::expand_globs(monorepo_root, &workspace.packages)?;
        let mut out = Vec::new();
        for member in members {
            let pkg = Self::read_package_json(&member.join("package.json"));
            let foreign_name = pkg.as_ref().and_then(|p| p.name.clone());
            out.push(ContributedProject {
                source: ProjectSource::PnpmWorkspace,
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
        let yaml = monorepo_root.join("pnpm-workspace.yaml");
        let Some(workspace) = Self::read_yaml(&yaml)? else {
            return Ok(Vec::new());
        };
        let members = Self::expand_globs(monorepo_root, &workspace.packages)?;
        let mut out = Vec::new();
        for member in members {
            let Some(pkg) = Self::read_package_json(&member.join("package.json")) else {
                continue;
            };
            let from = EdgeEndpoint::Path(member);
            for deps in [&pkg.dependencies, &pkg.dev_dependencies, &pkg.peer_dependencies] {
                for name in deps.keys() {
                    if view
                        .resolve(&EdgeEndpoint::ForeignName {
                            source: ProjectSource::PnpmWorkspace,
                            name: name.clone(),
                        })
                        .is_some()
                    {
                        out.push(ContributedEdge {
                            from: from.clone(),
                            to: EdgeEndpoint::ForeignName {
                                source: ProjectSource::PnpmWorkspace,
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
