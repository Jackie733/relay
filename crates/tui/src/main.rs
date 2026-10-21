mod app;
mod cli;
mod services;
mod session;
mod terminal;
mod ui;

use std::{
    io::{self, IsTerminal},
    sync::Arc,
    time::Duration,
};

use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
        KeyModifiers,
    },
    execute,
};
use futures_util::StreamExt;
use ratatui::layout::Rect;
use ssh::{PtyCommand, PtySize};
use tokio::{
    sync::Notify,
    time::{Instant, MissedTickBehavior},
};

use app::{App, Workspace};
use services::Services;
use session::TerminalSession;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Relay: {}", common::display_text(&error.to_string()));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = cli::parse(std::env::args_os().skip(1))?;
    if args.help {
        println!("{}", cli::HELP);
        return Ok(());
    }
    if args.version {
        println!("relay {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("interactive terminal required; use relay --help for usage".into());
    }
    let initial = tokio::fs::canonicalize(args.path.unwrap_or(std::env::current_dir()?)).await?;
    let connection = if let Some(target) = args.ssh {
        if !io::stderr().is_terminal() {
            return Err("interactive stderr required for SSH prompts".into());
        }
        let (options, warnings) = cli::connect_options(target).await?;
        for warning in warnings {
            eprintln!("{}", common::display_text(&warning));
        }
        Some(ssh::tty::connect(options).await?)
    } else {
        None
    };

    let mut app = App::new(initial);
    app.escape_keys = args.escape_keys;
    let mut services = Services::start();
    if let Some(connection) = &connection {
        let remote_fs = connection.open_sftp().await?;
        let dot = common::FsPath::Remote(common::RemotePath::from_posix(".")?);
        let remote_path = common::FileSystem::canonicalize(&remote_fs, &dot)
            .await
            .unwrap_or_else(|_| common::FsPath::Remote(common::RemotePath::root()));
        let host_id = connection.host().id.clone();
        services
            .resolver
            .register_remote(host_id.clone(), Arc::new(remote_fs));
        let remote_endpoint =
            common::Endpoint::new(common::Location::Remote(host_id), remote_path)?;
        app.panels[1].location = remote_endpoint.location().clone();
        app.panels[1].path = remote_endpoint.path().clone();
    }
    for panel in 0..2 {
        let command = app.load_endpoint(panel, app.panels[panel].endpoint());
        services.commands.try_send(command)?;
    }

    let _restore = RestoreTerminal;
    let mut terminal = ratatui::try_init()?;
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let updated = Arc::new(Notify::new());

    if let Some(connection) = &connection {
        let size = terminal.size()?;
        let (cols, rows) = ui::terminal_size(Rect::new(0, 0, size.width, size.height));
        let pty_size = PtySize::new(cols, rows);
        let (reader, writer) = connection.open_pty(pty_size, PtyCommand::Shell).await?;
        let host = connection.host();
        let label = format!("{}@{}:{}", host.username, host.hostname, host.port);
        app.terminals.push(TerminalSession::start_ssh(
            label,
            pty_size,
            updated.clone(),
            reader,
            writer,
        ));
        app.workspace = Workspace::Terminals;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut pty_dirty = false;
    let mut last_draw = Instant::now()
        .checked_sub(FRAME_INTERVAL)
        .unwrap_or_else(Instant::now);

    while !app.quit {
        if dirty || (pty_dirty && last_draw.elapsed() >= FRAME_INTERVAL) {
            terminal.draw(|frame| ui::render(frame, &app))?;
            dirty = false;
            pty_dirty = false;
            last_draw = Instant::now();
        }
        tokio::select! {
            event = input.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    if let Some(command) = app.key(key)
                        && let Err(error) = services.commands.try_send(command)
                    {
                        app.command_failed(&error.into_inner());
                    }
                    dirty = true;
                }
                Some(Ok(Event::Paste(text))) => {
                    app.paste(&text);
                    dirty = true;
                }
                Some(Ok(Event::Resize(width, height))) => {
                    let (cols, rows) = ui::terminal_size(Rect::new(0, 0, width, height));
                    for session in &app.terminals {
                        session.resize(cols, rows);
                    }
                    dirty = true;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
            Some(event) = services.events.recv() => {
                app.apply(event);
                dirty = true;
            }
            () = updated.notified() => {
                app.sessions_updated();
                pty_dirty = true;
            }
            _ = ticker.tick(), if pty_dirty => {}
        }
    }
    drop(app);
    if let Some(connection) = connection {
        let _ = connection.disconnect().await;
    }
    Ok(())
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}
