use std::collections::BTreeMap;
use std::path::Path;

use eyre::Result;
use serde::Deserialize;

use super::{expand_workspace_globs, filter_members_by_config_roots, read_optional_file};
use crate::project::ProjectSource;
use crate::project::reader::{
    ContributedEdge, ContributedProject, EdgeEndpoint, MonorepoConfigReader, ReaderContext,
    ResolutionView,
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
    fn read_yaml(path: &Path) -> Option<PnpmWorkspaceYaml> {
        let body = read_optional_file(path, "pnpm-workspace")?;
        match serde_yaml::from_str::<PnpmWorkspaceYaml>(&body) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!(
                    "project reader 'pnpm-workspace': could not parse {}: {e}",
                    path.display()
                );
                None
            }
        }
    }

    fn read_package_json(path: &Path) -> Option<PackageJson> {
        let body = read_optional_file(path, "pnpm-workspace")?;
        match serde_json::from_str::<PackageJson>(&body) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!(
                    "project reader 'pnpm-workspace': could not parse {}: {e}",
                    path.display()
                );
                None
            }
        }
    }

}

impl MonorepoConfigReader for PnpmReader {
    fn id(&self) -> &'static str {
        "pnpm-workspace"
    }

    fn collect_projects(&self, ctx: &ReaderContext) -> Result<Vec<ContributedProject>> {
        let monorepo_root = &ctx.monorepo_root;
        let yaml = monorepo_root.join("pnpm-workspace.yaml");
        let Some(workspace) = Self::read_yaml(&yaml) else {
            return Ok(Vec::new());
        };
        let members =
            expand_workspace_globs(monorepo_root, &workspace.packages, "pnpm-workspace")?;
        let members = filter_members_by_config_roots(members, ctx);
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
        ctx: &ReaderContext,
        view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let monorepo_root = &ctx.monorepo_root;
        let yaml = monorepo_root.join("pnpm-workspace.yaml");
        let Some(workspace) = Self::read_yaml(&yaml) else {
            return Ok(Vec::new());
        };
        let members =
            expand_workspace_globs(monorepo_root, &workspace.packages, "pnpm-workspace")?;
        let members = filter_members_by_config_roots(members, ctx);
        let mut out = Vec::new();
        for member in members {
            let Some(pkg) = Self::read_package_json(&member.join("package.json")) else {
                continue;
            };
            let from = EdgeEndpoint::Path(member);
            for deps in [
                &pkg.dependencies,
                &pkg.dev_dependencies,
                &pkg.peer_dependencies,
            ] {
                for name in deps.keys() {
                    let to = EdgeEndpoint::ForeignName {
                        source: ProjectSource::PnpmWorkspace,
                        name: name.clone(),
                    };
                    if view.resolve(&to).is_some() {
                        out.push(ContributedEdge {
                            from: from.clone(),
                            to,
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_yaml_handles_missing_file() {
        assert!(PnpmReader::read_yaml(Path::new("/nonexistent.yaml")).is_none());
    }
}
