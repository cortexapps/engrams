//! The host's chunk fill engine.
//!
//! Every chunk a host moves to or from the object store goes through one
//! [`FillEngine`]. Its [`Admission`] grants store transfers (its own
//! fetches, and the uploads of the store it hands out) in class order under
//! one adaptive limit, so uploads and downloads share the NIC. It runs each
//! fetch on its own task (so TLS, parsing and hashing use every core), and
//! hedges a fetch that a guest waits on. The chunk cache is the write
//! chokepoint: it verifies a fetched chunk once before it stores it, and
//! every later read trusts the stored file.

mod engine;

pub use engine::{
    Config, FillEngine, FillStats, PeerRegistration, SandboxRegistration, MAX_RANGE, RANGE_ALIGN,
};
pub use engram_chunk_store::{Admission, Class, Permit};

/// A host that holds a session's chunks on local disk: the tier between
/// this host's cache and the object store.
#[async_trait::async_trait]
pub trait PeerSource: Send + Sync {
    /// The chunk's bytes, or `None` when the peer does not have it or
    /// cannot answer (a lost peer answers `None` at once). The bytes are
    /// trusted: the peer checked the chunk when it stored it.
    async fn fetch(&self, hash: engram_chunk_store::ChunkHash) -> Option<bytes::Bytes>;
}
