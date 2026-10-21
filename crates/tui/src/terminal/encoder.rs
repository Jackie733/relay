//! Re-encodes crossterm's parsed key events as the legacy xterm byte
//! sequences a remote `xterm-256color` application expects.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::emulator::Modes;

const ESC: u8 = 0x1b;
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Returns `None` for events with no terminal encoding (releases, media keys).
pub fn encode_key(key: KeyEvent, modes: Modes) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // xterm modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4).
    let parameter = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let mut bytes = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if alt {
                bytes.push(ESC);
            }
            match ctrl.then(|| control_byte(c)).flatten() {
                Some(byte) => bytes.push(byte),
                None => bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            }
        }
        KeyCode::Enter => prefixed(&mut bytes, alt, b"\r"),
        KeyCode::Tab if shift => bytes.extend_from_slice(b"\x1b[Z"),
        KeyCode::Tab => prefixed(&mut bytes, alt, b"\t"),
        KeyCode::BackTab => bytes.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => prefixed(&mut bytes, alt, if ctrl { b"\x08" } else { b"\x7f" }),
        KeyCode::Esc => prefixed(&mut bytes, alt, b"\x1b"),
        KeyCode::Null => bytes.push(0),
        KeyCode::Up => cursor(&mut bytes, b'A', parameter, modes),
        KeyCode::Down => cursor(&mut bytes, b'B', parameter, modes),
        KeyCode::Right => cursor(&mut bytes, b'C', parameter, modes),
        KeyCode::Left => cursor(&mut bytes, b'D', parameter, modes),
        KeyCode::Home => cursor(&mut bytes, b'H', parameter, modes),
        KeyCode::End => cursor(&mut bytes, b'F', parameter, modes),
        KeyCode::Insert => tilde(&mut bytes, 2, parameter),
        KeyCode::Delete => tilde(&mut bytes, 3, parameter),
        KeyCode::PageUp => tilde(&mut bytes, 5, parameter),
        KeyCode::PageDown => tilde(&mut bytes, 6, parameter),
        KeyCode::F(n @ 1..=4) => {
            let final_byte = b"PQRS"[usize::from(n - 1)];
            if parameter == 1 {
                bytes.extend_from_slice(&[ESC, b'O', final_byte]);
            } else {
                bytes.extend_from_slice(format!("\x1b[1;{parameter}").as_bytes());
                bytes.push(final_byte);
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
            tilde(&mut bytes, code, parameter);
        }
        _ => return None,
    }
    Some(bytes)
}

/// Bracketed paste wraps the text and strips every embedded paste marker so
/// pasted content cannot end the paste early and inject commands. Without
/// bracketed paste, newlines become `CR` as when typed.
pub fn encode_paste(text: &str, modes: Modes) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if !modes.bracketed_paste {
        return normalized.into_bytes();
    }
    let mut body = normalized;
    // Removing one marker can join the halves of another; repeat to a fixpoint.
    loop {
        let stripped = body.replace(PASTE_END, "").replace(PASTE_START, "");
        if stripped.len() == body.len() {
            break;
        }
        body = stripped;
    }
    format!("{PASTE_START}{body}{PASTE_END}").into_bytes()
}

pub(crate) fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

fn prefixed(bytes: &mut Vec<u8>, alt: bool, sequence: &[u8]) {
    if alt {
        bytes.push(ESC);
    }
    bytes.extend_from_slice(sequence);
}

fn cursor(bytes: &mut Vec<u8>, final_byte: u8, parameter: u8, modes: Modes) {
    if parameter != 1 {
        bytes.extend_from_slice(format!("\x1b[1;{parameter}").as_bytes());
    } else if modes.application_cursor {
        bytes.extend_from_slice(b"\x1bO");
    } else {
        bytes.extend_from_slice(b"\x1b[");
    }
    bytes.push(final_byte);
}

fn tilde(bytes: &mut Vec<u8>, code: u8, parameter: u8) {
    let sequence = if parameter == 1 {
        format!("\x1b[{code}~")
    } else {
        format!("\x1b[{code};{parameter}~")
    };
    bytes.extend_from_slice(sequence.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(code, modifiers), Modes::default()).unwrap()
    }

    #[test]
    fn cursor_keys_follow_decckm_and_modifiers() {
        let none = KeyModifiers::NONE;
        let application = Modes {
            application_cursor: true,
            ..Modes::default()
        };
        assert_eq!(key(KeyCode::Up, none), b"\x1b[A");
        let up = KeyEvent::new(KeyCode::Up, none);
        assert_eq!(encode_key(up, application).unwrap(), b"\x1bOA");
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Home, none), application).unwrap(),
            b"\x1bOH"
        );
        // Modified keys use the CSI form regardless of DECCKM.
        let ctrl_up = KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(encode_key(ctrl_up, application).unwrap(), b"\x1b[1;5A");
        assert_eq!(key(KeyCode::Left, KeyModifiers::SHIFT), b"\x1b[1;2D");
        assert_eq!(
            key(KeyCode::Right, KeyModifiers::ALT | KeyModifiers::SHIFT),
            b"\x1b[1;4C"
        );
        assert_eq!(key(KeyCode::Delete, KeyModifiers::CONTROL), b"\x1b[3;5~");
        assert_eq!(key(KeyCode::PageDown, none), b"\x1b[6~");
        assert_eq!(key(KeyCode::F(1), none), b"\x1bOP");
        assert_eq!(key(KeyCode::F(2), KeyModifiers::SHIFT), b"\x1b[1;2Q");
        assert_eq!(key(KeyCode::F(5), none), b"\x1b[15~");
        assert_eq!(key(KeyCode::F(12), KeyModifiers::CONTROL), b"\x1b[24;5~");
    }

    #[test]
    fn control_alt_and_editing_keys() {
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        assert_eq!(key(KeyCode::Char('c'), ctrl), [0x03]);
        assert_eq!(key(KeyCode::Char('C'), ctrl | KeyModifiers::SHIFT), [0x03]);
        assert_eq!(key(KeyCode::Char(' '), ctrl), [0x00]);
        assert_eq!(key(KeyCode::Char(']'), ctrl), [0x1d]);
        assert_eq!(key(KeyCode::Char('x'), alt), b"\x1bx");
        assert_eq!(key(KeyCode::Char('b'), alt | ctrl), [0x1b, 0x02]);
        assert_eq!(
            key(KeyCode::Char('中'), KeyModifiers::NONE),
            "中".as_bytes()
        );
        assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(key(KeyCode::Enter, alt), b"\x1b\r");
        assert_eq!(key(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
        assert_eq!(key(KeyCode::Backspace, ctrl), [0x08]);
        assert_eq!(key(KeyCode::Esc, KeyModifiers::NONE), [0x1b]);
        assert_eq!(key(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)
        };
        assert!(encode_key(release, Modes::default()).is_none());
    }

    #[test]
    fn bracketed_paste_strips_injected_markers() {
        let bracketed = Modes {
            bracketed_paste: true,
            ..Modes::default()
        };
        let pasted = "echo safe\x1b[201~\nrm -rf ~\x1b[20\x1b[201~1~\n";
        let encoded = String::from_utf8(encode_paste(pasted, bracketed)).unwrap();
        assert_eq!(encoded, "\x1b[200~echo safe\rrm -rf ~\r\x1b[201~");
        assert_eq!(encoded.matches(PASTE_END).count(), 1);
        assert_eq!(encode_paste("a\r\nb\n", Modes::default()), b"a\rb\r");
    }
}
