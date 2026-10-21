//! Terminal subsystem: emulation state, input encoding and escape keys.

pub mod emulator;
pub mod encoder;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A key that returns from Terminal Input to Relay Control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyBinding {
    /// Default escape keys (ADR-008): F12, with `Ctrl+]` as the fallback for
    /// hosts that intercept F12. Both are active unless overridden.
    pub fn defaults() -> Vec<Self> {
        vec![
            Self {
                code: KeyCode::F(12),
                modifiers: KeyModifiers::NONE,
            },
            Self {
                code: KeyCode::Char(']'),
                modifiers: KeyModifiers::CONTROL,
            },
        ]
    }

    /// Accepts `F1`..`F12` or `ctrl+X` / `alt+X` / `ctrl+alt+X` for one
    /// printable ASCII character, case-insensitive.
    pub fn parse(text: &str) -> Option<Self> {
        let lower = text.trim().to_ascii_lowercase();
        let mut modifiers = KeyModifiers::NONE;
        let mut rest = lower.as_str();
        loop {
            if let Some(tail) = rest.strip_prefix("ctrl+") {
                modifiers |= KeyModifiers::CONTROL;
                rest = tail;
            } else if let Some(tail) = rest.strip_prefix("alt+") {
                modifiers |= KeyModifiers::ALT;
                rest = tail;
            } else {
                break;
            }
        }
        if let Some(n) = rest.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            return (1..=12).contains(&n).then_some(Self {
                code: KeyCode::F(n),
                modifiers,
            });
        }
        let mut chars = rest.chars();
        let c = chars.next()?;
        // A bare printable key would make that character untypeable remotely.
        let valid = chars.next().is_none()
            && c.is_ascii_graphic()
            && !modifiers.is_empty()
            && (!modifiers.contains(KeyModifiers::CONTROL) || encoder::control_byte(c).is_some());
        valid.then_some(Self {
            code: KeyCode::Char(c),
            modifiers,
        })
    }

    /// Legacy terminals report `Ctrl+]` as `Ctrl+5`; compare control bytes so
    /// the binding works with and without the kitty keyboard protocol.
    pub fn matches(&self, key: &KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let relevant = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let modifiers = key.modifiers & relevant;
        match (self.code, key.code) {
            (KeyCode::Char(want), KeyCode::Char(got))
                if self.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                modifiers == self.modifiers
                    && encoder::control_byte(want).is_some()
                    && encoder::control_byte(want) == encoder::control_byte(got)
            }
            (KeyCode::Char(want), KeyCode::Char(got)) => {
                modifiers == self.modifiers && want.eq_ignore_ascii_case(&got)
            }
            (want, got) => {
                want == got && key.modifiers & (relevant | KeyModifiers::SHIFT) == self.modifiers
            }
        }
    }

    pub fn label(&self) -> String {
        let mut label = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            label.push_str("Ctrl+");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            label.push_str("Alt+");
        }
        match self.code {
            KeyCode::F(n) => label.push_str(&format!("F{n}")),
            KeyCode::Char(c) => label.push(c.to_ascii_uppercase()),
            _ => label.push('?'),
        }
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_keys_parse_and_match_legacy_and_kitty_forms() {
        let [f12, ctrl_bracket] = KeyBinding::defaults().try_into().unwrap();
        assert!(f12.matches(&KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)));
        assert!(!f12.matches(&KeyEvent::new(KeyCode::F(12), KeyModifiers::SHIFT)));
        // Legacy decoding of 0x1d, and the kitty protocol report.
        assert!(ctrl_bracket.matches(&KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL)));
        assert!(ctrl_bracket.matches(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(!ctrl_bracket.matches(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert_eq!(ctrl_bracket.label(), "Ctrl+]");

        assert_eq!(KeyBinding::parse("f10").unwrap().code, KeyCode::F(10));
        let alt_x = KeyBinding::parse("Alt+x").unwrap();
        assert!(alt_x.matches(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)));
        assert!(KeyBinding::parse("ctrl+alt+g").is_some());
        for invalid in [
            "x", "f13", "ctrl+", "ctrl+ab", "ctrl+é", "ctrl+!", "shift+a",
        ] {
            assert!(KeyBinding::parse(invalid).is_none(), "{invalid}");
        }
    }
}
