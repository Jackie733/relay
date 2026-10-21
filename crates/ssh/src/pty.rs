//! Long-lived interactive PTY channels. Terminal emulation stays in the UI.
use std::{collections::VecDeque, sync::Arc, time::Duration};

use russh::{Channel, ChannelMsg, ChannelReadHalf, ChannelWriteHalf, Pty, client};
use tokio::time::timeout;

use crate::connection::{ConnectError, Connection};

/// Terminal type requested from the server; Relay encodes keys as xterm.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// Character-cell size of a remote PTY. Both dimensions are at least 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl PtySize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

/// What runs on the PTY; tests use `Exec` to start a specific program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PtyCommand {
    Shell,
    Exec(String),
}

/// Remote output and lifecycle; stdout and stderr share the terminal stream.
#[derive(Debug, Eq, PartialEq)]
pub enum PtyEvent {
    Output(Vec<u8>),
    Exited(u32),
    Signalled(String),
    /// The channel ended; no further events follow.
    Closed,
}

pub struct PtyReader {
    reader: ChannelReadHalf,
    buffered: VecDeque<PtyEvent>,
    closed: bool,
}

impl PtyReader {
    /// Returns `None` after [`PtyEvent::Closed`] has been delivered.
    pub async fn next(&mut self) -> Option<PtyEvent> {
        if let Some(event) = self.buffered.pop_front() {
            return Some(event);
        }
        if self.closed {
            return None;
        }
        loop {
            let Some(message) = self.reader.wait().await else {
                self.closed = true;
                return Some(PtyEvent::Closed);
            };
            if let Some(event) = map_message(message) {
                if event == PtyEvent::Closed {
                    self.closed = true;
                }
                return Some(event);
            }
        }
    }
}

/// Cloneable sender half. Writes are ordered per clone owner, not globally.
#[derive(Clone)]
pub struct PtyWriter {
    writer: Arc<ChannelWriteHalf<client::Msg>>,
}

impl PtyWriter {
    pub async fn write(&self, bytes: &[u8]) -> Result<(), ConnectError> {
        self.writer.data(bytes).await.map_err(Into::into)
    }

    pub async fn resize(&self, size: PtySize) -> Result<(), ConnectError> {
        self.writer
            .window_change(size.cols.into(), size.rows.into(), 0, 0)
            .await
            .map_err(Into::into)
    }

    pub async fn close(&self) -> Result<(), ConnectError> {
        let _ = self.writer.eof().await;
        self.writer.close().await.map_err(Into::into)
    }
}

impl Connection {
    /// Requests `xterm-256color`, the initial size and then a shell/command.
    /// Both requests need an explicit server acknowledgement.
    pub async fn open_pty(
        &self,
        size: PtySize,
        command: PtyCommand,
    ) -> Result<(PtyReader, PtyWriter), ConnectError> {
        let mut channel = self.open_session().await?;
        let mut buffered = VecDeque::new();
        // DEL as erase matches what the Input Encoder sends for Backspace.
        let modes = [(Pty::VERASE, 0x7f), (Pty::IUTF8, 1)];
        channel
            .request_pty(
                true,
                DEFAULT_TERM,
                size.cols.into(),
                size.rows.into(),
                0,
                0,
                &modes,
            )
            .await?;
        acknowledged(
            &mut channel,
            &mut buffered,
            self.network_timeout(),
            "pty-req",
        )
        .await?;
        match &command {
            PtyCommand::Shell => channel.request_shell(true).await?,
            PtyCommand::Exec(command) => channel.exec(true, command.as_bytes()).await?,
        }
        acknowledged(&mut channel, &mut buffered, self.network_timeout(), "shell").await?;
        let (reader, writer) = channel.split();
        let closed = buffered.back() == Some(&PtyEvent::Closed);
        Ok((
            PtyReader {
                reader,
                buffered,
                closed,
            },
            PtyWriter {
                writer: Arc::new(writer),
            },
        ))
    }
}

async fn acknowledged(
    channel: &mut Channel<client::Msg>,
    buffered: &mut VecDeque<PtyEvent>,
    limit: Duration,
    request: &'static str,
) -> Result<(), ConnectError> {
    timeout(limit, async {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => return Ok(()),
                Some(ChannelMsg::Failure) => return Err(ConnectError::ChannelRequest(request)),
                None => return Err(ConnectError::ChannelRequest(request)),
                Some(message) => {
                    if let Some(event) = map_message(message) {
                        let closed = event == PtyEvent::Closed;
                        buffered.push_back(event);
                        if closed {
                            return Err(ConnectError::ChannelRequest(request));
                        }
                    }
                }
            }
        }
    })
    .await
    .map_err(|_| ConnectError::NetworkTimeout)?
}

fn map_message(message: ChannelMsg) -> Option<PtyEvent> {
    match message {
        ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
            Some(PtyEvent::Output(data.to_vec()))
        }
        ChannelMsg::ExitStatus { exit_status } => Some(PtyEvent::Exited(exit_status)),
        ChannelMsg::ExitSignal { signal_name, .. } => {
            Some(PtyEvent::Signalled(format!("{signal_name:?}")))
        }
        ChannelMsg::Close => Some(PtyEvent::Closed),
        _ => None,
    }
}
