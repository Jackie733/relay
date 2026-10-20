use std::path::PathBuf;

use common::{DirEntry, FileKind, FsPath, display_text};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::services::{AppEvent, Command};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Workspace {
    #[default]
    Hosts,
    Files,
    Terminals,
    Transfers,
}

pub struct Panel {
    pub path: PathBuf,
    pub entries: Vec<DirEntry>,
    pub selected: usize,
    pub pending: Option<u64>,
    pub error: Option<String>,
}

impl Panel {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
            selected: 0,
            pending: None,
            error: None,
        }
    }
}

pub struct App {
    pub workspace: Workspace,
    pub panels: [Panel; 2],
    pub focus: usize,
    pub help: bool,
    pub quit: bool,
    request: u64,
}

impl App {
    pub fn new(path: PathBuf) -> Self {
        Self {
            workspace: Workspace::default(),
            panels: [Panel::new(path.clone()), Panel::new(path)],
            focus: 0,
            help: false,
            quit: false,
            request: 0,
        }
    }

    pub fn load(&mut self, panel: usize, path: PathBuf) -> Command {
        self.request += 1;
        self.panels[panel].pending = Some(self.request);
        self.panels[panel].error = None;
        Command {
            panel,
            request: self.request,
            path,
        }
    }

    pub fn apply(&mut self, event: AppEvent) {
        let Some(panel) = self.panels.get_mut(event.panel) else {
            return;
        };
        if panel.pending != Some(event.request) {
            return;
        }
        panel.pending = None;
        match event.result {
            Ok(entries) => {
                panel.path = event.path;
                panel.entries = entries;
                panel.selected = 0;
                panel.error = None;
            }
            Err(error) => {
                panel.error = Some(format!(
                    "{}: {}",
                    display_text(&event.path.to_string_lossy()),
                    display_text(&error.to_string())
                ))
            }
        }
    }

    pub fn command_failed(&mut self, command: &Command) {
        let panel = &mut self.panels[command.panel];
        if panel.pending == Some(command.request) {
            panel.pending = None;
            panel.error = Some("Directory queue is busy. Press r to retry.".into());
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return None;
        }
        if self.help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.help = false;
            }
            return None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('1') => self.workspace = Workspace::Hosts,
            KeyCode::Char('2') => self.workspace = Workspace::Files,
            KeyCode::Char('3') => self.workspace = Workspace::Terminals,
            KeyCode::Char('4') => self.workspace = Workspace::Transfers,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') => self.quit = true,
            _ if self.workspace == Workspace::Files => return self.file_key(key.code),
            _ => {}
        }
        None
    }

    fn file_key(&mut self, code: KeyCode) -> Option<Command> {
        let panel = &mut self.panels[self.focus];
        match code {
            KeyCode::Tab | KeyCode::BackTab => self.focus = 1 - self.focus,
            KeyCode::Down | KeyCode::Char('j') => {
                panel.selected = (panel.selected + 1).min(panel.entries.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => panel.selected = panel.selected.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => panel.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                panel.selected = panel.entries.len().saturating_sub(1)
            }
            KeyCode::Enter if panel.pending.is_none() => {
                if let Some(entry) = panel.entries.get(panel.selected)
                    && entry.kind == FileKind::Directory
                    && let FsPath::Local(path) = &entry.path
                {
                    let path = path.clone();
                    return Some(self.load(self.focus, path));
                }
            }
            KeyCode::Backspace if panel.pending.is_none() => {
                if let Some(parent) = panel.path.parent() {
                    let path = parent.to_path_buf();
                    return Some(self.load(self.focus, path));
                }
            }
            KeyCode::Char('r') if panel.pending.is_none() => {
                let path = panel.path.clone();
                return Some(self.load(self.focus, path));
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::FsError;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn keyboard_navigation_and_help_are_scoped() {
        let mut app = App::new("/".into());
        app.key(key(KeyCode::Char('2')));
        app.key(key(KeyCode::Tab));
        assert_eq!((app.workspace, app.focus), (Workspace::Files, 1));
        app.key(key(KeyCode::Char('?')));
        app.key(key(KeyCode::Char('3')));
        assert_eq!(app.workspace, Workspace::Files);
        app.key(key(KeyCode::Esc));
        app.key(key(KeyCode::Char('3')));
        assert_eq!(app.workspace, Workspace::Terminals);
        app.key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn stale_results_and_failed_navigation_preserve_the_current_directory() {
        let mut app = App::new("/original".into());
        let stale = app.load(0, "/old".into());
        let fresh = app.load(0, "/new".into());
        app.apply(AppEvent {
            panel: 0,
            request: stale.request,
            path: stale.path,
            result: Ok(vec![]),
        });
        assert_eq!(app.panels[0].pending, Some(fresh.request));
        app.apply(AppEvent {
            panel: 0,
            request: fresh.request,
            path: fresh.path,
            result: Err(FsError::UnsafePath),
        });
        assert_eq!(app.panels[0].path, PathBuf::from("/original"));
        assert!(app.panels[0].error.is_some());
        assert!(app.panels[0].pending.is_none());
    }

    #[test]
    fn file_navigation_only_opens_directories() {
        let mut app = App::new("/tmp".into());
        app.workspace = Workspace::Files;
        app.panels[0].entries.push(DirEntry {
            name: "child".into(),
            path: FsPath::Local("/tmp/child".into()),
            kind: FileKind::Directory,
        });
        let command = app.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(command.path, PathBuf::from("/tmp/child"));
        assert!(app.key(key(KeyCode::Enter)).is_none());
        app.apply(AppEvent {
            panel: command.panel,
            request: command.request,
            path: command.path,
            result: Ok(vec![]),
        });
        let parent = app.key(key(KeyCode::Backspace)).unwrap();
        assert_eq!(parent.path, PathBuf::from("/tmp"));
    }
}
