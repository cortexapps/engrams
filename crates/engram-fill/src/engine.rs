use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::{ChunkHash, ChunkStore, Result};
use tokio::task::JoinSet;

use crate::admission::{Admission, Permit};
use crate::Class;

/// Engine limits. The defaults come from dev-cluster runs: 64 fetches in
/// flight reached about 1 GB/s of 512 KiB chunks from GCS, and GCS GET p95
/// was about 50 ms with a tail of 1–10 s on about 0.3 % of GETs.
#[derive(Clone, Debug)]
pub struct Config {
    /// Store fetches in flight for every class except `Demand`.
    pub in_flight: usize,
    /// Extra slots only `Demand` may use.
    pub demand_reserve: usize,
    /// A `Demand` fetch still running after this gets a second request; the
    /// first answer wins. `None` turns hedging off.
    pub hedge_after: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            in_flight: 64,
            demand_reserve: 16,
            hedge_after: Some(Duration::from_millis(200)),
        }
    }
}

impl Config {
    /// [`Config::default`], with `ENGRAM_FILL_IN_FLIGHT`,
    /// `ENGRAM_FILL_DEMAND_RESERVE` and `ENGRAM_FILL_HEDGE_MS` (0 = off)
    /// read when set. A set value that does not parse is an error.
    pub fn from_env() -> std::result::Result<Self, String> {
        let mut cfg = Self::default();
        if let Some(v) = env_usize("ENGRAM_FILL_IN_FLIGHT")? {
            cfg.in_flight = v;
        }
        if let Some(v) = env_usize("ENGRAM_FILL_DEMAND_RESERVE")? {
            cfg.demand_reserve = v;
        }
        if let Some(ms) = env_usize("ENGRAM_FILL_HEDGE_MS")? {
            cfg.hedge_after = (ms > 0).then(|| Duration::from_millis(ms as u64));
        }
        Ok(cfg)
    }
}

fn env_usize(name: &str) -> std::result::Result<Option<usize>, String> {
    match std::env::var(name) {
        Ok(v) => v
            .parse()
            .map(Some)
            .map_err(|e| format!("{name}={v:?}: {e}")),
        Err(_) => Ok(None),
    }
}

/// What a [`FillEngine::fill`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FillStats {
    /// Chunks the cache already held.
    pub resident: usize,
    /// Chunks fetched and stored.
    pub fetched: usize,
    /// Repeated hashes in the request.
    pub duplicate: usize,
}

/// The host's one path from the object store to the chunk cache. Cheap to
/// clone.
#[derive(Clone)]
pub struct FillEngine {
    inner: Arc<Inner>,
}

struct Inner {
    store: ChunkStore,
    admission: Arc<Admission>,
    hedge_after: Option<Duration>,
}

impl FillEngine {
    pub fn new(store: ChunkStore, cfg: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                admission: Admission::new(cfg.in_flight, cfg.demand_reserve),
                hedge_after: cfg.hedge_after,
            }),
        }
    }

    pub fn store(&self) -> &ChunkStore {
        &self.inner.store
    }

    pub fn admission(&self) -> &Arc<Admission> {
        &self.inner.admission
    }

    /// The chunk's bytes. A cache hit is served at once; a miss waits for a
    /// `class` slot, then is fetched, verified and stored by the cache.
    /// Without a cache the resolver verifies.
    pub async fn get(&self, hash: ChunkHash, class: Class) -> Result<Bytes> {
        if let Some(cache) = self.store().chunk_cache() {
            if let Some(bytes) = cache.read_resident(hash).await? {
                return Ok(bytes);
            }
        }
        let permit = self.inner.admission.acquire(class).await;
        self.fetch_admitted(hash, class, permit).await
    }

    /// Make every chunk in `hashes` resident in the cache at `class`, each
    /// fetch on its own task. Fetches start in the given order. Returns after
    /// every fetch ends; the first error is returned, and the other fetches
    /// still run to completion. Without a cache there is nowhere to fill, so
    /// it does nothing.
    pub async fn fill(&self, hashes: Vec<ChunkHash>, class: Class) -> Result<FillStats> {
        let Some(cache) = self.store().chunk_cache() else {
            return Ok(FillStats::default());
        };
        let mut stats = FillStats::default();
        let mut seen = HashSet::with_capacity(hashes.len());
        let mut tasks = JoinSet::new();
        let mut first_err = None;
        for hash in hashes {
            if !seen.insert(hash) {
                stats.duplicate += 1;
                continue;
            }
            if cache.contains_on_disk(hash) {
                stats.resident += 1;
                continue;
            }
            // Waiting here, not in the task, keeps the fetch order and
            // spawns no more tasks than there are slots.
            let permit = self.inner.admission.acquire(class).await;
            let engine = self.clone();
            tasks.spawn(async move { engine.fetch_admitted(hash, class, permit).await });
            while let Some(done) = tasks.try_join_next() {
                record(done, &mut stats, &mut first_err);
            }
        }
        while let Some(done) = tasks.join_next().await {
            record(done, &mut stats, &mut first_err);
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(stats),
        }
    }

    async fn fetch_admitted(&self, hash: ChunkHash, class: Class, permit: Permit) -> Result<Bytes> {
        let _permit = permit;
        metrics::counter!("engram_fill_fetch_total", "class" => class.as_str()).increment(1);
        let store = self.store();
        match store.chunk_cache() {
            Some(cache) => cache.get(hash, || self.fetch_unverified(hash, class)).await,
            None => store.resolver().fetch_chunk(hash).await,
        }
    }

    /// One store fetch, hedged for `Demand`. The caller verifies.
    async fn fetch_unverified(&self, hash: ChunkHash, class: Class) -> Result<Bytes> {
        let resolver = self.store().resolver();
        let hedge_after = match (class, self.inner.hedge_after) {
            (Class::Demand, Some(d)) => d,
            _ => return resolver.fetch_chunk_unverified(hash).await,
        };
        let first = resolver.fetch_chunk_unverified(hash);
        tokio::pin!(first);
        tokio::select! {
            r = &mut first => return r,
            () = tokio::time::sleep(hedge_after) => {}
        }
        metrics::counter!("engram_fill_hedge_total").increment(1);
        let second = resolver.fetch_chunk_unverified(hash);
        tokio::pin!(second);
        // The first success wins; an error waits for the other request.
        tokio::select! {
            r = &mut first => match r {
                Ok(b) => Ok(b),
                Err(_) => second.await,
            },
            r = &mut second => match r {
                Ok(b) => Ok(b),
                Err(_) => first.await,
            },
        }
    }
}

fn record(
    done: std::result::Result<Result<Bytes>, tokio::task::JoinError>,
    stats: &mut FillStats,
    first_err: &mut Option<engram_chunk_store::ChunkStoreError>,
) {
    match done {
        Ok(Ok(_)) => stats.fetched += 1,
        Ok(Err(e)) => {
            first_err.get_or_insert(e);
        }
        Err(join) => {
            first_err.get_or_insert(engram_chunk_store::ChunkStoreError::Origin(format!(
                "fill task failed: {join}"
            )));
        }
    }
}

#[cfg(test)]
mod tests;
