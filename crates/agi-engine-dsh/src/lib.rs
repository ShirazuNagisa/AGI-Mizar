//! The DSH adapter.
//!
//! Two halves, split because they fail differently:
//!
//! - [`engine`] owns the link — handshake, request correlation, the caches, and
//!   the [`agi_engine::Engine`] implementation. Everything here is about being
//!   attached to a plugin.
//! - [`projection`] is pure: DSH's log records in, the shared model out. It has
//!   no I/O, so the cases that took a live host to discover are unit tests.
//!
//! The transport is deliberately absent. The HTTP server owns the socket and
//! hands [`engine::DshEngine::serve`] two channels of text frames.

pub mod engine;
pub mod projection;

pub use engine::DshEngine;
pub use projection::Projected;
