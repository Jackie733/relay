//! SSH domain boundary. Network, authentication, trust and SFTP are still planned.
//! No connection may be enabled until host-key verification is implemented.

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
