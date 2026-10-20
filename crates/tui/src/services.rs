//! Service orchestration is independent of Ratatui, ready to move to app.

use std::path::PathBuf;

use common::{DirEntry, FileSystem, FsPath, FsResult, LocalFs};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Debug)]
pub struct Command {
    pub panel: usize,
    pub request: u64,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct AppEvent {
    pub panel: usize,
    pub request: u64,
    pub path: PathBuf,
    pub result: FsResult<Vec<DirEntry>>,
}

pub struct Services {
    pub commands: mpsc::Sender<Command>,
    pub events: mpsc::Receiver<AppEvent>,
    worker: JoinHandle<()>,
}

impl Services {
    pub fn start() -> Self {
        let (commands, mut receiver) = mpsc::channel::<Command>(8);
        let (sender, events) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                let result = LocalFs.read_dir(&FsPath::Local(command.path.clone())).await;
                let event = AppEvent {
                    panel: command.panel,
                    request: command.request,
                    path: command.path,
                    result,
                };
                if sender.send(event).await.is_err() {
                    break;
                }
            }
        });
        Self {
            commands,
            events,
            worker,
        }
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn directory_commands_return_correlated_results_and_errors() {
        let root = tempfile::tempdir().unwrap();
        let mut services = Services::start();
        for (request, path, success) in [
            (1, root.path().to_path_buf(), true),
            (2, root.path().join("missing"), false),
        ] {
            services
                .commands
                .send(Command {
                    panel: 1,
                    request,
                    path: path.clone(),
                })
                .await
                .unwrap();
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), services.events.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!((event.panel, event.request, event.path), (1, request, path));
            assert_eq!(event.result.is_ok(), success);
        }
    }
}
