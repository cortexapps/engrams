use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::cache::{slice_chunk_range, FillSource};
use engram_chunk_store::{Admission, ChunkHash, ChunkStore, Class, Permit, Result};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
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
    /// The most slots `Hydrate` and `Background` use together, so a bulk
    /// fill leaves the rest of the limit to `Demand` and `Prefix`.
    pub bulk_in_flight: usize,
    /// A `Demand` fetch still running after this gets a second request; the
    /// first answer wins. `None` turns hedging off.
    pub hedge_after: Option<Duration>,
    /// With a peer, a `Demand` fetch the peer has not answered after this
    /// also asks the store; the first correct answer wins. A peer hit on
    /// the LAN takes about 1-2 ms.
    pub peer_race_after: Duration,
    /// The longest a fetch of any class waits for its peer; then it takes
    /// the store's answer. A peer answers a 512 KiB chunk in about 1-3 ms,
    /// and a batch of 64 in about 200 ms, so 2 s cuts only a peer that has
    /// stopped answering.
    pub peer_timeout: Duration,
    /// `Demand` fetches in flight for one registered sandbox
    /// ([`FillEngine::register_sandbox`]), so one cold restore cannot
    /// take every `Demand` slot from the others. `None`: no cap per
    /// sandbox; all of them share the limit and the reserve.
    pub sandbox_demand_cap: Option<usize>,
}

/// A ranged store GET starts and ends on a multiple of this.
pub const RANGE_ALIGN: u64 = 64 * 1024;

/// The widest ranged store GET, once aligned. Linux caps one block
/// request at about 1 MiB by default, so a guest read fits; and a wider
/// range is a large share of a 16 MiB disk chunk, where one whole-chunk
/// fetch costs about the same and also serves the reads that follow.
pub const MAX_RANGE: u64 = 1024 * 1024;

/// The lowest the adaptive limit goes.
const MIN_IN_FLIGHT: usize = 8;

impl Default for Config {
    fn default() -> Self {
        Self {
            in_flight: 64,
            max_in_flight: 256,
            demand_reserve: 16,
            bulk_in_flight: 64,
            hedge_after: Some(Duration::from_millis(200)),
            peer_race_after: Duration::from_millis(20),
            peer_timeout: Duration::from_secs(2),
            sandbox_demand_cap: None,
        }
    }
}

impl Config {
    /// [`Config::default`], with `ENGRAM_FILL_IN_FLIGHT`,
    /// `ENGRAM_FILL_IN_FLIGHT_MAX`, `ENGRAM_FILL_DEMAND_RESERVE`,
    /// `ENGRAM_FILL_BULK_IN_FLIGHT`,
    /// `ENGRAM_FILL_HEDGE_MS` (0 = off) and
    /// `ENGRAM_FILL_SANDBOX_DEMAND_CAP` read when set.
    /// `ENGRAM_FILL_IN_FLIGHT` sets the start and the ceiling, so the
    /// limit only backs off from it; `ENGRAM_FILL_IN_FLIGHT_MAX` then
    /// raises the ceiling. A set value that does not parse is an error,
    /// and so is an in-flight count or a sandbox cap of 0, or a ceiling
    /// below the start.
    pub fn from_env() -> std::result::Result<Self, String> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    fn from_vars(var: impl Fn(&str) -> Option<String>) -> std::result::Result<Self, String> {
        let get = |name: &str| -> std::result::Result<Option<usize>, String> {
            var(name)
                .map(|v| v.parse().map_err(|e| format!("{name}={v:?}: {e}")))
                .transpose()
        };
        let mut cfg = Self::default();
        if let Some(v) = get("ENGRAM_FILL_IN_FLIGHT")? {
            cfg.in_flight = v;
            cfg.max_in_flight = v;
        }
        if let Some(v) = get("ENGRAM_FILL_IN_FLIGHT_MAX")? {
            cfg.max_in_flight = v;
        }
        if let Some(v) = get("ENGRAM_FILL_DEMAND_RESERVE")? {
            cfg.demand_reserve = v;
        }
        if let Some(v) = get("ENGRAM_FILL_BULK_IN_FLIGHT")? {
            if v == 0 {
                return Err("ENGRAM_FILL_BULK_IN_FLIGHT must be at least 1".into());
            }
            cfg.bulk_in_flight = v;
        }
        if let Some(ms) = get("ENGRAM_FILL_HEDGE_MS")? {
            cfg.hedge_after = (ms > 0).then(|| Duration::from_millis(ms as u64));
        }
        if let Some(cap) = get("ENGRAM_FILL_SANDBOX_DEMAND_CAP")? {
            if cap == 0 {
                return Err("ENGRAM_FILL_SANDBOX_DEMAND_CAP must be at least 1".into());
            }
            cfg.sandbox_demand_cap = Some(cap);
        }
        if cfg.in_flight == 0 {
            return Err("ENGRAM_FILL_IN_FLIGHT must be at least 1".into());
        }
        if cfg.max_in_flight < cfg.in_flight {
            return Err(format!(
                "ENGRAM_FILL_IN_FLIGHT_MAX={} is below the in-flight start {}",
                cfg.max_in_flight, cfg.in_flight
            ));
        }
        Ok(cfg)
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
    /// The `Demand` cap of this view's sandbox, when it has one.
    sandbox_demand: Option<Arc<Semaphore>>,
}

struct Inner {
    store: ChunkStore,
    admission: Arc<Admission>,
    hedge_after: Option<Duration>,
    peer_race_after: Duration,
    peer_timeout: Duration,
    /// Peers by session manifest id, for fetches that know only which
    /// manifest they serve (a page fault).
    peers: Registry<Arc<dyn PeerSource>>,
    /// The `Demand` cap of each registered sandbox, by session manifest id.
    sandbox_demand: Registry<Arc<Semaphore>>,
    sandbox_demand_cap: Option<usize>,
    next_registration: AtomicU64,
}

/// Values by session manifest id, each with the id of the registration
/// that put it there.
struct Registry<T>(Mutex<HashMap<Uuid, (u64, T)>>);

impl<T: Clone> Registry<T> {
    fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    fn insert(&self, manifest: Uuid, id: u64, value: T) {
        self.0.lock().insert(manifest, (id, value));
    }

    fn get(&self, manifest: Uuid) -> Option<T> {
        self.0.lock().get(&manifest).map(|(_, v)| v.clone())
    }

    /// Remove registration `id`, unless a later one replaced it.
    fn remove(&self, manifest: Uuid, id: u64) {
        let mut map = self.0.lock();
        if map.get(&manifest).is_some_and(|(held, _)| *held == id) {
            map.remove(&manifest);
        }
    }
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
        self.inner.peers.remove(self.manifest, self.id);
    }
}

/// Keeps a sandbox's `Demand` cap in force; dropping it removes the
/// cap, unless a later registration for the same manifest replaced it.
pub struct SandboxRegistration {
    inner: Arc<Inner>,
    manifest: Uuid,
    /// `None` when the engine has no cap per sandbox.
    id: Option<u64>,
}

impl Drop for SandboxRegistration {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.inner.sandbox_demand.remove(self.manifest, id);
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
            cfg.bulk_in_flight,
        );
        Self {
            inner: Arc::new(Inner {
                store: store.with_admission(Arc::clone(&admission)),
                admission,
                hedge_after: cfg.hedge_after,
                peer_race_after: cfg.peer_race_after,
                peer_timeout: cfg.peer_timeout,
                peers: Registry::new(),
                sandbox_demand: Registry::new(),
                sandbox_demand_cap: cfg.sandbox_demand_cap,
                next_registration: AtomicU64::new(0),
            }),
            peer: None,
            sandbox_demand: None,
        }
    }

    /// This engine (same limits, same cache) with `peer` asked before the
    /// store.
    pub fn with_peer(&self, peer: Arc<dyn PeerSource>) -> Self {
        Self {
            peer: Some(peer),
            ..self.clone()
        }
    }

    /// Make `peer` the source for fetches of session `manifest`
    /// ([`Self::for_manifest`]) while the registration lives. A later
    /// registration for the same manifest replaces this one.
    pub fn register_peer(&self, manifest: Uuid, peer: Arc<dyn PeerSource>) -> PeerRegistration {
        let id = self.inner.next_registration.fetch_add(1, Ordering::Relaxed);
        self.inner.peers.insert(manifest, id, peer);
        PeerRegistration {
            inner: Arc::clone(&self.inner),
            manifest,
            id,
        }
    }

    /// While the registration lives, the views of session `manifest`
    /// ([`Self::for_manifest`]) share one cap of
    /// [`Config::sandbox_demand_cap`] `Demand` fetches in flight. Without
    /// that cap set, it changes nothing. A later registration for the
    /// same manifest replaces this one; views taken before it keep the
    /// cap they took.
    pub fn register_sandbox(&self, manifest: Uuid) -> SandboxRegistration {
        let id = self.inner.sandbox_demand_cap.map(|cap| {
            let id = self.inner.next_registration.fetch_add(1, Ordering::Relaxed);
            self.inner
                .sandbox_demand
                .insert(manifest, id, Arc::new(Semaphore::new(cap)));
            id
        });
        SandboxRegistration {
            inner: Arc::clone(&self.inner),
            manifest,
            id,
        }
    }

    /// This engine with the peer and the `Demand` cap registered for
    /// `manifest`, if any.
    pub fn for_manifest(&self, manifest: Uuid) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            peer: self.inner.peers.get(manifest),
            sandbox_demand: self.inner.sandbox_demand.get(manifest),
        }
    }

    /// A slot of this view's sandbox cap, for a `Demand` fetch. Take it
    /// before the host slot, so a capped sandbox waits without holding
    /// one. Hold it for the whole fetch, its hedge and peer race included.
    async fn sandbox_slot(&self, class: Class) -> Option<OwnedSemaphorePermit> {
        match (&self.sandbox_demand, class) {
            (Some(cap), Class::Demand) => Some(
                Arc::clone(cap)
                    .acquire_owned()
                    .await
                    .expect("the cap semaphore is never closed"),
            ),
            _ => None,
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
        self.get_admitted(hash, class, None).await
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
        self.fetch_verified(hash, class, None).await
    }

    /// `len` bytes at `offset` of a chunk, for a reader that needs part of
    /// a chunk now (a guest disk read):
    ///
    /// - A resident chunk (in the cache, or in memory while its fill
    ///   writes it) serves the range.
    /// - With a peer, the whole chunk comes through [`Self::get`] at
    ///   `class` (a peer serves only whole chunks), and the range is cut
    ///   from it. So does a range wider than [`MAX_RANGE`] once aligned.
    /// - Otherwise one ranged store GET at `class`, aligned out to
    ///   [`RANGE_ALIGN`], serves the range, and a whole-chunk fill at
    ///   `Hydrate` starts beside it (one flight with every other fill of
    ///   the chunk). Without a cache there is no fill.
    ///
    /// The ranged bytes are not hash-checked: only a whole chunk can be.
    /// They go to the caller only, never to the cache; the cache stores
    /// the chunk only from the whole-chunk fill, after its hash check.
    /// A caller that needs checked bytes uses [`Self::get`].
    pub async fn read_range(
        &self,
        hash: ChunkHash,
        offset: u64,
        len: usize,
        class: Class,
    ) -> Result<Bytes> {
        let cache = self.store().chunk_cache();
        if let Some(cache) = cache {
            if let Some(bytes) = cache.read_resident_range(hash, offset, len).await? {
                return Ok(bytes);
            }
        }
        let lo = offset / RANGE_ALIGN * RANGE_ALIGN;
        let hi = offset
            .saturating_add(len as u64)
            .div_ceil(RANGE_ALIGN)
            .saturating_mul(RANGE_ALIGN);
        if self.peer.is_some() || hi - lo > MAX_RANGE {
            let whole = self.get(hash, class).await?;
            return slice_chunk_range(hash, &whole, offset, len);
        }
        if cache.is_some() {
            let engine = self.clone();
            tokio::spawn(async move {
                if let Err(e) = engine.get(hash, Class::Hydrate).await {
                    tracing::debug!(%hash, error = %e, "fill after a ranged read failed");
                }
            });
        }
        let _sandbox_slot = self.sandbox_slot(class).await;
        let permit = self.inner.admission.acquire(class).await;
        metrics::counter!("engram_fill_range_total", "class" => class.as_str()).increment(1);
        let got = self
            .store()
            .resolver()
            .fetch_chunk_range_unverified(hash, lo, hi - lo)
            .await;
        permit.finish(got.as_ref().map(|b| b.len() as u64));
        slice_chunk_range(hash, &got?, offset - lo, len)
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
            let sandbox_slot = self.sandbox_slot(class).await;
            let permit = self.inner.admission.acquire(class).await;
            let engine = self.clone();
            tasks.spawn(async move {
                let _sandbox_slot = sandbox_slot;
                engine.get_admitted(hash, class, Some(permit)).await
            });
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

    /// [`Self::get`] with an optional slot taken ahead. The cache hands
    /// the slot to this call's fetch, or drops it when the call waits for
    /// another call's fetch of the same chunk.
    async fn get_admitted(
        &self,
        hash: ChunkHash,
        class: Class,
        permit: Option<Permit>,
    ) -> Result<Bytes> {
        match self.store().chunk_cache() {
            Some(cache) => {
                cache
                    .fill_with(hash, class, permit, |permit| {
                        self.fetch_unverified(hash, class, permit)
                    })
                    .await
            }
            None => self.fetch_verified(hash, class, permit).await,
        }
    }

    /// [`Self::fetch_unverified`], with the store's bytes hashed here.
    async fn fetch_verified(
        &self,
        hash: ChunkHash,
        class: Class,
        permit: Option<Permit>,
    ) -> Result<Bytes> {
        let (bytes, source) = self.fetch_unverified(hash, class, permit).await?;
        if source == FillSource::BlobStorage {
            engram_chunk_store::cache::verify_fetched(hash, &bytes).await?;
        }
        Ok(bytes)
    }

    /// The peer's answer, when this view has a peer and the peer has the
    /// chunk within `peer_timeout`. Trusted: the peer checked it when it
    /// stored it.
    async fn peer_answer(&self, hash: ChunkHash) -> Option<Bytes> {
        let peer = self.peer.as_ref()?;
        match tokio::time::timeout(self.inner.peer_timeout, peer.fetch(hash)).await {
            Ok(answer) => answer,
            Err(_) => {
                metrics::counter!("engram_fill_peer_timeout_total").increment(1);
                None
            }
        }
    }

    /// The peer (when this view has one), then the store, under `permit`
    /// (a `class` slot when `None`). A `Demand` fetch races the store
    /// against a peer that has not answered after `peer_race_after`. The
    /// slot is freed when the bytes are in hand (before the cache writes
    /// them); a peer answer frees it without a report to the adaptive
    /// limit. The caller hashes what came from the store.
    async fn fetch_unverified(
        &self,
        hash: ChunkHash,
        class: Class,
        permit: Option<Permit>,
    ) -> Result<(Bytes, FillSource)> {
        // A caller that took a host slot ahead took its sandbox slot too.
        let (permit, _sandbox_slot) = match permit {
            Some(p) => (p, None),
            None => {
                let sandbox_slot = self.sandbox_slot(class).await;
                (self.inner.admission.acquire(class).await, sandbox_slot)
            }
        };
        metrics::counter!("engram_fill_fetch_total", "class" => class.as_str()).increment(1);
        let from_store = |permit: Permit| async move {
            self.fetch_store(hash, class, permit)
                .await
                .map(|b| (b, FillSource::BlobStorage))
        };
        if self.peer.is_none() {
            return from_store(permit).await;
        }
        let peer = self.peer_answer(hash);
        tokio::pin!(peer);
        if class != Class::Demand {
            return match peer.await {
                Some(b) => Ok((b, FillSource::Peer)),
                None => from_store(permit).await,
            };
        }
        tokio::select! {
            got = &mut peer => return match got {
                Some(b) => Ok((b, FillSource::Peer)),
                None => from_store(permit).await,
            },
            () = tokio::time::sleep(self.inner.peer_race_after) => {}
        }
        metrics::counter!("engram_fill_peer_race_total").increment(1);
        let store = from_store(permit);
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

    /// One store fetch under `permit`, hedged for `Demand`: a request still
    /// running after `hedge_after` gets a second one under its own `Demand`
    /// slot, so each request on the wire holds a slot and reports to the
    /// adaptive limit. The caller verifies.
    async fn fetch_store(&self, hash: ChunkHash, class: Class, permit: Permit) -> Result<Bytes> {
        let first = self.store_get(hash, permit);
        let hedge_after = match (class, self.inner.hedge_after) {
            (Class::Demand, Some(d)) => d,
            _ => return first.await,
        };
        tokio::pin!(first);
        tokio::select! {
            r = &mut first => return r,
            () = tokio::time::sleep(hedge_after) => {}
        }
        let second = async {
            let permit = self.inner.admission.acquire(Class::Demand).await;
            metrics::counter!("engram_fill_hedge_total").increment(1);
            self.store_get(hash, permit).await
        };
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

    /// One store GET under `permit`, which reports its outcome.
    async fn store_get(&self, hash: ChunkHash, permit: Permit) -> Result<Bytes> {
        let got = self.store().resolver().fetch_chunk_unverified(hash).await;
        permit.finish(got.as_ref().map(|b| b.len() as u64));
        got
    }
}

/// Chunk-store operations that read chunks (a sparse re-chunk, a
/// materialize) read through the engine like every other fetch.
#[async_trait::async_trait]
impl engram_chunk_store::ChunkSource for FillEngine {
    async fn chunk(&self, hash: ChunkHash, class: Class) -> Result<Bytes> {
        self.get(hash, class).await
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
