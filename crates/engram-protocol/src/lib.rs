//! Wire types for the coordinator<->host channel.
//!
//! v1 (Phase 1+2) the coordinator and host run in the same process and
//! talk in-memory; these types are still used as the public contract.
//! Phase 3 will add a tonic+protobuf transport that serialises the same
//! shapes — at which point this crate gains a `build.rs` and a `.proto`
//! file. Wire-format-stable changes should land here first.

pub mod heartbeat;
pub mod scheduling;

pub use heartbeat::*;
pub use scheduling::*;
