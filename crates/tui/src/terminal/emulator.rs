//! vt100 state plus the minimal device replies and OSC policy it lacks.
//!
//! Replies are generated only from emulator state, never from remote bytes,
//! so a query cannot make Relay type attacker-controlled text into the shell.

use std::num::NonZeroU16;

use common::display_text;

/// Candidate default from the architecture; also bounds memory per tab.
pub const SCROLLBACK_LINES: usize = 10_000;
/// Remote output can request replies in a loop; cap what one batch queues.
const MAX_PENDING_REPLY: usize = 4096;
const MAX_TITLE_CHARS: usize = 128;

#[derive(Default)]
pub struct Responder {
    replies: Vec<u8>,
    pub title: Option<String>,
    pub bell: bool,
    pub clipboard_denied: u64,
}

impl Responder {
    fn reply(&mut self, bytes: &[u8]) {
        if self.replies.len() + bytes.len() <= MAX_PENDING_REPLY {
            self.replies.extend_from_slice(bytes);
        }
    }
}

impl vt100::Callbacks for Responder {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bell = true;
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let title: String = String::from_utf8_lossy(title)
            .chars()
            .take(MAX_TITLE_CHARS)
            .collect();
        self.title = Some(display_text(&title));
    }

    // OSC 52: writing is denied by default; reading is always denied and
    // never answered, so remote programs cannot observe the clipboard.
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8], _: &[u8]) {
        self.clipboard_denied += 1;
    }

    fn paste_from_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8]) {
        self.clipboard_denied += 1;
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, i2, c, first) {
            // DA1: VT220-class with ANSI colour, as many emulators report.
            (None, None, 'c', 0) => self.reply(b"\x1b[?62;22c"),
            // DA2: a low version so applications do not assume xterm extras.
            (Some(b'>'), None, 'c', 0) => self.reply(b"\x1b[>1;10;0c"),
            // DSR status and cursor position (CPR, also DEC form).
            (None, None, 'n', 5) => self.reply(b"\x1b[0n"),
            (None | Some(b'?'), None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                let prefix = if i1.is_some() { "?" } else { "" };
                let reply = format!("\x1b[{prefix}{};{}R", row + 1, col + 1);
                self.reply(reply.as_bytes());
            }
            // XTWINOPS 18: text area size in characters.
            (None, None, 't', 18) => {
                let (rows, cols) = screen.size();
                self.reply(format!("\x1b[8;{rows};{cols}t").as_bytes());
            }
            _ => {}
        }
    }
}

/// Input-encoding modes that the remote application has switched on.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
}

pub struct Emulator {
    parser: vt100::Parser<Responder>,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                nonzero(rows),
                nonzero(cols),
                SCROLLBACK_LINES,
                Responder::default(),
            ),
        }
    }

    /// Feeds remote output and returns query replies destined for the PTY.
    pub fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.process(bytes);
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(nonzero(rows), nonzero(cols));
    }

    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (cols.get(), rows.get())
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Scrolls the local view; 0 is the live screen.
    pub fn scroll(&mut self, lines: isize) {
        let screen = self.parser.screen_mut();
        let offset = screen.scrollback().saturating_add_signed(lines);
        screen.set_scrollback(offset);
    }

    pub fn modes(&self) -> Modes {
        let screen = self.parser.screen();
        Modes {
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
        }
    }

    pub fn responder(&self) -> &Responder {
        self.parser.callbacks()
    }

    pub fn responder_mut(&mut self) -> &mut Responder {
        self.parser.callbacks_mut()
    }
}

fn nonzero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).unwrap_or(NonZeroU16::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_queries_are_answered_from_state_only() {
        let mut emulator = Emulator::new(80, 24);
        assert_eq!(emulator.process(b"\x1b[c"), b"\x1b[?62;22c");
        assert_eq!(emulator.process(b"\x1b[0c"), b"\x1b[?62;22c");
        assert_eq!(emulator.process(b"\x1b[>c"), b"\x1b[>1;10;0c");
        assert_eq!(emulator.process(b"\x1b[5n"), b"\x1b[0n");
        assert_eq!(emulator.process(b"\x1b[3;7H\x1b[6n"), b"\x1b[3;7R");
        assert_eq!(emulator.process(b"\x1b[?6n"), b"\x1b[?3;7R");
        assert_eq!(emulator.process(b"\x1b[18t"), b"\x1b[8;24;80t");
        // Unknown queries, OSC colour/clipboard reads and DCS get no reply.
        assert!(emulator.process(b"\x1b]11;?\x07\x1b]52;c;?\x07").is_empty());
        assert!(emulator.process(b"\x1bP+q544e\x1b\\\x1b[?u").is_empty());
        assert_eq!(emulator.responder().clipboard_denied, 1);
    }

    #[test]
    fn reply_flood_is_bounded() {
        let mut emulator = Emulator::new(80, 24);
        let replies = emulator.process(&b"\x1b[6n".repeat(10_000));
        assert!(replies.len() <= MAX_PENDING_REPLY);
        assert!(!replies.is_empty());
    }

    #[test]
    fn title_is_sanitised_and_clipboard_write_denied() {
        let mut emulator = Emulator::new(80, 24);
        emulator.process(b"\x1b]2;evil\r\n\x08\xc2\x9b\xe2\x80\xaetitle\x07\x07");
        emulator.process(b"\x1b]52;c;aGVsbG8=\x07");
        let responder = emulator.responder();
        let title = responder.title.as_deref().unwrap();
        assert!(!title.chars().any(char::is_control), "{title:?}");
        assert!(!title.contains('\u{202e}'), "{title:?}");
        assert!(title.contains("title"));
        assert!(responder.bell);
        assert_eq!(responder.clipboard_denied, 1);
    }

    #[test]
    fn tracks_modes_wide_text_and_truecolor() {
        let mut emulator = Emulator::new(10, 3);
        emulator.process(b"\x1b[?1h\x1b[?2004h\x1b[38;2;1;2;3m\xe4\xb8\xad\xe6\x96\x87");
        assert_eq!(
            emulator.modes(),
            Modes {
                application_cursor: true,
                bracketed_paste: true
            }
        );
        let screen = emulator.screen();
        let cell = screen.cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "中");
        assert!(cell.is_wide());
        assert!(screen.cell(0, 1).unwrap().is_wide_continuation());
        assert_eq!(cell.fgcolor(), vt100::Color::Rgb(1, 2, 3));
        assert_eq!(screen.cursor_position(), (0, 4));
        emulator.process(b"\x1b[?1l\x1b[?2004l");
        assert_eq!(emulator.modes(), Modes::default());
    }
}
