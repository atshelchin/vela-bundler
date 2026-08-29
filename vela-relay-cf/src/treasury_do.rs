//! TreasuryDO — one Durable Object per chain: the REAL cross-lane lock plus
//! funding-outbox home (data-model §1). Unlike the lane lease (which the
//! LaneDO's single thread answers structurally), the treasury is contended by
//! every lane of a chain, so this DO keeps explicit lock state with the docker
//! store's exact semantics: acquire = `SET NX PX`, renew/release = guarded by
//! the holder token, expiry judged against the stored deadline.
//!
//! Storage layout: `lease` → holder token + deadline; `funding` → the chain's
//! single `PreparedFundingIntent` (put-if-absent, hash-guarded clear);
//! `probe:{txhash}` → receipt-probe throttle deadline (an expiring slot that
//! is never released, exactly the docker per-interval receipt lease).
//!
//! The docker shell's background lease heartbeat has no counterpart here;
//! instead every request may piggyback a renewal (`TreasuryRequest::renew`),
//! extending the deadline on each touch from the current holder (declared in
//! contracts/platform-bindings.md).

use serde::{Deserialize, Serialize};
use worker::{
    Date, DurableObject, Env, Request, Response, Result, State, durable_object, wasm_bindgen,
};

use crate::proto::{LeaseIdentity, TreasuryCommand, TreasuryReply, TreasuryRequest};

const LEASE_KEY: &str = "lease";
const FUNDING_KEY: &str = "funding";

#[derive(Serialize, Deserialize)]
struct LeaseState {
    token: String,
    deadline_ms: u64,
}

#[durable_object]
pub struct TreasuryDo {
    state: State,
    #[allow(dead_code)]
    env: Env,
}

impl DurableObject for TreasuryDo {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        // The funding intent carries a u128 (`amount_wei`); anything that
        // round-trips through a JS value (structured clone, JsValue serde)
        // degrades it to a float. The whole boundary therefore speaks JSON
        // text: request body parsed with serde_json, funding intent stored as
        // its JSON string (see contracts/platform-bindings.md).
        let text = req.text().await?;
        let request: TreasuryRequest = serde_json::from_str(&text)
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        let now = Date::now().as_millis();
        if let Some(renew) = &request.renew {
            self.renew_lease(renew, now).await?;
        }
        let reply = match request.command {
            TreasuryCommand::AcquireLease { lease } => TreasuryReply::Acquired {
                acquired: self.acquire_lease(&lease, now).await?,
            },
            TreasuryCommand::EnsureLease { lease } => TreasuryReply::Held {
                held: self.renew_lease(&lease, now).await?,
            },
            TreasuryCommand::ReleaseLease { token } => {
                if let Some(lease) = self.lease().await
                    && lease.token == token
                {
                    self.state.storage().delete(LEASE_KEY).await?;
                }
                TreasuryReply::Released
            }
            TreasuryCommand::LoadFunding => TreasuryReply::Funding {
                intent: self.funding().await,
            },
            TreasuryCommand::SaveFunding { intent } => {
                if self.funding().await.is_some() {
                    TreasuryReply::Saved { saved: false }
                } else {
                    let payload = serde_json::to_string(&intent)
                        .map_err(|error| worker::Error::RustError(error.to_string()))?;
                    self.state.storage().put(FUNDING_KEY, payload).await?;
                    TreasuryReply::Saved { saved: true }
                }
            }
            TreasuryCommand::ClearFunding { transaction_hash } => {
                let cleared = match self.funding().await {
                    Some(intent) if intent.transaction_hash == transaction_hash => {
                        self.state.storage().delete(FUNDING_KEY).await?;
                        // Housekeeping only: a cleared intent is never probed
                        // again (the docker slot just expires in Redis).
                        let _ = self
                            .state
                            .storage()
                            .delete(&format!("probe:{transaction_hash}"))
                            .await;
                        true
                    }
                    _ => false,
                };
                TreasuryReply::Cleared { cleared }
            }
            TreasuryCommand::AcquireReceiptProbe {
                transaction_hash,
                ttl_ms,
            } => {
                let key = format!("probe:{transaction_hash}");
                let deadline: Option<u64> = self.state.storage().get(&key).await.ok().flatten();
                let acquired = deadline.is_none_or(|deadline| now >= deadline);
                if acquired {
                    self.state.storage().put(&key, now + ttl_ms).await?;
                }
                TreasuryReply::Acquired { acquired }
            }
        };
        Response::from_json(&reply)
    }
}

impl TreasuryDo {
    async fn lease(&self) -> Option<LeaseState> {
        self.state.storage().get(LEASE_KEY).await.ok().flatten()
    }

    /// The funding intent is stored as its JSON string (u128 `amount_wei`
    /// must never round-trip through a JS number).
    async fn funding(&self) -> Option<vela_relay_core::task::PreparedFundingIntent> {
        let payload: Option<String> = self.state.storage().get(FUNDING_KEY).await.ok().flatten();
        payload.and_then(|payload| serde_json::from_str(&payload).ok())
    }

    /// `SET NX PX`: only an absent or expired lease can be taken.
    async fn acquire_lease(&self, lease: &LeaseIdentity, now: u64) -> Result<bool> {
        if self
            .lease()
            .await
            .is_some_and(|held| now < held.deadline_ms)
        {
            return Ok(false);
        }
        self.state
            .storage()
            .put(
                LEASE_KEY,
                &LeaseState {
                    token: lease.token.clone(),
                    deadline_ms: now + lease.ttl_ms,
                },
            )
            .await?;
        Ok(true)
    }

    /// Extends only while `token` still holds an unexpired lease.
    async fn renew_lease(&self, lease: &LeaseIdentity, now: u64) -> Result<bool> {
        match self.lease().await {
            Some(held) if held.token == lease.token && now < held.deadline_ms => {
                self.state
                    .storage()
                    .put(
                        LEASE_KEY,
                        &LeaseState {
                            token: lease.token.clone(),
                            deadline_ms: now + lease.ttl_ms,
                        },
                    )
                    .await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
