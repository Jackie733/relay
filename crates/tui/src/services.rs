//! Service orchestration is independent of Ratatui, ready to move to app.

use common::{DirEntry, Endpoint, FsResolver, FsResult};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command {
    pub panel: usize,
    pub request: u64,
    pub endpoint: Endpoint,
}

#[derive(Debug)]
pub struct AppEvent {
    pub panel: usize,
    pub request: u64,
    pub endpoint: Endpoint,
    pub result: FsResult<Vec<DirEntry>>,
}

pub struct Services {
    pub resolver: FsResolver,
    pub commands: mpsc::Sender<Command>,
    pub events: mpsc::Receiver<AppEvent>,
    worker: JoinHandle<()>,
}

impl Services {
    pub fn start() -> Self {
        Self::with_resolver(FsResolver::new())
    }

    pub fn with_resolver(resolver: FsResolver) -> Self {
        let (commands, mut receiver) = mpsc::channel::<Command>(8);
        let (sender, events) = mpsc::channel(8);
        let worker_resolver = resolver.clone();
        let worker = tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                let result = match worker_resolver.resolve(command.endpoint.location()) {
                    Ok(fs) => fs.read_dir(command.endpoint.path()).await,
                    Err(error) => Err(error),
                };
                let event = AppEvent {
                    panel: command.panel,
                    request: command.request,
                    endpoint: command.endpoint,
                    result,
                };
                if sender.send(event).await.is_err() {
                    break;
                }
            }
        });
        Self {
            resolver,
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
            let endpoint = Endpoint::local(path);
            services
                .commands
                .send(Command {
                    panel: 1,
                    request,
                    endpoint: endpoint.clone(),
                })
                .await
                .unwrap();
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), services.events.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                (event.panel, event.request, event.endpoint),
                (1, request, endpoint)
            );
            assert_eq!(event.result.is_ok(), success);
        }
    }
}
