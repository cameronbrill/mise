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
        let body = read_optional_file(&monorepo_root.join("package.json"), "npm-workspace")?;
        match serde_json::from_str::<RootPackageJson>(&body) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!(
                    "project reader 'npm-workspace': could not parse {}: {e}",
                    monorepo_root.join("package.json").display()
                );
                None
            }
        }
    }

    fn read_member(path: &Path) -> Option<MemberPackageJson> {
        let body = read_optional_file(path, "npm-workspace")?;
        match serde_json::from_str::<MemberPackageJson>(&body) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!(
                    "project reader 'npm-workspace': could not parse {}: {e}",
                    path.display()
                );
                None
            }
        }
    }

    fn workspace_globs(field: &WorkspacesField) -> &[String] {
        match field {
            WorkspacesField::Array(arr) => arr.as_slice(),
            WorkspacesField::Object { packages } => packages.as_slice(),
            WorkspacesField::None => &[],
        }
    }

    /// Expand workspace globs, rejecting unsafe patterns (absolute or
    /// `..`) and confirming every match canonicalizes inside the
    /// monorepo to defend against malicious workspace declarations.
    fn expand_globs(monorepo_root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
        let canonical_root = monorepo_root
            .canonicalize()
            .unwrap_or_else(|_| monorepo_root.to_path_buf());
        let mut out = Vec::new();
        for pat in patterns {
            if pat.starts_with('!') {
                continue;
            }
            if !is_safe_glob_pattern(pat) {
                warn!(
                    "project reader 'npm-workspace': rejecting unsafe workspace glob {pat:?}"
                );
                continue;
            }
            let abs = monorepo_root.join(pat);
            for entry in glob::glob(abs.to_string_lossy().as_ref())? {
                let p = match entry {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("project reader 'npm-workspace': skipping unreadable workspace member: {e}");
                        continue;
                    }
                };
                if !p.is_dir() || !p.join("package.json").is_file() {
                    continue;
                }
                let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
                if !canonical.starts_with(&canonical_root) {
                    warn!(
                        "project reader 'npm-workspace': rejecting workspace member {} outside monorepo root",
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

impl MonorepoConfigReader for NpmReader {
    fn id(&self) -> &'static str {
        "npm-workspace"
    }

    fn collect_projects(&self, ctx: &ReaderContext) -> Result<Vec<ContributedProject>> {
        let monorepo_root = &ctx.monorepo_root;
        let Some(root_pkg) = Self::read_root(monorepo_root) else {
            return Ok(Vec::new());
        };
        let members =
            Self::expand_globs(monorepo_root, Self::workspace_globs(&root_pkg.workspaces))?;
        let members = filter_members_by_config_roots(members, ctx);
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
        ctx: &ReaderContext,
        view: &ResolutionView,
    ) -> Result<Vec<ContributedEdge>> {
        let monorepo_root = &ctx.monorepo_root;
        let Some(root_pkg) = Self::read_root(monorepo_root) else {
            return Ok(Vec::new());
        };
        let members =
            Self::expand_globs(monorepo_root, Self::workspace_globs(&root_pkg.workspaces))?;
        let members = filter_members_by_config_roots(members, ctx);
        let mut out = Vec::new();
        for member in members {
            let Some(pkg) = Self::read_member(&member.join("package.json")) else {
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
                        source: ProjectSource::NpmWorkspace,
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
/// When `config_roots` is unset, all members are kept (the deprecated
/// implicit-discovery default).
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
    fn read_root_handles_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(NpmReader::read_root(tmp.path()).is_none());
    }

    #[test]
    fn workspace_globs_supports_array_and_object_forms() {
        let arr = WorkspacesField::Array(vec!["packages/*".into()]);
        assert_eq!(NpmReader::workspace_globs(&arr), &["packages/*"]);
        let obj = WorkspacesField::Object {
            packages: vec!["apps/*".into()],
        };
        assert_eq!(NpmReader::workspace_globs(&obj), &["apps/*"]);
        let none = WorkspacesField::None;
        assert!(NpmReader::workspace_globs(&none).is_empty());
    }

    #[test]
    fn expand_globs_rejects_absolute_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = vec!["/etc/*".to_string()];
        let result = NpmReader::expand_globs(tmp.path(), &bad).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn expand_globs_rejects_parent_dir_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = vec!["../../etc/*".to_string()];
        let result = NpmReader::expand_globs(tmp.path(), &bad).unwrap();
        assert!(result.is_empty());
    }
}
