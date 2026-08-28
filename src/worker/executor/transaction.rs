//! Deterministic transaction signing moved to the decision core (as
//! `signing`); this shim keeps the executor's historical
//! `super::transaction::…` paths stable. Key custody stays in the shell.
pub(super) use vela_relay_core::signing::*;
