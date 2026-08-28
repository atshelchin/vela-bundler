//! The Cloudflare Shell for vela-relay.
//!
//! Doctrine: every business decision — admission, lifecycle transitions,
//! settlement, hold, funding, broadcast classification — is consumed from
//! `vela-relay-core`. This crate only wires those decisions to Cloudflare
//! primitives: the fetch handler drives `AdmissionApp`, LaneDO drives
//! `ExecutionApp`, Durable Object storage supplies the guard semantics Redis
//! Lua supplies in the docker shell, and Queues carry the frozen envelope.
//!
//! The crate is wasm-only; see `Cargo.toml` for the target gating rationale.

#[cfg(target_arch = "wasm32")]
mod shell;
