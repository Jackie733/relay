//! TerminalSession: one PTY channel plus its emulator state.
//!
//! PTY output never travels through the UI event queue. A reader task feeds
//! the shared emulator and only signals "updated"; the UI locks briefly to
//! draw the latest screen, so output floods coalesce into the next frame.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use common::display_text;
use ssh::{PtyEvent, PtyReader, PtySize, PtyWriter};
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
};

use crate::terminal::emulator::Emulator;

/// Pending writes per session (keys, pastes and query replies).
const INPUT_QUEUE: usize = 256;
/// Window-change requests are debounced while the user drags a resize.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Status {
    Running,
    Exited(u32),
    Signalled(String),
    Closed(String),
}

impl Status {
    pub fn is_running(&self) -> bool {
        *self == Self::Running
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Running => "running".into(),
            Self::Exited(code) => format!("exited with status {code}"),
            Self::Signalled(signal) => format!("terminated by {}", display_text(signal)),
            Self::Closed(reason) => format!("closed: {}", display_text(reason)),
        }
    }
}

pub struct SessionState {
    pub emulator: Emulator,
    pub status: Status,
}

/// Service-side endpoints of a session, consumed by the PTY tasks.
pub struct SessionIo {
    pub state: Arc<Mutex<SessionState>>,
    pub input: mpsc::Receiver<Vec<u8>>,
    pub replies: mpsc::Sender<Vec<u8>>,
    pub size: watch::Receiver<PtySize>,
    pub updated: Arc<Notify>,
}

pub struct TerminalSession {
    pub label: String,
    state: Arc<Mutex<SessionState>>,
    input: mpsc::Sender<Vec<u8>>,
    size: watch::Sender<PtySize>,
    updated: Arc<Notify>,
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SendError {
    Busy,
    Closed,
}

impl TerminalSession {
    pub fn new(label: String, size: PtySize, updated: Arc<Notify>) -> (Self, SessionIo) {
        let state = Arc::new(Mutex::new(SessionState {
            emulator: Emulator::new(size.cols, size.rows),
            status: Status::Running,
        }));
        let (input, receiver) = mpsc::channel(INPUT_QUEUE);
        let (size, size_receiver) = watch::channel(size);
        let io = SessionIo {
            state: state.clone(),
            input: receiver,
            replies: input.clone(),
            size: size_receiver,
            updated: updated.clone(),
        };
        let session = Self {
            label,
            state,
            input,
            size,
            updated,
            tasks: Vec::new(),
        };
        (session, io)
    }

    /// Connects the session to a real SSH PTY channel.
    pub fn start_ssh(
        label: String,
        size: PtySize,
        updated: Arc<Notify>,
        reader: PtyReader,
        writer: PtyWriter,
    ) -> Self {
        let (mut session, io) = Self::new(label, size, updated);
        session.tasks = spawn_pty_tasks(io, reader, writer);
        session
    }

    pub fn state(&self) -> MutexGuard<'_, SessionState> {
        lock(&self.state)
    }

    /// Never blocks the UI: a full queue drops the input and reports it.
    pub fn send(&self, bytes: Vec<u8>) -> Result<(), SendError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if !self.state().status.is_running() {
            return Err(SendError::Closed);
        }
        self.input.try_send(bytes).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => SendError::Busy,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })
    }

    /// Resizes the local screen immediately; the remote PTY follows after a
    /// short debounce. Only the latest size is sent.
    pub fn resize(&self, cols: u16, rows: u16) {
        let size = PtySize::new(cols, rows);
        let mut state = self.state();
        if state.emulator.size() == (size.cols, size.rows) {
            return;
        }
        state.emulator.resize(size.cols, size.rows);
        drop(state);
        self.size.send_replace(size);
        self.updated.notify_one();
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn lock(state: &Mutex<SessionState>) -> MutexGuard<'_, SessionState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn finish(io_state: &Mutex<SessionState>, status: Status, updated: &Notify) {
    let mut state = lock(io_state);
    if state.status.is_running() {
        state.status = status;
    }
    drop(state);
    updated.notify_one();
}

fn spawn_pty_tasks(io: SessionIo, mut reader: PtyReader, writer: PtyWriter) -> Vec<JoinHandle<()>> {
    let SessionIo {
        state,
        mut input,
        replies,
        mut size,
        updated,
    } = io;
    let read_state = state.clone();
    let read_updated = updated.clone();
    let read = tokio::spawn(async move {
        let mut exit = None;
        while let Some(event) = reader.next().await {
            match event {
                PtyEvent::Output(data) => {
                    let answer = lock(&read_state).emulator.process(&data);
                    // Replies share the write queue so they stay ordered
                    // with keystrokes; waiting here applies backpressure.
                    let closed = !answer.is_empty() && replies.send(answer).await.is_err();
                    read_updated.notify_one();
                    if closed {
                        break;
                    }
                }
                PtyEvent::Exited(code) => exit = Some(Status::Exited(code)),
                PtyEvent::Signalled(signal) => exit = Some(Status::Signalled(signal)),
                PtyEvent::Closed => break,
            }
        }
        let status = exit.unwrap_or_else(|| Status::Closed("channel closed by remote".into()));
        finish(&read_state, status, &read_updated);
    });
    let write = tokio::spawn(async move {
        let result: Result<(), ssh::ConnectError> = async {
            loop {
                tokio::select! {
                    bytes = input.recv() => match bytes {
                        Some(bytes) => writer.write(&bytes).await?,
                        None => return Ok(()),
                    },
                    changed = size.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                        tokio::time::sleep(RESIZE_DEBOUNCE).await;
                        let latest = *size.borrow_and_update();
                        writer.resize(latest).await?;
                    }
                }
            }
        }
        .await;
        if let Err(error) = result {
            finish(&state, Status::Closed(error.to_string()), &updated);
        }
    });
    vec![read, write]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn input_is_bounded_and_resizes_coalesce() {
        let notify = Arc::new(Notify::new());
        let (session, mut io) = TerminalSession::new("t".into(), PtySize::new(80, 24), notify);
        for _ in 0..INPUT_QUEUE {
            session.send(b"x".to_vec()).unwrap();
        }
        assert_eq!(session.send(b"y".to_vec()), Err(SendError::Busy));
        assert_eq!(io.input.recv().await.unwrap(), b"x");

        session.resize(100, 30);
        session.resize(120, 40);
        assert_eq!(session.state().emulator.size(), (120, 40));
        assert_eq!(*io.size.borrow_and_update(), PtySize::new(120, 40));

        lock(&io.state).status = Status::Exited(0);
        assert_eq!(session.send(b"z".to_vec()), Err(SendError::Closed));
    }

    #[tokio::test]
    #[ignore = "requires tests/integration OpenSSH containers on localhost:2221"]
    async fn real_openssh_vim_htop_tmux_less_cjk_truecolor_and_escape_keys() {
        use std::path::Path;

        use common::HostId;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Color};
        use ssh::{
            ConnectOptions, Connection, HostProfile, PromptBroker, PromptKind, PromptResponse,
            PtyCommand, TrustStore,
        };

        use crate::{
            app::{App, InputMode, Workspace},
            ui::{render, terminal_size},
        };

        let root = tempfile::tempdir().unwrap();
        let options = ConnectOptions::new(
            HostProfile {
                id: HostId("p0-pty".into()),
                hostname: "127.0.0.1".into(),
                port: 2221,
                username: "relay".into(),
            },
            TrustStore {
                sources: vec![root.path().join("user"), root.path().join("global")],
                relay_file: root.path().join("relay"),
            },
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/integration/.fixtures/id_ed25519"),
        );
        let (broker, mut prompts) = PromptBroker::channel(1);
        let connect = tokio::spawn(Connection::connect(options, broker));
        let prompt = prompts.recv().await.unwrap();
        assert!(matches!(prompt.kind, PromptKind::HostKey { .. }));
        prompt.response.send(PromptResponse::Trust).unwrap();
        let connection = connect.await.unwrap().unwrap();

        let (width, height) = (100, 30);
        let (cols, rows) = terminal_size(Rect::new(0, 0, width, height));
        let updated = Arc::new(Notify::new());
        let (reader, writer) = connection
            .open_pty(PtySize::new(cols, rows), PtyCommand::Shell)
            .await
            .unwrap();

        let mut app = App::new("/tmp".into());
        app.terminals.push(TerminalSession::start_ssh(
            "relay@127.0.0.1:2221".into(),
            PtySize::new(cols, rows),
            updated.clone(),
            reader,
            writer,
        ));
        app.workspace = Workspace::Terminals;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::TerminalInput);

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let type_text = |app: &mut App, text: &str| {
            for ch in text.chars() {
                let key = match ch {
                    '\r' | '\n' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    '\x1b' => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                    c => KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                };
                app.key(key);
            }
        };
        async fn wait_for(
            terminal: &mut Terminal<TestBackend>,
            app: &mut App,
            updated: &Notify,
            predicate: impl Fn(&ratatui::buffer::Buffer, &App) -> bool,
            description: &str,
        ) {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    app.sessions_updated();
                    terminal.draw(|frame| render(frame, app)).unwrap();
                    if predicate(terminal.backend().buffer(), app) {
                        return;
                    }
                    let _ =
                        tokio::time::timeout(Duration::from_millis(50), updated.notified()).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                panic!("timed out waiting for {description}; screen:\n{text}");
            });
        }
        fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
            let mut text = String::new();
            let mut skip = false;
            for cell in &buf.content {
                if std::mem::take(&mut skip) {
                    continue;
                }
                let symbol = cell.symbol();
                // Wide CJK cells in Ratatui leave a blank continuation cell next.
                if symbol.len() >= 3 && symbol.chars().all(|c| c > '\u{2e7f}') {
                    skip = true;
                }
                text.push_str(symbol);
            }
            text
        }
        let has_text = |needle: &'static str| {
            move |buf: &ratatui::buffer::Buffer, _: &App| buffer_text(buf).contains(needle)
        };

        // 1. vim: DA/DSR queries, DECCKM cursor keys, Esc, CJK edit and save.
        type_text(
            &mut app,
            "rm -f /tmp/relay-vim.txt /tmp/.relay-vim.txt.swp && vim -u NONE -N -n /tmp/relay-vim.txt\r",
        );
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            |buf, app| {
                let text: String = buf.content.iter().map(|c| c.symbol()).collect();
                app.active_terminal()
                    .unwrap()
                    .state()
                    .emulator
                    .screen()
                    .alternate_screen()
                    && text.contains("~")
            },
            "vim alternate screen",
        )
        .await;
        type_text(&mut app, "i第一行\r第二行\x1b");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("第二行"),
            "vim CJK insert",
        )
        .await;
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        type_text(&mut app, "A-ok\x1b");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("第一行-ok"),
            "vim DECCKM Up arrow edit",
        )
        .await;
        type_text(&mut app, ":wq\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            |_, app| {
                !app.active_terminal()
                    .unwrap()
                    .state()
                    .emulator
                    .screen()
                    .alternate_screen()
            },
            "vim exit alternate screen",
        )
        .await;
        type_text(&mut app, "cat /tmp/relay-vim.txt; echo vim-''done\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("vim-done"),
            "vim output",
        )
        .await;
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("第一行-ok"),
            "saved CJK line",
        )
        .await;

        // 2. htop: alternate screen, colours and clean quit.
        type_text(&mut app, "htop\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            |buf, _| {
                let text: String = buf.content.iter().map(|c| c.symbol()).collect();
                let coloured = buf
                    .content
                    .iter()
                    .filter(|c| c.fg != Color::Reset || c.bg != Color::Reset)
                    .count();
                (text.contains("Tasks") || text.contains("Mem") || text.contains("CPU"))
                    && coloured > 20
            },
            "htop coloured UI",
        )
        .await;
        type_text(&mut app, "q");
        type_text(&mut app, "echo htop-''done\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("htop-done"),
            "htop exit",
        )
        .await;

        // 3. less: arrow keys under DECCKM and quit.
        type_text(&mut app, "seq 10 60 | less\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            |buf, app| {
                app.active_terminal()
                    .unwrap()
                    .state()
                    .emulator
                    .screen()
                    .alternate_screen()
                    && buffer_text(buf).contains("15")
            },
            "less alternate screen",
        )
        .await;
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        type_text(&mut app, "q");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            |_, app| {
                !app.active_terminal()
                    .unwrap()
                    .state()
                    .emulator
                    .screen()
                    .alternate_screen()
            },
            "less exit alternate screen",
        )
        .await;
        type_text(&mut app, "echo less-''done\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("less-done"),
            "less exit",
        )
        .await;

        // 4. tmux + truecolor + CJK wide characters.
        type_text(
            &mut app,
            "tmux -L relay-p0 kill-server 2>/dev/null; tmux -L relay-p0 -T RGB -f /dev/null new -s p0\r",
        );
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("[p0]"),
            "tmux status bar",
        )
        .await;
        type_text(
            &mut app,
            "printf '\\033[38;2;210;105;30m真彩色宽字符\\033[0m\\n'\r",
        );
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            |buf, _| {
                buf.content
                    .iter()
                    .any(|c| c.symbol() == "真" && c.fg == Color::Rgb(210, 105, 30))
            },
            "truecolor CJK inside tmux",
        )
        .await;
        type_text(&mut app, "exit\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            |_, app| {
                !app.active_terminal()
                    .unwrap()
                    .state()
                    .emulator
                    .screen()
                    .alternate_screen()
            },
            "tmux exit alternate screen",
        )
        .await;
        type_text(&mut app, "echo tmux-''done\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("tmux-done"),
            "tmux exit",
        )
        .await;

        // 5. Bracketed paste with injection attempt stripped, plus resize & escape keys.
        type_text(&mut app, "printf '\\033[?2004h'\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            |_, app| {
                app.active_terminal()
                    .unwrap()
                    .state()
                    .emulator
                    .modes()
                    .bracketed_paste
            },
            "bracketed paste mode",
        )
        .await;
        // Readline bracketed paste must keep the newline inert even if the
        // pasted payload tries to close bracketed paste early with ESC[201~.
        app.paste("echo pasted-''ok\x1b[201~\n");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("echo pasted-''ok"),
            "pasted text on prompt without executing",
        )
        .await;
        assert!(!buffer_text(terminal.backend().buffer()).contains("\npasted-ok"));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("pasted-ok"),
            "paste",
        )
        .await;

        terminal.backend_mut().resize(112, 39);
        app.active_terminal().unwrap().resize(110, 32);
        tokio::time::sleep(Duration::from_millis(120)).await;
        type_text(&mut app, "stty size; echo size-''done\r");
        wait_for(
            &mut terminal,
            &mut app,
            &updated,
            has_text("32 110"),
            "resized PTY",
        )
        .await;

        // Return to Relay Control with F12, switch workspace, then return and test Ctrl+].
        app.key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Control);
        app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.workspace, Workspace::Files);
        app.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::TerminalInput);
        app.key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL));
        assert_eq!(app.mode, InputMode::Control);

        drop(app);
        connection.disconnect().await.unwrap();
    }
}
