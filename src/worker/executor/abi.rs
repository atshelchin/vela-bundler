//! ERC-4337 ABI packing moved to the decision core; this shim keeps the
//! executor's historical `super::abi::…` paths stable.
pub(super) use vela_relay_core::abi::*;
