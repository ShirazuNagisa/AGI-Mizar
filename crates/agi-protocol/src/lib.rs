//! Types shared by every AGI Mizar component.
//!
//! Three layers, in dependency order:
//!
//! - [`engine`] — which products exist and what each can do.
//! - [`domain`] — the engine-neutral conversation model everything above reads.
//! - [`link`] and [`api`] — the two wires: to the DSH helper plugin, and to
//!   whatever is drawing the UI.
//!
//! Nothing here performs I/O. That is deliberate: this crate is the contract, and
//! a contract that can open a socket is a contract that is hard to test.

pub mod api;
pub mod domain;
pub mod engine;
pub mod link;
pub mod usage;

pub use domain::{Interaction, InteractionKind, Record, RecordKind, Session, SessionKey, ToolInfo};
pub use engine::{Capabilities, EngineId};
pub use usage::SessionUsage;
