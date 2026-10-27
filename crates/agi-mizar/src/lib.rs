//! The Mizar v1 server: what a phone talks to.
//!
//! The desktop has three faces now. The local API is for its own window, the DSH
//! link is for the helper plugin, and this is for the phone — the one that has to
//! work over a network the operator does not control, so it is the only face with
//! pairing, tokens and rate limits.
//!
//! Four modules:
//!
//! - [`frames`] is the wire: five server frames, two client ones, and the
//!   translation from our flat records back into the DSH event shape the shipped
//!   phone decodes.
//! - [`pairing`] makes and normalises codes. Crockford base32, because a code is
//!   read off a screen by a person.
//! - [`store`] is the device roster. Tokens are stored only as SHA-256.
//! - [`server`] binds the three routes and answers the methods this build has.
//!
//! ### What is not here yet
//!
//! Uploads, file preview, workspace listing, activity, relay switching and push
//! registration all answer `NOT_IMPLEMENTED`. That is the protocol's own signal
//! for "this host is older than you", which is a better answer than a stub that
//! looks like it worked.

pub mod frames;
pub mod pairing;
pub mod relay;
pub mod server;
pub mod store;
pub mod uploads;

pub use frames::{ErrorCode, ServerFrame, HTTP_PREFIX, PROTOCOL_VERSION, WS_PATH};
pub use relay::RelayClient;
pub use server::MizarServer;
pub use store::{Device, DeviceStore};
pub use uploads::UploadStore;
