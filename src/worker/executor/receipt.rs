//! Receipt interpretation moved to the decision core; this shim keeps the
//! executor's historical `super::receipt::…` paths stable.
pub(super) use vela_relay_core::receipt::*;
