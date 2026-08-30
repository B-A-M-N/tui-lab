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

/// Relationship engine: computes relationships from bounds.
pub struct RelationshipEngine;

impl RelationshipEngine {
    /// Find all relationships between controls.
    pub fn find_control_relationships(controls: &[Control]) -> Vec<(usize, usize, Relationship)> {
        let mut results = Vec::new();
        for (i, c1) in controls.iter().enumerate() {
            for (j, c2) in controls.iter().enumerate() {
                if i == j {
                    continue;
                }
                if let Some(rel) = Self::control_relationship(c1, c2) {
                    results.push((i, j, rel));
                }
            }
        }
        results
    }

    /// Find all relationships between a control and all regions.
    pub fn find_region_relationships(
        controls: &[Control],
        regions: &[Region],
    ) -> Vec<(String, String, Relationship)> {
        let mut results = Vec::new();
        for c in controls {
            for r in regions {
                if let Some(rel) = Self::control_region_relationship(c, r) {
                    results.push((c.label.clone(), r.id.clone(), rel));
                }
            }
        }
        results
    }

    /// Find all relationships between regions.
    pub fn find_region_region_relationships(
        regions: &[Region],
    ) -> Vec<(String, String, Relationship)> {
        let mut results = Vec::new();
        for (i, r1) in regions.iter().enumerate() {
            for (j, r2) in regions.iter().enumerate() {
                if i >= j {
                    continue;
                }
                if let Some(rel) = Self::region_relationship(r1, r2) {
                    results.push((r1.id.clone(), r2.id.clone(), rel));
                }
            }
        }
        results
    }

    /// Determine relationship between two controls.
    fn control_relationship(c1: &Control, c2: &Control) -> Option<Relationship> {
        let b1 = (c1.bounds.x, c1.bounds.y, c1.bounds.width, c1.bounds.height);
        let b2 = (c2.bounds.x, c2.bounds.y, c2.bounds.width, c2.bounds.height);
        Self::bounds_relationship(b1, b2)
    }

    /// Determine relationship between a control and a region.
    fn control_region_relationship(c: &Control, r: &Region) -> Option<Relationship> {
        let cb = (c.bounds.x, c.bounds.y, c.bounds.width, c.bounds.height);
        let rb = (r.bounds.x, r.bounds.y, r.bounds.width, r.bounds.height);
        Self::bounds_relationship(cb, rb)
    }

    /// Determine relationship between two regions.
    fn region_relationship(r1: &Region, r2: &Region) -> Option<Relationship> {
        let b1 = (r1.bounds.x, r1.bounds.y, r1.bounds.width, r1.bounds.height);
        let b2 = (r2.bounds.x, r2.bounds.y, r2.bounds.width, r2.bounds.height);
        Self::bounds_relationship(b1, b2)
    }

    /// Compute relationship from two bounding boxes.
    fn bounds_relationship(
        b1: (u16, u16, u16, u16),
        b2: (u16, u16, u16, u16),
    ) -> Option<Relationship> {
        let (x1, y1, w1, h1) = b1;
        let (x2, y2, w2, h2) = b2;

        let r1 = |x: u16, y: u16, w: u16, h: u16| -> (i32, i32, i32, i32) {
            (x as i32, y as i32, (x + w) as i32, (y + h) as i32)
        };

        let (l1, t1, r1r, b1b) = r1(x1, y1, w1, h1);
        let (l2, t2, r2r, b2b) = r1(x2, y2, w2, h2);

        // Inside: b1 is contained by b2
        if l1 >= l2
            && t1 >= t2
            && r1r <= r2r
            && b1b <= b2b
            && (l1 > l2 || t1 > t2 || r1r < r2r || b1b < b2b)
        {
            return Some(Relationship::Inside);
        }

        // Above: b1 is entirely above b2
        if b1b <= t2 {
            return Some(Relationship::Above);
        }

        // Below: b1 is entirely below b2
        if t1 >= b2b {
            return Some(Relationship::Below);
        }

        // LeftOf: b1 is entirely to the left of b2
        if r1r <= l2 {
            return Some(Relationship::LeftOf);
        }

        // RightOf: b1 is entirely to the right of b2
        if l1 >= r2r {
            return Some(Relationship::RightOf);
        }

        // AlignedRow: same row range
        if t1 <= b2b && t2 <= b1b {
            return Some(Relationship::AlignedRow);
        }

        // AlignedColumn: same column range
        if l1 <= r2r && l2 <= r1r {
            return Some(Relationship::AlignedColumn);
        }

        // Overlapping
        if l1 < r2r && l2 < r1r && t1 < b2b && t2 < b1b {
            return Some(Relationship::Overlapping);
        }

        None
    }
}
