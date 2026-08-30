//! Semantic relationships between UI elements (spec item 28).
//!
//! Computes spatial relationships between controls and regions, enabling
//! assertions like `inside`, `above`, `below`, `left_of`, `right_of`, `aligned`.

use crate::semantic::controls::Control;
use crate::semantic::regions::Region;

/// Spatial relationships between two elements.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relationship {
    Inside,
    Above,
    Below,
    LeftOf,
    RightOf,
    AlignedRow,
    AlignedColumn,
    Overlapping,
    Adjacent,
}

/// A box represented as inclusive-exclusive i32 ranges: (left, top, right, bottom).
type Rect = (i32, i32, i32, i32);

/// Convert a (x, y, w, h) in u16 to a Rect, guarding against overflow.
fn to_rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
    (x as i32, y as i32, (x as i32) + (w as i32), (y as i32) + (h as i32))
}

// ─── Pure predicates ───────────────────────────────────────────────

/// b1 is strictly inside b2 (strict containment; equal boxes are NOT inside).
fn is_inside(b1: Rect, b2: Rect) -> bool {
    let (l1, t1, r1, b1b) = b1;
    let (l2, t2, r2, b2b) = b2;
    l1 >= l2 && t1 >= t2 && r1 <= r2 && b1b <= b2b
        && (l1 > l2 || t1 > t2 || r1 < r2 || b1b < b2b)
}

/// b1 is entirely above b2 (bottom of b1 <= top of b2, vertical gap >= 0).
fn is_above(b1: Rect, b2: Rect) -> bool {
    b1.3 <= b2.1
}

/// b1 is entirely below b2 (top of b1 >= bottom of b2).
fn is_below(b1: Rect, b2: Rect) -> bool {
    b1.1 >= b2.3
}

/// b1 is entirely to the left of b2 (right of b1 <= left of b2).
fn is_left_of(b1: Rect, b2: Rect) -> bool {
    b1.2 <= b2.0
}

/// b1 is entirely to the right of b2 (left of b1 >= right of b2).
fn is_right_of(b1: Rect, b2: Rect) -> bool {
    b1.0 >= b2.2
}

/// Vertical ranges overlap (rows intersect), i.e. b1's vertical span intersects b2's.
fn is_aligned_row(b1: Rect, b2: Rect) -> bool {
    b1.1 <= b2.3 && b2.1 <= b1.3
}

/// Horizontal ranges overlap (columns intersect).
fn is_aligned_column(b1: Rect, b2: Rect) -> bool {
    b1.0 <= b2.2 && b2.0 <= b1.2
}

/// Rectangles intersect (overlapping interiors) but neither contains the other.
/// Identical boxes are NOT overlapping (they are the same rectangle).
fn is_overlapping(b1: Rect, b2: Rect) -> bool {
    // Must not be identical
    if b1 == b2 {
        return false;
    }
    b1.0 < b2.2 && b2.0 < b1.2 && b1.1 < b2.3 && b2.1 < b1.3
        && !is_inside(b1, b2)
        && !is_inside(b2, b1)
}

/// Gap is exactly 0 in one axis while ranges overlap in the other axis.
fn is_adjacent(b1: Rect, b2: Rect) -> bool {
    // Adjacent above/below: touch vertically (b1 bottom == b2 top or vice versa)
    // and horizontal ranges overlap.
    let touching_v = b1.3 == b2.1 || b2.3 == b1.1;
    let touching_h = b1.2 == b2.0 || b2.2 == b1.0;
    if touching_v && (b1.0 < b2.2 && b2.0 < b1.2) {
        return true;
    }
    if touching_h && (b1.1 < b2.3 && b2.1 < b1.3) {
        return true;
    }
    false
}

/// Compute ALL true relationships for a pair of boxes.
fn all_relationships(b1: Rect, b2: Rect) -> Vec<Relationship> {
    let mut result = Vec::new();
    if is_inside(b1, b2) {
        result.push(Relationship::Inside);
    }
    if is_above(b1, b2) {
        result.push(Relationship::Above);
    }
    if is_below(b1, b2) {
        result.push(Relationship::Below);
    }
    if is_left_of(b1, b2) {
        result.push(Relationship::LeftOf);
    }
    if is_right_of(b1, b2) {
        result.push(Relationship::RightOf);
    }
    if is_aligned_row(b1, b2) {
        result.push(Relationship::AlignedRow);
    }
    if is_aligned_column(b1, b2) {
        result.push(Relationship::AlignedColumn);
    }
    if is_overlapping(b1, b2) {
        result.push(Relationship::Overlapping);
    }
    if is_adjacent(b1, b2) {
        result.push(Relationship::Adjacent);
    }
    result
}

/// Relationship engine: computes relationships from bounds.
pub struct RelationshipEngine;

impl RelationshipEngine {
    /// Find all relationships between controls.
    ///
    /// Returns `(i, j, Vec<Relationship>)` for every ordered pair where
    /// at least one relationship holds.  Empty relationship lists are omitted.
    pub fn find_control_relationships(
        controls: &[Control],
    ) -> Vec<(usize, usize, Vec<Relationship>)> {
        let mut results = Vec::new();
        for (i, c1) in controls.iter().enumerate() {
            for (j, c2) in controls.iter().enumerate() {
                if i == j {
                    continue;
                }
                let b1 = to_rect(c1.bounds.x, c1.bounds.y, c1.bounds.width, c1.bounds.height);
                let b2 = to_rect(c2.bounds.x, c2.bounds.y, c2.bounds.width, c2.bounds.height);
                let rels = dedup(all_relationships(b1, b2));
                if !rels.is_empty() {
                    results.push((i, j, rels));
                }
            }
        }
        results
    }

    /// Find all relationships between controls and regions.
    ///
    /// Returns `(control_label, region_id, Vec<Relationship>)`.
    /// Empty relationship lists are omitted.
    pub fn find_region_relationships(
        controls: &[Control],
        regions: &[Region],
    ) -> Vec<(String, String, Vec<Relationship>)> {
        let mut results = Vec::new();
        for c in controls {
            for r in regions {
                let cb = to_rect(c.bounds.x, c.bounds.y, c.bounds.width, c.bounds.height);
                let rb = to_rect(r.bounds.x, r.bounds.y, r.bounds.width, r.bounds.height);
                let rels = dedup(all_relationships(cb, rb));
                if !rels.is_empty() {
                    results.push((c.label.clone(), r.id.clone(), rels));
                }
            }
        }
        results
    }

    /// Find all relationships between regions.
    ///
    /// Returns `(id1, id2, Vec<Relationship>)` for i < j pairs.
    /// Empty relationship lists are omitted.
    pub fn find_region_region_relationships(
        regions: &[Region],
    ) -> Vec<(String, String, Vec<Relationship>)> {
        let mut results = Vec::new();
        for (i, r1) in regions.iter().enumerate() {
            for (j, r2) in regions.iter().enumerate() {
                if i >= j {
                    continue;
                }
                let b1 = to_rect(r1.bounds.x, r1.bounds.y, r1.bounds.width, r1.bounds.height);
                let b2 = to_rect(r2.bounds.x, r2.bounds.y, r2.bounds.width, r2.bounds.height);
                let rels = dedup(all_relationships(b1, b2));
                if !rels.is_empty() {
                    results.push((r1.id.clone(), r2.id.clone(), rels));
                }
            }
        }
        results
    }
}

/// Deduplicate relationships while preserving order.
fn dedup(mut v: Vec<Relationship>) -> Vec<Relationship> {
    v.dedup();
    v
}

// ─── Unit tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Predicate tests ──

    /// LeftOf + AlignedRow + Adjacent: b1 at (0,0) w=3 h=2, b2 at (3,0) w=4 h=2
    #[test]
    fn test_left_of_aligned_row_adjacent() {
        let b1 = to_rect(0, 0, 3, 2); // (0,0,3,2)
        let b2 = to_rect(3, 0, 4, 2); // (3,0,7,2)
        let rels = all_relationships(b1, b2);
        assert!(rels.contains(&Relationship::LeftOf), "should be LeftOf: {:?}", rels);
        assert!(rels.contains(&Relationship::AlignedRow), "should be AlignedRow: {:?}", rels);
        assert!(rels.contains(&Relationship::Adjacent), "should be Adjacent: {:?}", rels);
    }

    /// Empty pair returns empty vec
    #[test]
    fn test_engine_empty_no_controls() {
        let controls: Vec<Control> = Vec::new();
        let rels = RelationshipEngine::find_control_relationships(&controls);
        assert!(rels.is_empty());
    }

    /// Containment: b1 strictly inside b2 → [Inside] only, NOT Overlapping
    #[test]
    fn test_inside_not_overlapping() {
        let b1 = to_rect(5, 5, 4, 4); // (5,5,9,9)
        let b2 = to_rect(0, 0, 20, 20); // (0,0,20,20)
        let rels = all_relationships(b1, b2);
        assert!(rels.contains(&Relationship::Inside), "b1 inside b2: {:?}", rels);
        assert!(!rels.contains(&Relationship::Overlapping), "Inside and Overlapping are exclusive: {:?}", rels);
    }

    /// Identical boxes: not Inside (strict containment), but AlignedRow + AlignedColumn
    #[test]
    fn test_identical_boxes() {
        let b = to_rect(10, 10, 5, 5); // (10,10,15,15)
        let rels = all_relationships(b, b);
        assert!(rels.contains(&Relationship::AlignedRow), "identical → AlignedRow: {:?}", rels);
        assert!(rels.contains(&Relationship::AlignedColumn), "identical → AlignedColumn: {:?}", rels);
        assert!(!rels.contains(&Relationship::Inside), "identical boxes are NOT Inside: {:?}", rels);
        assert!(!rels.contains(&Relationship::Overlapping), "identical boxes are NOT Overlapping: {:?}", rels);
        assert!(!rels.contains(&Relationship::Above), "identical → not Above: {:?}", rels);
        assert!(!rels.contains(&Relationship::LeftOf), "identical → not LeftOf: {:?}", rels);
    }

    /// Disjoint diagonal: b1 at (0,0) w=2 h=2, b2 at (3,3) w=2 h=2
    /// b1 is Above + LeftOf b2; b2 is Below + RightOf b1
    #[test]
    fn test_disjoint_diagonal() {
        let b1 = to_rect(0, 0, 2, 2); // (0,0,2,2)
        let b2 = to_rect(3, 3, 2, 2); // (3,3,5,5)
        let rels_12 = all_relationships(b1, b2);
        assert!(rels_12.contains(&Relationship::Above), "b1 above b2: {:?}", rels_12);
        assert!(rels_12.contains(&Relationship::LeftOf), "b1 left of b2: {:?}", rels_12);
        assert!(!rels_12.contains(&Relationship::Adjacent), "diagonal is NOT adjacent: {:?}", rels_12);
        assert!(!rels_12.contains(&Relationship::AlignedRow), "diagonal → not AlignedRow: {:?}", rels_12);
        assert!(!rels_12.contains(&Relationship::AlignedColumn), "diagonal → not AlignedColumn: {:?}", rels_12);

        // Reverse direction
        let rels_21 = all_relationships(b2, b1);
        assert!(rels_21.contains(&Relationship::Below), "b2 below b1: {:?}", rels_21);
        assert!(rels_21.contains(&Relationship::RightOf), "b2 right of b1: {:?}", rels_21);
        assert!(!rels_21.contains(&Relationship::Above), "b2 is NOT above b1: {:?}", rels_21);
    }

    /// Zero-width / zero-height boxes should not panic.
    #[test]
    fn test_zero_dimension_boxes() {
        let line_v = to_rect(5, 0, 0, 10); // vertical line at x=5
        let line_h = to_rect(5, 5, 10, 0); // horizontal line at y=5
        let rels = all_relationships(line_v, line_h);
        // They intersect at (5,5), so they should be AlignedRow + AlignedColumn
        assert!(rels.contains(&Relationship::AlignedRow), "line_v aligned row with line_h: {:?}", rels);
        assert!(rels.contains(&Relationship::AlignedColumn), "line_v aligned col with line_h: {:?}", rels);
    }

    /// Adjacent horizontally: b1 at (0,0) w=5 h=3, b2 at (5,1) w=4 h=2
    /// Touch at x=5, vertical ranges [0,3) and [1,3) overlap
    #[test]
    fn test_adjacent_horizontal() {
        let b1 = to_rect(0, 0, 5, 3); // (0,0,5,3)
        let b2 = to_rect(5, 1, 4, 2); // (5,1,9,3)
        let rels = all_relationships(b1, b2);
        assert!(rels.contains(&Relationship::LeftOf), "b1 left of b2: {:?}", rels);
        assert!(rels.contains(&Relationship::Adjacent), "adjacent: {:?}", rels);
    }

    /// Adjacent vertically: b1 above b2, touching
    #[test]
    fn test_adjacent_vertical() {
        let b1 = to_rect(0, 0, 5, 3); // (0,0,5,3)
        let b2 = to_rect(1, 3, 5, 4); // (1,3,6,7)
        let rels = all_relationships(b1, b2);
        assert!(rels.contains(&Relationship::Above), "b1 above b2: {:?}", rels);
        assert!(rels.contains(&Relationship::Adjacent), "adjacent: {:?}", rels);
        assert!(rels.contains(&Relationship::AlignedColumn), "aligned column: {:?}", rels);
    }

    /// Non-touching Above: gap of 1
    #[test]
    fn test_above_not_adjacent() {
        let b1 = to_rect(0, 0, 5, 3); // (0,0,5,3)
        let b2 = to_rect(1, 4, 5, 4); // (1,4,6,8)
        let rels = all_relationships(b1, b2);
        assert!(rels.contains(&Relationship::Above), "b1 above b2: {:?}", rels);
        assert!(!rels.contains(&Relationship::Adjacent), "gap=1 is not adjacent: {:?}", rels);
    }

    // ── Engine tests ──

    /// find_control_relationships returns multi-rels and deduplicates
    #[test]
    fn test_engine_multi_relationships() {
        use crate::semantic::controls::{Control, ControlBounds};
        let controls = vec![
            Control {
                id: "btn1".into(), label: "A".into(), kind: crate::semantic::controls::ControlKind::Button,
                value: None, bounds: ControlBounds { x: 0, y: 0, width: 3, height: 2 },
                region_id: None, focusable: true, focused: false, enabled: true,
                selected: false, checked: false, shortcut: None,
                confidence: crate::semantic::confidence::Confidence::native(),
                evidence: Vec::new(), source: "inferred".to_string(),
            },
            Control {
                id: "btn2".into(), label: "B".into(), kind: crate::semantic::controls::ControlKind::Button,
                value: None, bounds: ControlBounds { x: 3, y: 0, width: 4, height: 2 },
                region_id: None, focusable: true, focused: false, enabled: true,
                selected: false, checked: false, shortcut: None,
                confidence: crate::semantic::confidence::Confidence::native(),
                evidence: Vec::new(), source: "inferred".to_string(),
            },
        ];
        let rels = RelationshipEngine::find_control_relationships(&controls);
        assert_eq!(rels.len(), 2); // i=0→j=1 and i=1→j=0
        // Each pair should have LeftOf/RightOf + AlignedRow + Adjacent
        for (_, _, rel_vec) in &rels {
            assert!(!rel_vec.is_empty());
        }
    }

    /// find_region_relationships never emits empty Vecs
    #[test]
    fn test_engine_no_empty_relationships() {
        use crate::semantic::confidence::Confidence;
        use crate::semantic::controls::ControlBounds;
        use crate::semantic::regions::{Bounds, RegionKind};
        let controls = vec![
            crate::semantic::controls::Control {
                id: "c1".into(), label: "Btn".into(), kind: crate::semantic::controls::ControlKind::Button,
                value: None, bounds: ControlBounds { x: 0, y: 0, width: 3, height: 1 },
                region_id: None, focusable: true, focused: false, enabled: true,
                selected: false, checked: false, shortcut: None,
                confidence: crate::semantic::confidence::Confidence::native(),
                evidence: Vec::new(), source: "inferred".to_string(),
            },
        ];
        let regions = vec![
            Region {
                id: "r1".into(), kind: RegionKind::Panel, title: None,
                bounds: Bounds { x: 0, y: 0, width: 3, height: 1 },
                confidence: Confidence::native(), parent_id: None, child_ids: Vec::new(),
                clipping_state: crate::semantic::regions::ClippingState::None,
            },
            // A far-away region that won't have any relationship
            Region {
                id: "r2".into(), kind: RegionKind::Panel, title: None,
                bounds: Bounds { x: 100, y: 100, width: 5, height: 5 },
                confidence: Confidence::native(), parent_id: None, child_ids: Vec::new(),
                clipping_state: crate::semantic::regions::ClippingState::None,
            },
        ];
        let rels = RelationshipEngine::find_region_relationships(&controls, &regions);
        // Should have (c1, r1) because they overlap
        assert!(rels.iter().any(|(lbl, rid, _)| lbl == "Btn" && rid == "r1"));
        // r2 is far away but still has Above + LeftOf relationship
        assert!(rels.iter().any(|(_, rid, _)| rid == "r2"), "r2 should have a relationship (Above+LeftOf)");
        for (lbl, rid, rv) in &rels {
            assert!(!rv.is_empty(), "should not emit empty relationship vec: ({}, {}, {:?})", lbl, rid, rv);
        }
    }
}
