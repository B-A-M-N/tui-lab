//! Bounded raw-byte capture ring (Wave-2 protocol diagnostics).
//!
//! Round-2 decomposition (G2): the three `PortablePtyBackend` fields that
//! make up protocol-byte evidence — the ring itself, its declared head
//! eviction count, and the absolute stream total — move into this holder.
//! Pure ring policy: bounds, offsets, stats. No parsing, no PTY, no
//! threads.
//!
//! The ring retains the child's REAL raw output bytes — escape sequences,
//! OSC, DCS and all — so the protocol decoder can reconstruct "what did
//! this TUI actually emit". Offsets survive head eviction, so a
//! transaction can cite its exact byte range in the child's output stream
//! even after the ring has wrapped (re-review item 19).

use std::collections::VecDeque;

/// How many raw bytes the engine retains for the protocol decoder. 256 KiB
/// covers generous terminal traffic (a full-screen redraw is typically
/// well under 8 KiB) at a small fixed cost; a firehose beyond it degrades
/// by dropping the head, which [`Self::stats`] declares.
pub const RAW_RING_CAPACITY: usize = 256 * 1024;

/// The bounded raw-output ring + declared eviction + absolute total.
#[derive(Default)]
pub struct RawCapture {
    bytes: VecDeque<u8>,
    /// Bytes dropped off the ring's head (declared eviction).
    dropped: u64,
    /// Total raw bytes EVER absorbed (re-review item 19): the absolute
    /// stream position of the next byte. The CURRENT window covers
    /// absolute offsets `[total - bytes.len(), total)`.
    total: u64,
}

impl RawCapture {
    /// An empty capture ring.
    pub fn new() -> Self {
        RawCapture::default()
    }

    /// Absorb one output chunk; evict from the head past the capacity,
    /// declaring every dropped byte.
    pub fn absorb(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        for &b in bytes {
            if self.bytes.len() >= RAW_RING_CAPACITY {
                self.bytes.pop_front();
                self.dropped += 1;
            }
            self.bytes.push_back(b);
        }
    }

    /// The retained window (oldest first).
    pub fn window(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }

    /// `(capacity, dropped_head_count)` — the declared eviction stats.
    pub fn stats(&self) -> (usize, u64) {
        (RAW_RING_CAPACITY, self.dropped)
    }

    /// The absolute byte range the retained window covers in the child's
    /// output stream: `(start_offset, end_offset_exclusive)`.
    pub fn range(&self) -> (u64, u64) {
        let end = self.total;
        let start = end.saturating_sub(self.bytes.len() as u64);
        (start, end)
    }

    /// The absolute stream position at this instant — the offset the NEXT
    /// byte will get. Snapshot before and after an action to bracket it.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Reset to empty (backend start / re-attach). Counters restart: the
    /// stream being observed is a new one.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.dropped = 0;
        self.total = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_tracks_absorbed_bytes() {
        let mut rc = RawCapture::new();
        rc.absorb(b"hello ");
        rc.absorb(b"world");
        assert_eq!(rc.window(), b"hello world".to_vec());
        assert_eq!(rc.range(), (0, 11));
        assert_eq!(rc.total(), 11);
        assert_eq!(rc.stats(), (RAW_RING_CAPACITY, 0));
    }

    #[test]
    fn head_eviction_is_declared_and_offsets_survive() {
        let mut rc = RawCapture::new();
        // Absorb past the capacity so the head MUST drop.
        let chunk = vec![b'x'; RAW_RING_CAPACITY];
        rc.absorb(&chunk);
        rc.absorb(b"tail");
        let (cap, dropped) = rc.stats();
        assert_eq!(cap, RAW_RING_CAPACITY);
        assert_eq!(dropped, 4, "the four head bytes were evicted");
        assert_eq!(rc.window(), {
            let mut w = vec![b'x'; RAW_RING_CAPACITY - 4];
            w.extend_from_slice(b"tail");
            w
        });
        let (start, end) = rc.range();
        assert_eq!(end, RAW_RING_CAPACITY as u64 + 4);
        assert_eq!(start, end - rc.window().len() as u64);
    }

    #[test]
    fn clear_restarts_the_stream() {
        let mut rc = RawCapture::new();
        rc.absorb(b"first");
        rc.clear();
        rc.absorb(b"second");
        assert_eq!(rc.window(), b"second".to_vec());
        assert_eq!(rc.range(), (0, 6));
        assert_eq!(rc.stats(), (RAW_RING_CAPACITY, 0));
    }
}
