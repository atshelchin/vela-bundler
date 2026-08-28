//! Key derivation moved to the decision core; this shim keeps the shell's
//! historical `crate::utils::vault::…` paths stable.
pub use vela_relay_core::vault::*;
