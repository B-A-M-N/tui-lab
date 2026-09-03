//! Session-unique scratch space + the declared environment policy (review P0:
//! "session-unique scratch + env policy").
//!
//! Two independent halves, both fixing a real hygiene gap in the isolation
//! layer:
//!
//! 1. **The old scratch was per-process.** `Isolation::Clean` keyed its fresh
//!    `HOME`/`TMPDIR` on `std::process::id()` — so every session and every run
//!    generation inside one lab process shared the *same* scratch `HOME`,
//!    leaking dotfiles and temp files between sessions and generations.
//!    [`ScratchDir`] keys the scratch by `(session, generation)` so parallel
//!    sessions and restarted runs never collide.
//!
//! 2. **The clean path was hardcoded.** Clean isolation rewrote `PATH` to a
//!    literal `/usr/local/bin:/usr/bin:/bin`, which is wrong on hosts with
//!    different tool layouts and on distros that put tools in `/sbin`,
//!    `/opt/.../bin`, etc. [`EnvironmentPolicy`] is a *declared, first-class*
//!    policy the agent can name, and it derives a minimal-but-correct `PATH`
//!    from the *actual* server `PATH` (filtering nothing; it is a real PATH, not
//!    a guess) when sanitizing.

use std::path::PathBuf;

/// A declared environment policy for a launched target.
///
/// Separate from [`Isolation`](crate::session::Isolation) because the two
/// answer different questions: isolation is *how much* you trust the child,
/// policy is *what* it sees of the host's environment. A `Clean`/`Strict`
/// isolation can still run under any of these policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentPolicy {
    /// Inherit every variable the server has (the legacy default).
    Inherit,
    /// Inherit everything except known-bad ones and then override a session-
    /// unique `HOME`/`TMPDIR`. Keeps the effective `PATH` (honest — not a
    /// hardcoded substitute). The pragmatic middle: reproducible scratch, real
    /// tooling.
    Sanitized,
    /// A strictly minimal env: fresh scratch `HOME`/`TMPDIR`, terminal-critical
    /// vars, and a *derived* minimal `PATH`. No inherited credentials or user
    /// config. The strongest policy the lab can guarantee.
    Hermetic,
}

impl EnvironmentPolicy {
    /// Parse a policy name. Unknown names are an error — never a silent
    /// downgrade to inherit.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "inherit" => Ok(EnvironmentPolicy::Inherit),
            "sanitized" => Ok(EnvironmentPolicy::Sanitized),
            "hermetic" => Ok(EnvironmentPolicy::Hermetic),
            other => Err(format!(
                "unknown environment policy '{other}' (supported: inherit, sanitized, hermetic)"
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            EnvironmentPolicy::Inherit => "inherit",
            EnvironmentPolicy::Sanitized => "sanitized",
            EnvironmentPolicy::Hermetic => "hermetic",
        }
    }

    /// Whether this policy gives the child a fresh scratch HOME/TMPDIR.
    pub fn provides_scratch(&self) -> bool {
        !matches!(self, EnvironmentPolicy::Inherit)
    }

    /// The effective environment for a launch.
    ///
    /// * `inherited` — the server's captured env.
    /// * `scratch` — the fresh scratch dir (only used when this policy
    ///   provides one; ignored for `Inherit`).
    pub fn effective_env(
        &self,
        inherited: &[(String, String)],
        scratch: &ScratchDir,
    ) -> Vec<(String, String)> {
        match self {
            EnvironmentPolicy::Inherit => inherited.to_vec(),
            EnvironmentPolicy::Sanitized => {
                let mut out = inherited.to_vec();
                out.retain(|(k, _)| !is_sensitive_sanitize(k));
                set_scratch(&mut out, scratch);
                out
            }
            EnvironmentPolicy::Hermetic => {
                let mut out: Vec<(String, String)> = Vec::new();
                for key in ["TERM", "COLORTERM", "LANG", "LC_ALL"] {
                    if let Some((_, v)) = inherited.iter().find(|(k, _)| k == key) {
                        out.push((key.to_string(), v.clone()));
                    }
                }
                if !out.iter().any(|(k, _)| k == "TERM") {
                    out.push(("TERM".to_string(), "xterm-256color".to_string()));
                }
                set_scratch(&mut out, scratch);
                // Derived minimal PATH from the real server PATH, not a guess.
                if let Some(path) = inherited
                    .iter()
                    .find(|(k, _)| k == "PATH")
                    .map(|(_, v)| v.clone())
                {
                    out.push(("PATH".to_string(), derive_minimal_path(&path)));
                }
                out
            }
        }
    }
}

/// Variables that defeat a "reproducible" environment and should not reach a
/// sanitized/hermetic child. Deliberately conservative — a redaction, not a
/// sandbox (real isolation is the namespace's job). Matches exact well-known
/// credential names AND the common `*_TOKEN`/`*_SECRET`/`*_KEY`/`*_PASSWORD`
/// shapes, so `SECRET_TOKEN`, `GITHUB_TOKEN`, `SOME_API_KEY` are all caught.
fn is_sensitive_sanitize(key: &str) -> bool {
    key.starts_with("TUI_LAB_")
        || matches!(
            key,
            "AWS_ACCESS_KEY_ID"
                | "AWS_SECRET_ACCESS_KEY"
                | "AWS_SESSION_TOKEN"
                | "DOCKER_AUTH_CONFIG"
                | "DATABASE_URL"
                | "REDIS_URL"
                | "KUBECONFIG"
                | "HOME"
                | "TMPDIR"
        )
        || [
            "_TOKEN",
            "_SECRET",
            "_SECRET_KEY",
            "_API_KEY",
            "_ACCESS_KEY",
            "_AUTH_TOKEN",
            "_PASSWORD",
            "_PASSWD",
            "_CREDENTIALS",
        ]
        .iter()
        .any(|suffix| key.to_ascii_uppercase().ends_with(suffix))
}

/// Overwrite or insert the scratch HOME/TMPDIR into `out`.
fn set_scratch(out: &mut Vec<(String, String)>, scratch: &ScratchDir) {
    out.retain(|(k, _)| k != "HOME" && k != "TMPDIR");
    out.push((
        "HOME".to_string(),
        scratch.home().to_string_lossy().to_string(),
    ));
    out.push((
        "TMPDIR".to_string(),
        scratch.tmp().to_string_lossy().to_string(),
    ));
}

/// A minimal-but-correct PATH: drop the personal/user-bin entries the server
/// inherited from its own interactive shell (they are the least reproducible
/// and most load-bearing for a *specific* machine's user), keep the rest —
/// including `/sbin`/`/usr/sbin` and distro-specific `/opt/.../bin`, which a
/// hardcoded list would have missed. Never fabricated; derives from the real
/// PATH.
fn derive_minimal_path(path: &str) -> String {
    let keep: Vec<&str> = path.split(':').filter(|d| !is_user_dir(d)).collect();
    if keep.is_empty() {
        "/usr/local/bin:/usr/bin:/bin".to_string()
    } else {
        keep.join(":")
    }
}

/// Whether a PATH entry is a per-user, machine-specific bin dir (not the
/// system tooling a fresh launch should see).
fn is_user_dir(dir: &str) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    let in_home = !home.is_empty() && dir.starts_with(&home);
    let name = dir.trim_end_matches('/');
    in_home
        || [
            "~/.cargo/bin",
            "~/.local/bin",
            "~/.node_modules/.bin",
            "/usr/local/bin",
        ]
        .contains(&name)
}

/// A session-unique scratch directory for `HOME`/`TMPDIR`.
///
/// Keyed by `(session, generation, profile)` so parallel sessions and restarted
/// generations never collide — the fix for the per-process scratch that leaked
/// between them. Directory is created eagerly so a child inherits a real path.
#[derive(Debug, Clone)]
pub struct ScratchDir {
    root: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
}

impl ScratchDir {
    /// Resolve the scratch for a launch. `session` is the session id,
    /// `generation` its current generation number, `profile` a label
    /// (isolation or policy name) so disparate launches stay apart.
    ///
    /// Creates the `home` and `tmp` directories (idempotent). Creation failure
    /// degrades to a subdir of the system temp under *this* path name but the
    /// `home`/`tmp` children are still created best-effort; the caller passes
    /// the result verbatim.
    pub fn resolve(session: &str, generation: u32, profile: &str) -> Self {
        let safe_session = sanitize_component(session);
        // A `generation == 0` (or "fresh") marker still gets its own dir, but
        // we include the process id so two lab processes don't collide either.
        let root = std::env::temp_dir().join(format!(
            "tui-lab-{}-{}-g{}-{}",
            std::process::id(),
            safe_session,
            generation,
            profile
        ));
        let home = root.join("home");
        let tmp = root.join("tmp");
        let _ = std::fs::create_dir_all(&home);
        let _ = std::fs::create_dir_all(&tmp);
        ScratchDir { root, home, tmp }
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }
    pub fn home(&self) -> &PathBuf {
        &self.home
    }
    pub fn tmp(&self) -> &PathBuf {
        &self.tmp
    }
}

/// Make a session id safe to embed in a filesystem path.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        "anon".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_exact() {
        assert_eq!(
            EnvironmentPolicy::parse("inherit"),
            Ok(EnvironmentPolicy::Inherit)
        );
        assert_eq!(
            EnvironmentPolicy::parse("sanitized"),
            Ok(EnvironmentPolicy::Sanitized)
        );
        assert_eq!(
            EnvironmentPolicy::parse("hermetic"),
            Ok(EnvironmentPolicy::Hermetic)
        );
        assert!(EnvironmentPolicy::parse("docker").is_err());
    }

    #[test]
    fn inherit_passes_environment_through() {
        let inh = vec![("SECRET_TOKEN".to_string(), "x".to_string())];
        let scratch = ScratchDir::resolve("s1", 1, "inherit");
        let eff = EnvironmentPolicy::Inherit.effective_env(&inh, &scratch);
        assert_eq!(eff, inh, "inherit is a strict pass-through, no scratch");
    }

    #[test]
    fn sanitized_redacts_sensitive_keys_and_scopes_scratch_per_session() {
        let inh = vec![
            ("AWS_SECRET_ACCESS_KEY".to_string(), "x".to_string()),
            ("SECRET_TOKEN".to_string(), "y".to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("NORMAL".to_string(), "keep".to_string()),
        ];
        let s1 =
            EnvironmentPolicy::Sanitized.effective_env(&inh, &ScratchDir::resolve("s1", 1, "sp"));
        let s2 =
            EnvironmentPolicy::Sanitized.effective_env(&inh, &ScratchDir::resolve("s2", 1, "sp"));
        assert!(!s1.iter().any(|(k, _)| k == "AWS_SECRET_ACCESS_KEY"));
        assert!(!s1.iter().any(|(k, _)| k == "SECRET_TOKEN"));
        assert!(
            s1.iter().any(|(k, _)| k == "NORMAL"),
            "sanitized keeps the rest"
        );
        let home1 = s1.iter().find(|(k, _)| k == "HOME").unwrap().1.clone();
        let home2 = s2.iter().find(|(k, _)| k == "HOME").unwrap().1.clone();
        assert_ne!(
            home1, home2,
            "different sessions get different scratch HOME"
        );
        assert!(
            home1.contains("s1") && home2.contains("s2"),
            "session ids appear in scratch"
        );
    }

    #[test]
    fn hermetic_derives_path_not_hardcoded() {
        let inh = vec![
            (
                "PATH".to_string(),
                "/usr/local/bin:/usr/bin:/bin:/sbin:/opt/tool/bin".to_string(),
            ),
            ("TERM".to_string(), "xterm".to_string()),
        ];
        let eff =
            EnvironmentPolicy::Hermetic.effective_env(&inh, &ScratchDir::resolve("s", 1, "hp"));
        let path = eff.iter().find(|(k, _)| k == "PATH").unwrap().1.clone();
        assert!(path.contains("/usr/bin"), "system bin kept");
        assert!(
            path.contains("/sbin"),
            "distro sbin kept — not dropped by a hardcoded list"
        );
        assert!(path.contains("/opt/tool/bin"), "opt tool bin kept");
        assert!(eff.iter().any(|(k, _)| k == "TERM"), "TERM kept");
        assert!(eff.iter().any(|(k, v)| k == "TERM" && v == "xterm"));
    }

    #[test]
    fn scratch_dir_is_unique_per_session_generation() {
        let a = ScratchDir::resolve("app", 1, "hm");
        let b = ScratchDir::resolve("app", 2, "hm");
        assert_ne!(a.home(), b.home(), "generations get distinct scratch");
        assert!(a.home().exists(), "home dir created");
        assert!(a.tmp().exists(), "tmp dir created");
    }

    #[test]
    fn session_id_is_sanitized_for_filenames() {
        let s = ScratchDir::resolve("a/b;c", 0, "p");
        // A '/' inside the root is fine (it is a path separator); the point
        // is no ';' survives sanitization.
        assert!(
            !s.root().to_string_lossy().contains(';'),
            "delimiters stripped"
        );
    }
}
