//! RecordDO — one Durable Object per user operation (`{chain_id}:{hash}`),
//! the strongly consistent home of its `StoredUserOperation` (data-model §1).
//!
//! Storage layout: `record` → the frozen camelCase record JSON;
//! `expiresAtMs` → the record TTL deadline (the docker store's 3600 s class).
//! The single alarm currently serves TTL cleanup; US3 packs the receipt-check
//! schedule into the same alarm as earliest-of.
//!
//! Guard semantics: the DO is single-threaded, so create-if-absent and
//! read-modify-write are race-free by construction — the platform supplies
//! what the docker shell builds from Redis SETNX and Lua CAS.

use worker::{
    Date, DurableObject, Env, Request, Response, Result, State, durable_object, wasm_bindgen,
};

use crate::proto::{RecordCommand, RecordReply};

/// Mirrors the docker store's `USER_OPERATION_TTL_SECS` record class.
const RECORD_TTL_MS: u64 = 3_600 * 1_000;

const RECORD_KEY: &str = "record";
const EXPIRES_KEY: &str = "expiresAtMs";

#[durable_object]
pub struct RecordDo {
    state: State,
    #[allow(dead_code)]
    env: Env,
}

impl DurableObject for RecordDo {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let command: RecordCommand = req.json().await?;
        let reply = match command {
            RecordCommand::CreateQueued { operation } => {
                let existing = self.record().await;
                if existing.is_some() {
                    RecordReply::Created { created: false }
                } else {
                    let record = vela_relay_core::task::queued_record(operation, false);
                    let now = Date::now().as_millis();
                    self.state.storage().put(RECORD_KEY, &record).await?;
                    self.state
                        .storage()
                        .put(EXPIRES_KEY, now + RECORD_TTL_MS)
                        .await?;
                    self.schedule_alarm().await?;
                    RecordReply::Created { created: true }
                }
            }
            RecordCommand::Get => RecordReply::Record {
                record: self.record().await.map(Box::new),
            },
            RecordCommand::MarkAdmitted => match self.record().await {
                None => RecordReply::Marked { marked: false },
                Some(mut record) => {
                    record.admitted = true;
                    self.state.storage().put(RECORD_KEY, &record).await?;
                    RecordReply::Marked { marked: true }
                }
            },
        };
        Response::from_json(&reply)
    }

    async fn alarm(&self) -> Result<Response> {
        let now = Date::now().as_millis();
        let expires_at: Option<u64> = self.state.storage().get(EXPIRES_KEY).await.ok().flatten();
        match expires_at {
            Some(expires_at) if now < expires_at => {
                // Re-arm for the remaining lifetime (alarms are earliest-of;
                // US3 adds the receipt-check schedule here).
                self.state
                    .storage()
                    .set_alarm(std::time::Duration::from_millis(expires_at - now))
                    .await?;
            }
            _ => {
                // TTL elapsed (or state is gone): drop everything, exactly as
                // the Redis TTL would. The empty object then evicts.
                self.state.storage().delete_all().await?;
            }
        }
        Response::empty()
    }
}

impl RecordDo {
    async fn record(&self) -> Option<vela_relay_core::task::StoredUserOperation> {
        self.state.storage().get(RECORD_KEY).await.ok().flatten()
    }

    async fn schedule_alarm(&self) -> Result<()> {
        self.state
            .storage()
            .set_alarm(std::time::Duration::from_millis(RECORD_TTL_MS))
            .await
    }
}
