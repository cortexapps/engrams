//! When a failed read is attempted again, and when it is refused.

use std::io;
use std::time::Duration;

use tokio::sync::watch;

use crate::reader::DiskReader;

/// The first wait between attempts at a read that fails.
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(50);
/// The longest wait between attempts at a read that fails.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// A read that fails for a reason that may pass is attempted again until
/// it succeeds, so a slow or unreachable store stalls the guest's read
/// and never fails it. A chunk the store does not hold, or whose bytes
/// fail the hash check, is data loss: after one fresh fetch the read is
/// refused. A request outside the disk, or a read still waiting when
/// `stopping` turns true, is refused at once.
pub async fn read_until_served(
    reader: &DiskReader,
    offset: u64,
    len: usize,
    mut stopping: watch::Receiver<bool>,
) -> io::Result<Vec<u8>> {
    let stopped = || io::Error::other("the server is stopping");
    let mut delay = FIRST_RETRY_DELAY;
    let mut attempt = 1u32;
    let mut lost_once = false;
    loop {
        let result = tokio::select! {
            // A read that is ready is served even when a stop is too.
            biased;
            result = reader.read(offset, len) => result,
            _ = stopping.wait_for(|s| *s) => return Err(stopped()),
        };
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Err(e),
            Err(e) if is_lost(&e) && lost_once => {
                tracing::error!(offset, len, error = %e, "disk source data lost: the chunk store cannot serve this read");
                return Err(e);
            }
            Err(e) if is_lost(&e) => {
                // The failed fetch left no cached copy, so this attempt
                // fetches again.
                tracing::warn!(offset, len, error = %e, "read failed for lost data; fetching once more");
                lost_once = true;
            }
            Err(e) => {
                // An outage fails every attempt; log the 1st, 2nd, 4th, ...
                if attempt.is_power_of_two() {
                    tracing::warn!(offset, len, attempt, error = %e, "read failed; trying again");
                } else {
                    tracing::debug!(offset, len, attempt, error = %e, "read failed; trying again");
                }
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = stopping.wait_for(|s| *s) => return Err(stopped()),
                }
                delay = (delay * 2).min(MAX_RETRY_DELAY);
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

fn is_lost(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::InvalidData
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use engram_chunk_store::{ChunkHash, ChunkRef, ChunkSize, ChunkStore, Manifest, ManifestKind};
    use engram_core::traits::storage::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use engram_testkit::storage::{
        Counters, FaultPlan, FaultyBlobStorage, GetFault, GetFaultKind, InjectedError, KeyMatch,
        When,
    };

    const CHUNK: u64 = 64 * 1024;

    struct Fixture {
        _dir: tempfile::TempDir,
        reader: DiskReader,
        counters: Arc<Counters>,
        expected: Vec<u8>,
    }

    /// A one-chunk disk whose store applies `faults` to the chunk.
    async fn fixture(faults: Vec<(When, GetFaultKind)>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let expected: Vec<u8> = (0..CHUNK as usize).map(|i| (i % 251) as u8).collect();
        let hash = ChunkHash::of(&expected);
        let local: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path()));
        ChunkStore::new(Arc::clone(&local))
            .put_chunk(&expected)
            .await
            .unwrap();
        let plan = faults
            .into_iter()
            .fold(FaultPlan::new(), |plan, (when, kind)| {
                plan.with_get(GetFault {
                    key: KeyMatch::Exact(hash.storage_key()),
                    when,
                    kind,
                })
            });
        let (blob, counters) = FaultyBlobStorage::arc(local, plan);
        let mut manifest = Manifest::empty(ManifestKind::Disk, CHUNK);
        manifest.chunk_size = ChunkSize::bytes(CHUNK);
        manifest.chunks = vec![ChunkRef { offset: 0, hash }];
        Fixture {
            _dir: dir,
            reader: DiskReader::new(&manifest, ChunkStore::new(blob)).unwrap(),
            counters,
            expected,
        }
    }

    fn unreachable() -> GetFaultKind {
        GetFaultKind::NotFound(InjectedError::Sdk("the store is unreachable".into()))
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_store_error_is_tried_again_until_the_read_succeeds() {
        let f = fixture((1..=3).map(|n| (When::Nth(n), unreachable())).collect()).await;
        let (_stop, stopping) = watch::channel(false);
        let got = read_until_served(&f.reader, 0, 4096, stopping)
            .await
            .unwrap();
        assert_eq!(got, &f.expected[..4096]);
        assert_eq!(f.counters.gets_faulted(), 3);
        assert_eq!(f.counters.gets_attempted(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_chunk_the_store_does_not_hold_is_fetched_twice_then_refused() {
        let f = fixture(vec![(
            When::Always,
            GetFaultKind::NotFound(InjectedError::NotFound),
        )])
        .await;
        let (_stop, stopping) = watch::channel(false);
        let err = read_until_served(&f.reader, 0, 4096, stopping)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert_eq!(f.counters.gets_attempted(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_ends_a_read_that_is_trying_again() {
        let f = Arc::new(fixture(vec![(When::Always, unreachable())]).await);
        let (stop, stopping) = watch::channel(false);
        let read = {
            let f = Arc::clone(&f);
            tokio::spawn(async move { read_until_served(&f.reader, 0, 4096, stopping).await })
        };
        while f.counters.gets_faulted() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!read.is_finished(), "the read waits while the store fails");
        stop.send(true).unwrap();
        let err = read.await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), "the server is stopping");
        let attempts = f.counters.gets_attempted();
        tokio::time::sleep(MAX_RETRY_DELAY * 2).await;
        assert_eq!(
            f.counters.gets_attempted(),
            attempts,
            "a stopped read does not try again"
        );
    }
}
