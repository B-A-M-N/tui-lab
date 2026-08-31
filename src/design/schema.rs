//! The contract model (Wave E items 39–44).
//!
//! A [`ProjectContract`] is a machine-readable description of what the TUI
//! is *supposed* to be: the components it must expose, the interactions its
//! keybindings must produce, the layout it must survive, and the oracle
//! expressions that decide pass/fail. Contracts are authored in YAML next to
//! the app's source and checked against the running process by
//! [`crate::design::conformance`].
//!
//! Unknown fields are rejected (`deny_unknown_fields`): a contract is a
//! validated document, and a typo like `escap_closes_modal` must fail the
//! load, not silently no-op.

use serde::{Deserialize, Serialize};

/// Top-level contract document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectContract {
    /// Identity of the contract itself.
    #[serde(default = "default_schema")]
    pub schema: ContractSchema,
    /// How the target app is launched (contracts can start their own app).
    #[serde(default)]
    pub launch: Option<LaunchContract>,
    /// Viewports the layout must survive without clipping.
    #[serde(default)]
    pub viewports: Vec<ViewportReq>,
    /// Legacy flat keybinding table (kept from contract v0; interactions are
    /// the richer replacement).
    #[serde(default)]
    pub keybindings: Vec<Keybinding>,
    /// Property: Escape dismisses a visible modal.
    #[serde(default)]
    pub escape_closes_modal: bool,
    /// Property: Shift+Tab must exactly reverse Tab.
    #[serde(default)]
    pub reverse_tab_required: bool,
    /// Property: destructive actions require a confirmation step.
    #[serde(default)]
    pub destructive_require_confirmation: bool,
    /// Extra volatile-text regexes merged into the normalization policy
    /// (item 48). These are applied ON TOP of the built-in conservative
    /// classes (clocks, percentages, counters, spinners) when computing
    /// structure hashes.
    #[serde(default)]
    pub volatile_patterns: Vec<String>,
    /// UI components the app must expose (dialogs, tables, trees, …).
    #[serde(default)]
    pub components: Vec<ComponentContract>,
    /// Declared interactions: a key sequence plus the oracles that must hold
    /// after it runs.
    #[serde(default)]
    pub interactions: Vec<InteractionContract>,
    /// Layout constraints (minimum viable viewport, clipping requirements).
    #[serde(default)]
    pub layout: Vec<LayoutConstraint>,
    /// Standalone oracle assertions checked against the running app.
    #[serde(default)]
    pub oracles: Vec<OracleDecl>,
}

fn default_schema() -> ContractSchema {
    ContractSchema {
        name: "unnamed".to_string(),
        version: "1".to_string(),
    }
}

/// Contract identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContractSchema {
    pub name: String,
    pub version: String,
}

/// How to launch the app this contract describes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchContract {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

/// A viewport the layout must survive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewportReq {
    pub cols: u16,
    pub rows: u16,
}

/// Legacy flat keybinding declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Keybinding {
    pub action: String,
    pub keys: Vec<String>,
}

/// A component the app must expose.
///
/// `role` matches one of:
/// * a screen-level component kind — `table`, `tree`, `scrollbar` (matched
///   against [`crate::semantic::components::Component`]);
/// * a region kind — `dialog`, `panel`, `toolbar`, `footer`, `list`
///   (matched against detected regions);
/// * a node role slug — any [`crate::semantic::node::Role`] slug, e.g.
///   `menu`, `command_palette` (matched against the semantic tree).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComponentContract {
    /// Human name used in reports and evidence.
    pub name: String,
    /// Role slug to match (see above).
    pub role: String,
    /// Required components FAIL when absent; optional ones only WARN.
    #[serde(default)]
    pub required: bool,
    /// Extra oracle expressions evaluated when the component IS present
    /// (e.g. a scrollbar must not be at both edges at once). Absent
    /// components skip these rather than double-failing.
    #[serde(default)]
    pub expect: Vec<String>,
}

/// A declared interaction: keys, then the oracles that must hold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractionContract {
    /// Name used in reports and evidence.
    pub name: String,
    /// Key sequence to send, in ergonomic key names ("enter", "escape",
    /// "tab", "ctrl+s") — the same names `tui_act` accepts.
    pub keys: Vec<String>,
    /// Extra context for the reader (e.g. "when a modal is open"). Purely
    /// documentation; the oracles carry the real precondition claims.
    #[serde(default)]
    pub context: Option<String>,
    /// Oracle expressions evaluated after the keys run. Any FAIL fails the
    /// interaction; a WARN with no FAIL downgrades to WARN.
    pub expect: Vec<String>,
}

/// A layout constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayoutConstraint {
    /// Optional name for reports.
    #[serde(default)]
    pub name: Option<String>,
    /// Resize to this minimum and require the layout to still work.
    #[serde(default)]
    pub min_cols: Option<u16>,
    #[serde(default)]
    pub min_rows: Option<u16>,
    /// Require no clipped regions at the constrained size (default true).
    #[serde(default = "default_true")]
    pub no_clipping: bool,
}

fn default_true() -> bool {
    true
}

/// A standalone oracle declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OracleDecl {
    /// Optional id for reports; the expression itself is the fallback id.
    #[serde(default)]
    pub id: Option<String>,
    /// The oracle expression, e.g. `focused("#field/host")`.
    pub expr: String,
}

impl ProjectContract {
    /// The normalization policy this contract declares (item 48): the
    /// built-in conservative volatile classes plus every
    /// `volatile_patterns` entry. Invalid regexes are returned as errors —
    /// contract load fails loudly rather than silently using defaults.
    pub fn normalization_policy(&self) -> Result<crate::screen::NormalizationPolicy, regex::Error> {
        crate::screen::normalize::from_patterns(&self.volatile_patterns)
    }

    /// Static document validation: every oracle expression must parse, every
    /// regex must compile, viewports must be sane, names must not collide.
    /// Returns one result per problem found (empty = valid document).
    pub fn validate(&self) -> Vec<super::conformance::CheckResult> {
        super::conformance::validate_document(self)
    }
}

impl Default for ProjectContract {
    fn default() -> Self {
        ProjectContract {
            schema: default_schema(),
            launch: None,
            viewports: vec![
                ViewportReq { cols: 80, rows: 24 },
                ViewportReq {
                    cols: 100,
                    rows: 30,
                },
                ViewportReq {
                    cols: 120,
                    rows: 40,
                },
            ],
            keybindings: Vec::new(),
            escape_closes_modal: true,
            reverse_tab_required: true,
            destructive_require_confirmation: true,
            volatile_patterns: vec![r"\bCPU \d+%".to_string(), r"\b\d\d:\d\d:\d\d\b".to_string()],
            components: Vec::new(),
            interactions: Vec::new(),
            layout: Vec::new(),
            oracles: Vec::new(),
        }
    }
}
