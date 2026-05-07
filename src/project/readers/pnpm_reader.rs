use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::Result;
use serde::Deserialize;

use super::{is_safe_glob_pattern, read_optional_file};
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

    /// Expand workspace globs, rejecting absolute or `..`-bearing
    /// patterns and confirming every match canonicalizes inside the
    /// monorepo to defend against malicious workspace declarations.
    fn expand_globs(monorepo_root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
        let canonical_root = monorepo_root.canonicalize().unwrap_or_else(|_| monorepo_root.to_path_buf());
        let mut out = Vec::new();
        for pat in patterns {
            if pat.starts_with('!') {
                continue; // exclusion patterns not modeled yet
            }
            if !is_safe_glob_pattern(pat) {
                warn!(
                    "project reader 'pnpm-workspace': rejecting unsafe workspace glob {pat:?}"
                );
                continue;
            }
            let abs = monorepo_root.join(pat);
            for entry in glob::glob(abs.to_string_lossy().as_ref())? {
                let p = match entry {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("project reader 'pnpm-workspace': skipping unreadable workspace member: {e}");
                        continue;
                    }
                };
                if !p.is_dir() || !p.join("package.json").is_file() {
                    continue;
                }
                let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
                if !canonical.starts_with(&canonical_root) {
                    warn!(
                        "project reader 'pnpm-workspace': rejecting workspace member {} outside monorepo root",
                        canonical.display()
                    );
                    continue;
                }
                out.push(p);
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

    fn collect_projects(&self, ctx: &ReaderContext) -> Result<Vec<ContributedProject>> {
        let monorepo_root = &ctx.monorepo_root;
        let yaml = monorepo_root.join("pnpm-workspace.yaml");
        let Some(workspace) = Self::read_yaml(&yaml) else {
            return Ok(Vec::new());
        };
        let members = Self::expand_globs(monorepo_root, &workspace.packages)?;
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
        let members = Self::expand_globs(monorepo_root, &workspace.packages)?;
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

/// Drop workspace members that don't sit under any of `ctx.config_roots`.
/// Mirrors the npm reader's helper of the same name.
fn filter_members_by_config_roots(
    members: Vec<PathBuf>,
    ctx: &ReaderContext,
) -> Vec<PathBuf> {
    let Some(roots) = ctx.config_roots.as_ref() else {
        return members;
    };
    if roots.is_empty() {
        return members;
    }
    members
        .into_iter()
        .filter(|m| roots.iter().any(|r| m.starts_with(r) || m == r))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_yaml_handles_missing_file() {
        assert!(PnpmReader::read_yaml(Path::new("/nonexistent.yaml")).is_none());
    }

    #[test]
    fn expand_globs_rejects_absolute_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = vec!["/etc/*".to_string()];
        let result = PnpmReader::expand_globs(tmp.path(), &bad).unwrap();
        assert!(result.is_empty(), "absolute glob should be rejected");
    }

    #[test]
    fn expand_globs_rejects_parent_dir_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = vec!["../../etc/*".to_string()];
        let result = PnpmReader::expand_globs(tmp.path(), &bad).unwrap();
        assert!(result.is_empty(), "parent-dir glob should be rejected");
    }

    #[test]
    fn expand_globs_finds_workspace_members() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("packages/foo")).unwrap();
        std::fs::write(
            tmp.path().join("packages/foo/package.json"),
            r#"{"name":"foo"}"#,
        )
        .unwrap();
        let patterns = vec!["packages/*".to_string()];
        let members = PnpmReader::expand_globs(tmp.path(), &patterns).unwrap();
        assert_eq!(members.len(), 1);
        assert!(members[0].ends_with("packages/foo"));
    }
}
