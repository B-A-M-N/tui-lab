//! tui_session parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

selector_enum!(
    /// `tui_session` action.
    SessionAction;
    [
        Start => "start", Restart => "restart", Stop => "stop", List => "list",
        Status => "status", Lease => "lease", Release => "release",
        Attach => "attach",
    ]
);

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiSessionParams {
    pub action: Known<SessionAction>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// Spawn engine selector. Typed and transport-scoped: attach-only
    /// engines are not here; use `target` with `action=attach`.
    #[serde(default)]
    pub backend: Option<Known<BackendParam>>,
    /// Attach engine selector (`action=attach`), separate from spawn.
    #[serde(default)]
    pub attach_backend: Option<Known<AttachBackendParam>>,
    /// Isolation profile (Wave G item 77): `local` | `clean` | `strict`.
    /// Typed as `Known<IsolationParam>` so an unknown name still reaches the
    /// envelope as invalid_request with the accepted list.
    #[serde(default)]
    pub isolation: Option<Known<IsolationParam>>,
    #[serde(default)]
    pub id: Option<String>,
    /// lease: who takes control (free-form label).
    #[serde(default)]
    pub holder: Option<String>,
    /// lease: time-to-live in milliseconds (default 300000 = 5 min).
    #[serde(default)]
    pub ttl_ms: Option<u64>,
    /// release (finding 4): the lease_id token the lease action returned.
    /// Required to release a LIVE lease — holder labels are shared, so a
    /// tokenless release must not drop a stranger's grant. Expiry needs no
    /// token.
    #[serde(default)]
    pub lease_id: Option<String>,
    /// action=attach (re-review item 18): the tmux target
    /// `session:window.pane` to adopt.
    #[serde(default)]
    pub target: Option<String>,
}

/// Engine vocabulary (re-review P0: exact engine selection). `auto` resolves
/// to the portable PTY engine for screen programs; `cli` and `pipe` pick the
/// line/pipe transports honestly instead of routing everything through the
/// one boolean-ish "backend" string the handler matched ad hoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub enum BackendParam {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "portable_vt100")]
    PortableVt100,
    #[serde(rename = "cli")]
    Cli,
    #[serde(rename = "line_cli")]
    LineCli,
    #[serde(rename = "pipe")]
    Pipe,
}

impl EnumVariants for BackendParam {
    const VARIANTS: &'static [&'static str] =
        &["auto", "portable_vt100", "cli", "line_cli", "pipe"];
}

impl BackendParam {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendParam::Auto => "auto",
            BackendParam::PortableVt100 => "portable_vt100",
            BackendParam::Cli => "cli",
            BackendParam::LineCli => "line_cli",
            BackendParam::Pipe => "pipe",
        }
    }

    /// Resolve to the engine kind (`auto` → portable PTY). Attach targets
    /// use [`SessionAction::Attach`], not this spawn selector.
    pub fn to_kind(self) -> crate::session::state::BackendKind {
        match self {
            BackendParam::Auto | BackendParam::PortableVt100 => {
                crate::session::state::BackendKind::PortableVt
            }
            BackendParam::Cli | BackendParam::LineCli => {
                crate::session::state::BackendKind::PtyLine
            }
            BackendParam::Pipe => crate::session::state::BackendKind::Pipe,
        }
    }
}

impl std::str::FromStr for BackendParam {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(BackendParam::Auto),
            "portable_vt100" => Ok(BackendParam::PortableVt100),
            "cli" => Ok(BackendParam::Cli),
            "line_cli" => Ok(BackendParam::LineCli),
            "pipe" => Ok(BackendParam::Pipe),
            other => Err(format!(
                "unknown backend '{}' (expected one of: {})",
                other,
                Self::VARIANTS.join(", ")
            )),
        }
    }
}

/// Attach engine selector (`action=attach`). Kept separate so a spawn
/// request cannot express an attach-only transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub enum AttachBackendParam {
    #[serde(rename = "tmux")]
    Tmux,
}

impl EnumVariants for AttachBackendParam {
    const VARIANTS: &'static [&'static str] = &["tmux"];
}

impl AttachBackendParam {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttachBackendParam::Tmux => "tmux",
        }
    }
}

impl std::str::FromStr for AttachBackendParam {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tmux" => Ok(AttachBackendParam::Tmux),
            other => Err(format!(
                "unknown attach backend '{}' (expected one of: {})",
                other,
                Self::VARIANTS.join(", ")
            )),
        }
    }
}

/// Isolation profile vocabulary (Wave G item 77).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub enum IsolationParam {
    #[serde(rename = "local")]
    Local,
    #[serde(rename = "clean")]
    Clean,
    #[serde(rename = "strict")]
    Strict,
}

impl EnumVariants for IsolationParam {
    const VARIANTS: &'static [&'static str] = &["local", "clean", "strict"];
}

impl IsolationParam {
    pub fn as_str(&self) -> &'static str {
        match self {
            IsolationParam::Local => "local",
            IsolationParam::Clean => "clean",
            IsolationParam::Strict => "strict",
        }
    }
}

impl From<IsolationParam> for crate::session::isolation::Isolation {
    fn from(p: IsolationParam) -> Self {
        match p {
            IsolationParam::Local => crate::session::isolation::Isolation::Local,
            IsolationParam::Clean => crate::session::isolation::Isolation::Clean,
            IsolationParam::Strict => crate::session::isolation::Isolation::Strict,
        }
    }
}

#[cfg(test)]
mod backend_param_parity_tests {
    use super::*;

    /// Beta-audit P1.6: the schema's VARIANTS, the wire names, and the
    /// FromStr parser must agree — a name the schema advertises MUST
    /// parse. The `tmux` gap (advertised, never parseable) is pinned
    /// here forever.
    #[test]
    fn every_advertised_variant_parses() {
        for v in BackendParam::VARIANTS {
            let parsed: BackendParam = v.parse().unwrap_or_else(|e| panic!("{v} must parse: {e}"));
            assert_eq!(parsed.as_str(), *v, "round-trip for {v}");
        }
    }

    /// And every parseable name is advertised — no hidden vocabulary.
    /// `tmux` must not be a spawn selector; it belongs to attach only.
    #[test]
    fn every_parseable_name_is_advertised() {
        for name in ["auto", "portable_vt100", "cli", "line_cli", "pipe"] {
            assert!(name.parse::<BackendParam>().is_ok(), "{name} must parse");
            assert!(
                BackendParam::VARIANTS.contains(&name),
                "{name} must be advertised"
            );
        }
        assert!("nonsense".parse::<BackendParam>().is_err());
        assert!(
            "tmux".parse::<BackendParam>().is_err(),
            "tmux is attach-only"
        );
        let attach: AttachBackendParam = "tmux".parse().unwrap();
        assert_eq!(attach.as_str(), "tmux");
    }

    /// P2 (reduce schema mirror duplication): `IsolationParam` is the
    /// other hand-written VARIANTS list in this module. Its advertised
    /// set must equal its serde wire names — a variant added to the enum
    /// without updating VARIANTS (or vice versa) breaks the schema's
    /// promise. (BackendParam needs the FromStr legs above because it
    /// hand-parses; IsolationParam only round-trips through serde.)
    #[test]
    fn isolation_param_variants_match_serde_wire_names() {
        let wire: Vec<String> = [
            IsolationParam::Local,
            IsolationParam::Clean,
            IsolationParam::Strict,
        ]
        .iter()
        .map(|v| {
            serde_json::to_value(v)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
        let mut advertised: Vec<String> = IsolationParam::VARIANTS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut sorted_wire = wire.clone();
        advertised.sort();
        sorted_wire.sort();
        assert_eq!(advertised, sorted_wire, "VARIANTS == serde wire names");
        // And every advertised name deserializes.
        for name in IsolationParam::VARIANTS {
            let v: IsolationParam =
                serde_json::from_value(serde_json::Value::String(name.to_string()))
                    .unwrap_or_else(|e| panic!("{name} must deserialize: {e}"));
            assert_eq!(v.as_str(), *name);
        }
    }
}
