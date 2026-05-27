//! Operation-scoped data-plane tracing (ADR 0019).
//!
//! The deep I/O that dominates a session's lifecycle operations — NBD chunk
//! page-in, flush of dirty chunks — runs on long-lived background tasks that
//! outlive any single operation's call stack and serve a sandbox across its
//! whole life. We want those tasks' spans in the *operation's* trace, but
//! only during that operation's window (cold boot / idle→resume /
//! warm-launch / evacuate), not flooding a running session's steady state.
//!
//! [`OperationScope`] is the per-sandbox handle that bridges the two: the
//! host calls [`OperationScope::begin`] when an operation starts and
//! [`OperationScope::end`] when it completes; in-process data-plane tasks
//! (the NBD `serve_loop` via [`crate::disk_daemon::ChunkedDiskBackend`])
//! call [`OperationScope::current`] and, when an operation is active, parent
//! their `chunk.fetch` spans on the operation span. When no operation is
//! active they emit aggregate metrics only (the 0d counters) — no flood.
//!
//! Cross-process data-plane (the uffd-handler) uses the spawn-time
//! `TRACEPARENT` env instead (ADR 0019 0b); this scope is only for the
//! in-process tasks.

use std::sync::Arc;

use parking_lot::RwLock;

/// The operation a sandbox's data plane is currently serving, if any.
pub struct OpContext {
    /// The `op.<kind>` span. Kept alive in the scope across the operation's
    /// (possibly multi-RPC) window so data-plane spans nest under it and
    /// share its trace.
    pub span: tracing::Span,
    /// `"cold_boot" | "resume" | "warm_launch" | "evacuate"`.
    pub kind: &'static str,
}

/// Per-sandbox, cheaply cloneable handle to the active operation (if any).
/// Clones share one slot, so the backend's clone sees `begin`/`end` made
/// through any other clone. Default = no active operation.
#[derive(Clone, Default)]
pub struct OperationScope {
    inner: Arc<RwLock<Option<Arc<OpContext>>>>,
}

impl OperationScope {
    /// Begin an operation: open an operation span (child of the current
    /// trace context — typically the propagated session trace) and make it
    /// the active scope until [`end`](Self::end). Returns the span so the
    /// caller can also attach to it if useful.
    pub fn begin(&self, kind: &'static str) -> tracing::Span {
        // `otel.name` renames the exported span to `kind`; the static macro
        // name stays constant for `tracing` filtering.
        let span = tracing::info_span!("operation", otel.name = kind, op = kind);
        *self.inner.write() = Some(Arc::new(OpContext {
            span: span.clone(),
            kind,
        }));
        span
    }

    /// End the active operation. Data-plane tasks fall back to metrics-only.
    pub fn end(&self) {
        *self.inner.write() = None;
    }

    /// The active operation, if one is in progress. Cheap (read lock + Arc
    /// clone); the NBD read path calls this per chunk fetched.
    pub fn current(&self) -> Option<Arc<OpContext>> {
        self.inner.read().clone()
    }
}
