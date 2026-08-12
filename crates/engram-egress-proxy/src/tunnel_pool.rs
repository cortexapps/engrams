//! Generic pooling for tunnel upstreams (issue #1201 redesign).
//!
//! One long-lived endpoint per `(session, tunnel id)` replaces the
//! endpoint-per-connection design. A [`TunnelPool`] implements
//! [`TunnelUpstream`], so a connector only supplies an [`EndpointFactory`]
//! and inherits the whole lifecycle: single-flight spawn, generation
//! rotation before credential expiry, crash recovery, stale-serve with a
//! failure backoff, idle reaping, and session teardown.
//!
//! Ownership is the drain mechanism. Every relayed connection holds an
//! `Arc<Generation>`; rotation replaces the pool's `Arc`, and the old
//! endpoint drops exactly when its last live connection drops. There are
//! no connection counters and no drain lists — an endpoint's teardown is
//! its `Drop`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use engram_core::types::integration::SessionTunnel;
use engram_core::SessionId;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::guest_gateway::{TunnelStream, TunnelUpstream};
use crate::time_source::wall_now;

/// Spawns endpoints for one tunnel connector kind.
#[async_trait]
pub trait EndpointFactory: Send + Sync + 'static {
    type Endpoint: TunnelEndpoint;

    /// The connector kind the pool registers as.
    fn kind(&self) -> &'static str;

    /// Build one long-lived endpoint for this tunnel: mint credentials,
    /// start whatever serves the upstream, and stamp its `stale_after`.
    async fn spawn_endpoint(
        &self,
        session_id: SessionId,
        tunnel: &SessionTunnel,
    ) -> std::io::Result<Self::Endpoint>;
}

/// One long-lived upstream endpoint. Teardown is `Drop` — the pool never
/// calls an explicit shutdown, so an endpoint must release its resources
/// (child process, sockets, temp dirs) when the last reference drops.
#[async_trait]
pub trait TunnelEndpoint: Send + Sync + 'static {
    type Conn: AsyncRead + AsyncWrite + Send + Unpin + 'static;

    /// Open one relayed connection through this endpoint.
    async fn connect(&self) -> std::io::Result<Self::Conn>;

    /// When this endpoint must stop serving NEW connections (typically
    /// credential expiry). Established connections are never cut for
    /// staleness. `None` never goes stale.
    fn stale_after(&self) -> Option<DateTime<Utc>>;
}

/// Pool tuning. The defaults fit credential-minted endpoints with
/// ~15-minute token lifetimes.
#[derive(Clone)]
pub struct PoolConfig {
    /// Reap an entry whose endpoint has no live connections and no use
    /// for this long.
    pub idle_after: Duration,
    /// Rotate this long before `stale_after`, so a new connection never
    /// rides a nearly-expired credential.
    pub rotate_buffer: Duration,
    /// Never rotate a generation younger than this. The credential broker
    /// can hand back a cached near-expiry token; without a floor that
    /// token would trigger rotation again immediately, in a loop.
    pub churn_floor: Duration,
    /// After a failed rotation, serve the previous generation without
    /// retrying the spawn for this long (mirrors the inject-refresh
    /// stale-serve policy).
    pub failure_backoff: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            idle_after: Duration::seconds(600),
            rotate_buffer: Duration::seconds(120),
            churn_floor: Duration::seconds(30),
            failure_backoff: Duration::seconds(45),
        }
    }
}

struct Generation<E> {
    endpoint: E,
    created_at: DateTime<Utc>,
    stale_after: Option<DateTime<Utc>>,
}

struct Entry<E> {
    /// The tunnel this entry was spawned for. A policy update can replace
    /// the tunnel definition under the same id; a mismatch rotates.
    tunnel: SessionTunnel,
    current: Arc<Generation<E>>,
    last_used: DateTime<Utc>,
    failure_backoff_until: Option<DateTime<Utc>>,
}

impl<E: TunnelEndpoint> Entry<E> {
    fn new(tunnel: SessionTunnel, endpoint: E, now: DateTime<Utc>) -> Self {
        let stale_after = endpoint.stale_after();
        Self {
            tunnel,
            current: Arc::new(Generation {
                endpoint,
                created_at: now,
                stale_after,
            }),
            last_used: now,
            failure_backoff_until: None,
        }
    }

    /// The current generation should stop serving new connections.
    fn wants_rotation(&self, now: DateTime<Utc>, config: &PoolConfig) -> bool {
        let Some(stale_after) = self.current.stale_after else {
            return false;
        };
        now > stale_after - config.rotate_buffer
            && now - self.current.created_at >= config.churn_floor
    }

    /// The current generation's credential is still hard-valid, so it can
    /// keep serving when a rotation fails.
    fn hard_valid(&self, now: DateTime<Utc>) -> bool {
        self.current
            .stale_after
            .is_none_or(|stale_after| now < stale_after)
    }

    fn in_failure_backoff(&self, now: DateTime<Utc>) -> bool {
        self.failure_backoff_until.is_some_and(|until| now < until)
    }

    fn matches(&self, tunnel: &SessionTunnel) -> bool {
        self.tunnel.config_json == tunnel.config_json
            && self.tunnel.mint_source == tunnel.mint_source
    }
}

type Slot<E> = Arc<tokio::sync::Mutex<Option<Entry<E>>>>;
type PoolKey = (SessionId, String);

/// A pooled implementation of [`TunnelUpstream`]; see the module doc.
pub struct TunnelPool<F: EndpointFactory> {
    factory: F,
    config: PoolConfig,
    /// Slot map. The outer lock is held only for map lookups, never across
    /// an `.await`; the per-slot async mutex single-flights spawn/rotate
    /// per tunnel.
    slots: parking_lot::Mutex<HashMap<PoolKey, Slot<F::Endpoint>>>,
}

impl<F: EndpointFactory> TunnelPool<F> {
    pub fn new(factory: F, config: PoolConfig) -> Self {
        Self {
            factory,
            config,
            slots: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    fn slot(&self, session_id: SessionId, tunnel_id: &str) -> Slot<F::Endpoint> {
        self.slots
            .lock()
            .entry((session_id, tunnel_id.to_string()))
            .or_default()
            .clone()
    }

    async fn open(
        &self,
        session_id: SessionId,
        tunnel: &SessionTunnel,
    ) -> std::io::Result<PooledConn<F::Endpoint>> {
        let slot = self.slot(session_id, &tunnel.id);
        let mut entry = slot.lock().await;
        let now = wall_now();

        // A policy update replaced the tunnel definition under the same id:
        // the old endpoint serves the old config, so start over.
        if entry.as_ref().is_some_and(|e| !e.matches(tunnel)) {
            tracing::info!(
                %session_id,
                tunnel_id = %tunnel.id,
                "tunnel config changed; rotating pooled endpoint",
            );
            *entry = None;
        }

        let needs_fresh = match entry.as_ref() {
            None => true,
            Some(e) => e.wants_rotation(now, &self.config) && !e.in_failure_backoff(now),
        };
        if needs_fresh {
            match self.factory.spawn_endpoint(session_id, tunnel).await {
                Ok(endpoint) => *entry = Some(Entry::new(tunnel.clone(), endpoint, now)),
                Err(error) => match entry.as_mut() {
                    // The old generation's credential is still hard-valid:
                    // serve it and back off the spawn (a stale-credential
                    // failure is recoverable; a dropped connection is not).
                    Some(e) if e.hard_valid(now) => {
                        e.failure_backoff_until = Some(now + self.config.failure_backoff);
                        tracing::warn!(
                            %session_id,
                            tunnel_id = %tunnel.id,
                            %error,
                            "endpoint rotation failed; serving the previous generation",
                        );
                    }
                    _ => return Err(error),
                },
            }
        }

        let e = entry.as_mut().expect("an entry exists after rotation");
        e.last_used = now;
        let generation = e.current.clone();
        match generation.endpoint.connect().await {
            Ok(conn) => Ok(PooledConn {
                conn,
                _generation: generation,
            }),
            // The endpoint died under us (crashed child, closed listener).
            // Replace it once and retry; a second failure surfaces to the
            // gateway as the 502.
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    tunnel_id = %tunnel.id,
                    %error,
                    "pooled endpoint refused a connection; respawning",
                );
                let endpoint = self.factory.spawn_endpoint(session_id, tunnel).await?;
                *entry = Some(Entry::new(tunnel.clone(), endpoint, now));
                let e = entry.as_mut().expect("the entry was just created");
                e.last_used = now;
                let generation = e.current.clone();
                let conn = generation.endpoint.connect().await?;
                Ok(PooledConn {
                    conn,
                    _generation: generation,
                })
            }
        }
    }

    /// One reaper pass at time `now` (injected so tests drive it): reap
    /// idle entries and pre-rotate active near-stale generations so a
    /// guest connect never pays the spawn.
    pub async fn run_once(&self, now: DateTime<Utc>) {
        let slots: Vec<(PoolKey, Slot<F::Endpoint>)> = {
            let map = self.slots.lock();
            map.iter()
                .map(|(key, slot)| (key.clone(), slot.clone()))
                .collect()
        };
        for ((session_id, tunnel_id), slot) in slots {
            // A contended slot is in active use; the connect path handles
            // its own rotation. Skip it rather than queue behind it.
            let Ok(mut entry) = slot.try_lock() else {
                continue;
            };
            let remove = match entry.as_mut() {
                // An empty slot with no waiter (try_lock succeeded) is
                // leftover from a config reset; drop it.
                None => true,
                Some(e) => {
                    let idle = Arc::strong_count(&e.current) == 1
                        && now - e.last_used > self.config.idle_after;
                    if !idle && e.wants_rotation(now, &self.config) && !e.in_failure_backoff(now) {
                        match self.factory.spawn_endpoint(session_id, &e.tunnel).await {
                            Ok(endpoint) => {
                                tracing::debug!(
                                    %session_id,
                                    %tunnel_id,
                                    "pre-rotated near-stale pooled endpoint",
                                );
                                *e = Entry {
                                    last_used: e.last_used,
                                    ..Entry::new(e.tunnel.clone(), endpoint, now)
                                };
                            }
                            Err(error) => {
                                if e.hard_valid(now) {
                                    e.failure_backoff_until =
                                        Some(now + self.config.failure_backoff);
                                }
                                tracing::warn!(
                                    %session_id,
                                    %tunnel_id,
                                    %error,
                                    "pre-rotation spawn failed",
                                );
                            }
                        }
                    }
                    idle
                }
            };
            if remove {
                drop(entry);
                let mut map = self.slots.lock();
                // Only remove the slot we inspected; a racing connect may
                // have installed a fresh one.
                if map
                    .get(&(session_id, tunnel_id.clone()))
                    .is_some_and(|s| Arc::ptr_eq(s, &slot))
                {
                    map.remove(&(session_id, tunnel_id));
                }
            }
        }
    }

    /// The reaper loop: a thin timer around [`Self::run_once`] (ADR 0098
    /// split). Holds only a weak reference, so dropping the pool ends it.
    pub fn spawn_reaper(
        self: &Arc<Self>,
        period: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let pool = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(pool) = pool.upgrade() else { return };
                pool.run_once(wall_now()).await;
            }
        })
    }

    #[cfg(test)]
    fn live_slots(&self) -> usize {
        self.slots.lock().len()
    }
}

#[async_trait]
impl<F: EndpointFactory> TunnelUpstream for TunnelPool<F> {
    fn kind(&self) -> &'static str {
        self.factory.kind()
    }

    async fn connect(
        &self,
        session_id: SessionId,
        tunnel: &SessionTunnel,
    ) -> std::io::Result<Box<dyn TunnelStream>> {
        Ok(Box::new(self.open(session_id, tunnel).await?))
    }

    fn session_closed(&self, session_id: SessionId) {
        let mut map = self.slots.lock();
        let before = map.len();
        map.retain(|(owner, _), _| *owner != session_id);
        let removed = before - map.len();
        if removed > 0 {
            tracing::info!(
                %session_id,
                removed,
                "session closed; dropped its pooled tunnel endpoints",
            );
        }
    }
}

/// One relayed connection plus its grip on the generation that serves it.
/// The generation (and its endpoint) outlives the pool's rotation for as
/// long as any connection holds this.
pub struct PooledConn<E: TunnelEndpoint> {
    conn: E::Conn,
    _generation: Arc<Generation<E>>,
}

impl<E: TunnelEndpoint> AsyncRead for PooledConn<E> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.conn).poll_read(cx, buf)
    }
}

impl<E: TunnelEndpoint> AsyncWrite for PooledConn<E> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.conn).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.conn).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.conn).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct TestEndpoint {
        refuse: Arc<AtomicBool>,
        stale_after: Option<DateTime<Utc>>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for TestEndpoint {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl TunnelEndpoint for TestEndpoint {
        type Conn = tokio::io::DuplexStream;

        async fn connect(&self) -> std::io::Result<Self::Conn> {
            if self.refuse.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("endpoint is dead"));
            }
            let (near, far) = tokio::io::duplex(1024);
            tokio::spawn(async move {
                let (mut reader, mut writer) = tokio::io::split(far);
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
            Ok(near)
        }

        fn stale_after(&self) -> Option<DateTime<Utc>> {
            self.stale_after
        }
    }

    #[derive(Default)]
    struct SpawnState {
        count: AtomicUsize,
        fail_next: AtomicBool,
        stale_after: parking_lot::Mutex<Option<DateTime<Utc>>>,
        refuse_flags: parking_lot::Mutex<Vec<Arc<AtomicBool>>>,
        drop_flags: parking_lot::Mutex<Vec<Arc<AtomicBool>>>,
    }

    struct TestFactory {
        state: Arc<SpawnState>,
    }

    #[async_trait]
    impl EndpointFactory for TestFactory {
        type Endpoint = TestEndpoint;

        fn kind(&self) -> &'static str {
            "test.pooled"
        }

        async fn spawn_endpoint(
            &self,
            _session_id: SessionId,
            _tunnel: &SessionTunnel,
        ) -> std::io::Result<TestEndpoint> {
            if self.state.fail_next.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::other("mint refused"));
            }
            self.state.count.fetch_add(1, Ordering::SeqCst);
            let refuse = Arc::new(AtomicBool::new(false));
            let dropped = Arc::new(AtomicBool::new(false));
            self.state.refuse_flags.lock().push(refuse.clone());
            self.state.drop_flags.lock().push(dropped.clone());
            Ok(TestEndpoint {
                refuse,
                stale_after: *self.state.stale_after.lock(),
                dropped,
            })
        }
    }

    fn pool_with(state: Arc<SpawnState>, config: PoolConfig) -> TunnelPool<TestFactory> {
        TunnelPool::new(TestFactory { state }, config)
    }

    /// Zero churn floor so staleness tests rotate without waiting out the
    /// anti-churn window.
    fn no_floor() -> PoolConfig {
        PoolConfig {
            churn_floor: Duration::zero(),
            ..PoolConfig::default()
        }
    }

    fn tunnel(id: &str, config_json: &str) -> SessionTunnel {
        SessionTunnel {
            id: id.into(),
            connector: "test.pooled".into(),
            config_json: config_json.into(),
            mint_source: None,
        }
    }

    async fn roundtrip(conn: &mut (impl AsyncRead + AsyncWrite + Unpin)) {
        conn.write_all(b"ping").await.unwrap();
        let mut reply = [0_u8; 4];
        conn.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ping");
    }

    #[tokio::test]
    async fn many_connections_reuse_one_endpoint() {
        let state = Arc::new(SpawnState::default());
        let pool = pool_with(state.clone(), PoolConfig::default());
        let session = SessionId::new();
        let t = tunnel("db", "{}");

        let mut first = pool.open(session, &t).await.unwrap();
        roundtrip(&mut first).await;
        let mut second = pool.open(session, &t).await.unwrap();
        roundtrip(&mut second).await;

        assert_eq!(state.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_first_connects_share_one_spawn() {
        let state = Arc::new(SpawnState::default());
        let pool = Arc::new(pool_with(state.clone(), PoolConfig::default()));
        let session = SessionId::new();

        let opens = (0..8).map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move { pool.open(session, &tunnel("db", "{}")).await })
        });
        for open in opens {
            let mut conn = open.await.unwrap().unwrap();
            roundtrip(&mut conn).await;
        }
        assert_eq!(state.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_generation_rotates_and_drains_by_ownership() {
        let state = Arc::new(SpawnState::default());
        // Born already-stale: every later open wants a rotation.
        *state.stale_after.lock() = Some(wall_now() - Duration::seconds(1));
        let pool = pool_with(state.clone(), no_floor());
        let session = SessionId::new();
        let t = tunnel("db", "{}");

        let mut held = pool.open(session, &t).await.unwrap();
        roundtrip(&mut held).await;
        let mut fresh = pool.open(session, &t).await.unwrap();
        roundtrip(&mut fresh).await;

        assert_eq!(state.count.load(Ordering::SeqCst), 2);
        // The replaced generation lives while its connection is held...
        assert!(!state.drop_flags.lock()[0].load(Ordering::SeqCst));
        drop(held);
        // ...and drops with the last connection, with no counter machinery.
        assert!(state.drop_flags.lock()[0].load(Ordering::SeqCst));
        assert!(!state.drop_flags.lock()[1].load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn crashed_endpoint_respawns_once() {
        let state = Arc::new(SpawnState::default());
        let pool = pool_with(state.clone(), PoolConfig::default());
        let session = SessionId::new();
        let t = tunnel("db", "{}");

        let mut conn = pool.open(session, &t).await.unwrap();
        roundtrip(&mut conn).await;
        state.refuse_flags.lock()[0].store(true, Ordering::SeqCst);

        let mut recovered = pool.open(session, &t).await.unwrap();
        roundtrip(&mut recovered).await;
        assert_eq!(state.count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_rotation_serves_the_stale_generation_with_backoff() {
        let state = Arc::new(SpawnState::default());
        // Inside the rotate buffer but still hard-valid.
        *state.stale_after.lock() = Some(wall_now() + Duration::seconds(60));
        let pool = pool_with(state.clone(), no_floor());
        let session = SessionId::new();
        let t = tunnel("db", "{}");

        let mut first = pool.open(session, &t).await.unwrap();
        roundtrip(&mut first).await;

        state.fail_next.store(true, Ordering::SeqCst);
        let mut stale_served = pool.open(session, &t).await.unwrap();
        roundtrip(&mut stale_served).await;
        assert_eq!(state.count.load(Ordering::SeqCst), 1);

        // Backoff holds: the next open does not retry the spawn.
        let mut backed_off = pool.open(session, &t).await.unwrap();
        roundtrip(&mut backed_off).await;
        assert_eq!(state.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_rotation_past_hard_expiry_fails_the_connect() {
        let state = Arc::new(SpawnState::default());
        *state.stale_after.lock() = Some(wall_now() - Duration::seconds(1));
        let pool = pool_with(state.clone(), no_floor());
        let session = SessionId::new();
        let t = tunnel("db", "{}");

        let _first = pool.open(session, &t).await.unwrap();
        state.fail_next.store(true, Ordering::SeqCst);
        let Err(error) = pool.open(session, &t).await else {
            panic!("a hard-expired generation with a failed spawn must not serve");
        };
        assert_eq!(error.to_string(), "mint refused");
    }

    #[tokio::test]
    async fn churn_floor_blocks_immediate_rerotation() {
        let state = Arc::new(SpawnState::default());
        // Wants rotation by expiry, but the generation is younger than the
        // (huge) churn floor, so the pool must keep it.
        *state.stale_after.lock() = Some(wall_now() + Duration::seconds(60));
        let pool = pool_with(
            state.clone(),
            PoolConfig {
                churn_floor: Duration::seconds(3600),
                ..PoolConfig::default()
            },
        );
        let session = SessionId::new();
        let t = tunnel("db", "{}");

        let _first = pool.open(session, &t).await.unwrap();
        let _second = pool.open(session, &t).await.unwrap();
        assert_eq!(state.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn config_change_rotates_the_endpoint() {
        let state = Arc::new(SpawnState::default());
        let pool = pool_with(state.clone(), PoolConfig::default());
        let session = SessionId::new();

        let _old = pool
            .open(session, &tunnel("db", "{\"instance\":\"a\"}"))
            .await
            .unwrap();
        let mut new = pool
            .open(session, &tunnel("db", "{\"instance\":\"b\"}"))
            .await
            .unwrap();
        roundtrip(&mut new).await;
        assert_eq!(state.count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn run_once_reaps_idle_entries() {
        let state = Arc::new(SpawnState::default());
        let pool = pool_with(state.clone(), PoolConfig::default());
        let session = SessionId::new();
        let t = tunnel("db", "{}");

        let conn = pool.open(session, &t).await.unwrap();
        drop(conn);
        assert_eq!(pool.live_slots(), 1);

        // Not idle long enough: kept.
        pool.run_once(wall_now()).await;
        assert_eq!(pool.live_slots(), 1);

        pool.run_once(wall_now() + PoolConfig::default().idle_after + Duration::seconds(1))
            .await;
        assert_eq!(pool.live_slots(), 0);
        assert!(state.drop_flags.lock()[0].load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_once_keeps_an_entry_with_a_live_connection() {
        let state = Arc::new(SpawnState::default());
        let pool = pool_with(state.clone(), PoolConfig::default());
        let session = SessionId::new();

        let _held = pool.open(session, &tunnel("db", "{}")).await.unwrap();
        pool.run_once(wall_now() + PoolConfig::default().idle_after + Duration::seconds(1))
            .await;
        assert_eq!(pool.live_slots(), 1);
    }

    #[tokio::test]
    async fn run_once_pre_rotates_a_near_stale_active_generation() {
        let state = Arc::new(SpawnState::default());
        *state.stale_after.lock() = Some(wall_now() - Duration::seconds(1));
        let pool = pool_with(state.clone(), no_floor());
        let session = SessionId::new();
        let t = tunnel("db", "{}");

        let held = pool.open(session, &t).await.unwrap();
        // Fresh endpoints from now on never go stale.
        *state.stale_after.lock() = None;
        pool.run_once(wall_now()).await;
        assert_eq!(state.count.load(Ordering::SeqCst), 2);

        // The guest connect after pre-rotation pays no spawn.
        let mut conn = pool.open(session, &t).await.unwrap();
        roundtrip(&mut conn).await;
        assert_eq!(state.count.load(Ordering::SeqCst), 2);

        assert!(!state.drop_flags.lock()[0].load(Ordering::SeqCst));
        drop(held);
        assert!(state.drop_flags.lock()[0].load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn session_closed_drops_only_that_sessions_endpoints() {
        let state = Arc::new(SpawnState::default());
        let pool = pool_with(state.clone(), PoolConfig::default());
        let (a, b) = (SessionId::new(), SessionId::new());

        let mut held = pool.open(a, &tunnel("db", "{}")).await.unwrap();
        let _other = pool.open(b, &tunnel("db", "{}")).await.unwrap();
        assert_eq!(pool.live_slots(), 2);

        pool.session_closed(a);
        assert_eq!(pool.live_slots(), 1);

        // The in-flight connection finishes against its generation.
        roundtrip(&mut held).await;
        assert!(!state.drop_flags.lock()[0].load(Ordering::SeqCst));
        drop(held);
        assert!(state.drop_flags.lock()[0].load(Ordering::SeqCst));
    }
}
