//! Moved to the decision core (`vela_relay_core::wire`, spec 002 T005/T006)
//! so both shells share the envelope bytes; this shim keeps every historical
//! path stable for the handlers and their tests.

pub use vela_relay_core::wire::*;
