use common::{FileKind, display_text};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};

use crate::{
    app::{App, InputMode, Workspace},
    session::TerminalSession,
};

/// Usable character grid inside the Terminals border for a full-window `area`.
pub fn terminal_size(area: Rect) -> (u16, u16) {
    (
        area.width.saturating_sub(2).max(1),
        area.height.saturating_sub(7).max(1),
    )
}

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < 60 || area.height < 16 {
        frame.render_widget(
            Paragraph::new("Relay: resize terminal to at least 60 x 16. q: quit")
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let [header, body, status, keys] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Tabs::new(["1 Hosts", "2 Files", "3 Terminals", "4 Transfers"])
            .select(app.workspace as usize)
            .highlight_style(Style::new().fg(Color::Cyan).bold())
            .block(Block::bordered().title(" Relay / development scaffold ")),
        header,
    );
    match app.workspace {
        Workspace::Hosts => placeholder(
            frame,
            body,
            "Hosts",
            "No hosts configured.\n\nSSH connections and host-key verification are planned for P0.\nPress 2 to browse local files, or ? for available keys.",
        ),
        Workspace::Files => {
            if body.width < 100 {
                panel(frame, body, app, app.focus);
            } else {
                let areas = Layout::horizontal([Constraint::Percentage(50); 2]).split(body);
                panel(frame, areas[0], app, 0);
                panel(frame, areas[1], app, 1);
            }
        }
        Workspace::Terminals => {
            if let Some(session) = app.active_terminal() {
                terminal(frame, body, app, session);
            } else {
                placeholder(
                    frame,
                    body,
                    "Terminals / planned",
                    "No SSH terminal sessions.\n\nInteractive SSH PTY support is not implemented yet.",
                );
            }
        }
        Workspace::Transfers => placeholder(
            frame,
            body,
            "Transfers / planned",
            "No transfer jobs.\n\nCopying, overwriting and deleting files are not implemented yet.",
        ),
    }
    let (status_text, status_color) = status_line(app);
    frame.render_widget(Paragraph::new(status_text).fg(status_color), status);
    frame.render_widget(
        Paragraph::new(key_hint(app, area)).fg(Color::DarkGray),
        keys,
    );
    if app.help {
        let width = body.width.min(62);
        let height = body.height.min(15);
        let dialog = Rect::new(
            body.x + (body.width - width) / 2,
            body.y + (body.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, dialog);
        frame.render_widget(
            Paragraph::new(format!(
                "1 / 2 / 3 / 4   Switch workspace\nTab / Shift+Tab Switch file pane\nj / k, arrows   Move selection\ng / G           First / last entry\nEnter           Open directory / focus terminal\nBackspace       Parent directory\nr               Reload current directory\n[ / ]           Previous / next terminal tab\nPgUp / PgDn     Scroll terminal history (Control mode)\n{}   Return from terminal to Control\nq / Ctrl+C      Quit (Control mode)\nEsc / ?         Close help",
                app.escape_label()
            ))
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Help / available keys ")),
            dialog,
        );
    }
}

fn status_line(app: &App) -> (String, Color) {
    let detail = if let Some(notice) = &app.notice {
        display_text(notice)
    } else if let Some(session) = app.active_terminal() {
        let state = session.state();
        format!(
            "{} ({})  |  0 active transfers",
            display_text(&session.label),
            state.status.describe()
        )
    } else {
        "Local browsing only  |  0 active transfers".into()
    };
    match app.mode {
        InputMode::Control => (format!("[CONTROL]  {detail}"), Color::Cyan),
        InputMode::TerminalInput => (format!("[TERMINAL]  {detail}"), Color::Green),
    }
}

fn key_hint(app: &App, area: Rect) -> String {
    if app.mode == InputMode::TerminalInput {
        return format!(
            "{}: Relay control  (press twice to send to terminal)",
            app.escape_label()
        );
    }
    match app.workspace {
        Workspace::Files if area.width < 100 => {
            "Tab: pane  j/k: move  Enter: open  ?: help  q: quit".into()
        }
        Workspace::Files => {
            "Tab: pane  j/k: move  Enter: open  Backspace: parent  r: reload  ?: help  q: quit"
                .into()
        }
        Workspace::Terminals if !app.terminals.is_empty() => {
            "Enter: terminal input  [/]: tab  PgUp/PgDn: scrollback  1-4: workspace  ?: help  q: quit"
                .into()
        }
        _ => "1-4: workspace  ?: help  q: quit".into(),
    }
}

fn terminal(frame: &mut Frame, area: Rect, app: &App, session: &TerminalSession) {
    let state = session.state();
    let responder = state.emulator.responder();
    let screen = state.emulator.screen();
    let mut title = format!(
        " [{}/{}] {} ",
        app.active + 1,
        app.terminals.len(),
        display_text(&session.label)
    );
    if let Some(window_title) = &responder.title
        && !window_title.is_empty()
    {
        title.push_str(&format!("— {window_title} "));
    }
    if responder.bell {
        title.push_str("[BELL] ");
    }
    if screen.scrollback() > 0 {
        title.push_str(&format!("[+{} lines] ", screen.scrollback()));
    }
    if !state.status.is_running() {
        title.push_str(&format!("[{}] ", state.status.describe()));
    }
    let border = if app.mode == InputMode::TerminalInput {
        Color::Green
    } else {
        Color::Cyan
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let buf = frame.buffer_mut();
    for row in 0..inner.height {
        for col in 0..inner.width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let style = cell_style(cell);
            let target = &mut buf[(inner.x + col, inner.y + row)];
            if cell.is_wide_continuation() {
                target.reset();
                target.set_style(style);
            } else if cell.is_wide() && col + 1 >= inner.width {
                target.set_symbol(" ").set_style(style);
            } else if cell.has_contents() {
                target.set_symbol(cell.contents()).set_style(style);
            } else {
                target.set_symbol(" ").set_style(style);
            }
        }
    }
    if !screen.hide_cursor() && screen.scrollback() == 0 && !app.help {
        let (row, col) = screen.cursor_position();
        if row < inner.height && col < inner.width {
            let x = inner.x + col;
            let y = inner.y + row;
            buf[(x, y)].modifier ^= Modifier::REVERSED;
            if app.mode == InputMode::TerminalInput {
                frame.set_cursor_position((x, y));
            }
        }
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut modifier = Modifier::empty();
    if cell.bold() {
        modifier |= Modifier::BOLD;
    }
    if cell.dim() {
        modifier |= Modifier::DIM;
    }
    if cell.italic() {
        modifier |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifier |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifier |= Modifier::REVERSED;
    }
    Style::reset()
        .fg(map_color(cell.fgcolor()))
        .bg(map_color(cell.bgcolor()))
        .add_modifier(modifier)
}

fn map_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn placeholder(frame: &mut Frame, area: Rect, title: &str, text: &str) {
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" {title} "))),
        area,
    );
}

fn panel(frame: &mut Frame, area: Rect, app: &App, index: usize) {
    let panel = &app.panels[index];
    let color = if app.focus == index {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let title = format!(
        " {} {} {} ",
        if index == 0 { "Left" } else { "Right" },
        display_text(panel.location.label()),
        panel.path.display_name()
    );
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(color));
    if panel.pending.is_some() {
        frame.render_widget(Paragraph::new("Loading directory...").block(block), area);
    } else if let Some(error) = &panel.error {
        frame.render_widget(
            Paragraph::new(format!(
                "{error}\n\nPress r to reload, Backspace for parent."
            ))
            .wrap(Wrap { trim: false })
            .fg(Color::Red)
            .block(block),
            area,
        );
    } else if panel.entries.is_empty() {
        frame.render_widget(Paragraph::new("Empty directory").block(block), area);
    } else {
        let items = panel.entries.iter().map(|entry| {
            let prefix = match entry.kind {
                FileKind::Directory => "[dir] ",
                FileKind::Symlink => "[link]",
                FileKind::File => "      ",
                FileKind::Other => "[other]",
            };
            ListItem::new(format!("{prefix} {}", entry.display_name()))
        });
        let mut selection = ListState::default().with_selected(Some(panel.selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol("> ")
                .highlight_style(Style::new().fg(Color::Black).bg(color)),
            area,
            &mut selection,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use ssh::PtySize;
    use tokio::sync::Notify;

    #[test]
    fn all_views_render_at_supported_and_tiny_sizes() {
        let mut app = App::new("/tmp/中文".into());
        for (width, height) in [(120, 40), (80, 24), (60, 16), (20, 5), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for workspace in [
                Workspace::Hosts,
                Workspace::Files,
                Workspace::Terminals,
                Workspace::Transfers,
            ] {
                app.workspace = workspace;
                for help in [false, true] {
                    app.help = help;
                    terminal.draw(|frame| render(frame, &app)).unwrap();
                    let screen = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    if width >= 60 {
                        assert!(
                            screen.contains("[CONTROL]"),
                            "{width}x{height}, {workspace:?}, help={help}"
                        );
                        if workspace == Workspace::Files && !help {
                            assert!(screen.contains("Left Local"));
                            assert_eq!(screen.contains("Right Local"), width >= 100);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn terminal_view_renders_cjk_truecolor_and_cursor() {
        let mut app = App::new("/tmp".into());
        let (cols, rows) = terminal_size(Rect::new(0, 0, 80, 24));
        let (session, io) = TerminalSession::new(
            "relay@fixture:2221".into(),
            PtySize::new(cols, rows),
            Arc::new(Notify::new()),
        );
        io.state.lock().unwrap().emulator.process(
            b"\x1b]2;vim\x07\x1b[38;2;12;34;56m\x1b[48;2;65;43;21m\xe4\xb8\xad\xe6\x96\x87A",
        );
        app.terminals.push(session);
        app.workspace = Workspace::Terminals;
        app.mode = InputMode::TerminalInput;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let first = &buffer[(1, 4)];
        assert_eq!(first.symbol(), "中");
        assert_eq!(first.fg, Color::Rgb(12, 34, 56));
        assert_eq!(first.bg, Color::Rgb(65, 43, 21));
        assert_eq!(buffer[(3, 4)].symbol(), "文");
        assert_eq!(buffer[(5, 4)].symbol(), "A");
        assert!(buffer[(6, 4)].modifier.contains(Modifier::REVERSED));
        let screen = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("[TERMINAL]"));
        assert!(screen.contains("relay@fixture:2221"));
    }
}
