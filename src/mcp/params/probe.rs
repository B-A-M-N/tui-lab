//! tui_probe parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

/// `tui_probe` stimulus (re-review item 12): the FULL canonical action
/// grammar — the same `TuiActRequest` shapes `tui_act` takes — plus the
/// legacy `{kind: key|type|click|none}` compact form (kept deserializable so
/// existing callers and recorded probes keep working). One action vocabulary
/// across `tui_act`, `tui_probe`, scenario, and exploration: the probe's old
/// local key parser (`{"kind":"key","key":"c","ctrl":true}`) had already
/// drifted from the canonical one, which is exactly the class of divergence
/// this unification removes. `{"action":"none"}` (or `{"kind":"none"}`) is a
/// drift probe.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ProbeStimulus {
    /// A full canonical action request — identical wire shape to `tui_act`.
    Canonical(TuiActRequest),
    /// The legacy compact form.
    Legacy(LegacyStimulus),
}

impl ProbeStimulus {
    /// The canonical action for this stimulus; `None` = drift probe (only
    /// the legacy `none` shape means that — a canonical request always
    /// names a real action).
    pub fn to_action(&self) -> Option<crate::execution::CanonicalAction> {
        match self {
            ProbeStimulus::Canonical(req) => {
                crate::execution::CanonicalAction::from_request(req).ok()
            }
            ProbeStimulus::Legacy(l) => l.to_action(),
        }
    }

    /// The executor guard the stimulus may carry (canonical form only).
    pub fn guard(&self) -> Option<crate::execution::MutationGuard> {
        match self {
            ProbeStimulus::Canonical(req) => req.guard().map(|g| g.to_guard()),
            ProbeStimulus::Legacy(_) => None,
        }
    }

    /// The stimulus's sensitive-input policy (audit P0-3): a canonical
    /// request can carry `sensitive=true`, and that policy must reach the
    /// executor — the probe used to hardcode `InputVisibility::Normal`,
    /// so a probed password was persisted verbatim everywhere `tui_act`
    /// would have redacted it. The legacy form never carries secrets
    /// beyond literal `type` text (which is Normal by the caller's choice).
    pub fn visibility(&self) -> crate::execution::InputVisibility {
        match self {
            ProbeStimulus::Canonical(req) if req.sensitive() => {
                crate::execution::InputVisibility::Sensitive
            }
            _ => crate::execution::InputVisibility::Normal,
        }
    }

    /// Whether this stimulus is a REAL action (drives the TUI) as opposed
    /// to a drift probe. Audit P0-2: real stimuli are machine driving and
    /// must honor the human control lease; drift probes stay observational.
    pub fn drives(&self) -> bool {
        !matches!(
            self,
            ProbeStimulus::Legacy(crate::mcp::params::LegacyStimulus::None)
        )
    }
}

/// `tui_probe` legacy compact stimulus vocabulary. Retained for wire
/// compatibility; new callers send canonical actions.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegacyStimulus {
    /// A single named key, optionally modified (`key: "enter"`,
    /// `key: "ctrl+c"`, `key: "tab"`).
    Key {
        key: String,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
        #[serde(default)]
        shift: bool,
    },
    /// Type literal text.
    Type { text: String },
    /// Mouse click at cell coordinates.
    Click {
        button: MouseButtonParam,
        x: u16,
        y: u16,
    },
    /// No stimulus — observe drift between two settled frames.
    None,
}

impl LegacyStimulus {
    /// Convert to the canonical action (None stays None at the caller).
    pub fn to_action(&self) -> Option<crate::execution::CanonicalAction> {
        use crate::backend::{KeyModifiers, MouseButton};
        use crate::execution::CanonicalAction as CA;
        match self {
            LegacyStimulus::None => None,
            LegacyStimulus::Key {
                key,
                ctrl,
                alt,
                shift,
            } => {
                let mut mods = KeyModifiers::empty();
                if *ctrl {
                    mods |= KeyModifiers::CTRL;
                }
                if *alt {
                    mods |= KeyModifiers::ALT;
                }
                if *shift {
                    mods |= KeyModifiers::SHIFT;
                }
                let code = parse_key_name(key)?;
                Some(CA::Key {
                    key: crate::backend::KeyEvent {
                        code,
                        modifiers: mods,
                    },
                })
            }
            LegacyStimulus::Type { text } => Some(CA::Type { text: text.clone() }),
            LegacyStimulus::Click { button, x, y } => Some(CA::MouseClick {
                button: match button {
                    MouseButtonParam::Left => MouseButton::Left,
                    MouseButtonParam::Middle => MouseButton::Middle,
                    MouseButtonParam::Right => MouseButton::Right,
                },
                x: *x,
                y: *y,
            }),
        }
    }
}

/// Key-name parser for the probe stimulus vocabulary (the ergonomic subset:
/// named keys + single characters).
fn parse_key_name(name: &str) -> Option<crate::backend::KeyCode> {
    use crate::backend::KeyCode;
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "escape" | "esc" => KeyCode::Escape,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        other => {
            let mut chars = other.chars();
            let single = chars.next()?;
            if chars.next().is_none() {
                KeyCode::Char(single)
            } else {
                return None;
            }
        }
    })
}

// `tui_probe` completion vocabulary: how the probe decides "after".
selector_enum!(
    /// Probe completion (a deliberately small closed set of the canonical
    /// [`crate::capture::CompletionPolicy`] shapes an experiment needs).
    ProbeCompletion;
    [
        Stable => "stable", FirstChange => "first_change",
        AnyChange => "any_change", TextAppears => "text_appears",
        TextDisappears => "text_disappears", ProcessExit => "process_exit",
        SemanticChange => "semantic_change", MayBeSilent => "may_be_silent",
    ]
);

impl ProbeCompletion {
    /// Convert to the canonical completion policy.
    pub fn to_policy(&self, text: Option<&str>) -> Option<crate::capture::CompletionPolicy> {
        use crate::capture::CompletionPolicy as CP;
        Some(match self {
            ProbeCompletion::Stable => CP::StableScreen,
            ProbeCompletion::FirstChange => CP::FirstScreenChange,
            ProbeCompletion::AnyChange => CP::AnyObservableChange,
            ProbeCompletion::TextAppears => CP::TextAppears(text?.to_string()),
            ProbeCompletion::TextDisappears => CP::TextDisappears(text?.to_string()),
            ProbeCompletion::ProcessExit => CP::ProcessExit,
            ProbeCompletion::SemanticChange => CP::SemanticChange,
            ProbeCompletion::MayBeSilent => CP::MayBeSilent,
        })
    }
}

/// Parameters for `tui_probe` (re-review Wave-2: the troubleshooting
/// primitive is an agent-visible tool — "try this and tell me EVERYTHING
/// materially different", with causal event scoping and the settled frame).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiProbeParams {
    /// The experiment: any canonical action (`{"action": ...}` — the exact
    /// `tui_act` grammar) or the legacy compact `{"kind": ...}` shape;
    /// omit (or `{"kind":"none"}`) for a drift probe.
    #[serde(default)]
    pub stimulus: Option<ProbeStimulus>,
    /// How "after" is decided. Either the legacy bare name
    /// (`"stable"`, `"text_appears"`, …) or a capture spec object
    /// (re-review item 13): `{"strategy":"frames","count":8}` to grab the
    /// first N frames of a transition ("press Enter and show me the first
    /// 8 frames"), `{"strategy":"after_duration","ms":250}` to sample the
    /// screen a fixed interval after the stimulus.
    #[serde(default)]
    pub completion: Option<Known<ProbeCompletion>>,
    /// The structured capture spec (item 13). Takes precedence over
    /// `completion` when present.
    #[serde(default)]
    pub capture: Option<ProbeCapture>,
    /// Required text for `text_appears` / `text_disappears`.
    #[serde(default)]
    pub text: Option<String>,
    /// Which watched aspects to surface as material changes. Defaults to focus,
    /// controls, regions, cursor.
    #[serde(default)]
    pub watch: Option<Vec<Known<ProbeWatchParam>>>,
    /// Quiet interval for `stable` (ms). Default 120.
    #[serde(default)]
    pub quiet_ms: Option<u64>,
    /// Overall ceiling so a never-settling TUI still returns. Default 5000.
    #[serde(default)]
    pub budget_ms: Option<u64>,
    #[serde(default)]
    pub id: Option<String>,
}

/// The probe's capture strategies (re-review item 13): how the after-side
/// of the experiment is collected. The completion names decide *when the
/// action is done*; the frames/duration strategies decide *what to record*
/// — "the first N frames of the transition" is a capture question, not a
/// completion question, and the old enum could not ask it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum ProbeCapture {
    /// Collect the first `count` distinct frames after the stimulus
    /// (the terminal microscope: flicker/double-draw diagnosis).
    Frames {
        /// How many distinct post-stimulus frames to record.
        count: usize,
    },
    /// Let the stimulus settle, then sample one frame after `ms`.
    AfterDuration {
        /// Delay between the stimulus and the sampled frame.
        ms: u64,
    },
}

impl ProbeCapture {
    /// The capture's budget share of the overall probe budget (frames
    /// want most of it; a duration sample needs only its delay plus
    /// settle slack).
    pub fn budget_hint(&self, total_ms: u64) -> u64 {
        match self {
            ProbeCapture::Frames { .. } => total_ms,
            ProbeCapture::AfterDuration { ms } => ms.saturating_add(1000).min(total_ms),
        }
    }
}

selector_enum!(
    /// Watched probe aspects.
    ProbeWatchParam;
    [ Cursor => "cursor", Focus => "focus", Style => "style",
      Controls => "controls", Regions => "regions", Process => "process" ]
);

impl ProbeWatchParam {
    /// Convert to the diagnostic watch aspect.
    pub fn to_watch(self) -> crate::diagnostic::ProbeWatch {
        use crate::diagnostic::ProbeWatch as PW;
        match self {
            ProbeWatchParam::Cursor => PW::Cursor,
            ProbeWatchParam::Focus => PW::Focus,
            ProbeWatchParam::Style => PW::Style,
            ProbeWatchParam::Controls => PW::Controls,
            ProbeWatchParam::Regions => PW::Regions,
            ProbeWatchParam::Process => PW::Process,
        }
    }
}
