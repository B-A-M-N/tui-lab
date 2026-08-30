//! Asciicast recorder (spec item 40).
//!
//! Records the raw PTY byte stream and produces valid asciinema v3 `.cast`
//! format output. This is a real terminal recording, not a snapshot.

pub mod recorder;

pub use recorder::{AsciicastRecorder, RecordingEvent};
