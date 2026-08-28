//! Shared business vocabulary for a relayed UserOperation.
//!
//! The shell re-exports these types under its historical paths; wire names and
//! serde shapes are frozen (see `specs/001-crux-core-split/contracts/`).

use serde::{Deserialize, Serialize};

/// Lifecycle status of a UserOperation as stored and as exposed over RPC.
///
/// `NotFound` is an API-only value: responses use it for unknown hashes, but a
/// stored record never carries it. The transition rules between the stored
/// states live in [`crate::lifecycle`], the single authoritative table.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UserOperationStatus {
    NotFound,
    Queued,
    NotSubmitted,
    Submitted,
    Rejected,
    Included,
    Failed,
}

impl UserOperationStatus {
    /// A terminal status accepts same-status field merges but no transitions.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Rejected | Self::Included | Self::Failed)
    }

    /// Whether the executor may treat this operation as durably settled for
    /// consumer-offset purposes. Broader than [`Self::is_terminal`]:
    /// `Submitted` is durable (the bundle intent survives crashes) but not
    /// terminal (receipts still move it to `Included`/`Failed`).
    pub fn is_durable(self) -> bool {
        matches!(
            self,
            Self::Submitted | Self::Rejected | Self::Included | Self::Failed
        )
    }
}
