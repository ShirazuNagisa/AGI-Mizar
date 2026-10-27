//! The Claude Code adapter.
//!
//! Four modules, in the order a session touches them:
//!
//! - [`paths`] finds Claude Code's directories. A missing one is an answer.
//! - [`transcript`] parses a session's JSONL into the shared model. Pure, and the
//!   only place that decides what a turn is.
//! - [`engine`] is the [`agi_engine::Engine`] implementation: the list, a
//!   transcript read, `claude -p` for a prompt, and the hook receiver that lets an
//!   approval travel from a blocked `PreToolUse` to the operator.
//! - [`installer`] writes the eight hooks into `~/.claude/settings.json`, and can
//!   take them back out.
//!
//! ### The shape of this engine
//!
//! Claude Code has no inbound service, so nothing here is a connection. Reading is
//! a file read; observing is a hook that Claude Code invokes; prompting is a
//! process per turn. That is why this adapter has no supervisor and no reconnect
//! loop — there is nothing to keep alive.

pub mod engine;
pub mod installer;
pub mod paths;
pub mod transcript;

pub use engine::ClaudeEngine;
pub use installer::{install, uninstall, InstallOutcome, HOOK_EVENTS};
