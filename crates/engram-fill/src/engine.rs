use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::cache::FillSource;
use engram_chunk_store::{ChunkHash, ChunkStore, Result};
use parking_lot::Mutex;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::admission::{Admission, Permit};
use crate::{Class, PeerSource};

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
    /// With a peer, a `Demand` fetch the peer has not answered after this
    /// also asks the store; the first correct answer wins. A peer hit on
    /// the LAN takes about 1-2 ms.
    pub peer_race_after: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            in_flight: 64,
            demand_reserve: 16,
            hedge_after: Some(Duration::from_millis(200)),
            peer_race_after: Duration::from_millis(20),
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
    /// Asked before the store, when this view of the engine has one.
    peer: Option<Arc<dyn PeerSource>>,
}

struct Inner {
    store: ChunkStore,
    admission: Arc<Admission>,
    hedge_after: Option<Duration>,
    peer_race_after: Duration,
    /// Peers by session manifest id, for fetches that know only which
    /// manifest they serve (a page fault).
    peers: Mutex<HashMap<Uuid, Arc<dyn PeerSource>>>,
}

/// Keeps a peer registered; dropping it unregisters the peer.
pub struct PeerRegistration {
    inner: Arc<Inner>,
    manifest: Uuid,
}

impl Drop for PeerRegistration {
    fn drop(&mut self) {
        self.inner.peers.lock().remove(&self.manifest);
    }
}

impl FillEngine {
    pub fn new(store: ChunkStore, cfg: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                admission: Admission::new(cfg.in_flight, cfg.demand_reserve),
                hedge_after: cfg.hedge_after,
                peer_race_after: cfg.peer_race_after,
                peers: Mutex::new(HashMap::new()),
            }),
            peer: None,
        }
    }

    /// This engine (same limits, same cache) with `peer` asked before the
    /// store.
    pub fn with_peer(&self, peer: Arc<dyn PeerSource>) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            peer: Some(peer),
        }
    }

    /// Make `peer` the source for fetches of session `manifest`
    /// ([`Self::for_manifest`]) while the registration lives.
    pub fn register_peer(&self, manifest: Uuid, peer: Arc<dyn PeerSource>) -> PeerRegistration {
        self.inner.peers.lock().insert(manifest, peer);
        PeerRegistration {
            inner: Arc::clone(&self.inner),
            manifest,
        }
    }

    /// This engine with the peer registered for `manifest`, if any.
    pub fn for_manifest(&self, manifest: Uuid) -> Self {
        let peer = self.inner.peers.lock().get(&manifest).cloned();
        Self {
            inner: Arc::clone(&self.inner),
            peer,
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

    /// The chunk's bytes for a caller that keeps them itself (a volume
    /// write): a cache hit is served, a miss is fetched and verified but
    /// not stored in the cache, so the bytes are written to disk once.
    pub async fn read(&self, hash: ChunkHash, class: Class) -> Result<Bytes> {
        let store = self.store();
        if let Some(cache) = store.chunk_cache() {
            if let Some(bytes) = cache.read_resident(hash).await? {
                return Ok(bytes);
            }
        }
        let _permit = self.inner.admission.acquire(class).await;
        metrics::counter!("engram_fill_fetch_total", "class" => class.as_str()).increment(1);
        match self.peer_answer(hash).await {
            Some(bytes) => Ok(bytes),
            None => store.resolver().fetch_chunk(hash).await,
        }
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
            Some(cache) => {
                cache
                    .get_with_source(hash, || self.fetch_unverified(hash, class))
                    .await
            }
            None => match self.peer_answer(hash).await {
                Some(bytes) => Ok(bytes),
                None => store.resolver().fetch_chunk(hash).await,
            },
        }
    }

    /// The peer's answer, when this view has a peer and the peer has the
    /// chunk. Trusted: the peer checked it when it stored it.
    async fn peer_answer(&self, hash: ChunkHash) -> Option<Bytes> {
        self.peer.as_ref()?.fetch(hash).await
    }

    /// The peer (when this view has one), then the store. A `Demand`
    /// fetch races the store against a peer that has not answered after
    /// `peer_race_after`. The cache hashes what came from the store.
    async fn fetch_unverified(&self, hash: ChunkHash, class: Class) -> Result<(Bytes, FillSource)> {
        let from_store = |engine: &Self| {
            let engine = engine.clone();
            async move {
                engine
                    .fetch_store(hash, class)
                    .await
                    .map(|b| (b, FillSource::BlobStorage))
            }
        };
        if self.peer.is_none() {
            return from_store(self).await;
        }
        let peer = self.peer_answer(hash);
        tokio::pin!(peer);
        if class != Class::Demand {
            return match peer.await {
                Some(b) => Ok((b, FillSource::Peer)),
                None => from_store(self).await,
            };
        }
        tokio::select! {
            got = &mut peer => return match got {
                Some(b) => Ok((b, FillSource::Peer)),
                None => from_store(self).await,
            },
            () = tokio::time::sleep(self.inner.peer_race_after) => {}
        }
        metrics::counter!("engram_fill_peer_race_total").increment(1);
        let store = from_store(self);
        tokio::pin!(store);
        tokio::select! {
            got = &mut peer => match got {
                Some(b) => Ok((b, FillSource::Peer)),
                None => store.await,
            },
            got = &mut store => match got {
                Ok(b) => Ok(b),
                Err(e) => match peer.await {
                    Some(b) => Ok((b, FillSource::Peer)),
                    None => Err(e),
                },
            },
        }
    }

    /// One store fetch, hedged for `Demand`. The caller verifies.
    async fn fetch_store(&self, hash: ChunkHash, class: Class) -> Result<Bytes> {
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
