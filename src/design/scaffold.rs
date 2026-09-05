//! Multi-state contract scaffolding (finding 37).
//!
//! [`ProjectContract::scaffold_from`] builds a starter contract from ONE
//! observed frame — honest, but insufficient to derive a TUI
//! specification. This module gathers states the way the finding asks:
//! a bounded, explicitly-SAFE pass over the live app (initial screen,
//! focus order, common viewport sizes, dialogs opened only through
//! safe-classified interaction), every distinct state recorded, and the
//! scaffold built from the UNION.
//!
//! Every inferred requirement stays NON-required (`required: false`) and
//! the contract's `scaffold.inferred` extension names exactly which states
//! contributed — nothing here is a promise about the app until a human or
//! agent promotes it.

use super::schema::{ComponentContract, OracleDecl, ProjectContract, ViewportReq};
use crate::screen::ScreenState;
use crate::semantic::SemanticScreen;
use std::collections::BTreeMap;

/// One discovered state, at scaffold granularity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScaffoldState {
    /// Discovery order (0 = the initial screen).
    pub index: usize,
    /// How this state was reached ("initial", "tab:3", "resize:120x40",
    /// "escape").
    pub via: String,
    /// The state's layered identity (structure × semantic).
    pub identity: String,
    /// Stable control ids present in this state (bounded per state).
    pub controls: Vec<String>,
    /// Region kinds present (dialog, panel, footer, …).
    pub regions: Vec<String>,
    /// Screen-level components detected (table/tree/scrollbar).
    pub components: Vec<String>,
}

/// What the gather pass saw, before scaffolding.
#[derive(Debug, Default, serde::Serialize)]
pub struct GatheredStates {
    pub states: Vec<ScaffoldState>,
    /// Viewports probed (always includes the launch size).
    pub viewports: Vec<(u16, u16)>,
    /// The focus order observed by walking Tab from the initial state
    /// (stable control ids, in visit order).
    pub focus_order: Vec<String>,
    /// Distinct control ids seen across ALL states (the scaffold's
    /// oracle candidates).
    pub controls_seen: BTreeMap<String, String>,
    /// Region roles seen across all states.
    pub regions_seen: Vec<String>,
    /// Component kinds seen across all states.
    pub components_seen: Vec<String>,
}

impl GatheredStates {
    /// How many distinct states contributed (deduped by identity).
    pub fn distinct(&self) -> usize {
        let mut seen: Vec<&str> = Vec::new();
        for s in &self.states {
            if !seen.contains(&s.identity.as_str()) {
                seen.push(&s.identity);
            }
        }
        seen.len()
    }
}

/// Budget for the gather pass. Deliberately small: scaffolding is a
/// construction aid, not an exploration campaign.
#[derive(Debug, Clone, Copy)]
pub struct ScaffoldBudget {
    /// Maximum Tab hops for the focus walk.
    pub max_focus_hops: usize,
    /// Viewport probes (the launch size is always first and free).
    pub max_viewports: usize,
    /// Total distinct-state cap (states beyond this are ignored).
    pub max_states: usize,
    /// Per-step settle budget (ms).
    pub step_budget_ms: u64,
}

impl Default for ScaffoldBudget {
    fn default() -> Self {
        ScaffoldBudget {
            max_focus_hops: 12,
            max_viewports: 3,
            max_states: 16,
            step_budget_ms: 1500,
        }
    }
}

/// A frame of interest: semantics + layered identity. (The raw grid is
/// deliberately NOT carried per state — the semantic projection is what
/// the scaffold consumes.)
struct Observed {
    sem: SemanticScreen,
    identity: String,
}

/// Gather states from a live session through SAFE interactions only:
/// observation, Tab (focus movement), Escape (dismiss), and resizes
/// (restored afterwards). No Enter, no clicks, no typing — a scaffold
/// pass must never mutate the app to describe it.
///
/// `io` bundles the two session touches the pass needs — `observe` takes
/// one fused observation, `act` runs one canonical action (the caller
/// wires the driving pipeline so lease/origin/evidence invariants stay
/// intact). One object so a `&mut Session` can back both methods without
/// split borrows.
pub trait ScaffoldIo {
    fn observe(&mut self, idle_ms: u64) -> anyhow::Result<(ScreenState, SemanticScreen)>;
    /// act(action_name_for_the_record, canonical_action) — errors are
    /// per-step, never fatal (a focusless app just contributes fewer
    /// states).
    fn act(&mut self, name: &str, action: crate::execution::CanonicalAction) -> anyhow::Result<()>;
}

pub fn gather_states<I>(
    initial: (ScreenState, SemanticScreen),
    cols: u16,
    rows: u16,
    budget: ScaffoldBudget,
    io: &mut I,
) -> anyhow::Result<GatheredStates>
where
    I: ScaffoldIo + ?Sized,
{
    use crate::backend::KeyCode;
    use crate::execution::CanonicalAction;
    use crate::exploration::state_graph::StateIdentity;
    let mut out = GatheredStates::default();
    let mut seen_identities: Vec<String> = Vec::new();

    let note_state = |out: &mut GatheredStates,
                      seen: &mut Vec<String>,
                      index: usize,
                      via: &str,
                      o: &Observed| {
        if seen.contains(&o.identity) {
            return;
        }
        if seen.len() >= budget.max_states {
            return;
        }
        seen.push(o.identity.clone());
        out.states.push(ScaffoldState {
            index,
            via: via.to_string(),
            identity: o.identity.clone(),
            controls: o
                .sem
                .controls
                .iter()
                .take(24)
                .map(|c| c.id.clone())
                .collect(),
            regions: o
                .sem
                .regions
                .iter()
                .map(|r| format!("{:?}", r.kind).to_lowercase())
                .collect(),
            components: o
                .sem
                .components
                .iter()
                .map(|c| match c {
                    crate::semantic::Component::Table(_) => "table".to_string(),
                    crate::semantic::Component::Tree(_) => "tree".to_string(),
                    crate::semantic::Component::Scrollbar(_) => "scrollbar".to_string(),
                })
                .collect(),
        });
    };

    let observe_now = |io: &mut I,
                       out: &mut GatheredStates,
                       seen: &mut Vec<String>,
                       counter: usize,
                       via: &str|
     -> anyhow::Result<Option<Observed>> {
        let (screen, sem) = io.observe(budget.step_budget_ms)?;
        let identity = StateIdentity::with_semantic(&screen, &sem)
            .id()
            .as_str()
            .to_string();
        let o = Observed { sem, identity };
        // Accumulate cross-state facts.
        for c in &o.sem.controls {
            out.controls_seen
                .entry(c.id.clone())
                .or_insert_with(|| format!("{:?}", c.kind).to_lowercase());
        }
        for r in &o.sem.regions {
            let role = format!("{:?}", r.kind).to_lowercase();
            if !out.regions_seen.contains(&role) {
                out.regions_seen.push(role);
            }
        }
        for c in &o.sem.components {
            let kind = match c {
                crate::semantic::Component::Table(_) => "table",
                crate::semantic::Component::Tree(_) => "tree",
                crate::semantic::Component::Scrollbar(_) => "scrollbar",
            };
            if !out.components_seen.iter().any(|k| k == kind) {
                out.components_seen.push(kind.to_string());
            }
        }
        note_state(out, seen, counter, via, &o);
        Ok(Some(o))
    };

    // ── 1. the initial screen ────────────────────────────────────────
    let (screen0, sem0) = initial;
    out.viewports.push((cols, rows));
    let identity0 = StateIdentity::with_semantic(&screen0, &sem0)
        .id()
        .as_str()
        .to_string();
    let o0 = Observed {
        sem: sem0,
        identity: identity0,
    };
    for c in &o0.sem.controls {
        out.controls_seen
            .entry(c.id.clone())
            .or_insert_with(|| format!("{:?}", c.kind).to_lowercase());
    }
    for r in &o0.sem.regions {
        let role = format!("{:?}", r.kind).to_lowercase();
        if !out.regions_seen.contains(&role) {
            out.regions_seen.push(role);
        }
    }
    for c in &o0.sem.components {
        let kind = match c {
            crate::semantic::Component::Table(_) => "table",
            crate::semantic::Component::Tree(_) => "tree",
            crate::semantic::Component::Scrollbar(_) => "scrollbar",
        };
        if !out.components_seen.iter().any(|k| k == kind) {
            out.components_seen.push(kind.to_string());
        }
    }
    note_state(&mut out, &mut seen_identities, 0, "initial", &o0);

    // ── 2. focus order: walk Tab while focus actually moves ─────────
    let mut focus_order: Vec<String> = Vec::new();
    if let Some(first) = o0.sem.focus.control_id.clone() {
        focus_order.push(first);
    }
    let mut last_identity = o0.identity.clone();
    let mut stale = 0usize;
    for hop in 1..=budget.max_focus_hops {
        let before_focus = focus_order.last().cloned();
        if io
            .act(
                "tab",
                CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(KeyCode::Tab),
                },
            )
            .is_err()
        {
            break;
        }
        let Ok(Some(o)) = observe_now(
            io,
            &mut out,
            &mut seen_identities,
            hop,
            &format!("tab:{hop}"),
        ) else {
            break;
        };
        if let Some(fid) = o.sem.focus.control_id.clone() {
            // The ring is a cycle: a repeat means the traversal closed,
            // and the DISTINCT stops are the order worth declaring.
            if focus_order.contains(&fid) {
                break;
            }
            focus_order.push(fid);
        }
        stale = if o.identity == last_identity && o.sem.focus.control_id == before_focus {
            stale + 1
        } else {
            0
        };
        last_identity = o.identity;
        // Two consecutive no-op hops = the cycle closed; stop honestly.
        if stale >= 2 {
            break;
        }
    }
    out.focus_order = focus_order;

    // ── 3. Escape: dismiss whatever is open (safe, restores state) ──
    let _ = io.act(
        "escape",
        CanonicalAction::Key {
            key: crate::backend::KeyEvent::new(KeyCode::Escape),
        },
    );
    let next_index = out.states.len();
    let _ = observe_now(io, &mut out, &mut seen_identities, next_index, "escape");

    // ── 4. viewport probes: narrow + wide, restored to the launch size ──
    let probes: [(u16, u16); 2] = [(120, 40), (60, 20)];
    let mut probed = 0usize;
    for (c, r) in probes {
        if probed >= budget.max_viewports.saturating_sub(1) {
            break;
        }
        if (c, r) == (cols, rows) {
            continue;
        }
        if io
            .act("resize", CanonicalAction::Resize { cols: c, rows: r })
            .is_err()
        {
            continue;
        }
        probed += 1;
        if !out.viewports.contains(&(c, r)) {
            out.viewports.push((c, r));
        }
        let next_index = out.states.len();
        let _ = observe_now(
            io,
            &mut out,
            &mut seen_identities,
            next_index,
            &format!("resize:{c}x{r}"),
        );
    }
    // Restore the launch viewport (a scaffold pass leaves no residue).
    if probed > 0 {
        let _ = io.act("resize", CanonicalAction::Resize { cols, rows });
        let _ = io.observe(budget.step_budget_ms);
    }

    Ok(out)
}

/// Scaffold a multi-state contract from the gathered pass. Same
/// promotion rules as [`ProjectContract::scaffold_from`] — everything
/// observed, nothing required — but components/oracles now cite the
/// states they were seen in, the focus walk becomes declared
/// interactions' expect material, and every probed viewport is declared.
pub fn scaffold_multi_state(gathered: &GatheredStates, launch: (u16, u16)) -> ProjectContract {
    let mut contract = ProjectContract::default();
    contract.schema.name = format!("scaffold-{}state", gathered.distinct());
    contract.schema.extensions.insert(
        "scaffold.inferred".to_string(),
        serde_json::json!({
            "inferred": true,
            "mode": "explore",
            "states": gathered.states,
            "launch_viewport": { "cols": launch.0, "rows": launch.1 },
            "note": "generated by tui_contract action=scaffold mode=explore from a SAFE multi-state pass (initial screen, Tab focus walk, Escape, viewport probes); everything declared was SEEN — promote required=true deliberately",
        }),
    );

    // Every probed viewport becomes declared (the layout must survive
    // the sizes the app was actually seen at).
    contract.viewports = gathered
        .viewports
        .iter()
        .map(|(c, r)| ViewportReq { cols: *c, rows: *r })
        .collect();

    // Region roles seen anywhere → optional components (each names its
    // role slug; required stays false).
    let mut names = std::collections::HashSet::new();
    for role in &gathered.regions_seen {
        if role == "unknown" {
            continue;
        }
        let mut name = format!("{role}-observed");
        let mut n = 1;
        while !names.insert(name.clone()) {
            name = format!("{role}-observed-{n}");
            n += 1;
        }
        contract.components.push(ComponentContract {
            name,
            role: role.clone(),
            required: false,
            expect: Vec::new(),
        });
    }
    // Screen-level component kinds → optional components too.
    for kind in &gathered.components_seen {
        let mut name = format!("{kind}-observed");
        let mut n = 1;
        while !names.insert(name.clone()) {
            name = format!("{kind}-observed-{n}");
            n += 1;
        }
        contract.components.push(ComponentContract {
            name,
            role: kind.clone(),
            required: false,
            expect: Vec::new(),
        });
    }

    // Controls seen across states → oracle candidates (bounded). The
    // state count that showed each control rides the extension blob, so
    // a reviewer sees how strong each candidate is without this loop
    // growing a side channel.
    let mut state_counts = serde_json::Map::new();
    for (id, kind) in gathered.controls_seen.iter().take(24) {
        if kind == "unknown" || kind == "label" {
            continue;
        }
        let states_showing = gathered
            .states
            .iter()
            .filter(|s| s.controls.iter().any(|c| c == id))
            .count();
        state_counts.insert(id.clone(), serde_json::json!(states_showing));
        contract.oracles.push(OracleDecl {
            id: Some(format!("observed-{}", id.trim_start_matches('#'))),
            expr: format!("control_exists(\"{id}\")"),
        });
    }
    if let Some(scaffold_ext) = contract.schema.extensions.get_mut("scaffold.inferred") {
        scaffold_ext["control_state_counts"] = serde_json::Value::Object(state_counts);
    }

    // The focus walk becomes declared interaction material: a Tab cycle
    // is expected to hold after ANY state change the app ships.
    if gathered.focus_order.len() >= 2 {
        contract
            .interactions
            .push(super::schema::InteractionContract {
                name: "focus-cycle-observed".to_string(),
                keys: vec!["tab".to_string()],
                context: Some(format!(
                    "scaffold observed a {}-stop focus cycle (order: {})",
                    gathered.focus_order.len(),
                    gathered.focus_order.join(" → ")
                )),
                expect: vec![],
            });
    }

    // Viewport probes that produced clipped regions at the narrow size
    // are exactly the layout constraints worth declaring.
    let narrow = gathered
        .states
        .iter()
        .filter(|s| s.via.starts_with("resize:60x"))
        .count();
    if narrow > 0 {
        contract.layout.push(super::schema::LayoutConstraint {
            name: Some("narrow-viewport-probed".to_string()),
            min_cols: Some(60),
            min_rows: Some(20),
            no_clipping: true,
        });
    }

    // App-independent focus invariants, same as the single-frame scaffold.
    contract.escape_closes_modal = true;
    contract.reverse_tab_required = true;

    contract
}
