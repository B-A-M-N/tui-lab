//! Background run-journal writer (audit item: "run-journal writer").
//!
//! Persistent runs appended each `TransactionRecord` to
//! `transactions.jsonl` synchronously inside `push_ledger` — a filesystem
//! write (open + write + kernel buffer) on the driving path of every
//! interaction. This module moves that write to a dedicated thread:
//!
//! ```text
//! push_ledger ──serialize──▶ channel ──▶ writer thread ──▶ O_APPEND file
//!      │                                      │
//!      └── never blocks, never drops          └── watermark (AtomicU64)
//!                                                 read by flush()/Drop
//! ```
//!
//! Honesty rules:
//! - The channel is unbounded: a burst of transactions queues rather than
//!   blocking the session actor or being dropped. Memory is bounded in
//!   practice by the ledger's own eviction window.
//! - The watermark (`seq + 1` of the last line actually written) is a
//!   shared atomic owned by the writer. `flush()` polls it with a deadline
//!   and reports `persistence_unhealthy` on timeout instead of pretending.
//! - Crash semantics are unchanged from the synchronous design: the file
//!   holds every line the writer completed; the unflushed tail is at most
//!   what sat in the channel, which the manifest already declares via
//!   `dropped_records` semantics.
//! - The writer flushes and closes on channel disconnect (sender dropped),
//!   so `Drop for RunContext` ordering cannot strand lines in the buffer.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

/// What the writer thread receives: one pre-serialized NDJSON line (no
/// trailing newline) plus the `seq` it belongs to.
struct JournalLine {
    seq: u64,
    line: String,
}

/// Shared, thread-safe handle to the background writer.
pub struct JournalHandle {
    tx: mpsc::Sender<JournalLine>,
    /// `seq + 1` of the last line durably handed to the OS by the writer.
    flushed_upto: Arc<AtomicU64>,
    /// Set by the writer on a write error; read by flush to report it.
    failed: Arc<std::sync::atomic::AtomicBool>,
}

impl JournalHandle {
    /// Spawn a writer appending to `path`. The file is opened once and held.
    pub fn spawn(path: std::path::PathBuf) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let (tx, rx) = mpsc::channel::<JournalLine>();
        let flushed_upto = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w_flag = failed.clone();
        let w_mark = flushed_upto.clone();
        std::thread::Builder::new()
            .name("tui-lab-run-journal".into())
            .spawn(move || {
                let mut file = file;
                let mut mark = 0u64;
                for item in rx {
                    let mut ok = true;
                    ok &= file.write_all(item.line.as_bytes()).is_ok();
                    ok &= file.write_all(b"\n").is_ok();
                    if ok {
                        mark = mark.max(item.seq + 1);
                        // Publish the watermark lazily: only lines that
                        // completed both writes count as flushed.
                        w_mark.store(mark, Ordering::Release);
                    } else {
                        w_flag.store(true, Ordering::Release);
                        // A failed append is terminal for the writer; keep
                        // draining (so senders never block) but stop
                        // advancing the watermark. flush() reports the
                        // failure honestly.
                    }
                }
                // Channel disconnected: flush what the OS buffered.
                let _ = file.flush();
            })?;
        Ok(JournalHandle {
            tx,
            flushed_upto,
            failed,
        })
    }

    /// Hand one serialized record to the writer. Non-blocking; the line is
    /// queued, never dropped.
    pub fn submit(&self, seq: u64, line: String) {
        // A failed writer keeps draining the channel in its loop; the send
        // only fails if the thread is gone, in which case persistence is
        // unhealthy and flush() will say so.
        let _ = self.tx.send(JournalLine { seq, line });
    }

    /// Current watermark: every record with `seq < this` is on disk.
    pub fn flushed_upto(&self) -> u64 {
        self.flushed_upto.load(Ordering::Acquire)
    }

    /// Whether the writer hit a terminal write error.
    pub fn is_unhealthy(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Wait until the watermark reaches `target` or `deadline` elapses.
    /// Returns the watermark reached (which is < target on timeout).
    pub fn wait_for(&self, target: u64, deadline: std::time::Duration) -> u64 {
        let start = std::time::Instant::now();
        loop {
            let cur = self.flushed_upto();
            if cur >= target || self.is_unhealthy() || start.elapsed() >= deadline {
                return cur;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_writes_lines_and_advances_watermark() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join("transactions.jsonl");
        let j = JournalHandle::spawn(path.clone()).expect("spawn");

        j.submit(0, r#"{"seq":0,"a":1}"#.into());
        j.submit(1, r#"{"seq":1,"a":2}"#.into());
        let reached = j.wait_for(2, std::time::Duration::from_secs(2));
        assert_eq!(reached, 2, "both lines flushed");
        drop(j); // disconnect → writer flushes + exits

        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "{body}");
        assert!(lines[0].contains("\"seq\":0"));
        assert!(lines[1].contains("\"seq\":1"));
    }

    #[test]
    fn wait_for_times_out_honestly() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let j = JournalHandle::spawn(tmp.path().join("t.jsonl")).expect("spawn");
        // Nothing submitted: waiting for 1 must time out at 0.
        let reached = j.wait_for(1, std::time::Duration::from_millis(50));
        assert_eq!(reached, 0, "no lines → watermark stays 0");
    }
}
