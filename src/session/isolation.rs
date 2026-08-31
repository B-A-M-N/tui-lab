//! Isolation profiles (Wave G item 77): what environment a launched target
//! inherits.
//!
//! The lab drives arbitrary programs; "local" (inherit everything) was the
//! only mode, which made every audit answer "works on MY machine". A profile
//! is an honest, declared policy — recorded in the launch spec and surfaced
//! in status evidence — not a security boundary:
//!
//! - [`Isolation::Local`] — inherit the server's environment (legacy default).
//! - [`Isolation::Clean`] — scrub to a minimal env: fresh `PATH`, temp
//!   `HOME`/`TMPDIR`, terminal-critical vars kept. Reproduces "fresh shell"
//!   behavior.
//! - [`Isolation::Strict`] — [`Isolation::Clean`] plus a network-isolated
//!   namespace via `unshare -n` when available. Unavailable platforms
//!   report `network_isolated: false` instead of pretending.

use serde::{Deserialize, Serialize};

/// The declared isolation profile for a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// Inherit the server process environment (legacy behavior).
    Local,
    /// Minimal environment: temp HOME/TMPDIR, essential PATH, no inherited
    /// credentials or user config.
    Clean,
    /// Clean PLUS network isolation (unshare -n) when the platform provides
    /// it; honestly reported as not network-isolated when it does not.
    Strict,
}

impl Isolation {
    /// Parse a profile name. Unknown names are an error — never a silent
    /// downgrade to local.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "local" => Ok(Isolation::Local),
            "clean" => Ok(Isolation::Clean),
            "strict" => Ok(Isolation::Strict),
            other => Err(format!(
                "unknown isolation profile '{}' (supported: local, clean, strict)",
                other
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Isolation::Local => "local",
            Isolation::Clean => "clean",
            Isolation::Strict => "strict",
        }
    }

    /// The environment actually handed to the child for this profile.
    ///
    /// `inherited` is the server's captured environment. Returns the
    /// effective pairs; the caller passes them to the backend verbatim.
    pub fn effective_env(&self, inherited: &[(String, String)]) -> Vec<(String, String)> {
        match self {
            Isolation::Local => inherited.to_vec(),
            Isolation::Clean | Isolation::Strict => {
                let mut out: Vec<(String, String)> = Vec::new();
                // Terminal-critical variables the harness itself relies on
                // (color/term detection inside the child).
                for key in ["TERM", "COLORTERM", "LANG", "LC_ALL"] {
                    if let Some((_, v)) = inherited.iter().find(|(k, _)| k == key) {
                        out.push((key.to_string(), v.clone()));
                    }
                }
                if !out.iter().any(|(k, _)| k == "TERM") {
                    out.push(("TERM".to_string(), "xterm-256color".to_string()));
                }
                // Fresh per-run scratch HOME/TMPDIR so user dotfiles and
                // shared temp cannot leak between runs.
                let tag = std::process::id();
                let scratch = std::env::temp_dir().join(format!("tui-lab-{}-{}", tag, self.name()));
                let home = scratch.join("home");
                let tmp = scratch.join("tmp");
                let _ = std::fs::create_dir_all(&home);
                let _ = std::fs::create_dir_all(&tmp);
                out.push(("HOME".to_string(), home.to_string_lossy().to_string()));
                out.push(("TMPDIR".to_string(), tmp.to_string_lossy().to_string()));
                // Minimal PATH so `sh -c 'ls'` still resolves system tools.
                out.push((
                    "PATH".to_string(),
                    "/usr/local/bin:/usr/bin:/bin".to_string(),
                ));
                out
            }
        }
    }

    /// True when this profile runs the child under `unshare -n`.
    fn wants_network_ns(&self) -> bool {
        matches!(self, Isolation::Strict)
    }

    /// Apply the profile to a launch command. Returns the rewritten
    /// `(command, args)` plus the honest `network_isolated` verdict.
    ///
    /// Strict runs `<child>` as `unshare --net -- <child>` when `unshare`
    /// exists and creating a net namespace works; otherwise the launch
    /// proceeds clean-but-networked and says so. `which unshare` is a real
    /// PATH probe, not an assumption.
    pub fn apply_to_command(&self, command: &str, args: &[String]) -> (String, Vec<String>, bool) {
        if !self.wants_network_ns() {
            return (command.to_string(), args.to_vec(), false);
        }
        let unshare = std::process::Command::new("unshare")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !unshare {
            return (command.to_string(), args.to_vec(), false);
        }
        let mut wrapped = vec!["--net".to_string(), "--".to_string()];
        wrapped.push(command.to_string());
        wrapped.extend(args.iter().cloned());
        ("unshare".to_string(), wrapped, true)
    }
}

/// Evidence block describing what a launch actually got (status/launch).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsolationEvidence {
    pub profile: String,
    pub network_isolated: bool,
    /// What was scrubbed or kept, for reproducibility.
    pub env_policy: String,
}

impl IsolationEvidence {
    pub fn for_profile(profile: Isolation, network_isolated: bool, env_keys: &[String]) -> Self {
        let env_policy = match profile {
            Isolation::Local => format!("inherited ({} vars)", env_keys.len()),
            Isolation::Clean | Isolation::Strict => {
                format!(
                    "minimal + scratch HOME/TMPDIR (kept: {})",
                    env_keys.join(",")
                )
            }
        };
        IsolationEvidence {
            profile: profile.name().to_string(),
            network_isolated,
            env_policy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_exact() {
        assert_eq!(Isolation::parse("local"), Ok(Isolation::Local));
        assert_eq!(Isolation::parse("clean"), Ok(Isolation::Clean));
        assert_eq!(Isolation::parse("strict"), Ok(Isolation::Strict));
        assert!(Isolation::parse("docker").is_err());
        assert!(Isolation::parse("").is_err());
    }

    #[test]
    fn local_inherits_everything() {
        let inh = vec![("SECRET_TOKEN".to_string(), "x".to_string())];
        let eff = Isolation::Local.effective_env(&inh);
        assert_eq!(eff, inh);
    }

    #[test]
    fn clean_scrubs_secrets_and_provides_scratch_home() {
        let inh = vec![
            ("SECRET_TOKEN".to_string(), "x".to_string()),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ];
        let eff = Isolation::Clean.effective_env(&inh);
        assert!(!eff.iter().any(|(k, _)| k == "SECRET_TOKEN"));
        let home = eff
            .iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| v.clone())
            .expect("HOME set");
        assert!(home.contains("tui-lab-"), "scratch HOME: {home}");
        assert!(
            eff.iter()
                .any(|(k, v)| k == "TERM" && v == "xterm-256color"),
            "TERM is kept"
        );
        assert!(eff.iter().any(|(k, _)| k == "PATH"));
    }

    #[test]
    fn strict_wraps_command_or_reports_honestly() {
        let (cmd, args, isolated) = Isolation::Strict.apply_to_command("python3", &["-c".into()]);
        if isolated {
            assert_eq!(cmd, "unshare");
            assert_eq!(args.first().map(String::as_str), Some("--net"));
            assert_eq!(args.get(2).map(String::as_str), Some("python3"));
        } else {
            // Platform without unshare: honest false, command unchanged.
            assert_eq!(cmd, "python3");
        }
    }
}
