//! Frame-analysis state for one session (god-object round 2, G4).
//!
//! The per-frame semantic cache (Wave G review), the reactive fused
//! commit memo (re-review item 52), and the memo-hit counter move OUT
//! of the flat `Session` bucket into this holder. This is the session's
//! ANALYSIS state: what a frame's fused result is a function of, and
//! what gets reused instead of recomputed.
//!
//! Interior mutability is deliberate and stays exactly as it was:
//! `RefCell` for the cache/memo (read-only analysis paths mutate the
//! cache, so `&self` signatures are preserved) and `Cell<u64>` for the
//! hit counter. No locks — the session actor already serializes access
//! (invariant 15). `fuse` itself and the native channel stay outside:
//! the channel is the semantic domain's side channel, and the fused
//! pipeline lives in `crate::semantic`.

use crate::screen::ScreenState;

/// The reactive fused commit memo (re-review item 52): a computed fused
/// triple plus the frame identity it is a function of. The identity
/// covers every input the fused pipeline reads — structure (hash),
/// interaction state (visual hash + cursor + process state), and the
/// native channel's absorbed position (a fresh declare must invalidate).
#[derive(Clone)]
pub(crate) struct FusedMemo {
    pub(crate) key: String,
    pub(crate) sem: crate::semantic::SemanticScreen,
    pub(crate) tree: crate::semantic::node::SemanticTree,
    pub(crate) report: crate::semantic::native::NativeOverlayReport,
}

/// Frame identity key for the fused memo: structure hash (text/layout),
/// visual hash + cursor + process state (interaction), and the count of
/// native events absorbed so far (native overlay input). Two frames with
/// the same key are, by construction, the same input to `fuse`.
pub(crate) fn fused_key(screen: &ScreenState, native_seq: u64) -> String {
    format!(
        "{}/{}/{:?}/{:?}/{}",
        screen.structure_hash,
        screen.visual_hash,
        (screen.cursor.x, screen.cursor.y, screen.cursor.visible),
        screen.process.running,
        native_seq,
    )
}

/// The session's frame-analysis state: structural cache + fused memo.
pub(crate) struct SemanticState {
    /// Per-frame semantic cache (Wave G review): repeated
    /// `semantic::analyze` on an unchanged frame (same `structure_hash`)
    /// is served from cache instead of re-running all seven detectors.
    cache: std::cell::RefCell<crate::semantic::SemanticCache>,
    /// The memo: the last frame's FULL fused result (structure +
    /// interaction + native overlay), keyed on the frame's complete
    /// identity. An unchanged frame identity returns the memoized triple
    /// without re-running the interaction pass or the overlay; any input
    /// invalidates it. This is the "reactive" half: the fused result is a
    /// function of the frame identity, computed when the identity changes,
    /// not on every read.
    fused_memo: std::cell::RefCell<Option<FusedMemo>>,
    /// How many fused reads the memo served (evidence for the audit).
    fused_memo_hits: std::cell::Cell<u64>,
}

impl SemanticState {
    pub(crate) fn new() -> Self {
        SemanticState {
            cache: std::cell::RefCell::new(crate::semantic::SemanticCache::new()),
            fused_memo: std::cell::RefCell::new(None),
            fused_memo_hits: std::cell::Cell::new(0),
        }
    }

    /// Borrow the structural cache (analyze / fuse pipelines).
    pub(crate) fn cache(&self) -> &std::cell::RefCell<crate::semantic::SemanticCache> {
        &self.cache
    }

    /// Memo lookup: on an identity hit, count it and return the cloned
    /// triple; on a miss, return `None` and let the caller compute.
    pub(crate) fn memo_hit(
        &self,
        key: &str,
    ) -> Option<(
        crate::semantic::SemanticScreen,
        crate::semantic::node::SemanticTree,
        crate::semantic::native::NativeOverlayReport,
    )> {
        if let Some(memo) = self.fused_memo.borrow().as_ref() {
            if memo.key == key {
                self.fused_memo_hits.set(self.fused_memo_hits.get() + 1);
                return Some((memo.sem.clone(), memo.tree.clone(), memo.report.clone()));
            }
        }
        None
    }

    /// Install the freshly computed fused result for `key`.
    pub(crate) fn store_memo(
        &self,
        key: String,
        sem: crate::semantic::SemanticScreen,
        tree: crate::semantic::node::SemanticTree,
        report: crate::semantic::native::NativeOverlayReport,
    ) {
        *self.fused_memo.borrow_mut() = Some(FusedMemo {
            key,
            sem,
            tree,
            report,
        });
    }

    /// How many fused reads the memo served without recompute.
    pub(crate) fn fused_memo_hits(&self) -> u64 {
        self.fused_memo_hits.get()
    }

    /// Drop the fused memo (the explicit invalidation hook).
    pub(crate) fn invalidate_fused(&self) {
        *self.fused_memo.borrow_mut() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(marker: &str) -> ScreenState {
        let mut s = ScreenState::new(10, 2);
        s.viewport_text = vec![marker.to_string(), String::new()];
        s
    }

    #[test]
    fn fused_key_binds_every_identity_input() {
        let a = screen("x");
        let b = screen("x");
        // Identical screens → the same key (the key is a pure function of
        // the frame's hashes + interaction + native position).
        let k1 = fused_key(&a, 0);
        let k2 = fused_key(&b, 0);
        assert_eq!(k1, k2);
        // A different native position invalidates.
        assert_ne!(fused_key(&a, 1), k1);
        // A different structure invalidates.
        let mut c = screen("x");
        c.structure_hash = "different".to_string();
        assert_ne!(fused_key(&c, 0), k1);
        // A different interaction state invalidates.
        let mut d = screen("x");
        d.process.running = true;
        assert_ne!(fused_key(&d, 0), k1);
    }

    fn any_sem() -> crate::semantic::SemanticScreen {
        crate::semantic::analyze(&screen("x"))
    }

    fn any_tree() -> crate::semantic::node::SemanticTree {
        // The tree type has no Default; derive one the same way the
        // pipeline does (the memo never inspects it in this test).
        let mut cache = crate::semantic::SemanticCache::new();
        let native = crate::semantic::native::NativeChannel::default();
        crate::semantic::fuse_tree(&screen("x"), &mut cache, &native)
    }

    #[test]
    fn memo_round_trips_and_counts_hits() {
        let st = SemanticState::new();
        assert_eq!(st.fused_memo_hits(), 0);
        assert!(st.memo_hit("k").is_none());
        st.store_memo("k".to_string(), any_sem(), any_tree(), Default::default());
        assert!(st.memo_hit("k").is_some());
        assert!(st.memo_hit("k").is_some());
        assert_eq!(st.fused_memo_hits(), 2, "only hits are counted");
        assert!(st.memo_hit("other").is_none());
        assert_eq!(st.fused_memo_hits(), 2, "misses do not bump the counter");
        st.invalidate_fused();
        assert!(st.memo_hit("k").is_none());
    }
}
