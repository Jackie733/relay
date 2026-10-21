use std::{borrow::Cow, io::Read, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use russh::{
    Channel, Disconnect, client,
    keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate},
};
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox};
use thiserror::Error;
use tokio::{net::TcpStream, sync::watch, time::timeout};

use crate::{
    HostProfile,
    prompt::{PromptBroker, PromptError, PromptKind, PromptResponse},
    trust::{HostKey, HostKeyPolicy, TrustDecision, TrustError, TrustStore},
};

#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub host: HostProfile,
    pub host_key_alias: Option<String>,
    pub trust: TrustStore,
    pub host_key_policy: HostKeyPolicy,
    /// P0 supports OpenSSH-format identity files, including encrypted keys.
    pub identity_files: Vec<PathBuf>,
    pub network_timeout: Duration,
    pub prompt_timeout: Duration,
    pub keepalive_interval: Option<Duration>,
    pub keepalive_max: usize,
    pub host_key_algorithms: Option<Vec<String>>,
}

impl ConnectOptions {
    pub fn new(host: HostProfile, trust: TrustStore, identity_file: PathBuf) -> Self {
        Self {
            host,
            trust,
            host_key_alias: None,
            host_key_policy: HostKeyPolicy::Ask,
            identity_files: vec![identity_file],
            network_timeout: Duration::from_secs(15),
            prompt_timeout: Duration::from_secs(120),
            keepalive_interval: None,
            keepalive_max: 3,
            host_key_algorithms: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("SSH network connection failed: {0}")]
    Network(#[source] std::io::Error),
    #[error("SSH network operation timed out")]
    NetworkTimeout,
    #[error("SSH protocol failed: {0}")]
    Protocol(#[source] russh::Error),
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error("unknown or new-algorithm host key rejected by strict checking")]
    UnknownHostKey,
    #[error("SSH host certificates are not supported")]
    HostCertificateUnsupported,
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error("SSH connection cancelled")]
    Cancelled,
    #[error("cannot read identity file or decode an OpenSSH private key")]
    Identity,
    #[error("private-key passphrase is incorrect or unsupported")]
    KeyDecryption,
    #[error("no supported identity; P0 requires an Ed25519 or ECDSA OpenSSH private key")]
    UnsupportedIdentity,
    #[error("configured host-key algorithms have no supported Ed25519/ECDSA algorithm")]
    UnsupportedHostAlgorithms,
    #[error("SSH public-key authentication failed")]
    Authentication,
    #[error("SSH blocking worker failed")]
    Worker,
    #[error("SSH user, identities and nonzero timeouts must be configured")]
    InvalidOptions,
    #[error("SSH server rejected the {0} channel request")]
    ChannelRequest(&'static str),
}

impl From<russh::Error> for ConnectError {
    fn from(error: russh::Error) -> Self {
        match error {
            russh::Error::IO(error) => Self::Network(error),
            error => Self::Protocol(error),
        }
    }
}

pub struct Connection {
    host: HostProfile,
    session: client::Handle<VerifiedHost>,
    network_timeout: Duration,
    // Dropping this sender also cancels a prompt inside russh's session task.
    _lifetime: watch::Sender<()>,
}

impl Connection {
    pub async fn connect(
        options: ConnectOptions,
        prompts: PromptBroker,
    ) -> Result<Self, ConnectError> {
        if options.host.username.is_empty()
            || options.identity_files.is_empty()
            || options.network_timeout.is_zero()
            || options.prompt_timeout.is_zero()
        {
            return Err(ConnectError::InvalidOptions);
        }
        let name = TrustStore::lookup_name(
            options
                .host_key_alias
                .as_deref()
                .unwrap_or(&options.host.hostname),
            options.host.port,
        )?;
        // Validate the actual endpoint even if an alias supplies the trust name.
        TrustStore::lookup_name(&options.host.hostname, options.host.port)?;
        let store = options.trust.clone();
        let lookup = name.clone();
        let known = tokio::task::spawn_blocking(move || store.known_algorithms(&lookup))
            .await
            .map_err(|_| ConnectError::Worker)??;
        let mut config = client::Config {
            nodelay: true,
            keepalive_interval: options.keepalive_interval,
            keepalive_max: options.keepalive_max,
            ..Default::default()
        };
        let mut algorithms = config.preferred.key.to_vec();
        algorithms.retain(|algorithm| {
            matches!(
                algorithm,
                russh::keys::Algorithm::Ed25519 | russh::keys::Algorithm::Ecdsa { .. }
            ) && options
                .host_key_algorithms
                .as_ref()
                .is_none_or(|names| names.iter().any(|name| name == algorithm.as_str()))
        });
        if algorithms.is_empty() {
            return Err(ConnectError::UnsupportedHostAlgorithms);
        }
        // RSA SHA-2 signature variants share the ssh-rsa stored public key.
        algorithms.sort_by_key(|algorithm| {
            let name = if matches!(algorithm, russh::keys::Algorithm::Rsa { .. }) {
                "ssh-rsa"
            } else {
                algorithm.as_str()
            };
            !known.iter().any(|known| known == name)
        });
        config.preferred.key = Cow::Owned(algorithms);
        let stream = timeout(
            options.network_timeout,
            TcpStream::connect((options.host.hostname.as_str(), options.host.port)),
        )
        .await
        .map_err(|_| ConnectError::NetworkTimeout)?
        .map_err(ConnectError::Network)?;
        stream.set_nodelay(true).map_err(ConnectError::Network)?;
        let peer_address = stream.peer_addr().map_err(ConnectError::Network)?;
        let (lifetime, cancelled) = watch::channel(());
        let handler = VerifiedHost {
            options: options.clone(),
            name,
            prompts: prompts.clone(),
            cancelled,
            peer_address,
        };
        // The handshake budget allows a complete, independently bounded prompt.
        let handshake_timeout = options
            .network_timeout
            .saturating_add(options.prompt_timeout);
        let mut session = timeout(
            handshake_timeout,
            client::connect_stream(Arc::new(config), stream, handler),
        )
        .await
        .map_err(|_| ConnectError::NetworkTimeout)??;
        let result = authenticate(&mut session, &options, &prompts).await;
        if let Err(error) = result {
            let _ = timeout(
                options.network_timeout,
                session.disconnect(Disconnect::ByApplication, "", "en"),
            )
            .await;
            return Err(error);
        }
        Ok(Self {
            host: options.host,
            session,
            network_timeout: options.network_timeout,
            _lifetime: lifetime,
        })
    }

    pub fn host(&self) -> &HostProfile {
        &self.host
    }

    pub fn is_closed(&self) -> bool {
        self.session.is_closed()
    }

    pub(crate) fn network_timeout(&self) -> Duration {
        self.network_timeout
    }

    pub async fn open_session(&self) -> Result<Channel<client::Msg>, ConnectError> {
        timeout(self.network_timeout, self.session.channel_open_session())
            .await
            .map_err(|_| ConnectError::NetworkTimeout)?
            .map_err(Into::into)
    }

    pub async fn disconnect(self) -> Result<(), ConnectError> {
        timeout(
            self.network_timeout,
            self.session.disconnect(Disconnect::ByApplication, "", "en"),
        )
        .await
        .map_err(|_| ConnectError::NetworkTimeout)?
        .map_err(Into::into)
    }
}

struct VerifiedHost {
    options: ConnectOptions,
    name: String,
    prompts: PromptBroker,
    cancelled: watch::Receiver<()>,
    peer_address: SocketAddr,
}

impl client::Handler for VerifiedHost {
    type Error = ConnectError;

    async fn check_server_key(
        &mut self,
        presented: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        if presented.certificate().is_some() {
            return Err(ConnectError::HostCertificateUnsupported);
        }
        let key = HostKey::from_public_key(&presented.public_key())?;
        let options = self.options.clone();
        let name = self.name.clone();
        let prompts = self.prompts.clone();
        tokio::select! {
            biased;
            _ = self.cancelled.changed() => Err(ConnectError::Cancelled),
            result = verify_host(options, name, key, self.peer_address, prompts) => result,
        }
    }
}

async fn verify_host(
    options: ConnectOptions,
    name: String,
    key: HostKey,
    peer_address: SocketAddr,
    prompts: PromptBroker,
) -> Result<bool, ConnectError> {
    let store = options.trust.clone();
    let lookup = name.clone();
    let candidate = key.clone();
    let report = tokio::task::spawn_blocking(move || store.assess(&lookup, &candidate))
        .await
        .map_err(|_| ConnectError::Worker)??;
    match report.decision {
        TrustDecision::Trusted => return Ok(true),
        TrustDecision::Changed => return Err(TrustError::Changed.into()),
        TrustDecision::Revoked => return Err(TrustError::Revoked.into()),
        _ if options.host_key_policy == HostKeyPolicy::Strict => {
            return Err(ConnectError::UnknownHostKey);
        }
        _ => {}
    }
    let response = prompts
        .request(
            options.host,
            PromptKind::HostKey {
                key: key.clone(),
                report,
                peer_address,
            },
            options.prompt_timeout,
        )
        .await?;
    if !matches!(response, PromptResponse::Trust) {
        return Err(PromptError::Rejected.into());
    }
    tokio::task::spawn_blocking(move || options.trust.remember(&name, &key))
        .await
        .map_err(|_| ConnectError::Worker)??;
    Ok(true)
}

async fn authenticate(
    session: &mut client::Handle<VerifiedHost>,
    options: &ConnectOptions,
    prompts: &PromptBroker,
) -> Result<(), ConnectError> {
    let mut attempted = false;
    let mut unsupported = false;
    for path in &options.identity_files {
        let key_path = path.clone();
        let key = tokio::task::spawn_blocking(move || {
            let file = match std::fs::File::open(key_path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(ConnectError::Identity),
            };
            let mut secret = SecretBox::new(Box::new(Vec::new()));
            file.take(64 * 1024 + 1)
                .read_to_end(secret.expose_secret_mut())
                .map_err(|_| ConnectError::Identity)?;
            if secret.expose_secret().len() > 64 * 1024 {
                return Err(ConnectError::Identity);
            }
            PrivateKey::from_openssh(secret.expose_secret())
                .map(Some)
                .map_err(|_| ConnectError::Identity)
        })
        .await
        .map_err(|_| ConnectError::Worker)??;
        let Some(key) = key else {
            continue;
        };
        if !matches!(
            key.algorithm(),
            russh::keys::Algorithm::Ed25519 | russh::keys::Algorithm::Ecdsa { .. }
        ) {
            unsupported = true;
            continue;
        }
        let key = if key.is_encrypted() {
            let response = prompts
                .request(
                    options.host.clone(),
                    PromptKind::KeyPassphrase {
                        identity_file: path.clone(),
                    },
                    options.prompt_timeout,
                )
                .await?;
            let PromptResponse::Passphrase(passphrase) = response else {
                return Err(PromptError::Rejected.into());
            };
            tokio::task::spawn_blocking(move || {
                key.decrypt(passphrase.expose_secret())
                    .map_err(|_| ConnectError::KeyDecryption)
            })
            .await
            .map_err(|_| ConnectError::Worker)??
        } else {
            key
        };
        attempted = true;
        let response = timeout(
            options.network_timeout,
            session.authenticate_publickey(
                options.host.username.clone(),
                PrivateKeyWithHashAlg::new(Arc::new(key), None),
            ),
        )
        .await
        .map_err(|_| ConnectError::NetworkTimeout)??;
        if response.success() {
            return Ok(());
        }
    }
    Err(if attempted {
        ConnectError::Authentication
    } else if unsupported {
        ConnectError::UnsupportedIdentity
    } else {
        ConnectError::Identity
    })
}
