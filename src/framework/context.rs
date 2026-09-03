//! Project context resolution (re-review item 34).
//!
//! Detection used to run against a single directory. Real projects are
//! nested: a workspace root holds the lockfiles and shared manifests while
//! the actual TUI crate/app lives one or two levels down. Guessing "the
//! directory I was handed is the package" misfires in both directions —
//! a workspace root hides the package's deps (they live in member
//! manifests), and a package dir hides the lockfile evidence (it lives at
//! the root).
//!
//! [`ProjectContext`] resolves BOTH roots for any starting directory:
//!
//! ```text
//! walk upward  → workspace_root  (Cargo.lock, root package.json with
//!                                 workspaces, go.work, pyproject with
//!                                 tool.uv.workspace …)
//! walk inward  → package_root    (the nearest manifest naming a known
//!                                 TUI framework)
//! ```

use std::path::{Path, PathBuf};

/// The resolved shape of the project a target directory belongs to.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectContext {
    /// The directory detection was asked about.
    pub requested_dir: String,
    /// The package root: the directory whose manifest directly declares
    /// the app's dependencies. Equals the requested dir when it holds a
    /// manifest; otherwise the nearest ancestor that does (or None — a
    /// bare source tree with no manifest).
    pub package_root: Option<String>,
    /// The workspace/monorepo root: the nearest ancestor holding a
    /// workspace-level file (Cargo.lock, go.work, package.json with a
    /// `workspaces` key, pyproject with tool.uv.workspace). May equal
    /// `package_root`; None when no ancestor qualifies.
    pub workspace_root: Option<String>,
    /// Which manifest anchored `package_root` (`Cargo.toml`).
    pub package_manifest: Option<String>,
    /// Which file anchored `workspace_root` (`Cargo.lock`).
    pub workspace_anchor: Option<String>,
}

/// Manifests that directly declare dependencies (a package root).
const PACKAGE_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
];

/// Files that indicate a workspace/monorepo boundary.
const WORKSPACE_ANCHORS: &[&str] = &[
    "Cargo.lock",
    "go.work",
    "go.sum",
    "pnpm-workspace.yaml",
    "lerna.json",
];

/// True when a `package.json` declares a `workspaces` key (npm/yarn/pnpm
/// workspaces make the manifest itself the workspace anchor).
fn package_json_has_workspaces(dir: &Path) -> bool {
    let txt = std::fs::read_to_string(dir.join("package.json")).unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&txt)
        .ok()
        .and_then(|v| v.get("workspaces").cloned())
        .map(|w| w.is_object() || w.is_array())
        .unwrap_or(false)
}

/// True when a `pyproject.toml` declares a uv/pep workspace table.
fn pyproject_has_workspace(dir: &Path) -> bool {
    let txt = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap_or_default();
    txt.parse::<toml::Value>()
        .ok()
        .and_then(|v| {
            v.get("tool")
                .and_then(|t| t.get("uv"))
                .and_then(|u| u.get("workspace"))
                .cloned()
        })
        .is_some()
}

impl ProjectContext {
    /// Resolve the project shape for `dir` (the directory detection was
    /// asked about). Never fails: an unresolvable tree reports `None`
    /// roots honestly rather than guessing.
    pub fn resolve(dir: &str) -> ProjectContext {
        let start = PathBuf::from(dir);
        let requested = start.clone();

        // Package root: the start dir if it has a manifest, else the
        // nearest ANCESTOR that does (a src/ subdirectory belongs to the
        // crate above it).
        let mut package_root: Option<(PathBuf, &'static str)> = None;
        let mut probe = Some(start.as_path());
        while let Some(d) = probe {
            if let Some(m) = PACKAGE_MANIFESTS.iter().find(|m| d.join(m).is_file()) {
                package_root = Some((d.to_path_buf(), m));
                break;
            }
            probe = d.parent();
        }

        // Workspace root: the nearest ancestor (inclusive) with a
        // workspace anchor or a workspace-declaring manifest.
        let mut workspace_root: Option<(PathBuf, String)> = None;
        let mut probe = Some(start.as_path());
        while let Some(d) = probe {
            let mut anchor: Option<String> = WORKSPACE_ANCHORS
                .iter()
                .find(|a| d.join(a).is_file())
                .map(|a| a.to_string());
            if anchor.is_none() && package_json_has_workspaces(d) {
                anchor = Some("package.json (workspaces)".into());
            }
            if anchor.is_none() && pyproject_has_workspace(d) {
                anchor = Some("pyproject.toml (tool.uv.workspace)".into());
            }
            if let Some(a) = anchor {
                workspace_root = Some((d.to_path_buf(), a));
                break;
            }
            probe = d.parent();
        }

        ProjectContext {
            requested_dir: requested.to_string_lossy().to_string(),
            package_root: package_root
                .as_ref()
                .map(|(p, _)| p.to_string_lossy().to_string()),
            workspace_root: workspace_root
                .as_ref()
                .map(|(p, _)| p.to_string_lossy().to_string()),
            package_manifest: package_root.map(|(_, m)| m.to_string()),
            workspace_anchor: workspace_root.map(|(_, a)| a),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn flat_project_roots_are_the_same_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"a\"\n").unwrap();
        let ctx = ProjectContext::resolve(&dir.path().to_string_lossy());
        assert_eq!(
            ctx.package_root.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
        assert_eq!(ctx.package_manifest.as_deref(), Some("Cargo.toml"));
        // No lockfile → no workspace claim.
        assert!(ctx.workspace_root.is_none());
    }

    #[test]
    fn subdirectory_finds_package_root_above_it() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{\"name\":\"a\"}").unwrap();
        let sub = dir.path().join("src").join("ui");
        fs::create_dir_all(&sub).unwrap();
        let ctx = ProjectContext::resolve(&sub.to_string_lossy());
        assert_eq!(
            ctx.package_root.as_deref(),
            Some(dir.path().to_str().unwrap()),
            "a source subdirectory belongs to the manifest above it"
        );
    }

    #[test]
    fn workspace_root_found_from_member_crate() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("Cargo.lock"), "").unwrap();
        let member = dir.path().join("crates").join("app");
        fs::create_dir_all(&member).unwrap();
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname=\"app\"\n[dependencies]\nratatui=\"0.26\"\n",
        )
        .unwrap();
        let ctx = ProjectContext::resolve(&member.to_string_lossy());
        assert_eq!(ctx.package_root.as_deref(), Some(member.to_str().unwrap()));
        assert_eq!(
            ctx.workspace_root.as_deref(),
            Some(dir.path().to_str().unwrap()),
            "the lockfile at the repo root anchors the workspace"
        );
        assert_eq!(ctx.workspace_anchor.as_deref(), Some("Cargo.lock"));
        assert_ne!(
            ctx.package_root, ctx.workspace_root,
            "member crate and workspace root are distinct roots"
        );
    }

    #[test]
    fn npm_workspaces_anchor_at_the_manifest() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(
            dir.path().join("package.json"),
            "{\"name\":\"monorepo\",\"workspaces\":[\"packages/*\"]}",
        )
        .unwrap();
        let pkg = dir.path().join("packages").join("tui");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("package.json"), "{\"name\":\"tui\"}").unwrap();
        let ctx = ProjectContext::resolve(&pkg.to_string_lossy());
        assert_eq!(ctx.package_root.as_deref(), Some(pkg.to_str().unwrap()));
        assert_eq!(
            ctx.workspace_root.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
        assert!(ctx.workspace_anchor.unwrap().contains("workspaces"));
    }

    #[test]
    fn unresolvable_tree_is_honest() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("deep").join("tree");
        fs::create_dir_all(&sub).unwrap();
        let ctx = ProjectContext::resolve(&sub.to_string_lossy());
        assert!(ctx.package_root.is_none());
        assert!(ctx.workspace_root.is_none());
    }
}
