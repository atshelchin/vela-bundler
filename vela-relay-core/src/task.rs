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

// ---- Wire vocabulary (moved from the shell's RPC types and store; serde
// shapes and field names are frozen — see contracts/external-api.md) ----

pub type Address = String;
pub type HexData = String;
pub type Quantity = String;
pub type TransactionHash = String;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserOperation {
    V0_7(Box<UserOperationV0_7>),
    V0_6(Box<UserOperationV0_6>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserOperationV0_7 {
    pub sender: Address,
    pub nonce: Quantity,
    pub factory: Option<Address>,
    pub factory_data: Option<HexData>,
    pub call_data: HexData,
    pub call_gas_limit: Quantity,
    pub verification_gas_limit: Quantity,
    pub pre_verification_gas: Quantity,
    pub max_fee_per_gas: Quantity,
    pub max_priority_fee_per_gas: Quantity,
    pub paymaster: Option<Address>,
    pub paymaster_verification_gas_limit: Option<Quantity>,
    pub paymaster_post_op_gas_limit: Option<Quantity>,
    pub paymaster_data: Option<HexData>,
    pub signature: HexData,
    pub eip7702_auth: Option<Eip7702Authorization>,
    /// Tempo extension: the token used by the outer `0x76` transaction. It is deliberately
    /// outside ERC-4337's packed hash, but must survive queue persistence verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_token: Option<Address>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserOperationV0_6 {
    pub sender: Address,
    pub nonce: Quantity,
    pub init_code: HexData,
    pub call_data: HexData,
    pub call_gas_limit: Quantity,
    pub verification_gas_limit: Quantity,
    pub pre_verification_gas: Quantity,
    pub max_fee_per_gas: Quantity,
    pub max_priority_fee_per_gas: Quantity,
    pub paymaster_and_data: HexData,
    pub signature: HexData,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Eip7702Authorization {
    pub chain_id: Quantity,
    pub address: Address,
    pub nonce: Quantity,
    pub y_parity: Quantity,
    pub r: Quantity,
    pub s: Quantity,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationEvent {
    pub user_operation_hash: String,
    pub success: bool,
    pub actual_gas_cost: String,
    pub actual_gas_used: String,
}
