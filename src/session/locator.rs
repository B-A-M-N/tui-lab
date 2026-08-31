//! ProjectLocator — project-root discovery independent of VCS (W2.7).
//!
//! The review's concern: an agent repairing a TUI must know *which project*
//! it is working in without being forced to classify the framework first, and
//! without scanning the whole filesystem. This locator does a **bounded
//! upward walk** from the launch cwd looking for a recognizable build
//! manifest (Cargo.toml, package.json, pyproject.toml, go.mod, …), records
//! source roots beside the boundary, and separately (not as the discovery
//! driver) detects an enclosing VCS. An explicit `--project` root short-circuits
//! the walk. All discovery is best-effort and never errors.

use std::path::{Path, PathBuf};

/// Manifest filenames we recognize as project boundaries. Framework-agnostic
/// (not just Cargo) so mono-repos and polyglot sessions discover correctly.
const MANIFEST_NAMES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "mix.exs",
    "CMakeLists.txt",
];

/// Bounded walk depth: discovery must not march to the filesystem root.
const MAX_UPWARD_DEPTH: usize = 16;

/// Common directory names added as `source_roots` when present beside a
/// manifest boundary.
const SOURCE_DIR_NAMES: &[&str] = &["src", "lib", "app", "packages"];

/// Project discovery result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProjectLocator {
    pub explicit_root: Option<String>,
    pub launch_cwd: String,
    pub manifest_roots: Vec<String>,
    pub source_roots: Vec<String>,
    pub vcs: Option<String>,
}

impl ProjectLocator {
    /// Locate the project governing `cwd`. If `explicit` is given it wins and
    /// the walk is skipped (the caller already knows the boundary).
    pub fn locate(explicit: Option<&str>, cwd: &str) -> Self {
        if let Some(root) = explicit {
            let root = Path::new(root);
            return Self {
                explicit_root: Some(root.display().to_string()),
                launch_cwd: cwd.to_string(),
                manifest_roots: vec![root.display().to_string()],
                source_roots: Self::source_roots_under(root),
                vcs: detect_vcs(root),
            };
        }
        let start = Path::new(cwd);
        // Bounded upward walk: the first level that carries a recognized
        // manifest is the boundary; source roots + VCS follow from it.
        let mut boundary: Option<PathBuf> = None;
        let mut cur = start;
        for _ in 0..MAX_UPWARD_DEPTH {
            if MANIFEST_NAMES.iter().any(|m| cur.join(m).is_file()) {
                boundary = Some(cur.to_path_buf());
                break;
            }
            match cur.parent() {
                Some(p) => cur = p,
                None => break,
            }
        }
        let boundary = boundary.unwrap_or_else(|| start.to_path_buf());
        let manifest_roots = if boundary == start && !ManifestAt::is_manifest(&boundary) {
            Vec::new()
        } else {
            vec![boundary.display().to_string()]
        };
        Self {
            explicit_root: None,
            launch_cwd: cwd.to_string(),
            manifest_roots,
            source_roots: Self::source_roots_under(&boundary),
            vcs: detect_vcs(&boundary),
        }
    }

    /// The canonical root this locator resolved (explicit or the found
    /// boundary or the launch cwd as a last resort).
    pub fn root(&self) -> &str {
        self.explicit_root
            .as_deref()
            .or_else(|| self.manifest_roots.first().map(String::as_str))
            .unwrap_or(&self.launch_cwd)
    }

    fn source_roots_under(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for dir in SOURCE_DIR_NAMES {
            let p = root.join(dir);
            if p.is_dir() {
                out.push(p.display().to_string());
            }
        }
        out
    }
}

/// A small helper to re-check "is this path itself a manifest boundary?" so
/// the `boundary == start` case (cwd IS the project root) is handled without
/// re-walking.
struct ManifestAt;
impl ManifestAt {
    fn is_manifest(dir: &Path) -> bool {
        MANIFEST_NAMES.iter().any(|m| dir.join(m).is_file())
    }
}

/// Detect an enclosing VCS marker. Independent of the manifest walk — this is
/// an *additional* provenance signal, never the discovery driver.
fn detect_vcs(root: &Path) -> Option<String> {
    for (marker, name) in [(".git", "git"), (".hg", "hg"), (".svn", "svn")] {
        if root.join(marker).exists() {
            return Some(name.to_string());
        }
        // Manifest sits in a subdir of the VCS root (e.g. `src/<crate>/`).
        if let Some(parent) = root.parent() {
            if parent.join(marker).exists() {
                return Some(name.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_project_with(manifest: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(manifest), "# placeholder").expect("write manifest");
        dir
    }

    #[test]
    fn locates_cargo_project_upward_from_nested_cwd() {
        let dir = tmp_project_with("Cargo.toml");
        // cwd is *inside* src/main — the walk must climb to the Cargo boundary.
        let nested = dir.path().join("src").join("main");
        std::fs::create_dir_all(&nested).expect("nested dirs");
        let loc = ProjectLocator::locate(None, nested.to_str().unwrap());
        assert_eq!(loc.manifest_roots.len(), 1, "{:?}", loc.manifest_roots);
        assert_eq!(
            Path::new(&loc.manifest_roots[0]),
            dir.path(),
            "boundary must be the Cargo.toml dir"
        );
        // source_roots picks up src/ beside the manifest.
        assert!(
            loc.source_roots
                .iter()
                .any(|r| Path::new(r).ends_with("src")),
            "{:?}",
            loc.source_roots
        );
    }

    #[test]
    fn explicit_root_short_circuits_the_walk() {
        let dir = tmp_project_with("Cargo.toml");
        let loc = ProjectLocator::locate(Some(dir.path().to_str().unwrap()), "/tmp/unrelated");
        assert_eq!(loc.explicit_root.as_deref(), Some(dir.path().to_str().unwrap()));
        assert_eq!(loc.manifest_roots, vec![dir.path().display().to_string()]);
    }

    #[test]
    fn walk_is_bounded_and_quiet_in_manifest_less_places() {
        // A tmp dir with no manifest: must return empty roots, not error.
        let bare = tempfile::tempdir().expect("tempdir");
        let loc = ProjectLocator::locate(None, bare.path().to_str().unwrap());
        assert!(loc.manifest_roots.is_empty(), "{:?}", loc.manifest_roots);
        assert_eq!(loc.root(), loc.launch_cwd, "falls back to cwd, never panics");
    }

    #[test]
    fn detects_git_boundary_beside_manifest() {
        let dir = tmp_project_with("Cargo.toml");
        std::fs::create_dir(dir.path().join(".git")).expect("git marker");
        let loc = ProjectLocator::locate(None, dir.path().to_str().unwrap());
        assert_eq!(loc.vcs.as_deref(), Some("git"));
    }
}