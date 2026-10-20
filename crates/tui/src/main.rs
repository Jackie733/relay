mod app;
mod services;
mod ui;

use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};

use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;

use app::App;
use services::Services;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Relay: {}", common::display_text(&error.to_string()));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut path = None;
    let mut positional = false;
    for arg in std::env::args_os().skip(1) {
        if !positional {
            if arg == "--help" || arg == "-h" {
                println!(
                    "Relay — SSH workspace (development scaffold)\n\nUsage: relay [DIRECTORY]\n\n  -h, --help     Show help\n  -V, --version  Show version\n\nOnly local read-only browsing is available. Press ? inside Relay for keys."
                );
                return Ok(());
            }
            if arg == "--version" || arg == "-V" {
                println!("relay {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            if arg == "--" {
                positional = true;
                continue;
            }
            if arg.to_string_lossy().starts_with('-') {
                return Err("unknown option; use relay --help".into());
            }
        }
        if path.replace(PathBuf::from(arg)).is_some() {
            return Err("expected at most one directory; use relay --help".into());
        }
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("interactive terminal required; use relay --help for usage".into());
    }
    let initial = tokio::fs::canonicalize(path.unwrap_or(std::env::current_dir()?)).await?;
    let mut app = App::new(initial);
    let mut services = Services::start();
    for panel in 0..2 {
        let command = app.load(panel, app.panels[panel].path.clone());
        services.commands.try_send(command)?;
    }
    let _restore = RestoreTerminal;
    let mut terminal = ratatui::try_init()?;
    let mut input = EventStream::new();
    while !app.quit {
        terminal.draw(|frame| ui::render(frame, &app))?;
        tokio::select! {
            event = input.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    if let Some(command) = app.key(key)
                        && let Err(error) = services.commands.try_send(command)
                    {
                        app.command_failed(&error.into_inner());
                    }
                }
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
            Some(event) = services.events.recv() => app.apply(event),
        }
    }
    Ok(())
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}
