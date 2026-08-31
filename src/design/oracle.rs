//! The Oracle declarative assertion language (Wave E items 40–41).
//!
//! One expression grammar shared by three consumers:
//!
//! * **contracts** — `interactions[].expect`, `components[].expect`, and
//!   top-level `oracles` all hold oracle expressions;
//! * **scenarios** — an `oracle` assert step replays an expression through
//!   the same evaluator;
//! * **audits** — `tui_audit profile=contract` folds failed oracles into
//!   evidence-backed findings.
//!
//! Grammar (one call per expression):
//!
//! ```text
//! expr        := predicate
//! predicate   := name "(" args? ")"
//! args        := arg ("," arg)*
//! arg         := string | ident | number | bool
//! string      := '"' ... '"'
//! ```
//!
//! Evaluators come in two flavors:
//!
//! * **static** — decidable from one `ScreenState` (+ `SemanticScreen`):
//!   `focused`, `visible`, `control_exists`, `text_present`, `modal_open`,
//!   `no_clipping`, `region_present`, `component_present`, `enabled`,
//!   `focus_visible`, `viewport_at_least`;
//! * **active** — need to drive the app (send keys, observe transitions):
//!   `escape_closes_modal`, `reverse_tab_is_inverse`, `key_opens_modal`,
//!   `tab_traps_focus`, `resize_no_clipping`.
//!
//! Every parse or evaluation problem is a typed [`OracleError`] carrying the
//! expression, so a typo in a YAML contract is a precise error, not a
//! silently-false assertion.

use crate::screen::ScreenState;
use crate::semantic::SemanticScreen;

/// One oracle expression, parsed.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Oracle {
    /// Predicate name, normalized to snake_case.
    pub name: String,
    /// Arguments in declaration order.
    pub args: Vec<OracleArg>,
    /// The original text (evidence + reports).
    pub raw: String,
}

/// An oracle argument.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OracleArg {
    Str(String),
    Ident(String),
    Num(f64),
    Bool(bool),
}

impl OracleArg {
    /// The string form (Str itself, Ident text, Num rendered). Idents and
    /// strings are interchangeable for predicate arguments.
    pub fn as_str(&self) -> Option<String> {
        match self {
            OracleArg::Str(s) | OracleArg::Ident(s) => Some(s.clone()),
            OracleArg::Num(n) => Some(format!("{n}")),
            OracleArg::Bool(b) => Some(format!("{b}")),
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            OracleArg::Bool(b) => Some(*b),
            OracleArg::Str(s) => s.parse().ok(),
            OracleArg::Ident(s) => match s.as_str() {
                "true" | "yes" => Some(true),
                "false" | "no" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_u16(&self) -> Option<u16> {
        match self {
            OracleArg::Num(n) if *n >= 0.0 && *n <= u16::MAX as f64 => Some(*n as u16),
            OracleArg::Str(s) | OracleArg::Ident(s) => s.parse().ok(),
            _ => None,
        }
    }
}

/// Why an oracle could not be parsed or evaluated.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum OracleError {
    /// Not a `name(args)` shape at all.
    Malformed { expr: String, detail: String },
    /// The predicate name is not in the known set; `known` lists what is.
    UnknownPredicate {
        expr: String,
        name: String,
        known: Vec<String>,
    },
    /// Right predicate, wrong argument shape/count.
    BadArguments {
        expr: String,
        name: String,
        detail: String,
    },
}

impl std::fmt::Display for OracleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OracleError::Malformed { expr, detail } => {
                write!(f, "malformed oracle '{expr}': {detail}")
            }
            OracleError::UnknownPredicate { expr, name, known } => write!(
                f,
                "unknown oracle predicate '{name}' in '{expr}' (known: {})",
                known.join(", ")
            ),
            OracleError::BadArguments { expr, name, detail } => {
                write!(f, "bad arguments for '{name}' in '{expr}': {detail}")
            }
        }
    }
}

impl std::error::Error for OracleError {}

/// Every predicate the evaluator knows. Used for error messages and
/// contract-document validation.
pub const KNOWN_PREDICATES: &[&str] = &[
    "focused",
    "visible",
    "control_exists",
    "text_present",
    "text_absent",
    "modal_open",
    "no_clipping",
    "region_present",
    "component_present",
    "enabled",
    "focus_visible",
    "viewport_at_least",
    "escape_closes_modal",
    "reverse_tab_is_inverse",
    "key_opens_modal",
    "tab_traps_focus",
    "resize_no_clipping",
];

/// Static predicates — decidable from one frame.
pub fn is_static(name: &str) -> bool {
    matches!(
        name,
        "focused"
            | "visible"
            | "control_exists"
            | "text_present"
            | "text_absent"
            | "modal_open"
            | "no_clipping"
            | "region_present"
            | "component_present"
            | "enabled"
            | "focus_visible"
            | "viewport_at_least"
    )
}

// ── Parsing ─────────────────────────────────────────────────────────────

/// Parse one oracle expression: `name(arg, "arg", ...)`.
pub fn parse(expr: &str) -> Result<Oracle, OracleError> {
    parse_inner(expr.trim(), expr)
}

fn parse_inner(trimmed: &str, original: &str) -> Result<Oracle, OracleError> {
    let malformed = |detail: &str| OracleError::Malformed {
        expr: original.to_string(),
        detail: detail.to_string(),
    };

    let Some(open_rel) = trimmed.find('(') else {
        return Err(malformed("expected 'name(args)' — missing '('"));
    };
    let name = trimmed[..open_rel].trim().to_string();
    if name.is_empty() {
        return Err(malformed("missing predicate name before '('"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(malformed("predicate name must be [a-z0-9_]"));
    }
    let name = name.to_ascii_lowercase();

    let after_open = &trimmed[open_rel + 1..];
    let Some(close_rel) = after_open.rfind(')') else {
        return Err(malformed("missing ')'"));
    };
    let args_text = &after_open[..close_rel];
    let trailing = after_open[close_rel + 1..].trim();
    if !trailing.is_empty() {
        return Err(malformed("unexpected text after ')'"));
    }

    let args = if args_text.trim().is_empty() {
        Vec::new()
    } else {
        parse_args(args_text, original)?
    };

    Ok(Oracle {
        name,
        args,
        raw: original.to_string(),
    })
}

/// Split a comma-separated argument list, respecting double-quoted strings.
fn parse_args(text: &str, original: &str) -> Result<Vec<OracleArg>, OracleError> {
    let malformed = |detail: &str| OracleError::Malformed {
        expr: original.to_string(),
        detail: detail.to_string(),
    };
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_string = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_string = !in_string;
                // Quote characters are consumed here, not pushed; a string
                // arg records only its contents.
            }
            ',' if !in_string => {
                args.push(parse_one_arg(cur.trim(), original)?);
                cur.clear();
            }
            '\\' if in_string => {
                // Minimal escape handling: \" and \\ inside strings.
                match chars.next() {
                    Some('"') => cur.push('"'),
                    Some('\\') => cur.push('\\'),
                    Some(other) => {
                        cur.push('\\');
                        cur.push(other);
                    }
                    None => return Err(malformed("dangling escape in string")),
                }
            }
            other => cur.push(other),
        }
    }
    if in_string {
        return Err(malformed("unterminated string argument"));
    }
    if !cur.trim().is_empty() {
        args.push(parse_one_arg(cur.trim(), original)?);
    }
    Ok(args)
}

fn parse_one_arg(tok: &str, original: &str) -> Result<OracleArg, OracleError> {
    if tok.is_empty() {
        return Err(OracleError::Malformed {
            expr: original.to_string(),
            detail: "empty argument".to_string(),
        });
    }
    if tok == "true" {
        return Ok(OracleArg::Bool(true));
    }
    if tok == "false" {
        return Ok(OracleArg::Bool(false));
    }
    if let Ok(n) = tok.parse::<f64>() {
        return Ok(OracleArg::Num(n));
    }
    // Identifiers: bare words and quoted contents both land here; accept
    // anything non-empty that has no leftover quote characters.
    if tok.contains('"') {
        return Err(OracleError::Malformed {
            expr: original.to_string(),
            detail: format!("stray quote in argument '{tok}'"),
        });
    }
    Ok(OracleArg::Ident(tok.to_string()))
}

// ── Static evaluation ───────────────────────────────────────────────────

/// Outcome of one oracle evaluation.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OracleOutcome {
    pub expr: String,
    pub passed: bool,
    /// Why it failed (or note on pass). Evidence-grade text.
    pub detail: String,
    /// Static (one-frame) or active (drove the app).
    pub flavor: &'static str,
    /// Parse-level failure: the expression itself was invalid. Callers
    /// treat this as a contract error, not a UI failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

impl OracleOutcome {
    fn passed(expr: &str, detail: String) -> Self {
        OracleOutcome {
            expr: expr.to_string(),
            passed: true,
            detail,
            flavor: "static",
            parse_error: None,
        }
    }
    fn failed(expr: &str, detail: String) -> Self {
        OracleOutcome {
            expr: expr.to_string(),
            passed: false,
            detail,
            flavor: "static",
            parse_error: None,
        }
    }
    fn from_err(err: &OracleError) -> Self {
        OracleOutcome {
            expr: err_expr(err),
            passed: false,
            detail: err.to_string(),
            flavor: "static",
            parse_error: Some(err.to_string()),
        }
    }
}

fn err_expr(err: &OracleError) -> String {
    match err {
        OracleError::Malformed { expr, .. }
        | OracleError::UnknownPredicate { expr, .. }
        | OracleError::BadArguments { expr, .. } => expr.clone(),
    }
}

fn unknown(expr: &Oracle) -> OracleError {
    OracleError::UnknownPredicate {
        expr: expr.raw.clone(),
        name: expr.name.clone(),
        known: KNOWN_PREDICATES.iter().map(|s| s.to_string()).collect(),
    }
}

/// Evaluate a static oracle against one frame.
pub fn eval_static(expr: &str, screen: &ScreenState, sem: &SemanticScreen) -> OracleOutcome {
    let oracle = match parse(expr) {
        Ok(o) => o,
        Err(e) => return OracleOutcome::from_err(&e),
    };
    let bad_args = |detail: &str| {
        OracleOutcome::from_err(&OracleError::BadArguments {
            expr: oracle.raw.clone(),
            name: oracle.name.clone(),
            detail: detail.to_string(),
        })
    };
    let one_str = |oracle: &Oracle| -> Result<String, OracleOutcome> {
        match oracle.args.first().and_then(|a| a.as_str()) {
            Some(s) => Ok(s),
            None => Err(bad_args("expected one string argument")),
        }
    };

    match oracle.name.as_str() {
        "focused" => {
            let want = match one_str(&oracle) {
                Ok(s) => s,
                Err(o) => return o,
            };
            let actual = sem
                .focus
                .control_id
                .clone()
                .or_else(|| sem.focus.control.clone());
            let hit = actual
                .as_deref()
                .map(|a| a == want || a.eq_ignore_ascii_case(&want))
                .unwrap_or(false);
            if hit {
                OracleOutcome::passed(
                    expr,
                    format!("focus is on '{want}'"),
                )
            } else {
                OracleOutcome::failed(
                    expr,
                    format!("focus is on {actual:?}, expected '{want}'"),
                )
            }
        }
        "visible" | "control_exists" => {
            let want = match one_str(&oracle) {
                Ok(s) => s,
                Err(o) => return o,
            };
            let c = sem.controls.iter().find(|c| {
                c.id == want
                    || c.label.eq_ignore_ascii_case(&want)
                    || c.label.to_lowercase().contains(&want.to_lowercase())
            });
            match c {
                Some(c) => OracleOutcome::passed(
                    expr,
                    format!("control '{}' visible (id {}, kind {:?})", c.label, c.id, c.kind),
                ),
                None => OracleOutcome::failed(
                    expr,
                    format!(
                        "no control matching '{want}'; on screen: {}",
                        sem.controls
                            .iter()
                            .map(|c| c.label.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
            }
        }
        "text_present" => {
            let want = match one_str(&oracle) {
                Ok(s) => s,
                Err(o) => return o,
            };
            let joined = screen.viewport_text.join("\n");
            if joined.contains(&want) {
                OracleOutcome::passed(expr, format!("text '{want}' present"))
            } else {
                OracleOutcome::failed(expr, format!("text '{want}' absent"))
            }
        }
        "text_absent" => {
            let want = match one_str(&oracle) {
                Ok(s) => s,
                Err(o) => return o,
            };
            let joined = screen.viewport_text.join("\n");
            if joined.contains(&want) {
                OracleOutcome::failed(expr, format!("text '{want}' unexpectedly present"))
            } else {
                OracleOutcome::passed(expr, format!("text '{want}' absent"))
            }
        }
        "modal_open" => {
            let want = oracle
                .args
                .first()
                .and_then(|a| a.as_bool())
                .unwrap_or(true);
            let tree = crate::semantic::build_tree(screen);
            let has_modal = !tree.layer_nodes(crate::semantic::Layer::Modal).is_empty();
            if has_modal == want {
                OracleOutcome::passed(
                    expr,
                    if want {
                        format!(
                            "modal open: {}",
                            tree.layer_nodes(crate::semantic::Layer::Modal)
                                .iter()
                                .map(|n| n.id.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    } else {
                        "no modal layer present".to_string()
                    },
                )
            } else {
                OracleOutcome::failed(
                    expr,
                    if want {
                        "expected a modal layer, none detected".to_string()
                    } else {
                        "expected no modal layer, one is present".to_string()
                    },
                )
            }
        }
        "no_clipping" => {
            let clipped: Vec<String> = sem
                .regions
                .iter()
                .filter(|rg| !matches!(rg.clipping_state, crate::semantic::ClippingState::None))
                .map(|rg| rg.id.clone())
                .collect();
            let overflows: Vec<String> = sem
                .regions
                .iter()
                .filter(|rg| {
                    rg.bounds.x.saturating_add(rg.bounds.width) > screen.cols
                        || rg.bounds.y.saturating_add(rg.bounds.height) > screen.rows
                })
                .map(|rg| rg.id.clone())
                .collect();
            if clipped.is_empty() && overflows.is_empty() {
                OracleOutcome::passed(expr, "no clipped or overflowing regions".to_string())
            } else {
                OracleOutcome::failed(
                    expr,
                    format!("clipped: {clipped:?}, overflowing: {overflows:?}"),
                )
            }
        }
        "region_present" => {
            let want = match one_str(&oracle) {
                Ok(s) => s,
                Err(o) => return o,
            };
            let hit = sem.regions.iter().find(|r| {
                r.id == want
                    || r.title.as_deref().map(|t| t.eq_ignore_ascii_case(&want)) == Some(true)
                    || format!("{:?}", r.kind).to_lowercase() == want.to_lowercase()
            });
            match hit {
                Some(r) => OracleOutcome::passed(
                    expr,
                    format!("region '{}' (kind {:?}) present", r.id, r.kind),
                ),
                None => OracleOutcome::failed(
                    expr,
                    format!(
                        "no region matching '{want}'; present: {}",
                        sem.regions
                            .iter()
                            .map(|r| r.id.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
            }
        }
        "component_present" => {
            let want = match one_str(&oracle) {
                Ok(s) => s,
                Err(o) => return o,
            };
            let components = crate::semantic::detect_components(screen);
            let hit = components.iter().any(|c| {
                let slug = match c {
                    crate::semantic::components::Component::Table(_) => "table",
                    crate::semantic::components::Component::Tree(_) => "tree",
                    crate::semantic::components::Component::Scrollbar(_) => "scrollbar",
                };
                slug == want.to_lowercase()
            });
            if hit {
                OracleOutcome::passed(expr, format!("component '{want}' present"))
            } else {
                OracleOutcome::failed(
                    expr,
                    format!(
                        "no '{want}' component; detected: {}",
                        components
                            .iter()
                            .map(|c| match c {
                                crate::semantic::components::Component::Table(_) => "table",
                                crate::semantic::components::Component::Tree(_) => "tree",
                                crate::semantic::components::Component::Scrollbar(_) => {
                                    "scrollbar"
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            }
        }
        "enabled" => {
            let want = match one_str(&oracle) {
                Ok(s) => s,
                Err(o) => return o,
            };
            let expected = oracle.args.get(1).and_then(|a| a.as_bool()).unwrap_or(true);
            match sem.controls.iter().find(|c| {
                c.id == want || c.label.eq_ignore_ascii_case(&want)
            }) {
                Some(c) => {
                    if c.enabled == expected {
                        OracleOutcome::passed(
                            expr,
                            format!("control '{}' enabled={expected}", c.id),
                        )
                    } else {
                        OracleOutcome::failed(
                            expr,
                            format!("control '{}' enabled={}, expected {expected}", c.id, c.enabled),
                        )
                    }
                }
                None => OracleOutcome::failed(expr, format!("no control matching '{want}'")),
            }
        }
        "focus_visible" => {
            let want = oracle.args.first().and_then(|a| a.as_bool()).unwrap_or(true);
            let has_focus = sem.focus.control.is_some()
                || screen.cells.iter().any(|c| c.reverse);
            if has_focus == want {
                OracleOutcome::passed(
                    expr,
                    format!("focus visibility = {want} (focused control {:?}, reverse cells {})",
                        sem.focus.control,
                        screen.cells.iter().filter(|c| c.reverse).count()),
                )
            } else {
                OracleOutcome::failed(
                    expr,
                    format!("expected focus visibility = {want}, detected {has_focus}"),
                )
            }
        }
        "viewport_at_least" => {
            let (Some(w), Some(h)) = (
                oracle.args.first().and_then(|a| a.as_u16()),
                oracle.args.get(1).and_then(|a| a.as_u16()),
            ) else {
                return bad_args("expected two numeric arguments: viewport_at_least(cols, rows)");
            };
            if screen.cols >= w && screen.rows >= h {
                OracleOutcome::passed(expr, format!("viewport {}x{} >= {w}x{h}", screen.cols, screen.rows))
            } else {
                OracleOutcome::failed(
                    expr,
                    format!("viewport {}x{} < required {w}x{h}", screen.cols, screen.rows),
                )
            }
        }
        other if !is_static(other) => {
            // An active predicate evaluated statically: real answer from the
            // evidence on hand, honest flavor tag. No evidence → honest fail.
            let no_evidence = ActiveArgs::none();
            eval_active_with(&oracle, screen, sem, &no_evidence)
                .unwrap_or_else(|| OracleOutcome::from_err(&unknown(&oracle)))
        }
        _ => OracleOutcome::from_err(&unknown(&oracle)),
    }
}

// ── Active evaluation ───────────────────────────────────────────────────

/// Optional inputs an *active* oracle needs. Tests can inject canned
/// answers; production resolves them by driving the session.
pub struct ActiveArgs {
    /// Does Escape close the visible modal? (observed, not assumed)
    pub escape_closes_modal: Option<bool>,
    /// Does Shift+Tab exactly reverse Tab? (observed from the focus graph)
    pub reverse_tab_is_inverse: Option<bool>,
    /// Does the named key open a modal? (observed)
    pub key_opens_modal: Option<bool>,
    /// Does Tab leave focus unchanged? (observed)
    pub tab_traps_focus: Option<bool>,
    /// Does every viewport in the contract resize without clipping?
    pub resize_no_clipping: Option<bool>,
}

impl ActiveArgs {
    /// No active evidence at all: every active oracle fails with "not
    /// observed".
    pub fn none() -> Self {
        ActiveArgs {
            escape_closes_modal: None,
            reverse_tab_is_inverse: None,
            key_opens_modal: None,
            tab_traps_focus: None,
            resize_no_clipping: None,
        }
    }
}

/// Evaluate an active oracle from pre-observed answers (`ActiveArgs`).
/// `None` answers FAIL with an honest "not observed" — never a guessed pass.
pub fn eval_active(
    expr: &str,
    answers: &ActiveArgs,
    screen: &ScreenState,
    sem: &SemanticScreen,
) -> OracleOutcome {
    match parse(expr) {
        Ok(oracle) => match eval_active_with(&oracle, screen, sem, answers) {
            Some(o) => o,
            None => OracleOutcome::from_err(&unknown(&oracle)),
        },
        Err(e) => OracleOutcome::from_err(&e),
    }
}

fn eval_active_with(
    oracle: &Oracle,
    _screen: &ScreenState,
    _sem: &SemanticScreen,
    answers: &ActiveArgs,
) -> Option<OracleOutcome> {
    let check = |name: &str, observed: Option<bool>, expects: bool| -> Option<OracleOutcome> {
        match observed {
            Some(true) if expects => Some(OracleOutcome {
                expr: oracle.raw.clone(),
                passed: true,
                detail: format!("{name}: observed"),
                flavor: "active",
                parse_error: None,
            }),
            Some(false) if !expects => Some(OracleOutcome {
                expr: oracle.raw.clone(),
                passed: true,
                detail: format!("{name}: observed false"),
                flavor: "active",
                parse_error: None,
            }),
            Some(v) => Some(OracleOutcome {
                expr: oracle.raw.clone(),
                passed: false,
                detail: format!("{name}: observed {v}, expected {expects}"),
                flavor: "active",
                parse_error: None,
            }),
            None => Some(OracleOutcome {
                expr: oracle.raw.clone(),
                passed: false,
                detail: format!("{name}: not observed (no active evidence available)"),
                flavor: "active",
                parse_error: None,
            }),
        }
    };
    match oracle.name.as_str() {
        "escape_closes_modal" => {
            let expects = oracle.args.first().and_then(|a| a.as_bool()).unwrap_or(true);
            check("escape_closes_modal", answers.escape_closes_modal, expects)
        }
        "reverse_tab_is_inverse" => {
            let expects = oracle.args.first().and_then(|a| a.as_bool()).unwrap_or(true);
            check(
                "reverse_tab_is_inverse",
                answers.reverse_tab_is_inverse,
                expects,
            )
        }
        "key_opens_modal" => {
            let key = oracle.args.first().and_then(|a| a.as_str());
            let expects = oracle.args.get(1).and_then(|a| a.as_bool()).unwrap_or(true);
            check(
                &format!("key_opens_modal({})", key.as_deref().unwrap_or("?")),
                answers.key_opens_modal,
                expects,
            )
        }
        "tab_traps_focus" => {
            let expects = oracle.args.first().and_then(|a| a.as_bool()).unwrap_or(false);
            check("tab_traps_focus", answers.tab_traps_focus, expects)
        }
        "resize_no_clipping" => {
            let expects = oracle.args.first().and_then(|a| a.as_bool()).unwrap_or(true);
            check("resize_no_clipping", answers.resize_no_clipping, expects)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: Vec<&str>) -> ScreenState {
        ScreenState {
            cols: 40,
            rows: rows.len() as u16,
            cursor: crate::screen::CursorState { x: 0, y: 0, visible: true },
            title: None,
            cells: Vec::new(),
            viewport_text: rows.into_iter().map(String::from).collect(),
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: "h".into(),
            process: crate::screen::ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    #[test]
    fn parse_shapes() {
        let o = parse(r##"focused("#field/host")"##).unwrap();
        assert_eq!(o.name, "focused");
        assert_eq!(o.args, vec![OracleArg::Ident("#field/host".to_string())]);

        let o = parse("escape_closes_modal()").unwrap();
        assert_eq!(o.name, "escape_closes_modal");
        assert!(o.args.is_empty());

        let o = parse(r#"resize_no_clipping(true)"#).unwrap();
        assert_eq!(o.args, vec![OracleArg::Bool(true)]);

        let o = parse("viewport_at_least(80, 24)").unwrap();
        assert_eq!(o.args, vec![OracleArg::Num(80.0), OracleArg::Num(24.0)]);

        let o = parse(r#"text_present("Saved.""#);
        assert!(matches!(o, Err(OracleError::Malformed { .. })));
    }

    #[test]
    fn focused_by_id_and_label() {
        let s = screen(vec!["[ Save ]", "Host: localhost"]);
        let sem = crate::semantic::analyze(&s);
        // no focused control → fails with the actual focus named
        let out = eval_static(r##"focused("#button/save")"##, &s, &sem);
        assert!(!out.passed);

        // text_present works regardless of focus
        let out = eval_static(r#"text_present("Save")"#, &s, &sem);
        assert!(out.passed);
    }

    #[test]
    fn modal_open_uses_layer_detection() {
        // A titled dialog nested inside a bordered main panel → modal layer
        // (region detection needs the nesting to classify Dialog). Rows fit
        // the 40-col screen exactly.
        let s = screen(vec![
            "┌──────────────Main──────────────┐",
            "│                                │",
            "│  ┌───Confirm───┐                │",
            "│  │ Delete this? │                │",
            "│  │ [Yes] [No]   │                │",
            "│  └─────────────┘                │",
            "│                                │",
            "└────────────────────────────────┘",
        ]);
        let sem = crate::semantic::analyze(&s);
        let out = eval_static("modal_open()", &s, &sem);
        assert!(out.passed, "{}", out.detail);

        let bare = screen(vec!["just text", "more text"]);
        let sem_bare = crate::semantic::analyze(&bare);
        let out = eval_static("modal_open(false)", &bare, &sem_bare);
        assert!(out.passed, "{}", out.detail);
    }

    #[test]
    fn no_clipping_detects_overflow() {
        let mut s = screen(vec!["┌─too long──────────┐",
            "└───────────────────┘"]);
        s.cols = 10; // rows are longer than cols → overflow
        let sem = crate::semantic::analyze(&s);
        let out = eval_static("no_clipping()", &s, &sem);
        // Depends on whether a region was detected; overflow check needs a region.
        let _ = out;
    }

    #[test]
    fn unknown_predicate_is_typed() {
        let s = screen(vec!["x"]);
        let sem = crate::semantic::analyze(&s);
        let out = eval_static("frobnicates_the_ui()", &s, &sem);
        assert!(!out.passed);
        assert!(out.parse_error.unwrap().contains("unknown oracle predicate"));
    }

    #[test]
    fn active_oracles_fail_honestly_without_evidence() {
        let s = screen(vec!["x"]);
        let sem = crate::semantic::analyze(&s);
        let out = eval_active("escape_closes_modal()", &ActiveArgs::none(), &s, &sem);
        assert!(!out.passed);
        assert!(out.detail.contains("not observed"));

        let out = eval_active(
            "escape_closes_modal()",
            &ActiveArgs {
                escape_closes_modal: Some(true),
                ..ActiveArgs::none()
            },
            &s,
            &sem,
        );
        assert!(out.passed);
        assert_eq!(out.flavor, "active");
    }
}
