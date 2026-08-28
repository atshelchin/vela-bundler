//! Bundle gas allocation moved to the decision core; this shim keeps the
//! executor's historical `super::cost::…` paths stable. (`native_cost` is now
//! consumed only inside the core's settlement verdict.)
pub(super) use vela_relay_core::cost::allocate_bundle_gas;
