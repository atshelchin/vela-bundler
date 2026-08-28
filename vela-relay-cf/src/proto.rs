//! Serde fetch protocol between the worker handlers and the Durable Objects.
//! The DO is part of the shell; the payloads reuse the core's frozen store
//! vocabulary so record bytes stay shape-identical to the docker shell's.

use serde::{Deserialize, Serialize};
use vela_relay_core::task::{QueuedUserOperation, StoredUserOperation};

/// One command per RecordDO invocation (POST body).
#[derive(Serialize, Deserialize)]
pub enum RecordCommand {
    /// Create-if-absent — the SETNX of the docker store's `create_queued`;
    /// the DO builds the record via the core's `queued_record`.
    CreateQueued {
        operation: QueuedUserOperation,
    },
    Get,
    MarkAdmitted,
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
}
