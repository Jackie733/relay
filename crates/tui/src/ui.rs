use common::{FileKind, display_text};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};

use crate::app::{App, Workspace};

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
        Workspace::Terminals => placeholder(
            frame,
            body,
            "Terminals / planned",
            "No SSH terminal sessions.\n\nInteractive SSH PTY support is not implemented yet.",
        ),
        Workspace::Transfers => placeholder(
            frame,
            body,
            "Transfers / planned",
            "No transfer jobs.\n\nCopying, overwriting and deleting files are not implemented yet.",
        ),
    }
    frame.render_widget(
        Paragraph::new("[CONTROL]  Local browsing only  |  0 active transfers").fg(Color::Cyan),
        status,
    );
    let hint = if app.workspace == Workspace::Files && area.width < 100 {
        "Tab: pane  j/k: move  Enter: open  ?: help  q: quit"
    } else if app.workspace == Workspace::Files {
        "Tab: pane  j/k: move  Enter: open  Backspace: parent  r: reload  ?: help  q: quit"
    } else {
        "1-4: workspace  ?: help  q: quit"
    };
    frame.render_widget(Paragraph::new(hint).fg(Color::DarkGray), keys);
    if app.help {
        let width = body.width.min(58);
        let height = body.height.min(13);
        let dialog = Rect::new(
            body.x + (body.width - width) / 2,
            body.y + (body.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, dialog);
        frame.render_widget(Paragraph::new(
            "1 / 2 / 3 / 4   Switch workspace\nTab / Shift+Tab Switch file pane\nj / k, arrows   Move selection\ng / G           First / last entry\nEnter           Open directory\nBackspace       Parent directory\nr               Reload current directory\nq / Ctrl+C      Quit (close help first for q)\nEsc / ?         Close help"
        ).wrap(Wrap { trim: false }).block(Block::bordered().title(" Help / available keys ")), dialog);
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
        " {} Local {} ",
        if index == 0 { "Left" } else { "Right" },
        display_text(&panel.path.to_string_lossy())
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
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

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
}
