use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::cache::FillSource;
use engram_chunk_store::{
    Admission, ChunkHash, ChunkStore, ChunkStoreError, Class, Permit, Result,
};
use parking_lot::Mutex;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::PeerSource;

/// Engine limits. The defaults come from dev-cluster runs: 64 fetches in
/// flight reached about 1 GB/s of 512 KiB chunks from GCS, and GCS GET p95
/// was about 50 ms with a tail of 1–10 s on about 0.3 % of GETs.
#[derive(Clone, Debug)]
pub struct Config {
    /// Store transfers (GETs and PUTs) in flight at start, for every class
    /// except `Demand`. The limit then follows the measured throughput.
    pub in_flight: usize,
    /// The highest the limit goes.
    pub max_in_flight: usize,
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

/// The lowest the adaptive limit goes.
const MIN_IN_FLIGHT: usize = 8;

impl Default for Config {
    fn default() -> Self {
        Self {
            in_flight: 64,
            max_in_flight: 256,
            demand_reserve: 16,
            hedge_after: Some(Duration::from_millis(200)),
            peer_race_after: Duration::from_millis(20),
        }
    }
}

impl Config {
    /// [`Config::default`], with `ENGRAM_FILL_IN_FLIGHT`,
    /// `ENGRAM_FILL_IN_FLIGHT_MAX`, `ENGRAM_FILL_DEMAND_RESERVE` and
    /// `ENGRAM_FILL_HEDGE_MS` (0 = off) read when set.
    /// `ENGRAM_FILL_IN_FLIGHT` sets the start and the ceiling, so the
    /// limit only backs off from it; `ENGRAM_FILL_IN_FLIGHT_MAX` then
    /// raises the ceiling. A set value that does not parse is an error.
    pub fn from_env() -> std::result::Result<Self, String> {
        let mut cfg = Self::default();
        if let Some(v) = env_usize("ENGRAM_FILL_IN_FLIGHT")? {
            cfg.in_flight = v;
            cfg.max_in_flight = v;
        }
        if let Some(v) = env_usize("ENGRAM_FILL_IN_FLIGHT_MAX")? {
            cfg.max_in_flight = v;
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

/// The host's one path between the object store and the chunk cache. It
/// admits every store transfer of the host: its own fetches, and the PUTs
/// and GETs of every clone of [`Self::store`], which carries its
/// admission. Cheap to clone.
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
    /// manifest they serve (a page fault), each with the id of the
    /// registration that put it there.
    peers: Mutex<HashMap<Uuid, (u64, Arc<dyn PeerSource>)>>,
    next_registration: AtomicU64,
}

/// Keeps a peer registered; dropping it unregisters the peer, unless a
/// later registration for the same manifest replaced it.
pub struct PeerRegistration {
    inner: Arc<Inner>,
    manifest: Uuid,
    id: u64,
}

impl Drop for PeerRegistration {
    fn drop(&mut self) {
        let mut peers = self.inner.peers.lock();
        if peers
            .get(&self.manifest)
            .is_some_and(|(id, _)| *id == self.id)
        {
            peers.remove(&self.manifest);
        }
    }
}

impl FillEngine {
    /// An engine over `store`. The engine's store is `store` with the
    /// engine's admission: use [`Self::store`] (not `store`) for every
    /// upload on the host.
    pub fn new(store: ChunkStore, cfg: Config) -> Self {
        let admission = Admission::adaptive(
            "store",
            cfg.in_flight,
            MIN_IN_FLIGHT.min(cfg.in_flight),
            cfg.max_in_flight,
            cfg.demand_reserve,
        );
        Self {
            inner: Arc::new(Inner {
                store: store.with_admission(Arc::clone(&admission)),
                admission,
                hedge_after: cfg.hedge_after,
                peer_race_after: cfg.peer_race_after,
                peers: Mutex::new(HashMap::new()),
                next_registration: AtomicU64::new(0),
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
    /// ([`Self::for_manifest`]) while the registration lives. A later
    /// registration for the same manifest replaces this one.
    pub fn register_peer(&self, manifest: Uuid, peer: Arc<dyn PeerSource>) -> PeerRegistration {
        let id = self.inner.next_registration.fetch_add(1, Ordering::Relaxed);
        self.inner.peers.lock().insert(manifest, (id, peer));
        PeerRegistration {
            inner: Arc::clone(&self.inner),
            manifest,
            id,
        }
    }

    /// This engine with the peer registered for `manifest`, if any.
    pub fn for_manifest(&self, manifest: Uuid) -> Self {
        let peer = self
            .inner
            .peers
            .lock()
            .get(&manifest)
            .map(|(_, peer)| Arc::clone(peer));
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

    /// A slot for one bulk chunk write to local disk at `class` (a volume
    /// being staged), shared with the cache's writes; `None` without a
    /// cache. Hold it until the write returns. A bulk write takes at most
    /// a `Prefix` slot: the `Demand` slots are for the cache writes a
    /// faulting vCPU waits for.
    pub async fn disk_write(&self, class: Class) -> Option<Permit> {
        match self.store().chunk_cache() {
            Some(cache) => Some(cache.write_slot(class.max(Class::Prefix)).await),
            None => None,
        }
    }

    /// The chunk's bytes. A cache hit is served at once; a miss waits for a
    /// `class` slot, then is fetched, verified and stored by the cache.
    /// Without a cache the engine verifies.
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
        if let Some(cache) = self.store().chunk_cache() {
            if let Some(bytes) = cache.read_resident(hash).await? {
                return Ok(bytes);
            }
        }
        let permit = self.inner.admission.acquire(class).await;
        metrics::counter!("engram_fill_fetch_total", "class" => class.as_str()).increment(1);
        self.fetch_verified(hash, class, permit).await
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
            if cache.contains(hash) {
                stats.resident += 1;
                continue;
            }
            // A fetch frees its slot before its cache write, so the slots
            // alone do not bound the chunks this call holds in memory.
            while tasks.len() >= self.inner.admission.limit() {
                let done = tasks.join_next().await.expect("tasks is not empty");
                record(done, &mut stats, &mut first_err);
            }
            // Waiting here, not in the task, keeps the fetch order.
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
        metrics::counter!("engram_fill_fetch_total", "class" => class.as_str()).increment(1);
        match self.store().chunk_cache() {
            Some(cache) => {
                cache
                    .get_with_source(hash, class, || self.fetch_unverified(hash, class, permit))
                    .await
            }
            None => self.fetch_verified(hash, class, permit).await,
        }
    }

    /// [`Self::fetch_unverified`], with the store's bytes hashed here.
    async fn fetch_verified(&self, hash: ChunkHash, class: Class, permit: Permit) -> Result<Bytes> {
        let (bytes, source) = self.fetch_unverified(hash, class, permit).await?;
        if source == FillSource::BlobStorage {
            let actual = ChunkHash::of(&bytes);
            if actual != hash {
                return Err(ChunkStoreError::HashMismatch {
                    expected: hash.to_hex(),
                    actual: actual.to_hex(),
                });
            }
        }
        Ok(bytes)
    }

    /// The peer's answer, when this view has a peer and the peer has the
    /// chunk. Trusted: the peer checked it when it stored it.
    async fn peer_answer(&self, hash: ChunkHash) -> Option<Bytes> {
        self.peer.as_ref()?.fetch(hash).await
    }

    /// [`Self::fetch_tiers`] under `permit`, which is freed when the bytes
    /// are in hand (before the cache writes them). A store answer reports
    /// its outcome to the adaptive limit; a peer answer does not.
    async fn fetch_unverified(
        &self,
        hash: ChunkHash,
        class: Class,
        permit: Permit,
    ) -> Result<(Bytes, FillSource)> {
        let got = self.fetch_tiers(hash, class).await;
        match &got {
            Ok((bytes, FillSource::BlobStorage)) => permit.done(bytes.len() as u64),
            Ok((_, FillSource::Peer)) => drop(permit),
            Err(e) => permit.finish(Err(e)),
        }
        got
    }

    /// The peer (when this view has one), then the store. A `Demand`
    /// fetch races the store against a peer that has not answered after
    /// `peer_race_after`. The caller hashes what came from the store.
    async fn fetch_tiers(&self, hash: ChunkHash, class: Class) -> Result<(Bytes, FillSource)> {
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
