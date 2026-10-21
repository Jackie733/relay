//! SSH connections require host-key verification before user authentication.

pub mod config;
pub mod connection;
pub mod prompt;
pub mod trust;

pub use connection::{ConnectError, ConnectOptions, Connection};
pub use prompt::{Prompt, PromptBroker, PromptError, PromptKind, PromptResponse};
pub use trust::{HostKey, HostKeyPolicy, TrustDecision, TrustError, TrustStore};

use common::HostId;

/// Connection settings only; credentials must never be stored here.
#[derive(Clone, Debug)]
pub struct HostProfile {
    pub id: HostId,
    pub hostname: String,
    pub port: u16,
    pub username: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    HostKeyUnknown,
    HostKeyChanged,
    HostKeyRevoked,
    AuthFailed,
    NetworkError,
}
