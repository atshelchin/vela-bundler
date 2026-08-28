//! In-band settlement moved to the decision core; this shim keeps the
//! executor's historical `super::settlement::…` paths stable.
pub(crate) use vela_relay_core::settlement::*;
