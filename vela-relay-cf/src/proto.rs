//! Serde fetch protocol between the worker handlers and the Durable Objects.
//! The DOs are part of the shell; the payloads reuse the core's frozen store
//! vocabulary so record bytes stay shape-identical to the docker shell's.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vela_relay_core::task::{QueuedUserOperation, RoutedUserOperation, StoredUserOperation};

/// One command per RecordDO invocation (POST body).
#[derive(Serialize, Deserialize)]
pub enum RecordCommand {
    /// Create-if-absent — the SETNX of the docker store's `create_queued`;
    /// the DO builds the record via the core's `queued_record`.
    CreateQueued {
        operation: QueuedUserOperation,
    },
    /// Create-if-absent with `admitted = true` — the docker store's
    /// `restore_queued_from_durable_payload`.
    RestoreQueued {
        operation: QueuedUserOperation,
    },
    Get,
    MarkAdmitted,
    /// The docker store's `patch`: a field map, optionally carrying a status
    /// transition judged by `lifecycle::decide_patch`. Timestamps arrive
    /// pre-computed (`now_ms`-bearing patches) — the DO injects no clock into
    /// decisions (Constitution II).
    Patch {
        patch: Value,
    },
    /// One member of `mark_bundle_submitted`: `decide_bundle_submission`
    /// applied to this record (Transition merges status/transactionHash/
    /// admitted exactly as the docker Lua's 't' arm).
    MarkBundleMemberSubmitted {
        bundle_chain_id: u64,
        transaction_hash: String,
    },
}

#[derive(Serialize, Deserialize)]
pub enum RecordReply {
    Created {
        created: bool,
    },
    Record {
        record: Option<Box<StoredUserOperation>>,
    },
    Marked {
        marked: bool,
    },
    Patched {
        patched: bool,
    },
    Indexed {
        indexed: bool,
    },
}

/// One command per LaneDO invocation.
#[derive(Serialize, Deserialize)]
pub enum LaneCommand {
    /// Drive one lane batch through the core's `ExecutionApp`.
    ExecuteBatch {
        operations: Vec<RoutedUserOperation>,
    },
}

/// Serde mirror of `execution::ItemResolution` for the DO protocol.
#[derive(Serialize, Deserialize)]
pub enum ItemResolutionWire {
    Durable,
    Failed { reason: String },
}

#[derive(Serialize, Deserialize)]
pub enum LaneReply {
    Resolutions {
        resolutions: Vec<ItemResolutionWire>,
    },
}

impl From<vela_relay_core::execution::ItemResolution> for ItemResolutionWire {
    fn from(resolution: vela_relay_core::execution::ItemResolution) -> Self {
        match resolution {
            vela_relay_core::execution::ItemResolution::Durable => Self::Durable,
            vela_relay_core::execution::ItemResolution::Failed { reason } => {
                Self::Failed { reason }
            }
        }
    }
}
