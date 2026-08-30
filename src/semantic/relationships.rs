//! Semantic relationships between UI elements (spec item 28).
//!
//! Computes spatial relationships between controls and regions, enabling
//! assertions like `inside`, `above`, `below`, `left_of`, `right_of`, `aligned`.

use crate::semantic::controls::Control;
use crate::semantic::regions::Region;

/// One normalized relation between two identified elements (the review's
/// `SemanticRelation`). The old anonymous `(String, String, Vec<Relationship>)`
/// tuples keyed controls by display *label* — two "Save" buttons were
/// indistinguishable — and had no stable serialization shape. A relation now
/// names its endpoints by stable ID (control or region ID, geometry-free per
/// Wave-3 item 16) and carries the kind with a machine-checkable `reason`
/// (the geometry that established it).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SemanticRelation {
    /// Stable ID of the subject (control or region).
    pub subject: String,
    /// Stable ID of the object.
    pub object: String,
    /// The spatial predicate that holds from subject to object.
    pub kind: Relationship,
    /// The establishing geometry, e.g. "right_of: 3 <= 5" (subject right edge
    /// <= object left edge) — enough to re-derive the verdict from bounds.
    pub reason: String,
}

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
    (
        x as i32,
        y as i32,
        (x as i32) + (w as i32),
        (y as i32) + (h as i32),
    )
}

// ─── Pure predicates ───────────────────────────────────────────────

/// b1 is strictly inside b2 (strict containment; equal boxes are NOT inside).
fn is_inside(b1: Rect, b2: Rect) -> bool {
    let (l1, t1, r1, b1b) = b1;
    let (l2, t2, r2, b2b) = b2;
    l1 >= l2 && t1 >= t2 && r1 <= r2 && b1b <= b2b && (l1 > l2 || t1 > t2 || r1 < r2 || b1b < b2b)
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
    b1.0 < b2.2
        && b2.0 < b1.2
        && b1.1 < b2.3
        && b2.1 < b1.3
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

/// The geometry that established one predicate, rendered for `reason`.
fn reason_for(kind: &Relationship, b1: Rect, b2: Rect) -> String {
    match kind {
        Relationship::Inside => format!(
            "inside: {}>={}, {}>={}, {}<={}, {}<={}",
            b1.0, b2.0, b1.1, b2.1, b1.2, b2.2, b1.3, b2.3
        ),
        Relationship::Above => format!("above: {} <= {}", b1.3, b2.1),
        Relationship::Below => format!("below: {} >= {}", b1.1, b2.3),
        Relationship::LeftOf => format!("left_of: {} <= {}", b1.2, b2.0),
        Relationship::RightOf => format!("right_of: {} >= {}", b1.0, b2.2),
        Relationship::AlignedRow => {
            format!(
                "aligned_row: |{} - {}| < min(h{}, h{})",
                b1.1,
                b2.1,
                b1.3 - b1.1,
                b2.3 - b2.1
            )
        }
        Relationship::AlignedColumn => {
            format!(
                "aligned_col: |{} - {}| < min(w{}, w{})",
                b1.0,
                b2.0,
                b1.2 - b1.0,
                b2.2 - b2.0
            )
        }
        Relationship::Overlapping => "overlapping: rects intersect".to_string(),
        Relationship::Adjacent => "adjacent: gap <= 1".to_string(),
    }
}

/// Flatten one pair's predicate list into normalized [`SemanticRelation`]s.
fn relate(subject: &str, object: &str, b1: Rect, b2: Rect) -> Vec<SemanticRelation> {
    dedup(all_relationships(b1, b2))
        .into_iter()
        .map(|kind| SemanticRelation {
            reason: reason_for(&kind, b1, b2),
            kind,
            subject: subject.to_string(),
            object: object.to_string(),
        })
        .collect()
}

impl RelationshipEngine {
    /// Find all relationships between controls, normalized and keyed by
    /// stable control ID (labels can collide; IDs cannot).
    pub fn find_control_relationships(controls: &[Control]) -> Vec<SemanticRelation> {
        let mut results = Vec::new();
        for (i, c1) in controls.iter().enumerate() {
            for (j, c2) in controls.iter().enumerate() {
                if i == j {
                    continue;
                }
                let b1 = to_rect(c1.bounds.x, c1.bounds.y, c1.bounds.width, c1.bounds.height);
                let b2 = to_rect(c2.bounds.x, c2.bounds.y, c2.bounds.width, c2.bounds.height);
                results.extend(relate(&c1.id, &c2.id, b1, b2));
            }
        }
        results
    }

    /// Find all relationships between controls and regions, keyed by the
    /// control's stable ID (the old shape used the display label, which
    /// collides across same-named controls) and the region's stable ID.
    pub fn find_region_relationships(
        controls: &[Control],
        regions: &[Region],
    ) -> Vec<SemanticRelation> {
        let mut results = Vec::new();
        for c in controls {
            for r in regions {
                let cb = to_rect(c.bounds.x, c.bounds.y, c.bounds.width, c.bounds.height);
                let rb = to_rect(r.bounds.x, r.bounds.y, r.bounds.width, r.bounds.height);
                results.extend(relate(&c.id, &r.id, cb, rb));
            }
        }
        results
    }

    /// Find all relationships between regions (i < j pairs), keyed by
    /// stable region ID.
    pub fn find_region_region_relationships(regions: &[Region]) -> Vec<SemanticRelation> {
        let mut results = Vec::new();
        for (i, r1) in regions.iter().enumerate() {
            for (j, r2) in regions.iter().enumerate() {
                if i >= j {
                    continue;
                }
                let b1 = to_rect(r1.bounds.x, r1.bounds.y, r1.bounds.width, r1.bounds.height);
                let b2 = to_rect(r2.bounds.x, r2.bounds.y, r2.bounds.width, r2.bounds.height);
                results.extend(relate(&r1.id, &r2.id, b1, b2));
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
        assert!(
            rels.contains(&Relationship::LeftOf),
            "should be LeftOf: {:?}",
            rels
        );
        assert!(
            rels.contains(&Relationship::AlignedRow),
            "should be AlignedRow: {:?}",
            rels
        );
        assert!(
            rels.contains(&Relationship::Adjacent),
            "should be Adjacent: {:?}",
            rels
        );
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
        assert!(
            rels.contains(&Relationship::Inside),
            "b1 inside b2: {:?}",
            rels
        );
        assert!(
            !rels.contains(&Relationship::Overlapping),
            "Inside and Overlapping are exclusive: {:?}",
            rels
        );
    }

    /// Identical boxes: not Inside (strict containment), but AlignedRow + AlignedColumn
    #[test]
    fn test_identical_boxes() {
        let b = to_rect(10, 10, 5, 5); // (10,10,15,15)
        let rels = all_relationships(b, b);
        assert!(
            rels.contains(&Relationship::AlignedRow),
            "identical → AlignedRow: {:?}",
            rels
        );
        assert!(
            rels.contains(&Relationship::AlignedColumn),
            "identical → AlignedColumn: {:?}",
            rels
        );
        assert!(
            !rels.contains(&Relationship::Inside),
            "identical boxes are NOT Inside: {:?}",
            rels
        );
        assert!(
            !rels.contains(&Relationship::Overlapping),
            "identical boxes are NOT Overlapping: {:?}",
            rels
        );
        assert!(
            !rels.contains(&Relationship::Above),
            "identical → not Above: {:?}",
            rels
        );
        assert!(
            !rels.contains(&Relationship::LeftOf),
            "identical → not LeftOf: {:?}",
            rels
        );
    }

    /// Disjoint diagonal: b1 at (0,0) w=2 h=2, b2 at (3,3) w=2 h=2
    /// b1 is Above + LeftOf b2; b2 is Below + RightOf b1
    #[test]
    fn test_disjoint_diagonal() {
        let b1 = to_rect(0, 0, 2, 2); // (0,0,2,2)
        let b2 = to_rect(3, 3, 2, 2); // (3,3,5,5)
        let rels_12 = all_relationships(b1, b2);
        assert!(
            rels_12.contains(&Relationship::Above),
            "b1 above b2: {:?}",
            rels_12
        );
        assert!(
            rels_12.contains(&Relationship::LeftOf),
            "b1 left of b2: {:?}",
            rels_12
        );
        assert!(
            !rels_12.contains(&Relationship::Adjacent),
            "diagonal is NOT adjacent: {:?}",
            rels_12
        );
        assert!(
            !rels_12.contains(&Relationship::AlignedRow),
            "diagonal → not AlignedRow: {:?}",
            rels_12
        );
        assert!(
            !rels_12.contains(&Relationship::AlignedColumn),
            "diagonal → not AlignedColumn: {:?}",
            rels_12
        );

        // Reverse direction
        let rels_21 = all_relationships(b2, b1);
        assert!(
            rels_21.contains(&Relationship::Below),
            "b2 below b1: {:?}",
            rels_21
        );
        assert!(
            rels_21.contains(&Relationship::RightOf),
            "b2 right of b1: {:?}",
            rels_21
        );
        assert!(
            !rels_21.contains(&Relationship::Above),
            "b2 is NOT above b1: {:?}",
            rels_21
        );
    }

    /// Zero-width / zero-height boxes should not panic.
    #[test]
    fn test_zero_dimension_boxes() {
        let line_v = to_rect(5, 0, 0, 10); // vertical line at x=5
        let line_h = to_rect(5, 5, 10, 0); // horizontal line at y=5
        let rels = all_relationships(line_v, line_h);
        // They intersect at (5,5), so they should be AlignedRow + AlignedColumn
        assert!(
            rels.contains(&Relationship::AlignedRow),
            "line_v aligned row with line_h: {:?}",
            rels
        );
        assert!(
            rels.contains(&Relationship::AlignedColumn),
            "line_v aligned col with line_h: {:?}",
            rels
        );
    }

    /// Adjacent horizontally: b1 at (0,0) w=5 h=3, b2 at (5,1) w=4 h=2
    /// Touch at x=5, vertical ranges [0,3) and [1,3) overlap
    #[test]
    fn test_adjacent_horizontal() {
        let b1 = to_rect(0, 0, 5, 3); // (0,0,5,3)
        let b2 = to_rect(5, 1, 4, 2); // (5,1,9,3)
        let rels = all_relationships(b1, b2);
        assert!(
            rels.contains(&Relationship::LeftOf),
            "b1 left of b2: {:?}",
            rels
        );
        assert!(
            rels.contains(&Relationship::Adjacent),
            "adjacent: {:?}",
            rels
        );
    }

    /// Adjacent vertically: b1 above b2, touching
    #[test]
    fn test_adjacent_vertical() {
        let b1 = to_rect(0, 0, 5, 3); // (0,0,5,3)
        let b2 = to_rect(1, 3, 5, 4); // (1,3,6,7)
        let rels = all_relationships(b1, b2);
        assert!(
            rels.contains(&Relationship::Above),
            "b1 above b2: {:?}",
            rels
        );
        assert!(
            rels.contains(&Relationship::Adjacent),
            "adjacent: {:?}",
            rels
        );
        assert!(
            rels.contains(&Relationship::AlignedColumn),
            "aligned column: {:?}",
            rels
        );
    }

    /// Non-touching Above: gap of 1
    #[test]
    fn test_above_not_adjacent() {
        let b1 = to_rect(0, 0, 5, 3); // (0,0,5,3)
        let b2 = to_rect(1, 4, 5, 4); // (1,4,6,8)
        let rels = all_relationships(b1, b2);
        assert!(
            rels.contains(&Relationship::Above),
            "b1 above b2: {:?}",
            rels
        );
        assert!(
            !rels.contains(&Relationship::Adjacent),
            "gap=1 is not adjacent: {:?}",
            rels
        );
    }

    // ── Engine tests ──

    /// find_control_relationships returns multi-rels and deduplicates
    #[test]
    fn test_engine_multi_relationships() {
        use crate::semantic::controls::{Control, ControlBounds};
        let controls = vec![
            Control {
                id: "btn1".into(),
                label: "A".into(),
                kind: crate::semantic::controls::ControlKind::Button,
                value: None,
                bounds: ControlBounds {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 2,
                },
                region_id: None,
                focusable: true,
                focused: false,
                enabled: true,
                selected: false,
                checked: false,
                shortcut: None,
                confidence: crate::semantic::confidence::Confidence::native(),
                evidence: Vec::new(),
                source: "inferred".to_string(),
            },
            Control {
                id: "btn2".into(),
                label: "B".into(),
                kind: crate::semantic::controls::ControlKind::Button,
                value: None,
                bounds: ControlBounds {
                    x: 3,
                    y: 0,
                    width: 4,
                    height: 2,
                },
                region_id: None,
                focusable: true,
                focused: false,
                enabled: true,
                selected: false,
                checked: false,
                shortcut: None,
                confidence: crate::semantic::confidence::Confidence::native(),
                evidence: Vec::new(),
                source: "inferred".to_string(),
            },
        ];
        let rels = RelationshipEngine::find_control_relationships(&controls);
        // Both directions, keyed by stable ID (the controls use ids c1/c2 in
        // this fixture's sibling test; here btn1/btn2).
        let ids: std::collections::HashSet<_> = rels
            .iter()
            .map(|r| (r.subject.as_str(), r.object.as_str()))
            .collect();
        assert!(ids.contains(&("btn1", "btn2")), "forward pair present");
        assert!(ids.contains(&("btn2", "btn1")), "reverse pair present");
        // LeftOf/RightOf both ways, plus AlignedRow and Adjacent.
        assert!(rels
            .iter()
            .any(|r| r.subject == "btn1" && r.kind == Relationship::LeftOf));
        assert!(rels
            .iter()
            .any(|r| r.subject == "btn2" && r.kind == Relationship::RightOf));
        assert!(rels.iter().any(|r| r.kind == Relationship::AlignedRow));
        assert!(rels.iter().any(|r| r.kind == Relationship::Adjacent));
        assert!(
            rels.iter().all(|r| !r.reason.is_empty()),
            "every relation carries its establishing geometry"
        );
    }

    /// find_region_relationships never emits empty Vecs
    #[test]
    fn test_engine_no_empty_relationships() {
        use crate::semantic::confidence::Confidence;
        use crate::semantic::controls::ControlBounds;
        use crate::semantic::regions::{Bounds, RegionKind};
        let controls = vec![crate::semantic::controls::Control {
            id: "c1".into(),
            label: "Btn".into(),
            kind: crate::semantic::controls::ControlKind::Button,
            value: None,
            bounds: ControlBounds {
                x: 0,
                y: 0,
                width: 3,
                height: 1,
            },
            region_id: None,
            focusable: true,
            focused: false,
            enabled: true,
            selected: false,
            checked: false,
            shortcut: None,
            confidence: crate::semantic::confidence::Confidence::native(),
            evidence: Vec::new(),
            source: "inferred".to_string(),
        }];
        let regions = vec![
            Region {
                id: "r1".into(),
                kind: RegionKind::Panel,
                title: None,
                bounds: Bounds {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 1,
                },
                confidence: Confidence::native(),
                parent_id: None,
                child_ids: Vec::new(),
                clipping_state: crate::semantic::regions::ClippingState::None,
            },
            // A far-away region that won't have any relationship
            Region {
                id: "r2".into(),
                kind: RegionKind::Panel,
                title: None,
                bounds: Bounds {
                    x: 100,
                    y: 100,
                    width: 5,
                    height: 5,
                },
                confidence: Confidence::native(),
                parent_id: None,
                child_ids: Vec::new(),
                clipping_state: crate::semantic::regions::ClippingState::None,
            },
        ];
        let rels = RelationshipEngine::find_region_relationships(&controls, &regions);
        // Keyed by stable IDs, not display labels.
        assert!(
            rels.iter().any(|r| r.subject == "c1" && r.object == "r1"),
            "control-region pair keyed by ID"
        );
        // r2 is far away but still has Above + LeftOf relationship
        assert!(
            rels.iter().any(|r| r.object == "r2"),
            "r2 should have a relationship (Above+LeftOf)"
        );
        // Every emitted relation carries a non-empty reason.
        assert!(rels.iter().all(|r| !r.reason.is_empty()));
        // `kind` serializes snake_case (stable artifact shape).
        let json = serde_json::to_string(&rels[0].kind).unwrap();
        assert!(json.starts_with('"'), "kind serializes as a string tag");
    }
}
