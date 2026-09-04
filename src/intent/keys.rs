//! Canonical key serialization: the inverse of the MCP key parser, so
//! `KeyEvent::display()` always round-trips through `parse_key_public`.
//!
//! Split from the former single-file `intent` (review §15 god-object
//! residue).

use crate::backend::{KeyCode, KeyEvent, KeyModifiers};

impl KeyCode {
    /// Canonical key name, the exact vocabulary `parse_key_public` accepts.
    pub fn name(&self) -> String {
        match self {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Escape => "escape".to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Up => "up".to_string(),
            KeyCode::Down => "down".to_string(),
            KeyCode::Left => "left".to_string(),
            KeyCode::Right => "right".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::PageUp => "pageup".to_string(),
            KeyCode::PageDown => "pagedown".to_string(),
            KeyCode::Insert => "insert".to_string(),
            KeyCode::Delete => "delete".to_string(),
            KeyCode::Function(n) => format!("f{n}"),
        }
    }
}

impl KeyModifiers {
    /// Modifier prefix in canonical order (`ctrl+alt+`), empty when none.
    pub fn prefix(&self) -> String {
        let mut parts = Vec::new();
        if self.contains(KeyModifiers::CTRL) {
            parts.push("ctrl");
        }
        if self.contains(KeyModifiers::ALT) {
            parts.push("alt");
        }
        if self.contains(KeyModifiers::SHIFT) {
            parts.push("shift");
        }
        if self.contains(KeyModifiers::SUPER) {
            parts.push("super");
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("{}+", parts.join("+"))
        }
    }
}

impl KeyEvent {
    /// Canonical display form: `ctrl+alt+delete`, `shift+tab`, `a`.
    ///
    /// A shifted letter renders as its uppercase char — matching the
    /// parser's canonicalization (`shift+a` parses to `Char('A')`), so
    /// `display()` always round-trips through `parse_key_public`.
    pub fn display(&self) -> String {
        let body = match self.code {
            KeyCode::Char(c) if self.modifiers.shift() && c.is_ascii_lowercase() => {
                c.to_ascii_uppercase().to_string()
            }
            _ => self.code.name(),
        };
        format!("{}{}", self.modifiers.prefix(), body)
    }
}
