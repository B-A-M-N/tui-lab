//! Widget-family detectors (Wave C items 17–26).
//!
//! Each detector looks at the *grid* (not just token shapes) for the
//! structural evidence of one widget family: column alignment for tables,
//! branch glyphs + indentation for trees, thumb/track geometry for scroll
//! regions, caret markers for dropdowns, and so on. A detection becomes a
//! [`Widget`] which the tree builder expands into a subtree — table →
//! header/columns → rows → cells — so the "table-shaped thing here" blob
//! becomes an addressable structure an agent can act on ("select row 3").
//!
//! Everything is inferred with named evidence; confidence < 1.0 unless a
//! framework adapter supplied the fact.

use crate::screen::ScreenState;
use crate::semantic::confidence::Confidence;
use crate::semantic::regions::{Bounds, Region};
use crate::semantic::tree_builder::scroll_edges_from_thumb;

/// One detected widget, its parts, and the evidence for it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Widget {
    /// Stable, geometry-free ID.
    pub id: String,
    pub kind: WidgetKind,
    pub bounds: Bounds,
    pub confidence: Confidence,
    /// Kind-specific structure (columns, items, edges…).
    pub detail: WidgetDetail,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Table,
    Tree,
    ScrollableRegion,
    List,
    TextArea,
    Select,
    Dropdown,
    Menu,
    CommandPalette,
    SplitPane,
    Toast,
    Alert,
    Validation,
    HelpOverlay,
    KeyHintBar,
}

/// Structural detail per family.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WidgetDetail {
    /// Tables: `(column x, header label)`.
    pub columns: Vec<(u16, String)>,
    /// Tables: data rows with per-column cell text.
    pub rows: Vec<TableRow>,
    /// Trees/lists/menus: items with depth, label, and row y.
    pub items: Vec<WidgetItem>,
    /// Text areas / selects: the current value.
    pub value: Option<String>,
    /// Scroll edges (can_scroll_up/down…).
    pub scroll: Option<crate::semantic::node::ScrollEdges>,
    /// Split panes: the divider orientation and position.
    pub split: Option<SplitInfo>,
    /// Toasts/alerts: severity.
    pub severity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TableRow {
    /// Row y (header is separate).
    pub y: u16,
    /// Cell text per column (same order as `columns`).
    pub cells: Vec<String>,
    /// True when the row shows the selection marker/highlight.
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WidgetItem {
    pub y: u16,
    pub x: u16,
    pub depth: u32,
    pub label: String,
    pub selected: bool,
    pub expanded: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SplitInfo {
    pub orientation: char, // 'v' | 'h'
    pub pos: u16,
}

/// Detect all widget families on a screen.
pub fn detect_widgets(screen: &ScreenState, regions: &[Region]) -> Vec<Widget> {
    let mut out = Vec::new();
    if let Some(t) = detect_table(screen) {
        out.push(t);
    }
    if let Some(t) = detect_tree(screen) {
        out.push(t);
    }
    out.extend(detect_scroll_regions(screen));
    if let Some(l) = detect_list(screen) {
        out.push(l);
    }
    out.extend(detect_text_areas(screen, regions));
    out.extend(detect_selects(screen));
    if let Some(m) = detect_menu(screen) {
        out.push(m);
    }
    if let Some(p) = detect_palette(screen) {
        out.push(p);
    }
    out.extend(detect_splits(screen));
    out.extend(detect_toasts(screen));
    out.extend(detect_help_overlays(screen));
    if let Some(kb) = detect_key_hint_bar(screen) {
        out.push(kb);
    }
    out
}

// ─── Table (item 17) ──────────────────────────────────────────────────

/// Border glyphs that terminate/decorate content but never form labels.
fn is_borderish(c: char) -> bool {
    matches!(
        c,
        '─' | '│'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '╔'
            | '╗'
            | '╚'
            | '╝'
            | '║'
            | '═'
    )
}

/// Strip leading/trailing border glyphs and spaces, returning (x-offset, text).
fn strip_borders(line: &str) -> (u16, String) {
    let chars: Vec<char> = line.chars().collect();
    let mut s = 0;
    while s < chars.len() && (is_borderish(chars[s]) || chars[s] == ' ') {
        s += 1;
    }
    let mut e = chars.len();
    while e > s && (is_borderish(chars[e - 1]) || chars[e - 1] == ' ') {
        e -= 1;
    }
    (s as u16, chars[s..e].iter().collect())
}

/// Header columns: words separated by 2+ spaces (reuses the Wave-4 logic,
/// now feeding the node tree instead of a Component blob).
fn header_columns(line: &str) -> Vec<(u16, String)> {
    let trimmed = line.trim_end();
    if trimmed.trim().is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i] != ' ' {
            i += 1;
        }
        let label: String = chars[start..i].iter().collect();
        let sep_ok =
            i >= chars.len() || chars[i..].iter().take(2).filter(|&&c| c == ' ').count() >= 2;
        if sep_ok {
            out.push((start as u16, label));
        } else if let Some(last) = out.last_mut() {
            let gap: String = chars[last.0 as usize + last.1.chars().count()..i]
                .iter()
                .collect();
            last.1.push_str(&gap);
            last.1.push_str(&label);
        }
    }
    out
}

/// A plausible table column label: ≥2 chars, mostly alphanumeric, no
/// border/punctuation-only tokens ("│", "]", "…").
fn plausible_label(label: &str) -> bool {
    let n = label.chars().count();
    if n < 2 {
        return false;
    }
    let alnum = label.chars().filter(|c| c.is_alphanumeric()).count();
    alnum * 2 >= n && !label.chars().all(is_borderish)
}

fn word_start_at(line: &str, x: u16) -> Option<u16> {
    let chars: Vec<char> = line.chars().collect();
    let xi = x as usize;
    if xi >= chars.len() || chars[xi] == ' ' {
        return None;
    }
    (xi == 0 || chars[xi - 1] == ' ').then_some(x)
}

fn word_from(line: &str, x: u16) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut s = x as usize;
    while s < chars.len() && chars[s] == ' ' {
        s += 1;
    }
    let mut e = s;
    while e < chars.len() && chars[e] != ' ' {
        e += 1;
    }
    chars[s..e].iter().collect()
}

/// A row is selected when it carries a marker (▶, >, ●) or reverse video.
fn row_selected(screen: &ScreenState, y: u16) -> bool {
    screen
        .cells
        .iter()
        .any(|c| c.y == y && (c.reverse || matches!(c.text.as_str(), "▶" | "►" | "●" | "❯")))
}

fn detect_table(screen: &ScreenState) -> Option<Widget> {
    for (y, line) in screen.viewport_text.iter().enumerate() {
        if y + 1 >= screen.viewport_text.len() {
            break;
        }
        // Work on the border-stripped interior: a dialog's side walls must
        // not masquerade as column boundaries.
        let (bx, inner) = strip_borders(line);
        if inner.trim().is_empty() {
            continue;
        }
        let cols: Vec<(u16, String)> = header_columns(&inner)
            .into_iter()
            .filter(|(_, l)| plausible_label(l))
            .map(|(x, l)| (x + bx, l))
            .collect();
        if cols.len() < 2 {
            continue;
        }
        let xs: Vec<u16> = cols.iter().map(|(x, _)| *x).collect();

        let mut rows: Vec<TableRow> = Vec::new();
        for (ry, rline) in screen.viewport_text.iter().skip(y + 1).enumerate() {
            if rline.trim().is_empty() {
                break;
            }
            // Interior row: strip the dialog walls before matching columns.
            let (rbx, rinner) = strip_borders(rline);
            if rinner.trim().is_empty() {
                break; // border-only row ends the table
            }
            let _ = rbx;
            let starts: Vec<u16> = cols
                .iter()
                .map(|(hx, _)| word_start_at(rline, *hx).unwrap_or(u16::MAX))
                .collect();
            let aligned = starts.iter().filter(|&&s| s != u16::MAX).count();
            if aligned * 2 < xs.len() {
                break;
            }
            let selected = row_selected(screen, (y + 1 + ry) as u16);
            let cells: Vec<String> = cols.iter().map(|(x, _)| word_from(rline, *x)).collect();
            rows.push(TableRow {
                y: (y + 1 + ry) as u16,
                cells,
                selected,
            });
        }
        if rows.is_empty() {
            continue;
        }
        let x0 = cols[0].0;
        let width = cols
            .last()
            .map(|(x, l)| x + l.chars().count() as u16)
            .unwrap_or(0);
        let height = (rows.len() + 1) as u16;
        return Some(Widget {
            id: format!(
                "table/{}",
                slug(
                    &cols
                        .iter()
                        .map(|(_, l)| l.clone())
                        .collect::<Vec<_>>()
                        .join("-")
                )
            ),
            kind: WidgetKind::Table,
            bounds: Bounds {
                x: x0,
                y: y as u16,
                width,
                height,
            },
            confidence: Confidence::inferred(
                if rows.len() >= 3 { 0.9 } else { 0.75 },
                &["aligned-columns", "header-row"],
            ),
            detail: WidgetDetail {
                columns: cols,
                rows,
                ..Default::default()
            },
        });
    }
    None
}

// ─── Tree (item 18) ───────────────────────────────────────────────────

/// Detect a tree: 2+ rows with branch glyphs (├ └) at varying depths.
fn detect_tree(screen: &ScreenState) -> Option<Widget> {
    let mut items: Vec<WidgetItem> = Vec::new();
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        // Find the branch glyph position and depth (2 columns per level).
        let Some(pos) = chars.iter().position(|&c| c == '├' || c == '└') else {
            continue;
        };
        // Expand/collapse: the char after the branch run.
        let expanded = {
            // Look ahead past the glyph and its dashes.
            let rest: String = chars[pos..].iter().collect();
            if rest.starts_with("├─") || rest.starts_with("│") {
                Some(true)
            } else if rest.contains('▸') {
                Some(false)
            } else {
                None
            }
        };
        let label: String = chars[pos + 1..]
            .iter()
            .collect::<String>()
            .trim_start_matches(['─', ' ', '│', '├', '└', '▸', '▾'])
            .trim()
            .to_string();
        let depth = (pos / 2) as u32;
        let selected = row_selected(screen, y as u16);
        items.push(WidgetItem {
            y: y as u16,
            x: pos as u16,
            depth,
            label,
            selected,
            expanded,
        });
    }
    if items.len() < 2 {
        return None;
    }
    let max_depth = items.iter().map(|i| i.depth).max().unwrap_or(0);
    let y0 = items.first().map(|i| i.y).unwrap_or(0);
    let y1 = items.last().map(|i| i.y).unwrap_or(0);
    Some(Widget {
        id: "tree/items".to_string(),
        kind: WidgetKind::Tree,
        bounds: Bounds {
            x: 0,
            y: y0,
            width: screen.cols,
            height: y1 - y0 + 1,
        },
        confidence: Confidence::inferred(0.85, &["branch-glyphs", "indent-depths"]),
        detail: WidgetDetail {
            items,
            ..Default::default()
        },
    })
    .map(|mut w| {
        // depth variety strengthens the tree claim
        if max_depth == 0 {
            w.confidence.score = 0.7;
        }
        w
    })
}

// ─── ScrollableRegion (item 19) ───────────────────────────────────────

const THUMB_GLYPHS: [char; 5] = ['█', '▓', '▒', '#', '='];
const TRACK_GLYPHS: [char; 4] = ['░', ' ', '.', '-'];

/// Detect scroll regions: scrollbar geometry at a region/screen edge →
/// can_scroll_up/down / at_start/at_end.
fn detect_scroll_regions(screen: &ScreenState) -> Vec<Widget> {
    let mut out = Vec::new();
    // Vertical bars in the last two columns.
    if screen.rows > 0 && screen.cols >= 3 {
        let last_col = screen.cols - 1;
        for x in [last_col, screen.cols - 2] {
            let mut track = 0u16;
            let mut thumb = 0u16;
            let mut first_thumb = None;
            for (y, line) in screen.viewport_text.iter().enumerate() {
                let Some(c) = line.chars().nth(x as usize) else {
                    continue;
                };
                if THUMB_GLYPHS.contains(&c) {
                    thumb += 1;
                    if first_thumb.is_none() {
                        first_thumb = Some(y as u16);
                    }
                } else if TRACK_GLYPHS.contains(&c) {
                    track += 1;
                }
            }
            if thumb > 0 && track + thumb >= screen.rows / 2 && thumb < track + thumb {
                let start = first_thumb.unwrap_or(0);
                let edges = scroll_edges_from_thumb(start, thumb, screen.rows);
                out.push(Widget {
                    id: format!("scroll/vertical/@{},{}", x, 0),
                    kind: WidgetKind::ScrollableRegion,
                    bounds: Bounds {
                        x,
                        y: 0,
                        width: 1,
                        height: screen.rows,
                    },
                    confidence: Confidence::inferred(0.8, &["bar-glyphs"]),
                    detail: WidgetDetail {
                        scroll: Some(edges),
                        ..Default::default()
                    },
                });
                break;
            }
        }
    }
    // Horizontal bars in the last two rows.
    if screen.rows > 1 {
        for y in [screen.rows - 1, screen.rows - 2] {
            let Some(line) = screen.viewport_text.get(y as usize) else {
                continue;
            };
            let chars: Vec<char> = line.chars().collect();
            let mut thumb = 0u16;
            let mut track = 0u16;
            let mut first_thumb = None;
            for (x, &c) in chars.iter().enumerate() {
                if THUMB_GLYPHS.contains(&c) {
                    thumb += 1;
                    if first_thumb.is_none() {
                        first_thumb = Some(x as u16);
                    }
                } else if TRACK_GLYPHS.contains(&c) {
                    track += 1;
                }
            }
            if thumb > 0 && track + thumb >= screen.cols / 2 {
                let start = first_thumb.unwrap_or(0);
                let mut edges = scroll_edges_from_thumb(start, thumb, screen.cols);
                edges.up = false;
                edges.down = false;
                out.push(Widget {
                    id: format!("scroll/horizontal/@{},{}", 0, y),
                    kind: WidgetKind::ScrollableRegion,
                    bounds: Bounds {
                        x: 0,
                        y,
                        width: screen.cols,
                        height: 1,
                    },
                    confidence: Confidence::inferred(0.8, &["bar-glyphs"]),
                    detail: WidgetDetail {
                        scroll: Some(edges),
                        ..Default::default()
                    },
                });
                break;
            }
        }
    }
    // "More below" indicators: an ellipsis-only row below content.
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let t = line.trim();
        if t == "…" || t == "..." || t == "⋮" {
            out.push(Widget {
                id: format!("scroll/more/@{}", y),
                kind: WidgetKind::ScrollableRegion,
                bounds: Bounds {
                    x: 0,
                    y: y as u16,
                    width: screen.cols,
                    height: 1,
                },
                confidence: Confidence::inferred(0.6, &["ellipsis-row"]),
                detail: WidgetDetail {
                    scroll: Some(crate::semantic::node::ScrollEdges {
                        up: false,
                        down: true,
                        left: false,
                        right: false,
                    }),
                    ..Default::default()
                },
            });
        }
    }
    out
}

// ─── List (item 20) ───────────────────────────────────────────────────

/// Detect a list: 2+ consecutive bullet/numbered rows.
fn detect_list(screen: &ScreenState) -> Option<Widget> {
    let mut items = Vec::new();
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let t = line.trim_start();
        let indent = (line.len() - t.len()) as u16;
        let rest = t
            .strip_prefix("• ")
            .or_else(|| t.strip_prefix("- "))
            .or_else(|| t.strip_prefix("* "));
        if let Some(rest) = rest {
            let label = rest.trim().to_string();
            if !label.is_empty() {
                items.push(WidgetItem {
                    y: y as u16,
                    x: indent,
                    depth: 0,
                    label,
                    selected: row_selected(screen, y as u16),
                    expanded: None,
                });
            }
        }
    }
    if items.len() < 2 {
        return None;
    }
    let y0 = items.first().map(|i| i.y).unwrap_or(0);
    let y1 = items.last().map(|i| i.y).unwrap_or(0);
    Some(Widget {
        id: "list/items".to_string(),
        kind: WidgetKind::List,
        bounds: Bounds {
            x: 0,
            y: y0,
            width: screen.cols,
            height: y1 - y0 + 1,
        },
        confidence: Confidence::inferred(0.8, &["bullet-glyphs"]),
        detail: WidgetDetail {
            items,
            ..Default::default()
        },
    })
}

// ─── TextArea (item 21) ───────────────────────────────────────────────

/// Detect text areas: bordered multi-line input regions, or cursor-holding
/// field groups without labels.
fn detect_text_areas(screen: &ScreenState, _regions: &[Region]) -> Vec<Widget> {
    let mut out = Vec::new();
    // A 2+ row bordered box whose interior is blank-ish and contains the
    // cursor → an editor.
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let t = line.trim();
        if !t.starts_with("┌") {
            continue;
        }
        // Find the matching bottom.
        let width = display_width(t);
        for (by, bottom) in screen.viewport_text.iter().enumerate().skip(y + 2) {
            let bt = bottom.trim();
            if bt.starts_with("└") && display_width(bt) == width {
                // Interior rows: between y+1 and by-1.
                if by - y < 3 {
                    break;
                }
                let interior_blank = screen.viewport_text[y + 1..by]
                    .iter()
                    .all(|r| r.trim().trim_matches('│').trim().is_empty());
                let cursor_inside = screen.cursor.y > y as u16
                    && screen.cursor.y < by as u16
                    && screen.cursor.visible;
                if interior_blank || cursor_inside {
                    let value = screen.viewport_text[y + 1..by]
                        .iter()
                        .map(|r| r.trim_matches('│').trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push(Widget {
                        id: format!("textarea/@{},{}", 0, y),
                        kind: WidgetKind::TextArea,
                        bounds: Bounds {
                            x: 0,
                            y: y as u16,
                            width,
                            height: (by - y + 1) as u16,
                        },
                        confidence: Confidence::inferred(0.7, &["bordered-editor"]),
                        detail: WidgetDetail {
                            value: Some(value),
                            ..Default::default()
                        },
                    });
                }
                break;
            }
        }
    }
    out
}

// ─── Select/Dropdown (item 22) ────────────────────────────────────────

/// Detect a select: `Label: value ▸/▾` rows (closed) or a bordered option
/// list that popped over content (open dropdown).
fn detect_selects(screen: &ScreenState) -> Vec<Widget> {
    let mut out = Vec::new();
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let t = line.trim_end();
        // Closed select: value followed by a caret.
        if t.ends_with('▾') || t.ends_with('▸') || t.ends_with('▼') || t.ends_with('►') {
            if let Some(colon) = t.find(": ") {
                let label = t[..colon].trim().to_string();
                let value = t[colon + 2..]
                    .trim_end_matches(['▾', '▸', '▼', '►'])
                    .trim()
                    .to_string();
                if !label.is_empty() && !value.is_empty() {
                    out.push(Widget {
                        id: format!("select/{}", slug(&label)),
                        kind: WidgetKind::Select,
                        bounds: Bounds {
                            x: 0,
                            y: y as u16,
                            width: display_width(t),
                            height: 1,
                        },
                        confidence: Confidence::inferred(0.7, &["caret-suffix"]),
                        detail: WidgetDetail {
                            value: Some(value),
                            ..Default::default()
                        },
                    });
                    continue;
                }
            }
            // Bare dropdown marker without a label part.
            out.push(Widget {
                id: format!("dropdown/@{}", y),
                kind: WidgetKind::Dropdown,
                bounds: Bounds {
                    x: 0,
                    y: y as u16,
                    width: display_width(t),
                    height: 1,
                },
                confidence: Confidence::inferred(0.55, &["caret-suffix"]),
                detail: WidgetDetail::default(),
            });
        }
    }
    out
}

// ─── Menu (item 23) ───────────────────────────────────────────────────

/// Detect a menu bar: one row of short items separated by 2+ spaces with a
/// menu-ish word (File/Edit/View/Help…), and no colon-field on the line.
fn detect_menu(screen: &ScreenState) -> Option<Widget> {
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let t = line.trim();
        if t.contains(':') || t.contains('[') || t.contains('<') {
            continue;
        }
        let words: Vec<&str> = t.split_whitespace().collect();
        if words.len() < 2 || words.len() > 10 {
            continue;
        }
        let menu_word = words.iter().any(|w| {
            matches!(
                w.to_lowercase().as_str(),
                "file" | "edit" | "view" | "help" | "options" | "tools" | "window"
            )
        });
        if !menu_word {
            continue;
        }
        if !words.iter().all(|w| w.chars().count() <= 12) {
            continue;
        }
        let items = words
            .iter()
            .map(|w| WidgetItem {
                y: y as u16,
                x: 0,
                depth: 0,
                label: (*w).to_string(),
                selected: false,
                expanded: None,
            })
            .collect();
        return Some(Widget {
            id: "menu/bar".to_string(),
            kind: WidgetKind::Menu,
            bounds: Bounds {
                x: 0,
                y: y as u16,
                width: display_width(t),
                height: 1,
            },
            confidence: Confidence::inferred(0.75, &["menu-bar-words"]),
            detail: WidgetDetail {
                items,
                ..Default::default()
            },
        });
    }
    None
}

// ─── CommandPalette (item 24) ─────────────────────────────────────────

/// Detect a command palette: a top row starting with `:` or `>` followed by
/// a query (vim/emacs-style command line), often with suggestion rows below.
fn detect_palette(screen: &ScreenState) -> Option<Widget> {
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let t = line.trim_start();
        if !(t.starts_with(':') || t.starts_with('>')) {
            continue;
        }
        let query = t[1..].trim();
        if query.is_empty() || query.chars().count() > 60 {
            continue;
        }
        // Suggestion rows: following lines with short labels until a blank.
        let mut items = Vec::new();
        for (sy, sline) in screen.viewport_text.iter().enumerate().skip(y + 1) {
            let st = sline.trim();
            if st.is_empty() {
                break;
            }
            if st.chars().count() > 40 {
                break;
            }
            items.push(WidgetItem {
                y: sy as u16,
                x: 0,
                depth: 0,
                label: st.to_string(),
                selected: row_selected(screen, sy as u16),
                expanded: None,
            });
        }
        let height = 1 + items.len() as u16;
        return Some(Widget {
            id: "palette/command".to_string(),
            kind: WidgetKind::CommandPalette,
            bounds: Bounds {
                x: 0,
                y: y as u16,
                width: screen.cols,
                height,
            },
            confidence: Confidence::inferred(0.7, &["command-prompt-row"]),
            detail: WidgetDetail {
                value: Some(query.to_string()),
                items,
                ..Default::default()
            },
        });
    }
    None
}

// ─── SplitPane (item 25) ──────────────────────────────────────────────

/// Detect split panes: full-height/width divider lines of box glyphs.
fn detect_splits(screen: &ScreenState) -> Vec<Widget> {
    let mut out = Vec::new();
    // Vertical divider: a column where ≥ 3/4 of rows are │ ├ ┤ ┬ ┴ ┼.
    if screen.rows >= 3 {
        for x in 0..screen.cols {
            let mut hits = 0u16;
            for line in &screen.viewport_text {
                if let Some(c) = line.chars().nth(x as usize) {
                    if matches!(c, '│' | '├' | '┤' | '┬' | '┴' | '┼' | '║') {
                        hits += 1;
                    }
                }
            }
            if hits as u32 * 4 >= screen.rows as u32 * 3 {
                out.push(Widget {
                    id: format!("split/v/@{}", x),
                    kind: WidgetKind::SplitPane,
                    bounds: Bounds {
                        x,
                        y: 0,
                        width: 1,
                        height: screen.rows,
                    },
                    confidence: Confidence::inferred(0.7, &["full-height-divider"]),
                    detail: WidgetDetail {
                        split: Some(SplitInfo {
                            orientation: 'v',
                            pos: x,
                        }),
                        ..Default::default()
                    },
                });
            }
        }
    }
    // Horizontal divider.
    if screen.cols >= 3 {
        for (y, line) in screen.viewport_text.iter().enumerate() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let divider = t
                .chars()
                .all(|c| matches!(c, '─' | '├' | '┤' | '┬' | '┴' | '┼' | '═'))
                && t.chars().count() as u32 * 4 >= screen.cols as u32 * 3;
            if divider {
                out.push(Widget {
                    id: format!("split/h/@{}", y),
                    kind: WidgetKind::SplitPane,
                    bounds: Bounds {
                        x: 0,
                        y: y as u16,
                        width: screen.cols,
                        height: 1,
                    },
                    confidence: Confidence::inferred(0.7, &["full-width-divider"]),
                    detail: WidgetDetail {
                        split: Some(SplitInfo {
                            orientation: 'h',
                            pos: y as u16,
                        }),
                        ..Default::default()
                    },
                });
            }
        }
    }
    out
}

// ─── Toast/Alert/Validation (item 26) ─────────────────────────────────

/// Detect floating messages: a small bordered box with a severity word,
/// or an unbordered one-line severity message over content.
fn detect_toasts(screen: &ScreenState) -> Vec<Widget> {
    let mut out = Vec::new();
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let t = line.trim();
        let lower = t.to_lowercase();
        let (severity, kind) = if lower.starts_with("error") || lower.contains("failed") {
            ("error", WidgetKind::Alert)
        } else if lower.starts_with("warning") || lower.starts_with("warn") {
            ("warning", WidgetKind::Alert)
        } else if lower.starts_with("success") || lower.contains("saved") {
            ("success", WidgetKind::Toast)
        } else if lower.contains("required") || lower.contains("invalid") {
            ("validation", WidgetKind::Validation)
        } else {
            continue;
        };
        // One-line message; bounded boxes come through as regions already.
        out.push(Widget {
            id: format!("{}/{}/@{}", kind_slug(&kind), severity, y),
            kind,
            bounds: Bounds {
                x: 0,
                y: y as u16,
                width: display_width(t),
                height: 1,
            },
            confidence: Confidence::inferred(0.6, &["severity-word"]),
            detail: WidgetDetail {
                severity: Some(severity.to_string()),
                value: Some(t.to_string()),
                ..Default::default()
            },
        });
    }
    out
}

// ─── Help overlay / key hints (item 29) ────────────────────────────────

/// Detect a help overlay: a bordered box titled "help"/"keys"/"shortcuts".
fn detect_help_overlays(screen: &ScreenState) -> Vec<Widget> {
    let mut out = Vec::new();
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let t = line.trim();
        if !t.starts_with('┌') && !t.starts_with('╔') {
            continue;
        }
        let inner = t.trim_start_matches(['┌', '╔', '─', '═', ' ']);
        let inner = inner.trim_end_matches(['┐', '╗', '─', '═', ' ']);
        let lower = inner.to_lowercase();
        if !(lower.starts_with("help")
            || lower.starts_with("keys")
            || lower.starts_with("shortcuts")
            || lower.starts_with("key bindings"))
        {
            continue;
        }
        // Find the matching bottom border.
        let width = display_width(t);
        for (by, bottom) in screen.viewport_text.iter().enumerate().skip(y + 2) {
            let bt = bottom.trim();
            if (bt.starts_with('└') || bt.starts_with('╚')) && display_width(bt) == width {
                out.push(Widget {
                    id: format!("help/overlay/@{}", y),
                    kind: WidgetKind::HelpOverlay,
                    bounds: Bounds {
                        x: 0,
                        y: y as u16,
                        width,
                        height: (by - y + 1) as u16,
                    },
                    confidence: Confidence::inferred(0.85, &["titled-help-box"]),
                    detail: WidgetDetail::default(),
                });
                break;
            }
        }
    }
    out
}

/// Detect a key-hint bar: a top/bottom row of 2+ `key action` pairs.
fn detect_key_hint_bar(screen: &ScreenState) -> Option<Widget> {
    let edge_rows = [0usize, screen.viewport_text.len().saturating_sub(1)];
    for y in edge_rows {
        let Some(line) = screen.viewport_text.get(y) else {
            continue;
        };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Reuse the affordance hint extractor: 2+ pairs on one edge row is a
        // key-hint bar.
        let pairs = crate::semantic::affordance::extract_hint_pairs(t);
        if pairs.len() >= 2 {
            return Some(Widget {
                id: format!("keyhint/bar/@{}", y),
                kind: WidgetKind::KeyHintBar,
                bounds: Bounds {
                    x: 0,
                    y: y as u16,
                    width: screen.cols,
                    height: 1,
                },
                confidence: Confidence::inferred(0.8, &["edge-row-key-pairs"]),
                detail: WidgetDetail::default(),
            });
        }
    }
    None
}

fn kind_slug(k: &WidgetKind) -> String {
    format!("{:?}", k).to_lowercase()
}

fn slug(s: &str) -> String {
    let cleaned: String = s
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed: String = cleaned
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let truncated: String = collapsed.chars().take(24).collect();
    truncated.trim_end_matches('-').to_string()
}

fn display_width(s: &str) -> u16 {
    crate::screen::display_width(s)
}

// ─── Widget → SemanticNode expansion ──────────────────────────────────

/// Expand a detected widget into its node subtree (used by the tree builder).
pub fn widget_to_node(w: &Widget, screen: &ScreenState) -> crate::semantic::node::SemanticNode {
    use crate::semantic::node::{NodeState, Role, SemanticNode};

    let mut root = SemanticNode {
        id: w.id.clone(),
        role: match &w.kind {
            WidgetKind::Table => Role::Table,
            WidgetKind::Tree => Role::Tree,
            WidgetKind::ScrollableRegion => Role::ScrollableRegion,
            WidgetKind::List => Role::List,
            WidgetKind::TextArea => Role::TextArea,
            WidgetKind::Select => Role::Select,
            WidgetKind::Dropdown => Role::Dropdown,
            WidgetKind::Menu => Role::Menu,
            WidgetKind::CommandPalette => Role::CommandPalette,
            WidgetKind::SplitPane => Role::SplitPane,
            WidgetKind::Toast => Role::Toast,
            WidgetKind::Alert => Role::Alert,
            WidgetKind::Validation => Role::Validation,
            WidgetKind::HelpOverlay => Role::HelpOverlay,
            WidgetKind::KeyHintBar => Role::KeyHint,
        },
        parent: None,
        bounds: w.bounds.clone(),
        label: None,
        value: w.detail.value.clone(),
        state: NodeState::default(),
        children: Vec::new(),
        affordances: Vec::new(),
        identity: None,
        confidence: w.confidence.clone(),
    };

    match &w.kind {
        WidgetKind::Table => {
            // Header node with one child per column, then one row node per
            // data row with one cell child per column.
            let mut header = SemanticNode {
                id: format!("{}/header", w.id),
                role: Role::TableHeader,
                parent: Some(w.id.clone()),
                bounds: Bounds {
                    x: w.bounds.x,
                    y: w.bounds.y,
                    width: w.bounds.width,
                    height: 1,
                },
                label: w
                    .detail
                    .columns
                    .iter()
                    .map(|(_, l)| l.clone())
                    .collect::<Vec<_>>()
                    .join(" | ")
                    .into(),
                value: None,
                state: NodeState::default(),
                children: Vec::new(),
                affordances: Vec::new(),
                identity: None,
                confidence: w.confidence.clone(),
            };
            for (i, (x, l)) in w.detail.columns.iter().enumerate() {
                header.children.push(SemanticNode {
                    id: format!("{}/header/col/{}", w.id, i),
                    role: Role::TableCell,
                    parent: Some(header.id.clone()),
                    bounds: Bounds {
                        x: *x,
                        y: w.bounds.y,
                        width: crate::screen::display_width(l).max(1),
                        height: 1,
                    },
                    label: Some(l.clone()),
                    value: None,
                    state: NodeState::default(),
                    children: Vec::new(),
                    affordances: Vec::new(),
                    identity: None,
                    confidence: w.confidence.clone(),
                });
            }
            root.children.push(header);

            for (ri, row) in w.detail.rows.iter().enumerate() {
                let mut rnode = SemanticNode {
                    id: format!("{}/row/{}", w.id, ri),
                    role: Role::TableRow,
                    parent: Some(w.id.clone()),
                    bounds: Bounds {
                        x: w.bounds.x,
                        y: row.y,
                        width: w.bounds.width,
                        height: 1,
                    },
                    label: row.cells.first().cloned(),
                    value: None,
                    state: NodeState {
                        selected: row.selected,
                        focusable: true,
                        ..NodeState::default()
                    },
                    children: Vec::new(),
                    affordances: Vec::new(),
                    identity: None,
                    confidence: w.confidence.clone(),
                };
                for (ci, ((x, hl), cell)) in
                    w.detail.columns.iter().zip(row.cells.iter()).enumerate()
                {
                    let cw = crate::screen::display_width(hl)
                        .max(crate::screen::display_width(cell))
                        .max(1);
                    rnode.children.push(SemanticNode {
                        id: format!("{}/row/{}/cell/{}", w.id, ri, ci),
                        role: Role::TableCell,
                        parent: Some(rnode.id.clone()),
                        bounds: Bounds {
                            x: *x,
                            y: row.y,
                            width: cw,
                            height: 1,
                        },
                        label: Some(cell.clone()),
                        value: None,
                        state: NodeState::default(),
                        children: Vec::new(),
                        affordances: Vec::new(),
                        identity: None,
                        confidence: w.confidence.clone(),
                    });
                }
                root.children.push(rnode);
            }
        }
        WidgetKind::Tree | WidgetKind::List | WidgetKind::Menu => {
            for (i, item) in w.detail.items.iter().enumerate() {
                root.children.push(SemanticNode {
                    id: format!("{}/item/{}/{}", w.id, i, slug(&item.label)),
                    role: match &w.kind {
                        WidgetKind::Tree => Role::TreeItem,
                        WidgetKind::List => Role::ListItem,
                        _ => Role::MenuItem,
                    },
                    parent: Some(w.id.clone()),
                    bounds: Bounds {
                        x: item.x,
                        y: item.y,
                        width: crate::screen::display_width(&item.label).max(1),
                        height: 1,
                    },
                    label: Some(item.label.clone()),
                    value: None,
                    state: NodeState {
                        selected: item.selected,
                        focusable: true,
                        depth: Some(item.depth),
                        expanded: item.expanded,
                        ..NodeState::default()
                    },
                    children: Vec::new(),
                    affordances: Vec::new(),
                    identity: None,
                    confidence: w.confidence.clone(),
                });
            }
        }
        WidgetKind::CommandPalette => {
            for (i, item) in w.detail.items.iter().enumerate() {
                root.children.push(SemanticNode {
                    id: format!("{}/suggestion/{}", w.id, i),
                    role: Role::ListItem,
                    parent: Some(w.id.clone()),
                    bounds: Bounds {
                        x: 0,
                        y: item.y,
                        width: crate::screen::display_width(&item.label).max(1),
                        height: layer_height(screen),
                    },
                    label: Some(item.label.clone()),
                    value: None,
                    state: NodeState {
                        selected: item.selected,
                        focusable: true,
                        ..NodeState::default()
                    },
                    children: Vec::new(),
                    affordances: Vec::new(),
                    identity: None,
                    confidence: w.confidence.clone(),
                });
            }
        }
        WidgetKind::ScrollableRegion => {
            if let Some(edges) = &w.detail.scroll {
                root.state.can_scroll = Some(*edges);
            }
        }
        _ => {}
    }
    root
}

fn layer_height(_screen: &ScreenState) -> u16 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{CursorState, ProcessState};

    fn screen(rows: Vec<&str>, cols: u16) -> ScreenState {
        ScreenState {
            cols,
            rows: rows.len() as u16,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: rows.into_iter().map(String::from).collect(),
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    #[test]
    fn table_with_rows_and_cells() {
        let s = screen(
            vec![
                "Name        Status      Latency",
                "db-primary  connected   12ms",
                "db-replica  connected   14ms",
            ],
            60,
        );
        let w = detect_table(&s).expect("table");
        assert_eq!(w.detail.columns.len(), 3);
        assert_eq!(w.detail.rows.len(), 2);
        assert_eq!(w.detail.rows[0].cells[0], "db-primary");
        let n = widget_to_node(&w, &s);
        assert_eq!(n.children.len(), 3, "header + 2 rows");
        let header = &n.children[0];
        assert_eq!(header.role, crate::semantic::node::Role::TableHeader);
        assert_eq!(header.children.len(), 3, "one cell per column");
        let row0 = &n.children[1];
        assert_eq!(row0.role, crate::semantic::node::Role::TableRow);
        assert_eq!(row0.children.len(), 3, "cells nested in the row");
        assert_eq!(row0.children[1].label.as_deref(), Some("connected"));
    }

    #[test]
    fn tree_items_with_depth() {
        let s = screen(vec!["root", "├─ src", "│  └─ main.rs", "└─ tests"], 40);
        let w = detect_tree(&s).expect("tree");
        assert!(w.detail.items.len() >= 3);
        let n = widget_to_node(&w, &s);
        let roles: Vec<_> = n.children.iter().map(|c| c.role.clone()).collect();
        assert!(roles
            .iter()
            .all(|r| *r == crate::semantic::node::Role::TreeItem));
        // Depth from glyph position / 2.
        let depths: Vec<u32> = n.children.iter().filter_map(|c| c.state.depth).collect();
        assert!(depths.contains(&0), "depths: {depths:?}");
    }

    #[test]
    fn scroll_region_reports_edges() {
        // 21 columns: content in 0..19, bar column at 20 (the last).
        let mut rows: Vec<String> = Vec::new();
        for i in 0..6 {
            let bar = if (3..6).contains(&i) { "█" } else { "░" };
            rows.push(format!("content {:<12}{}", "", bar));
        }
        assert_eq!(rows[0].chars().count(), 21, "fixture width");
        assert_eq!(rows[0].chars().nth(20), Some('░'), "bar at last column");
        // Thumb at rows 3-5: can scroll up, not down (at end).
        let s = screen(rows.iter().map(|r| r.as_str()).collect(), 21);
        let ws = detect_scroll_regions(&s);
        let v = ws
            .iter()
            .find(|w| w.detail.scroll.is_some() && w.bounds.width == 1)
            .expect("vertical scroll region");
        let edges = v.detail.scroll.unwrap();
        assert!(edges.up, "thumb not at top");
        assert!(!edges.down, "thumb at bottom");
        assert!(edges.at_end());
    }

    #[test]
    fn ellipsis_row_means_more_below() {
        let s = screen(vec!["item one", "item two", "…"], 20);
        let ws = detect_scroll_regions(&s);
        assert!(
            ws.iter().any(|w| w.detail.scroll.is_some_and(|e| e.down)),
            "ellipsis row must report can_scroll_down: {ws:?}"
        );
    }

    #[test]
    fn list_items_detected() {
        let s = screen(vec!["• one", "• two", "• three"], 20);
        let w = detect_list(&s).expect("list");
        assert_eq!(w.detail.items.len(), 3);
        let n = widget_to_node(&w, &s);
        assert_eq!(n.children.len(), 3);
    }

    #[test]
    fn select_with_caret() {
        let s = screen(vec!["Theme: dark ▾"], 30);
        let ws = detect_selects(&s);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].kind, WidgetKind::Select);
        assert_eq!(ws[0].detail.value.as_deref(), Some("dark"));
        assert_eq!(ws[0].id, "select/theme");
    }

    #[test]
    fn command_palette_detected() {
        let s = screen(vec![":grep", "find in files", "replace all"], 30);
        let w = detect_palette(&s).expect("palette");
        assert_eq!(w.detail.value.as_deref(), Some("grep"));
        assert_eq!(w.detail.items.len(), 2);
    }

    #[test]
    fn split_pane_vertical_divider() {
        let rows = vec!["left   │   right"; 6];
        let s = screen(rows, 15);
        let ws = detect_splits(&s);
        assert!(ws.iter().any(|w| w.kind == WidgetKind::SplitPane), "{ws:?}");
    }

    #[test]
    fn toast_and_alert_detected() {
        let s = screen(
            vec!["body", "Settings saved", "error: host unreachable"],
            40,
        );
        let ws = detect_toasts(&s);
        assert!(ws.iter().any(|w| w.kind == WidgetKind::Toast));
        assert!(ws.iter().any(|w| w.kind == WidgetKind::Alert));
    }

    #[test]
    fn menu_bar_detected() {
        let s = screen(vec!["File  Edit  View  Help"], 40);
        let w = detect_menu(&s).expect("menu");
        assert_eq!(w.detail.items.len(), 4);
    }

    #[test]
    fn all_families_surface_in_the_tree() {
        let rows = vec![
            "File  Edit  View  Help",
            "Name        Status      Latency",
            "db-primary  connected   12ms",
            "db-replica  connected   14ms",
            "├─ src",
            "│  └─ main.rs",
            "└─ tests",
        ];
        let s = screen(rows, 60);
        let regions = crate::semantic::regions::detect_regions(&s);
        let ws = detect_widgets(&s, &regions);
        let kinds: Vec<_> = ws.iter().map(|w| w.kind.clone()).collect();
        assert!(kinds.contains(&WidgetKind::Table), "{kinds:?}");
        assert!(kinds.contains(&WidgetKind::Tree), "{kinds:?}");
        assert!(kinds.contains(&WidgetKind::Menu), "{kinds:?}");
        let tree = crate::semantic::tree_builder::build_tree(&s);
        let flat = tree.flatten();
        let roles: Vec<_> = flat.iter().map(|n| &n.role).collect();
        use crate::semantic::node::Role;
        assert!(roles.contains(&&Role::TableHeader), "{:?}", tree.render());
        assert!(roles.contains(&&Role::TableRow));
        assert!(roles.contains(&&Role::TableCell));
        assert!(roles.contains(&&Role::TreeItem));
        assert!(roles.contains(&&Role::MenuItem));
    }

    #[test]
    fn prose_yields_no_widgets() {
        let s = screen(
            vec![
                "Hello and welcome to the application.",
                "Second line of body copy.",
            ],
            50,
        );
        let ws = detect_widgets(&s, &[]);
        assert!(ws.is_empty(), "no widgets in prose: {ws:?}");
    }
}
