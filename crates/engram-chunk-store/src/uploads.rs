//! Chunk uploads of one process: one PUT of a chunk at a time, and a
//! memo of the chunks the process knows the store holds.
//!
//! The object store limits writes to one object to about one per second
//! (GCS answers more with 429), so concurrent PUTs of one chunk fail and
//! retry. Here the second upload of a chunk waits for the first, then
//! finds the chunk in the memo and sends nothing.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::OwnedMutexGuard;

use crate::manifest::ChunkHash;

/// The chunks the memo holds at most (32 bytes and an instant each).
const KNOWN_CAP: usize = 65_536;

/// How long the memo trusts that the store holds a chunk. Chunk GC
/// deletes a chunk only after it has been unreferenced for its grace
/// period (24 h by default), so a memo far shorter than that cannot
/// skip the upload of a chunk GC deleted.
const KNOWN_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct Uploads {
    /// One lock per chunk with an upload running or waiting.
    flights: Mutex<HashMap<ChunkHash, Arc<tokio::sync::Mutex<()>>>>,
    known: Mutex<Known>,
}

#[derive(Default)]
struct Known {
    at: HashMap<ChunkHash, Instant>,
    /// Insertion order, oldest first, for the size cap.
    order: VecDeque<ChunkHash>,
}

impl Uploads {
    /// Whether this process uploaded `hash`, or saw it in the store, in
    /// the last [`KNOWN_TTL`].
    pub(crate) fn knows(&self, hash: ChunkHash) -> bool {
        self.knows_at(hash, crate::time_source::memo_now())
    }

    fn knows_at(&self, hash: ChunkHash, now: Instant) -> bool {
        self.known
            .lock()
            .at
            .get(&hash)
            .is_some_and(|at| now.saturating_duration_since(*at) < KNOWN_TTL)
    }

    /// Record that the store holds `hash`.
    pub(crate) fn learn(&self, hash: ChunkHash) {
        self.learn_at(hash, crate::time_source::memo_now());
    }

    fn learn_at(&self, hash: ChunkHash, now: Instant) {
        let mut known = self.known.lock();
        if known.at.insert(hash, now).is_none() {
            known.order.push_back(hash);
        }
        while known.order.len() > KNOWN_CAP {
            let oldest = known.order.pop_front().expect("order is not empty");
            known.at.remove(&oldest);
        }
    }

    /// Wait until no other upload of `hash` runs in this process; the
    /// returned guard keeps the others out until it drops.
    pub(crate) async fn one_at_a_time(self: &Arc<Self>, hash: ChunkHash) -> Flight {
        let lock = Arc::clone(self.flights.lock().entry(hash).or_default());
        // Made before the wait, so a waiter dropped mid-wait also cleans up.
        let mut flight = Flight {
            guard: None,
            uploads: Arc::clone(self),
            hash,
        };
        flight.guard = Some(lock.lock_owned().await);
        flight
    }
}

pub(crate) struct Flight {
    guard: Option<OwnedMutexGuard<()>>,
    uploads: Arc<Uploads>,
    hash: ChunkHash,
}

impl Drop for Flight {
    fn drop(&mut self) {
        drop(self.guard.take());
        // Every waiter holds a clone of the lock, taken under the map
        // lock: a count of 1 means no upload of the chunk waits.
        let mut flights = self.uploads.flights.lock();
        if flights
            .get(&self.hash)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            flights.remove(&self.hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memo_forgets_a_chunk_after_its_ttl() {
        let uploads = Uploads::default();
        let hash = ChunkHash::of(b"x");
        let t0 = crate::time_source::memo_now();
        uploads.learn_at(hash, t0);
        assert!(uploads.knows_at(hash, t0 + KNOWN_TTL / 2));
        assert!(!uploads.knows_at(hash, t0 + KNOWN_TTL));
    }

    #[test]
    fn the_memo_holds_at_most_its_cap() {
        let uploads = Uploads::default();
        let now = crate::time_source::memo_now();
        let first = ChunkHash::of(&0u32.to_le_bytes());
        for i in 0..=KNOWN_CAP as u32 {
            uploads.learn_at(ChunkHash::of(&i.to_le_bytes()), now);
        }
        assert_eq!(uploads.known.lock().at.len(), KNOWN_CAP);
        assert!(!uploads.knows_at(first, now), "the oldest went first");
    }

    #[tokio::test]
    async fn the_lock_of_a_chunk_goes_when_its_last_upload_ends() {
        let uploads = Arc::new(Uploads::default());
        let hash = ChunkHash::of(b"x");
        let first = uploads.one_at_a_time(hash).await;
        let waiter = tokio::spawn({
            let uploads = Arc::clone(&uploads);
            async move { drop(uploads.one_at_a_time(hash).await) }
        });
        tokio::task::yield_now().await;
        drop(first);
        assert_eq!(uploads.flights.lock().len(), 1, "the waiter still needs it");
        waiter.await.unwrap();
        assert!(uploads.flights.lock().is_empty());
    }

    #[tokio::test]
    async fn a_waiter_dropped_mid_wait_leaves_no_lock_behind() {
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
