//! Where chunk-store operations get the chunks they read.

use async_trait::async_trait;
use bytes::Bytes;

use crate::admission::Class;
use crate::error::Result;
use crate::manifest::ChunkHash;
use crate::store::ChunkStore;

/// The fetch path for an operation that reads chunks (a sparse re-chunk
/// seeds each dirty chunk from its previous content; a materialize reads
/// every chunk). On a host this is the fill engine (`engram-fill`), the
/// host's one fetch path: it admits in class order, hedges and races the
/// peer. A [`ChunkStore`] is a source for a process with no fill engine.
#[async_trait]
pub trait ChunkSource: Send + Sync {
    /// The chunk's bytes, verified. A miss is fetched at `class`.
    async fn chunk(&self, hash: ChunkHash, class: Class) -> Result<Bytes>;
}

/// The store's own path: its cache, then its resolver, under its
/// admission, with no hedge and no peer.
#[async_trait]
impl ChunkSource for ChunkStore {
    async fn chunk(&self, hash: ChunkHash, class: Class) -> Result<Bytes> {
        self.clone().with_class(class).get_chunk(hash).await
    }
}
