//! Chunk uploads of one process: one upload of a chunk at a time.
//!
//! The object store limits writes to one object to about one per second
//! (GCS answers more with 429), so concurrent PUTs of one chunk fail and
//! retry. Here an upload of a chunk waits while another upload of it
//! runs. When that upload's PUT succeeded, the waiters send nothing:
//! their requests overlapped that PUT, so it serves them as their own
//! PUT at the same time would. Nothing is remembered after the last
//! waiter leaves: a later upload asks the store again, because chunk GC
//! can delete a chunk at any time once it has been unreferenced for its
//! grace period.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::OwnedMutexGuard;

use crate::manifest::ChunkHash;

/// Per chunk with an upload running or waiting: whether a PUT of it
/// succeeded while those uploads overlapped.
type Slot = Arc<tokio::sync::Mutex<bool>>;

#[derive(Default)]
pub(crate) struct Uploads {
    flights: Mutex<HashMap<ChunkHash, Slot>>,
}

impl Uploads {
    /// Wait until no other upload of `hash` runs in this process; the
    /// returned guard keeps the others out until it drops.
    pub(crate) async fn one_at_a_time(self: &Arc<Self>, hash: ChunkHash) -> Flight {
        let slot = Arc::clone(self.flights.lock().entry(hash).or_default());
        // Made before the wait, so a waiter dropped mid-wait also cleans up.
        let mut flight = Flight {
            guard: None,
            uploads: Arc::clone(self),
            hash,
        };
        flight.guard = Some(slot.lock_owned().await);
        flight
    }
}

pub(crate) struct Flight {
    guard: Option<OwnedMutexGuard<bool>>,
    uploads: Arc<Uploads>,
    hash: ChunkHash,
}

impl Flight {
    /// Whether an overlapping upload's PUT of the chunk succeeded.
    pub(crate) fn put_landed(&self) -> bool {
        **self.guard.as_ref().expect("set until drop")
    }

    /// Record that this upload's PUT succeeded, for the waiters.
    pub(crate) fn landed(&mut self) {
        **self.guard.as_mut().expect("set until drop") = true;
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        drop(self.guard.take());
        // Every waiter holds a clone of the slot, taken under the map
        // lock: a count of 1 means no upload of the chunk waits.
        let mut flights = self.uploads.flights.lock();
        if flights
            .get(&self.hash)
            .is_some_and(|slot| Arc::strong_count(slot) == 1)
        {
            flights.remove(&self.hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_landed_put_serves_the_waiters_and_is_forgotten_after_them() {
        let uploads = Arc::new(Uploads::default());
        let hash = ChunkHash::of(b"x");
        let mut first = uploads.one_at_a_time(hash).await;
        let waiter = tokio::spawn({
            let uploads = Arc::clone(&uploads);
            async move { uploads.one_at_a_time(hash).await.put_landed() }
        });
        tokio::task::yield_now().await;
        assert!(!first.put_landed());
        first.landed();
        drop(first);
        assert_eq!(uploads.flights.lock().len(), 1, "the waiter still needs it");
        assert!(waiter.await.unwrap(), "the waiter overlapped the PUT");
        assert!(uploads.flights.lock().is_empty());
        assert!(
            !uploads.one_at_a_time(hash).await.put_landed(),
            "a later upload asks the store again"
        );
    }

    #[tokio::test]
    async fn a_waiter_dropped_mid_wait_leaves_no_slot_behind() {
        let uploads = Arc::new(Uploads::default());
        let hash = ChunkHash::of(b"x");
        let first = uploads.one_at_a_time(hash).await;
        let waiter = tokio::spawn({
            let uploads = Arc::clone(&uploads);
            async move { drop(uploads.one_at_a_time(hash).await) }
        });
        tokio::task::yield_now().await;
        drop(first);
        waiter.abort();
        let _ = waiter.await;
        assert!(uploads.flights.lock().is_empty());
    }
}
