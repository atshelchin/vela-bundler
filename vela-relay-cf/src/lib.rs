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
mod admission;
#[cfg(target_arch = "wasm32")]
mod arms;
#[cfg(target_arch = "wasm32")]
mod config;
#[cfg(target_arch = "wasm32")]
mod http;
#[cfg(target_arch = "wasm32")]
mod lane_do;
#[cfg(target_arch = "wasm32")]
mod proto;
#[cfg(target_arch = "wasm32")]
mod record_do;
#[cfg(target_arch = "wasm32")]
mod shell;
#[cfg(target_arch = "wasm32")]
mod treasury_do;
