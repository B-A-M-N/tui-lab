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
//!   namespace via `unshare -n`. Beta-audit P0.8: strict FAILS CLOSED —
//!   if the network namespace cannot be proven (no `unshare`, or the
//!   net-ns operation is not permitted), the launch is REFUSED, not run
//!   networked. An option called strict never silently degrades.

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
    /// Clean PLUS a PROVEN network-isolated namespace (`unshare --net`).
    /// If isolation cannot be proven at launch, the launch fails — it is
    /// never downgraded to clean-but-networked.
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
    /// `session_id` and `generation` key the scratch `HOME`/`TMPDIR` so
    /// parallel sessions / restarted generations never collide (review P0:
    /// session-unique scratch; the old per-process scratch leaked between
    /// them). `inherited` is the server's captured environment. The effective
    /// policy is spelled out in [`EnvironmentPolicy`] and applies here:
    /// Local inherits, Clean/Strict run under `Hermetic` (fresh scratch + a
    /// derived minimal PATH — honest, not a hardcoded list).
    pub fn effective_env(
        &self,
        session_id: &str,
        generation: u32,
        inherited: &[(String, String)],
    ) -> Vec<(String, String)> {
        use crate::session::scratch::{EnvironmentPolicy, ScratchDir};
        let scratch = ScratchDir::resolve(session_id, generation, self.name());
        let policy = match self {
            Isolation::Local => EnvironmentPolicy::Inherit,
            Isolation::Clean | Isolation::Strict => EnvironmentPolicy::Hermetic,
        };
        policy.effective_env(inherited, &scratch)
    }

    /// True when this profile runs the child under `unshare -n`.
    fn wants_network_ns(&self) -> bool {
        matches!(self, Isolation::Strict)
    }

    /// Apply the profile to a launch command. On success returns the
    /// rewritten `(command, args)`, whether the `unshare` wrapper is in
    /// use, and the network-isolation verdict.
    ///
    /// Strict runs `<child>` as `unshare --net -- <child>` ONLY when
    /// `unshare` is present AND an actual net namespace can be created —
    /// and, since beta-audit P0.8, REFUSES the launch otherwise. Merely
    /// checking that the `unshare` executable exists is not evidence of
    /// isolation — a host may have `unshare` installed while
    /// `unshare --net` fails with `Operation not permitted` (containers,
    /// hardened kernels, seccomp). So we preflight the exact namespace
    /// operation (`unshare --net true`):
    ///
    /// - executable absent → `Err` (fail closed; no unshare means no way
    ///   to prove isolation).
    /// - executable present, probe succeeds → `Verified`; the child is
    ///   wrapped.
    /// - executable present, probe fails (e.g. not permitted) → `Err`
    ///   (fail closed; the old behavior launched the child networked and
    ///   reported `Failed` afterward — observability is not enforcement).
    ///
    /// Callers that want best-effort behavior on such hosts should ask
    /// for `clean` explicitly — a visible, deliberate downgrade, never a
    /// silent one.
    pub fn apply_to_command(
        &self,
        command: &str,
        args: &[String],
    ) -> anyhow::Result<(String, Vec<String>, bool, VerifiedState)> {
        if !self.wants_network_ns() {
            return Ok((
                command.to_string(),
                args.to_vec(),
                false,
                VerifiedState::NotApplied,
            ));
        }
        // Presence of the executable: a cheap upper bound. Not proof.
        let present = std::process::Command::new("unshare")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !present {
            return Err(anyhow::anyhow!(
                "isolation=strict cannot be proven: no 'unshare' executable on PATH, \
                 so a network namespace cannot be created. The launch is REFUSED \
                 (strict never runs networked). Use isolation=clean for an \
                 env-scrubbed launch without network isolation."
            ));
        }
        // Preflight the exact operation. If it cannot run (not permitted),
        // isolation is impossible — refuse rather than launch networked.
        let probe_ok = std::process::Command::new("unshare")
            .arg("--net")
            .arg("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !probe_ok {
            return Err(anyhow::anyhow!(
                "isolation=strict cannot be proven: 'unshare --net' is not permitted \
                 on this host (container/hardened kernel/seccomp). The launch is \
                 REFUSED (strict never runs networked). Use isolation=clean for an \
                 env-scrubbed launch without network isolation."
            ));
        }
        let mut wrapped = vec!["--net".to_string(), "--".to_string()];
        wrapped.push(command.to_string());
        wrapped.extend(args.iter().cloned());
        Ok((
            "unshare".to_string(),
            wrapped,
            true,
            VerifiedState::Verified,
        ))
    }
}

/// Evidence block describing what a launch actually got (status/launch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifiedState {
    Verified,
    NotApplied,
    Unverified,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsolationEvidence {
    pub profile: String,
    pub requested: bool,
    pub wrapper_available: bool,
    pub launch_succeeded: bool,
    pub network_isolated: VerifiedState,
    pub env_policy: String,
}

impl IsolationEvidence {
    /// Build the evidence block from what actually happened at launch.
    ///
    /// `requested` is whether the caller asked for a namespace profile
    /// (`Strict`); `wrapper_available` and `network_isolated` come from
    /// `apply_to_command`'s pre-launch probe; `launch_succeeded` is the one
    /// fact only the caller knows — it sets it after `backend.start()`.
    pub fn for_profile(
        profile: Isolation,
        requested: bool,
        wrapper_available: bool,
        network_isolated: VerifiedState,
        launch_succeeded: bool,
        env_keys: &[String],
    ) -> Self {
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
            requested,
            wrapper_available,
            launch_succeeded,
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
        let eff = Isolation::Local.effective_env("s1", 1, &inh);
        assert_eq!(eff, inh);
    }

    #[test]
    fn clean_scrubs_secrets_and_provides_scratch_home() {
        let inh = vec![
            ("SECRET_TOKEN".to_string(), "x".to_string()),
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ];
        let eff = Isolation::Clean.effective_env("s1", 1, &inh);
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

    /// Beta-audit P0.8: strict FAILS CLOSED. Depending on the host, the
    /// preflight either proves the net namespace (wrapped launch) or the
    /// launch is REFUSED with an error that names the clean downgrade.
    /// There is no outcome in which strict launches the child networked.
    #[test]
    fn strict_fails_closed_never_launches_networked() {
        match Isolation::Strict.apply_to_command("python3", &["-c".into()]) {
            Ok((cmd, args, wrapper_available, state)) => {
                // The probe actually created a net namespace, so we wrapped.
                assert!(wrapper_available);
                assert_eq!(state, VerifiedState::Verified);
                assert_eq!(cmd, "unshare");
                assert_eq!(args.first().map(String::as_str), Some("--net"));
                assert_eq!(args.get(2).map(String::as_str), Some("python3"));
            }
            Err(e) => {
                // Refusal: the message names the proof failure AND the
                // explicit clean downgrade.
                let msg = format!("{e}");
                assert!(
                    msg.contains("cannot be proven") && msg.contains("REFUSED"),
                    "refusal names the failure: {msg}"
                );
                assert!(
                    msg.contains("isolation=clean"),
                    "refusal names the explicit downgrade: {msg}"
                );
            }
        }
    }

    /// The refusal branches are exhaustive over the failure modes; pin
    /// the verified wrap for local/clean (unchanged) and that Failed /
    /// NotApplied can no longer reach a strict launch.
    #[test]
    fn local_and_clean_are_unchanged_by_fail_closed() {
        let (cmd, args, wrapped, state) = Isolation::Clean
            .apply_to_command("sh", &["-c".into()])
            .unwrap();
        assert_eq!(cmd, "sh");
        assert_eq!(args, vec!["-c".to_string()]);
        assert!(!wrapped);
        assert_eq!(state, VerifiedState::NotApplied);
        let (cmd, args, wrapped, state) = Isolation::Local.apply_to_command("sh", &[]).unwrap();
        assert_eq!(cmd, "sh");
        assert!(args.is_empty());
        assert!(!wrapped);
        assert_eq!(state, VerifiedState::NotApplied);
    }

    #[test]
    fn local_never_wraps() {
        let (cmd, args, wrapper_available, state) =
            Isolation::Local.apply_to_command("sh", &[]).unwrap();
        assert_eq!(cmd, "sh");
        assert!(args.is_empty());
        assert!(!wrapper_available);
        assert_eq!(state, VerifiedState::NotApplied);
    }
}
