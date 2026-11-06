//! The host's chunk fill engine.
//!
//! Every chunk a host fetches from the object store goes through one
//! [`FillEngine`]. It grants store fetches in class order under one limit on
//! fetches in flight, runs each fetch on its own task (so TLS, parsing and
//! hashing use every core), and hedges a fetch that a guest waits on. The
//! chunk cache is the write chokepoint: it verifies a fetched chunk once
//! before it stores it, and every later read trusts the stored file.

mod admission;
mod engine;

pub use admission::{Admission, Permit};
pub use engine::{Config, FillEngine, FillStats, PeerRegistration};

/// A host that holds a session's chunks on local disk: the tier between
/// this host's cache and the object store.
#[async_trait::async_trait]
pub trait PeerSource: Send + Sync {
    /// The chunk's bytes, or `None` when the peer does not have it or
    /// cannot answer (a lost peer answers `None` at once). The bytes are
    /// trusted: the peer checked the chunk when it stored it.
    async fn fetch(&self, hash: engram_chunk_store::ChunkHash) -> Option<bytes::Bytes>;
}

/// Why a chunk is fetched. A fetch of a higher class is never granted after
/// a waiting fetch of a lower class; fetches already in flight are not
/// cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    /// A guest waits for it now (a page fault, a disk read, a disk the boot
    /// waits for).
    Demand = 0,
    /// Needed soon on the critical path (the pages a resume touches first).
    Prefix = 1,
    /// The rest of a restore, after the critical path.
    Hydrate = 2,
    /// Work no session waits for (staging, cache warming).
    Background = 3,
}

impl Class {
    pub const ALL: [Class; 4] = [
        Class::Demand,
        Class::Prefix,
        Class::Hydrate,
        Class::Background,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Demand => "demand",
            Class::Prefix => "prefix",
            Class::Hydrate => "hydrate",
            Class::Background => "background",
        }
    }
}
