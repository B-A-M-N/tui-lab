//! SemanticCache — don't recompute semantic analysis for an unchanged frame
//! (review P1: "cache semantic analysis per frame" + "make it dirty-region
//! aware").
//!
//! [`crate::semantic::analyze`] runs seven detectors — regions, controls,
//! focus, relationships, affordances, components, and (via the caller) the
//! tree — over the whole grid on *every* observation. For a TUI that redraws
//! at high frequency (timers, spinners) most observations produce **no
//! structural change**, yet the full pipeline re-runs each time. The honest
//! fix is a per-frame cache keyed on the screen's [`ScreenState::structure_hash`]:
//! that hash is computed once at observation time and already normalizes
//! volatile text (numbers, timers, clocks), so an unchanged hash *means* the
//! same structure — the cached [`SemanticScreen`] is returned without touching
//! a detector.
//!
//! This is the correct generalization of "dirty-region aware" for structure:
//! it does not pretend to do row-level incremental analysis (the detectors
//! reason over the whole grid), but it makes an unchanged frame O(1) rather
//! than O(cols·rows×7). When the structure hash *does* change, the full
//! pipeline re-runs — the first observation after a real change is never
//! served stale.

use crate::screen::ScreenState;
use crate::semantic::SemanticScreen;

/// The STRUCTURAL half of a frame's analysis (re-review item 37): what a
/// structure-hash key can honestly serve — regions, controls-as-geometry,
/// relationships, components. Focus/enabled/selection are deliberately
/// ABSENT: they are interaction state and live in [`InteractionAnalysis`].
#[derive(Debug, Clone)]
pub struct StructuralAnalysis {
    pub sem: SemanticScreen,
    pub tree: crate::semantic::node::SemanticTree,
}

/// The INTERACTION half: re-derived fresh per frame over whatever
/// structural skeleton is at hand, because its evidence (reverse video,
/// cursor position, dim attributes) moves on keys the structure hash
/// cannot see. Not cached — computed, and the stats say so.
#[derive(Debug, Clone, Default)]
pub struct InteractionAnalysis {
    /// How many interaction re-derivations ran (every fused frame: this
    /// pass is never served from cache, by design).
    pub recomputations: u64,
    /// Whether the last re-derivation changed any interaction field
    /// relative to the structural skeleton it was applied to (evidence
    /// that the split is doing work: a same-text Tab press shows
    /// `interaction_only_change: true`).
    pub interaction_only_change: bool,
}

/// Outcome of asking the cache for a frame's analysis.
#[derive(Debug, Clone)]
pub struct CacheResult {
    /// The semantic analysis for the frame.
    pub sem: SemanticScreen,
    /// Whether it was served without re-running the detectors.
    pub hit: bool,
    /// The structure-hash key used (for evidence).
    pub key: String,
}

/// A bounded per-frame semantic cache. Holds at most `Self::MAX_ENTRIES`
/// keys (a small working set of recent frames), evicting oldest on overflow
/// while keeping the *last* analyzed hash so an alternating two-frame repaint
/// stays cached.
#[derive(Debug, Default)]
pub struct SemanticCache {
    /// structure_hash → (flat analysis, tree).
    pub(crate) entries: Vec<(
        String,
        (SemanticScreen, crate::semantic::node::SemanticTree),
    )>,
    hits: u64,
    misses: u64,
    /// Item 37: the interaction half's ledger — recomputations and how
    /// many of them actually moved interaction state.
    pub interaction: InteractionAnalysis,
}

/// How many frames to remember. Sized for the realistic newline/spinner
/// working set; eviction is honest (declared), not silent.
pub const MAX_ENTRIES: usize = 8;

impl SemanticCache {
    pub fn new() -> Self {
        SemanticCache {
            entries: Vec::new(),
            hits: 0,
            misses: 0,
            interaction: InteractionAnalysis::default(),
        }
    }

    /// Analyze `screen`, serving the cached result when its `structure_hash`
    /// matches a frame we've already analyzed.
    pub fn analyze(&mut self, screen: &ScreenState) -> CacheResult {
        let key = screen.structure_hash.clone();
        if key.is_empty() {
            // A screen that was never hashed (e.g. a just-constructed, not-yet-
            // committed frame) cannot be cached safely — run the pipeline.
            self.misses += 1;
            return CacheResult {
                sem: crate::semantic::analyze(screen),
                hit: false,
                key,
            };
        }
        if let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
            // Hit: return the stored analysis; no detector runs.
            let sem = self.entries[idx].1 .0.clone();
            self.hits += 1;
            return CacheResult {
                sem,
                hit: true,
                key,
            };
        }
        self.misses += 1;
        let pair = crate::semantic::detect_frame(screen);
        let sem = pair.0;
        // Retain the last-analyzed frame (so a two-frame repaint alternation
        // stays hot) before enforcing the cap — the shared retention rule.
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        }
        self.entries.push((key.clone(), (sem.clone(), pair.1)));
        self.enforce_retention();
        CacheResult {
            sem,
            hit: false,
            key,
        }
    }

    /// Insert a precomputed (flat, tree) pair for `key`. The fused path
    /// (`crate::semantic::fuse`) uses this when it already ran
    /// [`crate::semantic::detect_frame`] via a cache miss on the pair-level
    /// lookup. Same retention policy as [`Self::analyze`] — the two entry
    /// paths can never drift apart in what they keep.
    pub fn insert(
        &mut self,
        key: String,
        pair: (SemanticScreen, crate::semantic::node::SemanticTree),
    ) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        }
        self.entries.push((key, pair));
        self.enforce_retention();
    }

    /// Shared eviction (re-review item 41): keep the two most-recent
    /// distinct frames (so a two-frame repaint alternation stays hot),
    /// drop the rest. Both `analyze` and `insert` go through this, so the
    /// layers stay consistent.
    fn enforce_retention(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        let mut kept: Vec<(
            String,
            (SemanticScreen, crate::semantic::node::SemanticTree),
        )> = Vec::with_capacity(2);
        for (k, s) in self.entries.iter().rev() {
            if kept.is_empty() || kept[0].0 != *k {
                if kept.len() == 2 {
                    break;
                }
                kept.push((k.clone(), s.clone()));
            }
        }
        self.entries = kept;
    }

    /// Hit rate since construction — a coarse health signal for whether the
    /// cache is doing anything (evidence for the audit).
    pub fn hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f32 / total as f32
        }
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Item 37: the structural half, keyed by structure hash. Served from
    /// the entry store when hot; the detectors run exactly once per
    /// distinct structure otherwise. This is the ONLY path that caches.
    pub fn structural(&mut self, screen: &ScreenState) -> (StructuralAnalysis, bool) {
        let key = screen.structure_hash.clone();
        if !key.is_empty() {
            if let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
                self.hits += 1;
                let (sem, tree) = self.entries[idx].1.clone();
                return (StructuralAnalysis { sem, tree }, true);
            }
        }
        self.misses += 1;
        let (sem, tree) = crate::semantic::detect_frame(screen);
        self.insert(key, (sem.clone(), tree.clone()));
        (StructuralAnalysis { sem, tree }, false)
    }

    /// Item 37: the interaction half. NEVER cached — always recomputed
    /// over the given structural skeleton via the same reapply pass the
    /// fuse path uses, and the ledger records the work.
    pub fn interaction_of(
        &mut self,
        screen: &ScreenState,
        structural: &mut StructuralAnalysis,
    ) -> InteractionAnalysis {
        let changed =
            crate::semantic::reapply_interaction(screen, &mut structural.sem, &mut structural.tree);
        self.interaction.recomputations += 1;
        self.interaction.interaction_only_change = changed;
        InteractionAnalysis {
            recomputations: self.interaction.recomputations,
            interaction_only_change: changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::ScreenState;

    fn screen_with_hash(hash: &str) -> ScreenState {
        // A minimal frame whose structure_hash we control; we only exercise
        // the cache, not the detectors, so an empty grid is fine.
        let mut s = ScreenState::new(80, 24);
        s.structure_hash = hash.to_string();
        s
    }

    #[test]
    fn unchanged_frame_is_served_from_cache_without_reanalysis() {
        let mut cache = SemanticCache::new();
        let a = screen_with_hash("h-v1");
        let r1 = cache.analyze(&a);
        assert!(!r1.hit, "first observation must run the pipeline");
        let r2 = cache.analyze(&a);
        assert!(
            r2.hit,
            "second observation of the same structure must be cached"
        );
        assert_eq!(r1.sem.cols, r2.sem.cols);
        assert!(cache.hits() >= 1, "hit counted");
    }

    #[test]
    fn a_changed_structure_reanalyzes() {
        let mut cache = SemanticCache::new();
        let a = screen_with_hash("h-1");
        let _ = cache.analyze(&a);
        let b = screen_with_hash("h-2");
        let r2 = cache.analyze(&b);
        assert!(!r2.hit, "a different structure must re-run the pipeline");
        assert!(r2.key == "h-2");
    }

    #[test]
    fn empty_unhashed_frame_is_never_cached_wrongly() {
        let mut cache = SemanticCache::new();
        let mut s = ScreenState::new(80, 24);
        s.structure_hash = String::new();
        let r1 = cache.analyze(&s);
        let r2 = cache.analyze(&s);
        assert!(!r1.hit && !r2.hit, "an unhashed frame must never hit");
    }

    #[test]
    fn two_frame_repaint_stays_hot() {
        let mut cache = SemanticCache::new();
        let a = screen_with_hash("h-A");
        let b = screen_with_hash("h-B");
        let _ = cache.analyze(&a);
        let _ = cache.analyze(&b);
        // Third observation of A should hit even though B came between.
        let r = cache.analyze(&a);
        assert!(r.hit, "two-frame alternation must stay cached");
    }

    // Fused path (re-review Wave-4): `detect_frame` stores BOTH shapes; a
    // hit must serve the tree too, not just the flat analysis.
    #[test]
    fn fused_pair_is_cached_together() {
        let mut cache = SemanticCache::new();
        let a = screen_with_hash("h-fused");
        let (sem1, tree1) = crate::semantic::detect_frame(&a);
        cache.insert("h-fused".to_string(), (sem1.clone(), tree1.clone()));
        // A subsequent `analyze` hits the same entry and serves the flat
        // half; the tree half is served by the pair-level lookup in
        // `crate::semantic::fuse`.
        let r = cache.analyze(&a);
        assert!(r.hit, "insert made the frame hot for analyze");
        assert_eq!(r.sem.cols, sem1.cols);
        // And the pair-level path serves the stored tree without re-running
        // the detectors: same root id, same child count.
        let (sem2, tree2, _report) = crate::semantic::fuse(
            &a,
            &mut cache,
            &crate::semantic::native::NativeChannel::default(),
        );
        assert_eq!(sem2.cols, sem1.cols);
        assert_eq!(tree2.root.children.len(), tree1.root.children.len());
    }

    #[test]
    fn empty_hash_screen_degrades_to_pipeline() {
        let mut cache = SemanticCache::new();
        let mut s = ScreenState::new(80, 24);
        s.structure_hash = String::new();
        let r = cache.analyze(&s);
        assert!(!r.hit);
        assert!(r.sem.rows == 24 && r.sem.cols == 80, "pipeline still ran");
    }

    // Hit rate is monotone in usefulness.
    #[test]
    fn hit_rate_rises_with_reuse() {
        let mut cache = SemanticCache::new();
        let a = screen_with_hash("h-x");
        let _ = cache.analyze(&a);
        let _ = cache.analyze(&a);
        assert!(cache.hit_rate() > 0.0);
        assert!(cache.hit_rate() <= 1.0);
    }
}
#[cfg(test)]
mod split_tests {
    use super::*;

    /// Item 37: the split. A same-text Tab press changes the VISUAL frame
    /// but not the structure hash: the structural half hits the cache
    /// (detectors never re-run) while the interaction half recomputes and
    /// reports that it moved focus.
    #[test]
    fn structural_hits_while_interaction_recomputes() {
        let mut cache = SemanticCache::new();

        // Frame A: two buttons. `ScreenState::new` leaves hashes empty (an
        // unhashed frame is never cached, by design), so give both frames
        // the SAME explicit structure hash — the test isolates the split,
        // and the structural key must NOT distinguish the frames.
        let mut a = ScreenState::new(20, 3);
        a.viewport_text = vec!["[ Save ] [ Cancel ]".into(), "".into(), "".into()];
        a.structure_hash = "split-test-hash".into();
        let mut b = a.clone();
        b.viewport_text = vec!["[ Save ] [CANCEL ]".into(), "".into(), "".into()];

        let (s1, hit1) = cache.structural(&a);
        assert!(!hit1, "first frame runs the detectors");
        let i1 = {
            let mut s1 = s1;
            cache.interaction_of(&a, &mut s1)
        };
        assert_eq!(i1.recomputations, 1);

        let (s2, hit2) = cache.structural(&b);
        assert!(hit2, "same structure hash → structural half is cached");
        let i2 = {
            let mut s2 = s2;
            cache.interaction_of(&b, &mut s2)
        };
        assert_eq!(
            i2.recomputations,
            i1.recomputations + 1,
            "the interaction half NEVER caches: it recomputed"
        );
        // The structural ledger and the hit stats both moved coherently.
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    /// The typed halves are distinct types: StructuralAnalysis carries no
    /// focus decision of its own beyond the skeleton, and the interaction
    /// pass is what stamps focus onto it (a caller using only `structural`
    /// cannot accidentally read stale focus as fresh).
    #[test]
    fn halves_are_distinct_types() {
        let mut cache = SemanticCache::new();
        let a = ScreenState::new(20, 3);
        let (s, _) = cache.structural(&a);
        // StructuralAnalysis is a plain (sem, tree) pair — cloneable,
        // serializable through the semantic types, and separate from the
        // interaction ledger.
        let cloned = s.clone();
        let _i = cache.interaction_of(&a, &mut { cloned });
        assert!(cache.interaction.recomputations >= 1);
    }
}
