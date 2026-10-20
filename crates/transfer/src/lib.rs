//! Transfer domain boundary. Scheduling and file writes are not implemented yet.
//! This crate deliberately depends on neither SSH nor the UI.

use common::Endpoint;

#[derive(Clone, Debug)]
pub struct TransferRequest {
    pub source: Endpoint,
    pub destination: Endpoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Queued,
    Planning,
    AwaitingDecision,
    Running,
    Completed,
    CompletedWithIssues,
    Failed,
    Cancelled,
    Interrupted,
}
