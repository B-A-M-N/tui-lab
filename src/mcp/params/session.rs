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
    /// Engine selector (re-review P0): typed as `Known<BackendParam>` so an
    /// unknown name still reaches the envelope as `invalid_request` with the
    /// accepted list, matching the isolation param's contract.
    #[serde(default)]
    pub backend: Option<Known<BackendParam>>,
    /// Isolation profile (Wave G item 77): `local` | `clean` | `strict`.
    /// Typed as Known<IsolationParam> so an unknown name still reaches the
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
    #[serde(rename = "tmux")]
    Tmux,
}

impl EnumVariants for BackendParam {
    const VARIANTS: &'static [&'static str] =
        &["auto", "portable_vt100", "cli", "line_cli", "pipe", "tmux"];
}

impl BackendParam {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendParam::Auto => "auto",
            BackendParam::PortableVt100 => "portable_vt100",
            BackendParam::Cli => "cli",
            BackendParam::LineCli => "line_cli",
            BackendParam::Pipe => "pipe",
            BackendParam::Tmux => "tmux",
        }
    }

    /// Resolve to the engine kind (`auto` → portable PTY).
    pub fn to_kind(self) -> crate::session::state::BackendKind {
        match self {
            BackendParam::Auto | BackendParam::PortableVt100 => {
                crate::session::state::BackendKind::PortableVt
            }
            BackendParam::Cli | BackendParam::LineCli => {
                crate::session::state::BackendKind::PtyLine
            }
            BackendParam::Pipe => crate::session::state::BackendKind::Pipe,
            BackendParam::Tmux => crate::session::state::BackendKind::TmuxAttach,
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
