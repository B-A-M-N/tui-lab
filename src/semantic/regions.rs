//! Region recognition using a border graph (spec items 22-23).

use crate::screen::ScreenState;
use crate::semantic::confidence::Confidence;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RegionKind {
    Dialog,
    Panel,
    Toolbar,
    Footer,
    List,
    Table,
    Unknown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Region {
    pub id: String,
    pub kind: RegionKind,
    pub title: Option<String>,
    pub bounds: Bounds,
    pub confidence: Confidence,
    pub parent_id: Option<String>,
    pub child_ids: Vec<String>,
    pub clipping_state: ClippingState,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ClippingState {
    None,
    Top,
    Bottom,
    Left,
    Right,
    Multiple,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bounds {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BorderConnectivity {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

fn cell_connectivity(c: char) -> BorderConnectivity {
    use BorderConnectivity as B;
    match c {
        '─' | '═' | '╾' | '╼' | '╌' => B {
            left: true,
            right: true,
            up: false,
            down: false,
        },
        '│' | '║' | '┃' => B {
            left: false,
            right: false,
            up: true,
            down: true,
        },
        '┌' => B {
            left: false,
            right: true,
            up: false,
            down: true,
        },
        '┐' => B {
            left: true,
            right: false,
            up: false,
            down: true,
        },
        '└' => B {
            left: false,
            right: true,
            up: true,
            down: false,
        },
        '┘' => B {
            left: true,
            right: false,
            up: true,
            down: false,
        },
        '╔' => B {
            left: false,
            right: true,
            up: false,
            down: true,
        },
        '╗' => B {
            left: true,
            right: false,
            up: false,
            down: true,
        },
        '╚' => B {
            left: false,
            right: true,
            up: true,
            down: false,
        },
        '╝' => B {
            left: true,
            right: false,
            up: true,
            down: false,
        },
        '┏' => B {
            left: false,
            right: true,
            up: false,
            down: true,
        },
        '┓' => B {
            left: true,
            right: false,
            up: false,
            down: true,
        },
        '┗' => B {
            left: false,
            right: true,
            up: true,
            down: false,
        },
        '┛' => B {
            left: true,
            right: false,
            up: true,
            down: false,
        },
        '├' => B {
            left: false,
            right: true,
            up: true,
            down: true,
        },
        '┤' => B {
            left: true,
            right: false,
            up: true,
            down: true,
        },
        '┬' => B {
            left: true,
            right: true,
            up: false,
            down: true,
        },
        '┴' => B {
            left: true,
            right: true,
            up: true,
            down: false,
        },
        '╠' => B {
            left: false,
            right: true,
            up: true,
            down: true,
        },
        '╣' => B {
            left: true,
            right: false,
            up: true,
            down: true,
        },
        '╦' => B {
            left: true,
            right: true,
            up: false,
            down: true,
        },
        '╩' => B {
            left: true,
            right: true,
            up: true,
            down: false,
        },
        '┼' => B {
            left: true,
            right: true,
            up: true,
            down: true,
        },
        '╬' => B {
            left: true,
            right: true,
            up: true,
            down: true,
        },
        '╭' => B {
            left: false,
            right: true,
            up: false,
            down: true,
        },
        '╮' => B {
            left: true,
            right: false,
            up: false,
            down: true,
        },
        '╰' => B {
            left: false,
            right: true,
            up: true,
            down: false,
        },
        '╯' => B {
            left: true,
            right: false,
            up: true,
            down: false,
        },
        _ => B {
            left: false,
            right: false,
            up: false,
            down: false,
        },
    }
}

pub fn detect_regions(screen: &ScreenState) -> Vec<Region> {
    let rows = screen.rows as usize;
    if rows == 0 {
        return Vec::new();
    }
    let cols = screen.cols as usize;

    let mut conn: Vec<Vec<Option<BorderConnectivity>>> = vec![vec![None; cols]; rows];
    for (y, row) in screen.viewport_text.iter().enumerate() {
        if y >= rows {
            break;
        }
        for (x, c) in row.chars().enumerate() {
            if x >= cols {
                break;
            }
            let b = cell_connectivity(c);
            if b != cell_connectivity('\0') {
                conn[y][x] = Some(b);
            }
        }
    }

    let mut regions = Vec::new();
    let mut visited = vec![vec![false; cols]; rows];

    for y in 0..rows {
        for x in 0..cols {
            if visited[y][x] {
                continue;
            }
            let is_top_left = match conn[y][x] {
                Some(c) => c.right && c.down && !c.left && !c.up,
                None => false,
            };
            if !is_top_left {
                continue;
            }
            if let Some(region) = trace_rectangle(screen, &conn, &mut visited, x, y, rows, cols) {
                regions.push(region);
            }
        }
    }

    regions.sort_by_key(|r| {
        let area = r.bounds.width as u32 * r.bounds.height as u32;
        std::cmp::Reverse(area)
    });

    assign_hierarchy(&mut regions);

    for i in 0..regions.len() {
        regions[i].kind = infer_role(&regions[i], cols as u16, rows as u16);
    }

    regions
}

fn trace_rectangle(
    screen: &ScreenState,
    conn: &[Vec<Option<BorderConnectivity>>],
    visited: &mut [Vec<bool>],
    start_x: usize,
    start_y: usize,
    rows: usize,
    cols: usize,
) -> Option<Region> {
    let mut top_right = start_x;
    while top_right + 1 < cols {
        match conn[start_y][top_right + 1] {
            Some(c) if c.left => top_right += 1,
            _ => break,
        }
    }

    let mut bottom_y = start_y;
    while bottom_y + 1 < rows {
        match conn[bottom_y + 1][top_right] {
            Some(c) if c.up => bottom_y += 1,
            _ => break,
        }
    }

    let mut valid_bottom = true;
    for x in start_x..=top_right {
        match conn[bottom_y][x] {
            Some(c) if c.left || c.right => {}
            _ => {
                valid_bottom = false;
                break;
            }
        }
    }

    let mut valid_left = true;
    for y in start_y..=bottom_y {
        match conn[y][start_x] {
            Some(c) if c.up || c.down => {}
            _ => {
                valid_left = false;
                break;
            }
        }
    }

    if !valid_bottom || !valid_left {
        return None;
    }

    let width = (top_right - start_x + 1) as u16;
    let height = (bottom_y - start_y + 1) as u16;

    for y in start_y..=bottom_y {
        for x in start_x..=top_right {
            visited[y][x] = true;
        }
    }

    let clipping_state = check_clipping(start_x, start_y, width, height, cols as u16, rows as u16);

    let title = if height >= 3 {
        extract_title_from_text(&screen.viewport_text, start_x, start_y + 1, top_right)
    } else if height >= 2 {
        extract_title_from_text(&screen.viewport_text, start_x, start_y, top_right)
    } else {
        None
    };

    Some(Region {
        id: format!("region-{}-{}", start_x, start_y),
        kind: RegionKind::Unknown,
        title,
        bounds: Bounds {
            x: start_x as u16,
            y: start_y as u16,
            width,
            height,
        },
        confidence: Confidence::inferred(0.9, &["border-graph-traced"]),
        parent_id: None,
        child_ids: Vec::new(),
        clipping_state,
    })
}

fn extract_title_from_text(
    viewport_text: &[String],
    start_x: usize,
    row: usize,
    end_x: usize,
) -> Option<String> {
    if row >= viewport_text.len() {
        return None;
    }
    let line = &viewport_text[row];
    if start_x >= line.len() {
        return None;
    }
    let title: String = line
        .chars()
        .skip(start_x + 1)
        .take(end_x.saturating_sub(start_x + 1))
        .collect();
    let trimmed = title.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn check_clipping(x: usize, y: usize, w: u16, h: u16, cols: u16, rows: u16) -> ClippingState {
    let mut flags = Vec::new();
    if y == 0 {
        flags.push(ClippingState::Top);
    }
    if x == 0 {
        flags.push(ClippingState::Left);
    }
    if x + w as usize >= cols as usize {
        flags.push(ClippingState::Right);
    }
    if y + h as usize >= rows as usize {
        flags.push(ClippingState::Bottom);
    }
    match flags.len() {
        0 => ClippingState::None,
        1 => flags.into_iter().next().unwrap(),
        _ => ClippingState::Multiple,
    }
}

fn assign_hierarchy(regions: &mut [Region]) {
    for i in 0..regions.len() {
        for j in 0..regions.len() {
            if i == j {
                continue;
            }
            if contains_bounds(&regions[j].bounds, &regions[i].bounds) {
                regions[i].parent_id = Some(regions[j].id.clone());
                regions[j].child_ids.push(regions[i].id.clone());
                break;
            }
        }
    }
}

fn contains_bounds(outer: &Bounds, inner: &Bounds) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
        && !(inner.x == outer.x
            && inner.y == outer.y
            && inner.width == outer.width
            && inner.height == outer.height)
}

fn infer_role(region: &Region, cols: u16, rows: u16) -> RegionKind {
    let b = &region.bounds;
    let area = b.width as f32 * b.height as f32;
    let screen_area = cols as f32 * rows as f32;
    let coverage = area / screen_area;

    let is_centered = (b.x as i32 - (cols as i32 - b.width as i32) / 2).abs() < 5
        && (b.y as i32 - (rows as i32 - b.height as i32) / 2).abs() < 5;
    let has_title = region.title.is_some();

    if is_centered && has_title && coverage < 0.8 {
        RegionKind::Dialog
    } else if b.y < 3 && coverage > 0.5 {
        RegionKind::Toolbar
    } else if b.y + b.height >= rows - 3 && coverage > 0.5 {
        RegionKind::Footer
    } else if b.height > b.width * 2 {
        RegionKind::List
    } else if has_title {
        RegionKind::Panel
    } else {
        RegionKind::Unknown
    }
}
