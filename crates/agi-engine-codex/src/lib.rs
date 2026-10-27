//! The Codex adapter.
//!
//! Three layers, split the same way as the DSH adapter:
//!
//! - [`rpc`] owns the child process: framing, correlation, server-initiated
//!   requests, restart. It knows nothing about threads.
//! - [`projection`] is pure: a thread and its turns in, the shared model out. The
//!   sequence numbering that makes a transcript stable lives here, so it is
//!   unit-tested rather than observed.
//! - [`engine`] is the [`agi_engine::Engine`] implementation: requests, approvals,
//!   and the supervisor that keeps the child alive.

pub mod engine;
pub mod projection;
pub mod rpc;
pub mod usage;

pub use engine::CodexEngine;
pub use rpc::{AppServerConfig, RpcError};
