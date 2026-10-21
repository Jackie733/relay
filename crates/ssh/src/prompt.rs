//! UI-independent, bounded prompts. No response is ever an approval.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use secrecy::SecretString;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{
    HostProfile,
    trust::{HostKey, TrustReport},
};

#[derive(Clone, Debug)]
pub enum PromptKind {
    HostKey {
        key: HostKey,
        report: TrustReport,
        peer_address: SocketAddr,
    },
    KeyPassphrase {
        identity_file: PathBuf,
    },
}

#[derive(Debug)]
pub enum PromptResponse {
    Reject,
    Trust,
    Passphrase(SecretString),
}

#[derive(Debug)]
pub struct Prompt {
    pub host: HostProfile,
    pub kind: PromptKind,
    pub response: oneshot::Sender<PromptResponse>,
}

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("SSH confirmation cancelled or rejected")]
    Rejected,
    #[error("SSH confirmation timed out")]
    TimedOut,
    #[error("SSH confirmation interface closed")]
    Closed,
}

#[derive(Clone)]
pub struct PromptBroker {
    sender: mpsc::Sender<Prompt>,
}

impl PromptBroker {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Prompt>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self { sender }, receiver)
    }

    pub async fn request(
        &self,
        host: HostProfile,
        kind: PromptKind,
        timeout: Duration,
    ) -> Result<PromptResponse, PromptError> {
        tokio::time::timeout(timeout, async {
            let (response, receiver) = oneshot::channel();
            self.sender
                .send(Prompt {
                    host,
                    kind,
                    response,
                })
                .await
                .map_err(|_| PromptError::Closed)?;
            match receiver.await.map_err(|_| PromptError::Closed)? {
                PromptResponse::Reject => Err(PromptError::Rejected),
                response => Ok(response),
            }
        })
        .await
        .map_err(|_| PromptError::TimedOut)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::HostId;

    fn host(name: &str) -> HostProfile {
        HostProfile {
            id: HostId(name.into()),
            hostname: name.into(),
            port: 22,
            username: "user".into(),
        }
    }

    #[tokio::test]
    async fn concurrent_replies_are_correlated_and_secrets_redacted() {
        let (broker, mut receiver) = PromptBroker::channel(2);
        let a = broker.clone();
        let a = tokio::spawn(async move {
            a.request(
                host("a"),
                PromptKind::KeyPassphrase {
                    identity_file: "key-a".into(),
                },
                Duration::from_secs(5),
            )
            .await
        });
        let b = tokio::spawn(async move {
            broker
                .request(
                    host("b"),
                    PromptKind::KeyPassphrase {
                        identity_file: "key-b".into(),
                    },
                    Duration::from_secs(5),
                )
                .await
        });
        for _ in 0..2 {
            let prompt = receiver.recv().await.unwrap();
            let response = if prompt.host.hostname == "a" {
                PromptResponse::Reject
            } else {
                PromptResponse::Passphrase(SecretString::from("private-value"))
            };
            assert!(!format!("{response:?}").contains("private-value"));
            prompt.response.send(response).unwrap();
        }
        assert!(matches!(a.await.unwrap(), Err(PromptError::Rejected)));
        assert!(matches!(
            b.await.unwrap(),
            Ok(PromptResponse::Passphrase(_))
        ));
    }

    #[tokio::test]
    async fn deadline_covers_full_queue_and_late_approval_is_rejected() {
        let (broker, mut receiver) = PromptBroker::channel(1);
        let kind = PromptKind::KeyPassphrase {
            identity_file: "key".into(),
        };
        assert!(matches!(
            broker
                .request(host("a"), kind.clone(), Duration::from_millis(10))
                .await,
            Err(PromptError::TimedOut)
        ));
        assert!(matches!(
            broker
                .request(host("b"), kind, Duration::from_millis(10))
                .await,
            Err(PromptError::TimedOut)
        ));
        let late = receiver.recv().await.unwrap();
        assert!(late.response.send(PromptResponse::Trust).is_err());
        drop(receiver);
        assert!(matches!(
            broker
                .request(
                    host("c"),
                    PromptKind::KeyPassphrase {
                        identity_file: "key".into()
                    },
                    Duration::from_secs(1)
                )
                .await,
            Err(PromptError::Closed)
        ));
    }
}
