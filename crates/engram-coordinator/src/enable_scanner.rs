//! ADR 0036 — async image-enable scanner.
//!
//! Background task that drives `enable_jobs` rows through
//! `pending → materializing → capturing → prestaging → ready | failed`.
//! Sibling to [`crate::evac_resumer`]: same polling shape, same
//! shared-state surface, distinct table.
//!
//! ## Flow
//!
//! 1. Tick: atomically claim up to `claim_limit` non-terminal jobs
//!    whose lease is free or expired
//!    ([`engram_core::traits::MetadataStore::claim_enable_jobs`]).
//! 2. Per job, run the enable pipeline from the top — every step
//!    fast-forwards or is cheap to re-run, so a job resumed after a
//!    coordinator crash converges:
//!    - `materializing` (ADR 0080 phase 3b, HOST-side):
//!      [`crate::api::enabled_images::materialize_image_on_host`] picks
//!      a disk-healthy host and drives the streaming `MaterializeImage`
//!      RPC — the host pulls the STANDARD docker image, packs a
//!      bootable ext4, and chunks it into its write-through chunk
//!      store (→ BlobStorage). Every stage frame the host streams is
//!      persisted onto the job row
//!      (`update_enable_job_materialize_progress`) — the operator's
//!      progress line AND the claim renewal that stops a peer from
//!      stealing a long materialize. A re-run re-pulls, but chunk PUTs
//!      content-address-dedup and the content-derived ManifestRef
//!      reproduces, so convergence is exact.
//!    - `capturing`: [`crate::api::enabled_images::capture_and_record_base_snapshot`]
//!      boots the capture VM from the materialized manifest on a host
//!      (idempotent via the content/digest-keyed reuse checks).
//!    - `prestaging` (ADR 0036 amendment, issue #538, INTERIM): advertise
//!      the freshly-captured base snapshot as a `prestage_images`
//!      heartbeat-ack entry and wait for every eligible (`stages_images`)
//!      host to report the digest in `ready_images`, or a deadline
//!      (`ENGRAM_ENABLE_PRESTAGE_TIMEOUT_SECS`, default 1200 s) — see
//!      [`eval_prestage`]. A fleet where NO host has `stages_images`
//!      (dev/Process backend) passes vacuously; a fleet that has
//!      staging-capable hosts but none currently schedulable (e.g. a
//!      MIG roll blip) keeps polling under the deadline instead — see
//!      review finding 1 on PR #565.
//!    - upsert the `enabled_images` row → `ready`.
//! 3. On any pipeline error: record the failure (bumps `attempts`,
//!    stores `error`, releases the claim) and leave the job in its
//!    current state for the next tick. After `max_attempts` the job
//!    flips to `failed` — `POST /api/v1/enable-jobs/:id/retry`
//!    re-queues it.
//!
//! ## Why this pattern
//!
//! Per `[async_via_state_machine]` — the enable pipeline does minutes
//! of I/O (10 GB-class images) plus a VM boot. Synchronous
//! orchestration inside the POST handler meant the external LB's
//! ~30 s timeout killed every real enable (hence the port-forward +
//! curl ritual this scanner retires). Multi-coordinator safety comes
//! from the lease claim: one pod owns a job at a time; a crashed
//! pod's claim expires and a peer re-claims, resuming idempotently.

use std::time::Duration;

use chrono::{DateTime, Utc};
use engram_core::types::host::HostRecord;
use engram_core::types::{EnableJob, EnableJobState};
use engram_core::MetaError;

use crate::api::enabled_images::{
    capture_and_record_base_snapshot, materialize_image_on_host, new_enable_row,
};
use crate::state::SharedState;

#[derive(Clone, Debug)]
pub struct EnableScannerConfig {
    /// Sweep cadence. Enables are operator-interactive (someone is
    /// watching a progress bar), so this is snappier than the 10s
    /// fleet scanners.
    pub poll_interval: Duration,
    /// Claim lease. Must comfortably exceed the longest gap between
    /// claim renewals — both the materialize AND the capture step run a
    /// ticker that renews every `progress_interval`. (Until that ticker
    /// covered the capture step, a long `[warm]`-hook capture — tens of
    /// minutes of warm boot + snapshot upload — would expire this lease
    /// mid-capture, and a peer would re-claim and spawn a duplicate
    /// concurrent capture that fought the first for host resources.)
    pub lease_secs: u32,
    /// Jobs claimed per tick. Enables are heavyweight (registry +
    /// GCS I/O, then a capture VM per job); a small bound keeps one
    /// coord pod from absorbing the whole queue.
    pub claim_limit: u32,
    /// Pipeline failures per job before flipping to `failed`.
    pub max_attempts: u32,
    /// How often the materialize progress counter is checkpointed to
    /// PG (progress bar + claim renewal).
    pub progress_interval: Duration,
    /// ADR 0036 amendment (issue #538): how long the `prestaging` stage
    /// waits for every eligible host to report the digest before
    /// proceeding to `ready` with stragglers recorded `timed_out`.
    /// `ENGRAM_ENABLE_PRESTAGE_TIMEOUT_SECS`, default 1200 s (minutes-class
    /// — dev-brain-sized images pull ~33 GB through a 16-permit semaphore;
    /// see `engram_enable_prestage_seconds` before retuning).
    pub prestage_timeout: Duration,
}

/// Every field here is a pure constant — no I/O. Env resolution
/// (`ENGRAM_ENABLE_PRESTAGE_TIMEOUT_SECS`) happens at the use-site via
/// [`EnableScannerConfig::from_env`], cf. `placement::placement_ttl`
/// (review finding 6, PR #565: a `Default` impl doing env I/O + silently
/// swallowing a rejected value is surprising and untestable).
impl Default for EnableScannerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(3),
            // Capture on a nested-KVM dev-vm has been observed at
            // ~150 s; 300 s keeps a healthy margin before a peer
            // declares the claim stale.
            lease_secs: 300,
            claim_limit: 2,
            max_attempts: 5,
            progress_interval: Duration::from_secs(2),
            prestage_timeout: DEFAULT_PRESTAGE_TIMEOUT,
        }
    }
}

const DEFAULT_PRESTAGE_TIMEOUT: Duration = Duration::from_secs(1200);

impl EnableScannerConfig {
    /// Resolves `ENGRAM_ENABLE_PRESTAGE_TIMEOUT_SECS` on top of the pure
    /// [`Default`]. This is the constructor `spawn`'s caller should use in
    /// production; tests that want the bare constant use `::default()` (or
    /// `..Default::default()`) directly. Unlike the old `Default` impl, an
    /// unparseable or non-positive value is NOT silently swallowed — it's
    /// a config typo (e.g. `=0` plausibly meant "skip the wait"), so it's
    /// worth a `warn!` on boot rather than a silent 1200s.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(raw) = std::env::var("ENGRAM_ENABLE_PRESTAGE_TIMEOUT_SECS") {
            match raw.parse::<u64>() {
                Ok(secs) if secs > 0 => cfg.prestage_timeout = Duration::from_secs(secs),
                Ok(_) => tracing::warn!(
                    raw = %raw,
                    default_secs = DEFAULT_PRESTAGE_TIMEOUT.as_secs(),
                    "ENGRAM_ENABLE_PRESTAGE_TIMEOUT_SECS must be > 0; using the default"
                ),
                Err(e) => tracing::warn!(
                    raw = %raw,
                    error = %e,
                    default_secs = DEFAULT_PRESTAGE_TIMEOUT.as_secs(),
                    "ENGRAM_ENABLE_PRESTAGE_TIMEOUT_SECS is not a valid u64; using the default"
                ),
            }
        }
        cfg
    }
}

/// Spawn the scanner as a background task. Caller holds the
/// JoinHandle for the process lifetime; dropping aborts the loop.
/// Mirrors [`crate::evac_resumer::spawn`].
pub fn spawn(cfg: EnableScannerConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the first immediate tick — coord just started, give
        // hosts a beat to heartbeat in before capture-host picking.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "enable-scanner tick failed; will retry");
            }
        }
    })
}

/// Single scanner tick. `pub(crate)` so live-PG tests can drive the
/// scanner deterministically without `tokio::spawn`-ing the loop.
pub(crate) async fn run_once(
    cfg: &EnableScannerConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Pod identity for the claim audit trail: the k8s pod name in
    // prod (HOSTNAME), the machine hostname in dev. Purely
    // observability — claim correctness comes from the atomic UPDATE.
    let claimant = std::env::var("HOSTNAME").unwrap_or_else(|_| "coord".into());
    let jobs = state
        .services
        .meta
        .claim_enable_jobs(&claimant, cfg.lease_secs, cfg.claim_limit)
        .await?;
    if jobs.is_empty() {
        return Ok(());
    }
    tracing::debug!(count = jobs.len(), "enable-scanner claimed jobs");
    for job in jobs {
        let job_id = job.id;
        match advance_one(cfg, state, &claimant, job).await {
            Ok(()) => {}
            Err(AdvanceError::LeaseLost(msg)) => {
                // #232: our lease expired and a peer re-claimed the job
                // mid-flight. Abandon immediately — every state write is
                // fenced, so we hold no authority over the row anymore.
                // Recording a failure here would clear the new
                // claimant's lease and stamp `error` on a job it is
                // actively completing. Do nothing; the peer drives it.
                tracing::warn!(%job_id, reason = %msg, "enable job lease lost; abandoning to peer");
            }
            Err(e @ (AdvanceError::Pipeline(_) | AdvanceError::NonRetryable(_))) => {
                // Per-job failure: ONE atomic, fenced write bumps attempts,
                // stores the error, releases the claim, AND flips to `failed`
                // if the budget is spent (transient) or the failure is
                // non-retryable (deterministic — bail fast). Keep sweeping;
                // one wedged job must not stall the queue. A Conflict means
                // the lease went away between the failure and now, so we drop.
                let force_terminal = matches!(e, AdvanceError::NonRetryable(_));
                let msg = match &e {
                    AdvanceError::Pipeline(inner) | AdvanceError::NonRetryable(inner) => {
                        inner.to_string()
                    }
                    AdvanceError::LeaseLost(_) => unreachable!("guarded by the outer pattern"),
                };
                tracing::warn!(%job_id, error = %msg, non_retryable = force_terminal, "enable job pipeline failed");
                match state
                    .services
                    .meta
                    .record_enable_job_failure(
                        job_id,
                        &claimant,
                        &msg,
                        cfg.max_attempts,
                        force_terminal,
                    )
                    .await
                {
                    Ok((attempts, EnableJobState::Failed)) => {
                        tracing::warn!(
                            %job_id,
                            attempts,
                            max_attempts = cfg.max_attempts,
                            non_retryable = force_terminal,
                            "enable job marked failed",
                        );
                    }
                    Ok(_) => {}
                    Err(MetaError::Conflict(msg)) => {
                        tracing::warn!(%job_id, reason = %msg, "enable job lease lost while recording failure; abandoning to peer");
                    }
                    Err(e2) => {
                        tracing::warn!(%job_id, error = %e2, "failed to record enable job failure");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Outcome of [`advance_one`] when it doesn't complete the pipeline.
enum AdvanceError {
    /// A fenced state write returned [`MetaError::Conflict`]: the lease
    /// expired and a peer re-claimed the job. The worker must abandon
    /// the job WITHOUT any further state writes (#232).
    LeaseLost(String),
    /// A transient pipeline error (registry/GCS, or a `NoCapacity` from
    /// the capture host picker). Eligible for the attempts-budget retry
    /// path — it may clear on its own next tick.
    Pipeline(Box<dyn std::error::Error + Send + Sync>),
    /// A DETERMINISTIC failure that retrying cannot fix — most importantly
    /// a `[warm]` hook that exits non-zero. Bail fast: fail the job on the
    /// first occurrence instead of re-loading + re-capturing the image
    /// `max_attempts` times for nothing. The operator can `RetryEnableJob`
    /// after fixing the image.
    NonRetryable(Box<dyn std::error::Error + Send + Sync>),
}

impl From<MetaError> for AdvanceError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Conflict(msg) => AdvanceError::LeaseLost(msg),
            other => AdvanceError::Pipeline(Box::new(other)),
        }
    }
}

/// Classify a base-snapshot capture failure. `ApiError::Unavailable` is the
/// capture-host picker's `NoCapacity` — transient, retry.
///
/// Issue #539: `ApiError::CaptureFailed` carries a `CaptureFailureKind` —
/// only `WarmExecTransport` (the exec stream died mid-run, e.g. a vsock/
/// gRPC connection loss) is retryable via the attempts budget; every other
/// kind (`WarmExitNonZero`/`WarmStall`/`WarmStageDeadline`/
/// `WarmGlobalTimeout`/`SnapshotFailed`) is a deterministic outcome that
/// retrying can't fix — bail fast on the first occurrence, same as the old
/// blanket `ApiError::Internal` treatment (a `[warm]` hook non-zero exit,
/// a snapshot that failed HEAD-verify, …).
fn classify_capture_error(e: crate::error::ApiError) -> AdvanceError {
    match e {
        crate::error::ApiError::Unavailable(_) => AdvanceError::Pipeline(Box::new(e)),
        crate::error::ApiError::CaptureFailed { kind, .. } if kind.is_retryable() => {
            AdvanceError::Pipeline(Box::new(e))
        }
        other => AdvanceError::NonRetryable(Box::new(other)),
    }
}

/// ADR 0080 phase 3b: classify a host-side materialize failure.
/// `ApiError::Unavailable` covers the host picker's `NoCapacity` plus
/// connect-time transport/WIRE_VERSION deaths — transient, retry.
/// `ApiError::MaterializeFailed` defers to the kind's own contract
/// (`MaterializeFailureKind::is_retryable`): busy / disk / registry /
/// store / mid-stream transport retry via the attempts budget (each
/// retry re-picks a host); a too-large image or deterministic content
/// problem bails fast on the first occurrence.
fn classify_materialize_error(e: crate::error::ApiError) -> AdvanceError {
    match e {
        crate::error::ApiError::Unavailable(_) => AdvanceError::Pipeline(Box::new(e)),
        crate::error::ApiError::MaterializeFailed { kind, .. } if kind.is_retryable() => {
            AdvanceError::Pipeline(Box::new(e))
        }
        other => AdvanceError::NonRetryable(Box::new(other)),
    }
}

/// Run the enable pipeline for one claimed job. Every step is
/// idempotent, so this always starts from the top and fast-forwards:
/// metadata pull is KBs, materialize skips present chunks, capture
/// reuses a digest-matched base snapshot.
async fn advance_one(
    cfg: &EnableScannerConfig,
    state: &SharedState,
    claimant: &str,
    job: EnableJob,
) -> Result<(), AdvanceError> {
    let job_id = job.id;
    let image_uri = job.image_uri.clone();
    tracing::info!(
        %job_id,
        %image_uri,
        from_state = job.state.as_str(),
        attempt = job.attempts + 1,
        "enable-scanner: driving job",
    );

    // Pipeline I/O (registry/host RPC/capture) is a `Pipeline` error;
    // fenced store writes (`?` on `MetaError`) become `LeaseLost` on
    // Conflict via `From<MetaError>` and bubble all the way out,
    // abandoning the job without further writes.
    //
    // ADR 0080: the full image config rides the job (set on enable/
    // update, inherited on refresh); the ready-time upsert persists it
    // for the dashboard's edit form, so config edits stay invisible to
    // session-create until the new base snapshot actually exists. A
    // config that fails validation is deterministic — bail fast (the
    // POST-time validation makes this unreachable in practice, but a
    // job written by an older coordinator must not loop the budget).
    job.image_config.validate().map_err(|e| {
        AdvanceError::NonRetryable(Box::new(crate::error::ApiError::BadRequest(format!(
            "image config for `{image_uri}`: {e}"
        ))))
    })?;
    let mut row = new_enable_row(&image_uri, &job.image_config);

    // ---- materializing (host-side, ADR 0080 phase 3b) ----
    state
        .services
        .meta
        .set_enable_job_state(job_id, claimant, EnableJobState::Materializing)
        .await?;
    // The host streams a stage frame (`pull → flatten → pack → chunk`)
    // per transition plus a ≤30 s keepalive re-send; each persisted
    // frame is the operator's progress line AND the fenced claim
    // renewal (`update_enable_job_materialize_progress` bumps
    // `claimed_at`) — renewals stop exactly when the stream dies,
    // letting a peer legitimately re-claim. Mirrors the capture
    // consumer below.
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::channel::<engram_core::types::MaterializeProgress>(64);
    let progress_consumer = {
        let meta = state.services.meta.clone();
        let claimant = claimant.to_string();
        tokio::spawn(async move {
            while let Some(frame) = progress_rx.recv().await {
                match meta
                    .update_enable_job_materialize_progress(job_id, &claimant, &frame)
                    .await
                {
                    Ok(()) => {}
                    Err(MetaError::Conflict(msg)) => {
                        tracing::warn!(%job_id, reason = %msg, "enable materialize progress write lost the lease; abandoning to peer");
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(%job_id, error = %e, "enable materialize progress write failed");
                    }
                }
            }
        })
    };
    let materialize_result = materialize_image_on_host(state, &image_uri, progress_tx).await;
    // `materialize_image_on_host` returning means every `Sender` clone
    // is dropped — awaiting the consumer guarantees the final frame is
    // persisted before we act on the result (same ordering property as
    // the capture consumer below).
    let _ = progress_consumer.await;
    let materialized = materialize_result.map_err(classify_materialize_error)?;
    tracing::info!(
        %job_id,
        %image_uri,
        disk_manifest = %materialized.disk_manifest,
        manifest_digest = %materialized.manifest_digest,
        ext4_size_bytes = materialized.ext4_size_bytes,
        "enable job materialized image on host",
    );
    row.disk_manifest = Some(materialized.disk_manifest);
    row.oci_defaults = materialized.oci_defaults;
    row.manifest_digest = materialized.manifest_digest;

    // ---- capturing ----
    state
        .services
        .meta
        .set_enable_job_state(job_id, claimant, EnableJobState::Capturing)
        .await?;
    // Issue #539: `build_base_snapshot` now streams `CaptureProgress` at
    // least every 30s (host keepalive) for the whole capture, so THIS
    // replaces the old blind lease-renewal ticker (deleted — it used to
    // re-write the static `done` count purely to keep the claim alive):
    // every progress write doubles as the renewal
    // (`update_enable_job_capture_progress` bumps `claimed_at`), so a
    // `[warm]`-hook capture that runs tens of minutes past `lease_secs`
    // still holds its claim — and renewals stop exactly when the stream
    // dies (a transport failure), letting a peer legitimately re-claim
    // instead of racing a still-healthy owner.
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::channel::<engram_core::types::CaptureProgress>(64);
    let progress_consumer = {
        let meta = state.services.meta.clone();
        let claimant = claimant.to_string();
        tokio::spawn(async move {
            while let Some(event) = progress_rx.recv().await {
                match meta
                    .update_enable_job_capture_progress(job_id, &claimant, &event)
                    .await
                {
                    Ok(()) => {}
                    Err(MetaError::Conflict(msg)) => {
                        tracing::warn!(%job_id, reason = %msg, "enable capture progress write lost the lease; abandoning to peer");
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(%job_id, error = %e, "enable capture progress write failed");
                    }
                }
            }
        })
    };
    let capture_result = capture_and_record_base_snapshot(state, &row, progress_tx).await;
    // `capture_and_record_base_snapshot` returning means every `Sender`
    // clone it (or the host RPC underneath it) held has been dropped —
    // awaiting the consumer here guarantees every progress event,
    // INCLUDING the very last one written right before a failure, is
    // persisted before we act on `capture_result`. This is load-bearing
    // for the "failing stage + tail survive a WarmExecTransport kill"
    // acceptance criterion: `record_enable_job_failure` (below) never
    // touches these columns itself — it relies on this write having
    // already landed.
    let _ = progress_consumer.await;
    let (base_snapshot_id, base_snapshot_disk_manifest, base_snapshot_memory_manifest) =
        capture_result.map_err(classify_capture_error)?;
    row.base_snapshot_id = Some(base_snapshot_id);
    row.base_snapshot_disk_manifest = Some(base_snapshot_disk_manifest);
    // `None` for cold-boot backends (VZ) — no memory snapshot to stamp.
    row.base_snapshot_memory_manifest = base_snapshot_memory_manifest;

    // ---- prestaging (ADR 0036 amendment, issue #538, INTERIM) ----
    //
    // Advertise the freshly-captured base snapshot to the fleet BEFORE the
    // `enabled_images` upsert below makes the digest visible to
    // session-create — closing the window where the first restore after a
    // refresh pulls ~thousands of chunks from GCS on demand (90-118 s,
    // observed failing outright 3-for-3 in prod). All pieces already
    // exist: the per-host prefetch supervisor and the placement digest
    // gate; this stage just sequences the enable flip to happen AFTER the
    // fleet has warmed, not at the same instant as the first user create.
    let image_ref = crate::api::host_http::enabled_image_ref(&row).ok_or_else(|| {
        // Can't happen in practice — base_snapshot_id/disk_manifest were
        // just stamped `Some` three lines up — but bail loudly rather than
        // silently skip prestage and race the create path anyway.
        AdvanceError::NonRetryable(Box::new(std::io::Error::other(format!(
            "enable job {job_id}: captured row has no advertisable base-snapshot refs",
        ))))
    })?;
    let prestage_ref_json =
        serde_json::to_value(&image_ref).map_err(|e| AdvanceError::NonRetryable(Box::new(e)))?;
    state
        .services
        .meta
        .begin_enable_job_prestage(job_id, claimant, prestage_ref_json)
        .await?;

    // Renew the claim lease throughout the wait, same trick as the capture
    // consumer above (progress is static — materialize/capture are done —
    // so the write is purely a `claimed_at` renewal; post-ADR-0080 the
    // chunk counters are vestigial and stay 0).
    let prestage_ticker = {
        let meta = state.services.meta.clone();
        let interval = cfg.progress_interval;
        let claimant = claimant.to_string();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await;
            loop {
                tick.tick().await;
                match meta
                    .update_enable_job_progress(job_id, &claimant, 0, None)
                    .await
                {
                    Ok(()) => {}
                    Err(MetaError::Conflict(msg)) => {
                        tracing::warn!(%job_id, reason = %msg, "enable prestage lease lost; stopping renewal ticker");
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(%job_id, error = %e, "enable prestage lease renewal failed");
                    }
                }
            }
        })
    };

    let digest = image_ref.manifest_digest.as_str().to_string();
    let deadline = tokio::time::Instant::now() + cfg.prestage_timeout;
    let wait_started = std::time::Instant::now();
    // Last-known counts from a successful poll, surfaced in the TimedOut
    // outcome if the deadline is hit inside the error arm below (review
    // finding 2, PR #565) — a persistent `list_active_hosts` failure
    // shouldn't discard whatever we last observed.
    let mut last_seen = (0usize, 0usize); // (staged, eligible)
    let prestage_outcome = loop {
        let hosts = match state.services.meta.list_active_hosts().await {
            Ok(hosts) => hosts,
            Err(e) => {
                tracing::warn!(%job_id, error = %e, "enable prestage: list_active_hosts failed; retrying");
                // A persistent PG read failure must still respect the
                // documented hard deadline (acceptance criterion 3) instead
                // of spinning forever — `run_once` drives claimed jobs
                // sequentially, so a wedged wait here stalls every other
                // claimed enable job too.
                if tokio::time::Instant::now() >= deadline {
                    break PrestageOutcome::TimedOut {
                        staged: last_seen.0,
                        eligible: last_seen.1,
                    };
                }
                tokio::time::sleep(
                    cfg.poll_interval
                        .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                )
                .await;
                continue;
            }
        };
        match eval_prestage(
            &hosts,
            &digest,
            Utc::now(),
            crate::placement::placement_ttl(),
        ) {
            PrestageEval::Complete => break PrestageOutcome::Complete,
            PrestageEval::EmptyFleet => break PrestageOutcome::EmptyFleet,
            PrestageEval::Waiting { staged, eligible } => {
                last_seen = (staged, eligible);
                if tokio::time::Instant::now() >= deadline {
                    break PrestageOutcome::TimedOut { staged, eligible };
                }
                tokio::time::sleep(
                    cfg.poll_interval
                        .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                )
                .await;
            }
        }
    };
    prestage_ticker.abort();
    ::metrics::histogram!(
        crate::metrics::ENABLE_PRESTAGE_SECONDS,
        "outcome" => prestage_outcome.metric_label(),
    )
    .record(wait_started.elapsed().as_secs_f64());

    // Zero-staged timeout: transient (a fleet mid-roll, or every staging
    // host briefly unreachable) — retry under the attempts budget rather
    // than flip ready with nothing warm.
    if let PrestageOutcome::TimedOut {
        staged: 0,
        eligible,
    } = prestage_outcome
    {
        return Err(AdvanceError::Pipeline(Box::new(
            crate::error::ApiError::Unavailable(format!(
            "enable job {job_id}: prestage deadline hit with 0/{eligible} eligible hosts staged",
        )),
        )));
    }

    // Record the per-host outcome map (audit / dashboard surface) — one
    // more hosts read so the map reflects the hosts as of stage-end, not
    // the last poll (a host that appeared mid-wait should show up here).
    let waited_ms = wait_started.elapsed().as_millis() as u64;
    match state.services.meta.list_active_hosts().await {
        Ok(hosts) => {
            let entries = prestage_host_outcomes(
                &hosts,
                &digest,
                Utc::now(),
                crate::placement::placement_ttl(),
                waited_ms,
            );
            for (_, outcome, _) in &entries {
                ::metrics::counter!(
                    crate::metrics::ENABLE_PRESTAGE_HOST_OUTCOMES_TOTAL,
                    "outcome" => *outcome,
                )
                .increment(1);
            }
            let map: serde_json::Map<String, serde_json::Value> = entries
                .into_iter()
                .map(|(host_id, outcome, waited_ms)| {
                    let entry = match waited_ms {
                        Some(ms) => serde_json::json!({ "outcome": outcome, "waited_ms": ms }),
                        None => serde_json::json!({ "outcome": outcome }),
                    };
                    (host_id, entry)
                })
                .collect();
            state
                .services
                .meta
                .set_enable_job_prestage_hosts(job_id, claimant, serde_json::Value::Object(map))
                .await?;
        }
        Err(e) => {
            // Best-effort: the audit map is operator-facing, not correctness-
            // bearing (the create-path gate reads `ready_images` directly,
            // not this column) — don't fail the whole enable over it.
            tracing::warn!(%job_id, error = %e, "enable prestage: list_active_hosts failed while recording outcomes");
        }
    }

    // Log the terminal outcome for operator forensics (the audit map above
    // is the durable record; this is the same-tick log line).
    match prestage_outcome {
        PrestageOutcome::Complete => {
            tracing::info!(%job_id, %digest, "enable prestage: all eligible hosts staged");
        }
        PrestageOutcome::EmptyFleet => {
            tracing::info!(%job_id, %digest, "enable prestage: no eligible staging hosts in the fleet; vacuous pass");
        }
        PrestageOutcome::TimedOut { staged, eligible } => {
            tracing::warn!(%job_id, %digest, staged, eligible, "enable prestage: deadline hit with stragglers; proceeding to ready — stragglers self-heal via the per-host readiness gate");
        }
    }

    // ---- ready ----
    state
        .services
        .meta
        .upsert_enabled_image(row)
        .await
        .map_err(AdvanceError::from)?;
    state
        .services
        .meta
        .set_enable_job_state(job_id, claimant, EnableJobState::Ready)
        .await?;
    tracing::info!(%job_id, %image_uri, "enable job ready; image enabled");
    Ok(())
}

/// ADR 0036 amendment (issue #538): outcome of one `prestaging`-stage poll —
/// pure decision over a hosts snapshot, unit-tested without I/O. Eligible =
/// [`crate::placement::host_is_schedulable`] ∧ `stages_images`; staged =
/// eligible ∧ `ready_images` contains the digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrestageEval {
    /// Every eligible host has staged the digest.
    Complete,
    /// At least one eligible host hasn't staged yet — including the
    /// transient "zero eligible right now" case (every staging-capable
    /// host is momentarily unschedulable, e.g. mid host-agent MIG roll)
    /// via `staged: 0, eligible: 0`. The deadline / zero-staged-timeout
    /// retry policy applies exactly as it does to a genuine partial wait.
    Waiting { staged: usize, eligible: usize },
    /// No host in the fleet has `stages_images` at all (Process/dev
    /// fleet) — genuinely nothing will ever report, so the stage passes
    /// vacuously rather than waiting out the full deadline for nothing.
    /// Distinct from "staging-capable hosts exist but are transiently
    /// unschedulable" (review finding 1): that case must NOT take this
    /// arm, or a MIG-roll blip silently flips the image ready with 0
    /// hosts actually staged.
    EmptyFleet,
}

fn eval_prestage(
    hosts: &[HostRecord],
    digest: &str,
    now: DateTime<Utc>,
    ttl: Duration,
) -> PrestageEval {
    // Genuinely vacuous iff no host in the fleet even claims to stage
    // images — a Process/dev fleet. This is independent of schedulability:
    // a fleet that DOES have staging-capable hosts, all of them transiently
    // unschedulable, is a `Waiting{0,0}` below, not `EmptyFleet`.
    if !hosts.iter().any(|h| h.stages_images) {
        return PrestageEval::EmptyFleet;
    }
    let eligible: Vec<&HostRecord> = hosts
        .iter()
        .filter(|h| crate::placement::host_is_schedulable(h, now, ttl) && h.stages_images)
        .collect();
    let staged = eligible
        .iter()
        .filter(|h| h.ready_images.iter().any(|d| d == digest))
        .count();
    if !eligible.is_empty() && staged == eligible.len() {
        PrestageEval::Complete
    } else {
        PrestageEval::Waiting {
            staged,
            eligible: eligible.len(),
        }
    }
}

/// The terminal disposition of a `prestaging` wait loop — what
/// `advance_one` breaks the poll loop with. Distinct from [`PrestageEval`]
/// (a per-poll snapshot): this is the loop's final verdict, deadline
/// applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrestageOutcome {
    Complete,
    EmptyFleet,
    TimedOut { staged: usize, eligible: usize },
}

impl PrestageOutcome {
    /// `engram_enable_prestage_seconds`'s `outcome` label.
    fn metric_label(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::EmptyFleet => "empty_fleet",
            Self::TimedOut { staged: 0, .. } => "timeout_zero",
            Self::TimedOut { .. } => "partial",
        }
    }
}

/// ADR 0036 amendment (issue #538): the per-host `prestage_hosts` audit
/// entries — `(host_id, outcome, waited_ms)`, `waited_ms` set only for
/// `staged`/`timed_out` (an `unschedulable` host was never part of the
/// wait). Same eligibility split as [`eval_prestage`], read fresh at
/// stage-end so a host that (de)registered mid-wait is reflected honestly.
fn prestage_host_outcomes(
    hosts: &[HostRecord],
    digest: &str,
    now: DateTime<Utc>,
    ttl: Duration,
    waited_ms: u64,
) -> Vec<(String, &'static str, Option<u64>)> {
    hosts
        .iter()
        .map(|h| {
            let eligible = crate::placement::host_is_schedulable(h, now, ttl) && h.stages_images;
            if !eligible {
                (h.id.to_string(), "unschedulable", None)
            } else if h.ready_images.iter().any(|d| d == digest) {
                (h.id.to_string(), "staged", Some(waited_ms))
            } else {
                (h.id.to_string(), "timed_out", Some(waited_ms))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The scanner's full loop exercises Postgres + a registry + a
    // capture host; deterministic end-to-end coverage lives in the
    // live-PG integration test `enable_jobs_live_pg` (claim/lease
    // semantics, re-POST dedup, failure budget, plus the #232
    // two-claimant fencing test) and the FC e2e suite (bake → push →
    // POST 202 → poll → boot). The per-step plumbing
    // (`materialize_chunk_blob`, capture reuse) is unit-tested in
    // `api::enabled_images`.

    // #232: the worker's abandon-vs-record decision hinges entirely on
    // how a store error maps into `AdvanceError`. A fenced write that
    // lost the lease returns `MetaError::Conflict`; that MUST become
    // `LeaseLost` so `run_once` abandons the job WITHOUT calling
    // `record_enable_job_failure` (which would clear the new
    // claimant's lease + burn its attempts budget). Every other
    // `MetaError`, and every pipeline error, must stay a `Pipeline`
    // error so the budget path still runs.
    #[test]
    fn lease_conflict_maps_to_lease_lost_not_pipeline() {
        match AdvanceError::from(MetaError::Conflict("lease lost: held by pod-b".into())) {
            AdvanceError::LeaseLost(msg) => assert!(msg.contains("pod-b")),
            AdvanceError::Pipeline(_) | AdvanceError::NonRetryable(_) => {
                panic!("a lost-lease Conflict must NOT enter the failure-budget path")
            }
        }
    }

    #[test]
    fn other_meta_errors_map_to_pipeline() {
        for e in [
            MetaError::NotFound,
            MetaError::Db("connection reset".into()),
            MetaError::Migration("schema drift".into()),
        ] {
            match AdvanceError::from(e) {
                AdvanceError::Pipeline(_) => {}
                AdvanceError::LeaseLost(_) | AdvanceError::NonRetryable(_) => {
                    panic!("only a Conflict should abandon; other errors retry via the budget")
                }
            }
        }
    }

    // 1b: a capture failure must be classified so a transient NoCapacity
    // retries but a deterministic warm-hook/capture failure bails fast.
    #[test]
    fn capture_no_capacity_is_retryable_but_internal_is_not() {
        // `ApiError::Unavailable` == the capture-host picker's NoCapacity.
        match classify_capture_error(crate::error::ApiError::Unavailable(
            "no host available (NoCapacity)".into(),
        )) {
            AdvanceError::Pipeline(_) => {}
            _ => panic!("NoCapacity is transient — it must retry via the budget"),
        }
        // `ApiError::Internal` == a [warm] hook non-zero exit / verify-fail.
        match classify_capture_error(crate::error::ApiError::Internal(
            "[warm] hook exited with status Some(1)".into(),
        )) {
            AdvanceError::NonRetryable(_) => {}
            _ => panic!("a deterministic capture failure must bail fast, not retry"),
        }
    }

    // ---- ADR 0036 amendment (issue #538): prestage stage ----

    use engram_core::types::host::{HostCapacity, HostMetadata, HostUtilization};

    const DIGEST: &str = "sha256:deadbeef";
    const TTL: Duration = Duration::from_secs(60);

    /// A schedulable, staging-capable host that has NOT yet reported the
    /// digest. Tests flip individual fields to build the other shapes.
    fn eligible_host(n: u128) -> HostRecord {
        HostRecord {
            id: engram_core::HostId(uuid::Uuid::from_u128(n)),
            hostname: format!("h{n}"),
            cloud_metadata: HostMetadata::default(),
            capacity: HostCapacity {
                total_gb: 0,
                used_gb: 0,
                total_mib: 0,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: HostUtilization::default(),
            status: engram_core::types::host::HostStatus::Ready,
            last_heartbeat_at: Utc::now(),
            host_addr: None,
            ready_images: Vec::new(),
            current_bundles: Vec::new(),
            cordoned: false,
            total_vcpus: 0,
            wire_version: 0,
            stages_images: true,
            capabilities: Default::default(),
        }
    }

    fn staged(n: u128) -> HostRecord {
        let mut h = eligible_host(n);
        h.ready_images.push(DIGEST.to_string());
        h
    }

    // Truth-table case 1: every eligible host has staged → Complete.
    #[test]
    fn eval_prestage_all_staged_is_complete() {
        let hosts = [staged(1), staged(2)];
        assert_eq!(
            eval_prestage(&hosts, DIGEST, Utc::now(), TTL),
            PrestageEval::Complete
        );
    }

    // Truth-table case 2: some but not all staged → Waiting with counts.
    #[test]
    fn eval_prestage_partial_is_waiting_with_counts() {
        let hosts = [staged(1), eligible_host(2), eligible_host(3)];
        assert_eq!(
            eval_prestage(&hosts, DIGEST, Utc::now(), TTL),
            PrestageEval::Waiting {
                staged: 1,
                eligible: 3
            }
        );
    }

    // Truth-table case 3: no host in the fleet has `stages_images` at all
    // (dev/Process fleet) → EmptyFleet, the vacuous-pass signal. This is
    // distinct from "staging-capable hosts exist but none are currently
    // schedulable" — see case 4a below (review finding 1).
    #[test]
    fn eval_prestage_no_staging_capable_hosts_is_empty_fleet() {
        assert_eq!(
            eval_prestage(&[], DIGEST, Utc::now(), TTL),
            PrestageEval::EmptyFleet
        );
        let mut not_staging = eligible_host(1);
        not_staging.stages_images = false;
        assert_eq!(
            eval_prestage(&[not_staging], DIGEST, Utc::now(), TTL),
            PrestageEval::EmptyFleet
        );
    }

    // Truth-table case 4: a host that goes unschedulable mid-wait (cordoned
    // / dead / draining) drops out of `eligible` — it must not block
    // Complete, and must not count toward `staged`.
    #[test]
    fn eval_prestage_host_unschedulable_mid_wait_is_excluded() {
        let mut cordoned = staged(1); // staged, but cordoned mid-wait
        cordoned.cordoned = true;
        let ok = staged(2);
        assert_eq!(
            eval_prestage(&[cordoned, ok], DIGEST, Utc::now(), TTL),
            PrestageEval::Complete,
            "the cordoned host must not block completion",
        );
    }

    // Truth-table case 4a (review finding 1, PR #565): every staging-capable
    // host is transiently unschedulable (e.g. a host-agent MIG roll leaves
    // heartbeats stale past the placement TTL for a few minutes) — this
    // must be `Waiting{0,0}`, NOT `EmptyFleet`. `EmptyFleet` vacuously
    // passes the stage; a fleet that has real staging hosts (just none
    // reachable this poll) must keep polling under the deadline so the
    // existing zero-staged-timeout retry policy applies instead of
    // silently flipping the image ready with 0 hosts actually staged.
    #[test]
    fn eval_prestage_all_staging_hosts_transiently_unschedulable_is_waiting_not_empty_fleet() {
        let mut dead = eligible_host(3);
        dead.status = engram_core::types::host::HostStatus::Dead;
        assert_eq!(
            eval_prestage(&[dead], DIGEST, Utc::now(), TTL),
            PrestageEval::Waiting {
                staged: 0,
                eligible: 0
            },
        );
    }

    // Truth-table case 5: `stages_images = false` hosts are excluded from
    // `eligible` regardless of schedulability or ready_images content —
    // Process/dev fleets never wait on them.
    #[test]
    fn eval_prestage_stages_images_false_is_excluded() {
        let mut non_staging_but_ready = staged(1);
        non_staging_but_ready.stages_images = false;
        let waiting = eligible_host(2);
        assert_eq!(
            eval_prestage(&[non_staging_but_ready, waiting], DIGEST, Utc::now(), TTL),
            PrestageEval::Waiting {
                staged: 0,
                eligible: 1
            },
            "a stages_images=false host must not count as eligible even though it reports ready",
        );
    }

    // Truth-table case 6: a stale heartbeat (host_is_schedulable's freshness
    // gate) excludes a host the same way cordoning does. When it's the ONLY
    // staging-capable host, that's the same transient-unschedulable case as
    // 4a (review finding 1) — `Waiting{0,0}`, not `EmptyFleet`.
    #[test]
    fn eval_prestage_stale_heartbeat_is_excluded() {
        let mut stale = staged(1);
        stale.last_heartbeat_at = Utc::now() - chrono::Duration::seconds(300);
        let ok = staged(2);
        assert_eq!(
            eval_prestage(&[stale.clone(), ok], DIGEST, Utc::now(), TTL),
            PrestageEval::Complete,
            "a stale host must not block completion",
        );
        assert_eq!(
            eval_prestage(&[stale], DIGEST, Utc::now(), TTL),
            PrestageEval::Waiting {
                staged: 0,
                eligible: 0
            },
        );
    }

    #[test]
    fn prestage_outcome_metric_labels() {
        assert_eq!(PrestageOutcome::Complete.metric_label(), "complete");
        assert_eq!(PrestageOutcome::EmptyFleet.metric_label(), "empty_fleet");
        assert_eq!(
            PrestageOutcome::TimedOut {
                staged: 0,
                eligible: 2
            }
            .metric_label(),
            "timeout_zero"
        );
        assert_eq!(
            PrestageOutcome::TimedOut {
                staged: 1,
                eligible: 2
            }
            .metric_label(),
            "partial"
        );
    }

    #[test]
    fn prestage_host_outcomes_classifies_staged_timed_out_and_unschedulable() {
        let staged_host = staged(1);
        let straggler = eligible_host(2); // eligible, hasn't staged
        let mut not_staging = eligible_host(3);
        not_staging.stages_images = false; // never eligible
        let mut cordoned = staged(4);
        cordoned.cordoned = true; // unschedulable despite reporting ready

        let entries = prestage_host_outcomes(
            &[
                staged_host.clone(),
                straggler.clone(),
                not_staging.clone(),
                cordoned.clone(),
            ],
            DIGEST,
            Utc::now(),
            TTL,
            4_242,
        );
        let find = |id: &engram_core::HostId| {
            entries
                .iter()
                .find(|(hid, _, _)| *hid == id.to_string())
                .unwrap()
        };
        assert_eq!(find(&staged_host.id).1, "staged");
        assert_eq!(find(&staged_host.id).2, Some(4_242));
        assert_eq!(find(&straggler.id).1, "timed_out");
        assert_eq!(find(&straggler.id).2, Some(4_242));
        assert_eq!(find(&not_staging.id).1, "unschedulable");
        assert_eq!(find(&not_staging.id).2, None);
        assert_eq!(find(&cordoned.id).1, "unschedulable");
        assert_eq!(find(&cordoned.id).2, None);
    }

    // The scanner's `Prestaging` stage builds its heartbeat-ack ref via
    // `crate::api::host_http::enabled_image_ref` — the SAME projection
    // `enabled_image_refs_from_rows` uses for already-live `enabled_images`
    // rows. This is a compile-time/structural check that the shared fn
    // exists and produces a ref for a fully-captured row; the two call
    // sites literally sharing the function is what rules out drift (there
    // is no second, divergent implementation to test against).
    #[test]
    fn enabled_image_ref_projection_matches_a_freshly_captured_row() {
        let row = engram_core::types::EnabledImage {
            id: uuid::Uuid::new_v4(),
            image_uri: "localhost:5001/demo:warm".into(),
            image_config: Default::default(),
            oci_defaults: Default::default(),
            manifest_digest: DIGEST.to_string(),
            disk_manifest: None,
            base_snapshot_id: Some(engram_core::SnapshotId::new()),
            base_snapshot_disk_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            }),
            base_snapshot_memory_manifest: None,
            last_refreshed_at: Utc::now(),
            created_at: Utc::now(),
            updated_at: None,
            soft_deleted_at: None,
        };
        let r = crate::api::host_http::enabled_image_ref(&row)
            .expect("a fully-stamped row must project to a ref");
        assert_eq!(r.image_uri, row.image_uri);
        assert_eq!(r.manifest_digest.as_str(), DIGEST);
    }

    // ---- review finding 6 (PR #565): `Default` stays pure; env I/O + the
    // reject-vs-fallback decision live in `from_env` ----

    const PRESTAGE_ENV: &str = "ENGRAM_ENABLE_PRESTAGE_TIMEOUT_SECS";

    #[test]
    fn default_is_pure_and_does_not_touch_the_env() {
        // SAFETY: serial test on a process-global env var (matches the
        // `chunk_gc::tests` convention for this crate's other `from_env`s).
        std::env::set_var(PRESTAGE_ENV, "99999");
        let cfg = EnableScannerConfig::default();
        std::env::remove_var(PRESTAGE_ENV);
        assert_eq!(
            cfg.prestage_timeout, DEFAULT_PRESTAGE_TIMEOUT,
            "Default must be a pure constant, unaffected by the env"
        );
    }

    #[test]
    fn from_env_unset_uses_the_default() {
        std::env::remove_var(PRESTAGE_ENV);
        let cfg = EnableScannerConfig::from_env();
        assert_eq!(cfg.prestage_timeout, DEFAULT_PRESTAGE_TIMEOUT);
    }

    #[test]
    fn from_env_valid_value_overrides_the_default() {
        std::env::set_var(PRESTAGE_ENV, "45");
        let cfg = EnableScannerConfig::from_env();
        std::env::remove_var(PRESTAGE_ENV);
        assert_eq!(cfg.prestage_timeout, Duration::from_secs(45));
    }

    #[test]
    fn from_env_zero_or_garbage_falls_back_to_the_default() {
        // Zero (plausibly meant "skip the wait") and unparseable garbage
        // must both fall back rather than producing a zero-wait or
        // negative-duration timeout — the review flagged the OLD `Default`
        // impl for swallowing this silently. Falling back is still
        // correct; the fix is that it's now observable (`warn!`), which
        // this unit test can't assert on but the reject-path is exercised.
        for v in ["0", "not-a-number", "-5"] {
            std::env::set_var(PRESTAGE_ENV, v);
            let cfg = EnableScannerConfig::from_env();
            assert_eq!(
                cfg.prestage_timeout, DEFAULT_PRESTAGE_TIMEOUT,
                "value {v:?} must fall back to the default, not silently zero/garbage"
            );
        }
        std::env::remove_var(PRESTAGE_ENV);
    }
}
