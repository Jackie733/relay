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

    async fn connect_port(root: &std::path::Path, id: &str, port: u16) -> ssh::Connection {
        let options = ssh::ConnectOptions::new(
            ssh::HostProfile {
                id: common::HostId(id.into()),
                hostname: "127.0.0.1".into(),
                port,
                username: "relay".into(),
            },
            ssh::TrustStore {
                sources: vec![
                    root.join(format!("user-{port}")),
                    root.join(format!("global-{port}")),
                ],
                relay_file: root.join(format!("relay-{port}")),
            },
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/integration/.fixtures/id_ed25519"),
        );
        let (broker, mut prompts) = ssh::PromptBroker::channel(1);
        let mut connect = tokio::spawn(ssh::Connection::connect(options, broker));
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    result = &mut connect => return result.unwrap().unwrap(),
                    Some(prompt) = prompts.recv() => {
                        assert!(matches!(prompt.kind, ssh::PromptKind::HostKey { .. }));
                        prompt.response.send(ssh::PromptResponse::Trust).unwrap();
                    }
                }
            }
        })
        .await
        .expect("fixture connection timed out")
    }

    async fn exec_stdout(conn: &ssh::Connection, cmd: &str) -> String {
        tokio::time::timeout(Duration::from_secs(20), async {
            let (mut reader, _writer) = conn
                .open_pty(PtySize::new(200, 24), ssh::PtyCommand::Exec(cmd.into()))
                .await
                .unwrap();
            let mut out = Vec::new();
            let mut code = None;
            while let Some(event) = reader.next().await {
                match event {
                    PtyEvent::Output(data) => out.extend_from_slice(&data),
                    PtyEvent::Exited(status) => code = Some(status),
                    PtyEvent::Closed => break,
                    _ => {}
                }
            }
            assert_eq!(code, Some(0), "remote command failed: {cmd}");
            String::from_utf8(out).unwrap().trim().to_owned()
        })
        .await
        .expect("fixture command timed out")
    }

    fn current_rss_kib() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest
                    .split_whitespace()
                    .next()
                    .and_then(|num| num.parse::<u64>().ok());
            }
        }
        None
    }

    fn percentiles_ms(mut samples_us: Vec<u64>) -> (f64, f64, f64) {
        samples_us.sort_unstable();
        assert!(
            !samples_us.is_empty(),
            "latency benchmark collected no samples"
        );
        let p50 = samples_us[samples_us.len() * 50 / 100] as f64 / 1000.0;
        let p99 =
            samples_us[(samples_us.len() * 99 / 100).min(samples_us.len() - 1)] as f64 / 1000.0;
        let max = *samples_us.last().unwrap() as f64 / 1000.0;
        (p50, p99, max)
    }

    async fn measure_pty_echo_us(
        session: &TerminalSession,
        updated: &Arc<Notify>,
        token: &str,
    ) -> u64 {
        let start = std::time::Instant::now();
        session.send(format!("{token}\n").into_bytes()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if session.state().emulator.screen().contents().contains(token) {
                    return;
                }
                let _ = tokio::time::timeout(Duration::from_millis(10), updated.notified()).await;
            }
        })
        .await
        .expect("PTY echo timed out");
        start.elapsed().as_micros() as u64
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires tests/integration OpenSSH containers on localhost:2221/2222"]
    async fn real_openssh_cross_host_transfer_and_pty_latency() {
        tokio::time::timeout(
            Duration::from_secs(90),
            cross_host_transfer_and_pty_latency(),
        )
        .await
        .expect("P0-04 integration test timed out");
    }

    async fn cross_host_transfer_and_pty_latency() {
        use common::{Endpoint, FileSystem, FsPath, FsResolver, HostId, RemotePath, WriteOptions};
        use ssh::PtyCommand;
        use transfer::{
            CancelToken, ConflictPolicy, JobState, TargetLocks, TransferConfig, TransferRequest,
            run_transfer,
        };

        let root = tempfile::tempdir().unwrap();
        let conn_a = Arc::new(connect_port(root.path(), "ssh:a", 2221).await);
        let conn_b = Arc::new(connect_port(root.path(), "ssh:b", 2222).await);

        // A random 128 MiB file is 512x larger than the 256 KiB relay queue.
        let work_dir = format!(
            "/tmp/relay-p0-04-{}",
            root.path().file_name().unwrap().to_str().unwrap()
        );
        let src_raw = format!("{work_dir}/source.bin");
        let dst_same_raw = format!("{work_dir}/destination.bin");
        let keep_raw = format!("{work_dir}/keep.bin");
        eprintln!("P0-04: preparing isolated fixtures");
        exec_stdout(
            &conn_a,
            &format!("mkdir -m 700 {work_dir} && head -c 134217728 /dev/urandom > {src_raw}"),
        )
        .await;
        exec_stdout(&conn_b, &format!("mkdir -m 700 {work_dir}")).await;
        let src_sha = exec_stdout(&conn_a, &format!("sha256sum {src_raw}"))
            .await
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();

        // Start interactive PTY on OpenSSH-A for latency measurements.
        let updated = Arc::new(Notify::new());
        let (reader, writer) = conn_a
            .open_pty(PtySize::new(80, 24), PtyCommand::Shell)
            .await
            .unwrap();
        let pty = TerminalSession::start_ssh(
            "relay@a".into(),
            PtySize::new(80, 24),
            updated.clone(),
            reader,
            writer,
        );
        let _ = measure_pty_echo_us(&pty, &updated, "pty-ready-0").await;

        // 1. Measure idle PTY echo latency (30 keystrokes).
        let mut idle_samples = Vec::new();
        for i in 0..30 {
            idle_samples.push(measure_pty_echo_us(&pty, &updated, &format!("idle-echo-{i}")).await);
        }
        let (idle_p50, idle_p99, idle_max) = percentiles_ms(idle_samples);

        // 2. Same-connection 128 MiB A -> Relay bounded buffer (4 * 64 KiB = 256 KiB) -> B transfer.
        let fs_a_same = Arc::new(conn_a.open_sftp().await.unwrap());
        let fs_b = Arc::new(conn_b.open_sftp().await.unwrap());
        let resolver_same = FsResolver::new();
        resolver_same.register_remote(HostId("ssh:a".into()), fs_a_same);
        resolver_same.register_remote(HostId("ssh:b".into()), fs_b.clone());

        let config = TransferConfig {
            chunk_size: 64 * 1024,
            channel_chunks: 4, // 256 KiB bounded buffer (file is 512x larger)
            ..TransferConfig::default()
        };
        let locks = TargetLocks::new();
        let cancel = CancelToken::new();
        let req_same = TransferRequest::new(
            "same-conn",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix(&src_raw).unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b".into()),
                RemotePath::from_posix(&dst_same_raw).unwrap(),
            ),
        );

        let rss_before = current_rss_kib().unwrap_or(0);
        let peak_rss_delta = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let peak_clone = peak_rss_delta.clone();
        let (stop_sampling, mut sampling_stopped) = tokio::sync::oneshot::channel::<()>();
        let sampler = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut sampling_stopped => break,
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        if let Some(rss) = current_rss_kib() {
                            peak_clone.fetch_max(rss.saturating_sub(rss_before), std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }
        });
        let same_start = std::time::Instant::now();
        let tx_handle = {
            let resolver = resolver_same.clone();
            let config = config.clone();
            let locks = locks.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_transfer(
                    &resolver,
                    &config,
                    &locks,
                    req_same,
                    ConflictPolicy::Overwrite,
                    &cancel,
                    |_| {},
                )
                .await
            })
        };

        let mut same_samples = Vec::new();
        let mut seq = 0usize;
        while !tx_handle.is_finished() {
            same_samples
                .push(measure_pty_echo_us(&pty, &updated, &format!("same-echo-{seq}")).await);
            seq += 1;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let report_same = tx_handle.await.unwrap().unwrap();
        let same_elapsed = same_start.elapsed();
        assert_eq!(report_same.state, JobState::Completed);
        assert_eq!(report_same.bytes_transferred, 134_217_728);
        let dst_same_sha = exec_stdout(&conn_b, &format!("sha256sum {dst_same_raw}"))
            .await
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(dst_same_sha, src_sha);
        let same_sample_count = same_samples.len();
        let (same_p50, same_p99, same_max) = percentiles_ms(same_samples);

        // 3. Dedicated-connection 128 MiB A -> B transfer + atomic overwrite verification.
        eprintln!("P0-04: same-connection transfer verified; opening trusted dedicated connection");
        let conn_a_ded = Arc::new(connect_port(root.path(), "ssh:a", 2221).await);
        let fs_a_ded = Arc::new(conn_a_ded.open_sftp().await.unwrap());
        let resolver_ded = FsResolver::new();
        resolver_ded.register_remote(HostId("ssh:a".into()), fs_a_ded);
        resolver_ded.register_remote(HostId("ssh:b".into()), fs_b.clone());

        let req_ded = TransferRequest::new(
            "ded-conn",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix(&src_raw).unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b".into()),
                RemotePath::from_posix(&dst_same_raw).unwrap(),
            ),
        );
        let ded_start = std::time::Instant::now();
        let tx_ded_handle = {
            let resolver = resolver_ded.clone();
            let config = config.clone();
            let locks = locks.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_transfer(
                    &resolver,
                    &config,
                    &locks,
                    req_ded,
                    ConflictPolicy::Overwrite,
                    &cancel,
                    |_| {},
                )
                .await
            })
        };

        let mut ded_samples = Vec::new();
        seq = 0;
        while !tx_ded_handle.is_finished() {
            ded_samples.push(measure_pty_echo_us(&pty, &updated, &format!("ded-echo-{seq}")).await);
            seq += 1;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let report_ded = tx_ded_handle.await.unwrap().unwrap();
        let ded_elapsed = ded_start.elapsed();
        assert_eq!(report_ded.state, JobState::Completed);
        let dst_overwritten_sha = exec_stdout(&conn_b, &format!("sha256sum {dst_same_raw}"))
            .await
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(dst_overwritten_sha, src_sha);
        let ded_sample_count = ded_samples.len();
        let (ded_p50, ded_p99, ded_max) = percentiles_ms(ded_samples);

        let _ = stop_sampling.send(());
        sampler.await.unwrap();
        let peak_kib = peak_rss_delta.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            peak_kib < 64 * 1024,
            "peak RSS delta {peak_kib} KiB exceeded bounded memory limit"
        );
        eprintln!(
            "P0-04 128 MiB A->B baseline:\n  same-conn: {:.2}s ({:.1} MiB/s), PTY echo p50={:.2}ms p99={:.2}ms max={:.2}ms n={}\n  ded-conn:  {:.2}s ({:.1} MiB/s), PTY echo p50={:.2}ms p99={:.2}ms max={:.2}ms n={}\n  idle PTY:  p50={:.2}ms p99={:.2}ms max={:.2}ms n=30, peak_rss_delta={} KiB",
            same_elapsed.as_secs_f64(),
            128.0 / same_elapsed.as_secs_f64(),
            same_p50,
            same_p99,
            same_max,
            same_sample_count,
            ded_elapsed.as_secs_f64(),
            128.0 / ded_elapsed.as_secs_f64(),
            ded_p50,
            ded_p99,
            ded_max,
            ded_sample_count,
            idle_p50,
            idle_p99,
            idle_max,
            peak_kib,
        );

        // 4. Cancellation mid-transfer preserves old target and cleans up `.part` file.
        let keep_path = FsPath::Remote(RemotePath::from_posix(&keep_raw).unwrap());
        let mut w = fs_b
            .open_write(&keep_path, WriteOptions::overwrite(0o600))
            .await
            .unwrap();
        w.write_all(b"intact-before-cancel").await.unwrap();
        w.close().await.unwrap();

        let cancel_mid = CancelToken::new();
        let cancel_trigger = cancel_mid.clone();
        let req_cancel = TransferRequest::new(
            "cancel-job",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix(&src_raw).unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b".into()),
                RemotePath::from_posix(&keep_raw).unwrap(),
            ),
        );
        let cancel_report = run_transfer(
            &resolver_ded,
            &config,
            &locks,
            req_cancel,
            ConflictPolicy::Overwrite,
            &cancel_mid,
            move |prog| {
                if prog.bytes_transferred >= 2 * 1024 * 1024 {
                    cancel_trigger.cancel();
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(cancel_report.state, JobState::Cancelled);
        assert!(cancel_report.residual_temp_files.is_empty());
        assert_eq!(
            exec_stdout(&conn_b, &format!("cat {keep_raw}")).await,
            "intact-before-cancel"
        );

        // 5. Connection drop mid-transfer reports Interrupted and preserves old target (P0-TX-03).
        let conn_b_drop = connect_port(root.path(), "ssh:b-drop", 2222).await;
        let fs_b_drop = Arc::new(conn_b_drop.open_sftp().await.unwrap());
        let resolver_drop = FsResolver::new();
        resolver_drop.register_remote(
            HostId("ssh:a".into()),
            Arc::new(conn_a_ded.open_sftp().await.unwrap()),
        );
        resolver_drop.register_remote(HostId("ssh:b-drop".into()), fs_b_drop);

        let (drop_tx, drop_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if drop_rx.await.is_ok() {
                let _ = conn_b_drop.disconnect().await;
            }
        });
        let mut drop_option = Some(drop_tx);
        let req_drop = TransferRequest::new(
            "drop-job",
            Endpoint::remote(
                HostId("ssh:a".into()),
                RemotePath::from_posix(&src_raw).unwrap(),
            ),
            Endpoint::remote(
                HostId("ssh:b-drop".into()),
                RemotePath::from_posix(&keep_raw).unwrap(),
            ),
        );
        let drop_report = run_transfer(
            &resolver_drop,
            &config,
            &locks,
            req_drop,
            ConflictPolicy::Overwrite,
            &CancelToken::new(),
            move |prog| {
                if prog.bytes_transferred >= 2 * 1024 * 1024
                    && let Some(tx) = drop_option.take()
                {
                    let _ = tx.send(());
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(drop_report.state, JobState::Interrupted);
        assert_eq!(
            exec_stdout(&conn_b, &format!("cat {keep_raw}")).await,
            "intact-before-cancel"
        );

        // Clean up remote test artifacts.
        exec_stdout(&conn_a, &format!("rm -f {src_raw} && rmdir {work_dir}")).await;
        exec_stdout(
            &conn_b,
            &format!(
                "rm -f {dst_same_raw} {keep_raw} {work_dir}/.*.relay-*.part && rmdir {work_dir}"
            ),
        )
        .await;
        drop(pty);
    }
}
