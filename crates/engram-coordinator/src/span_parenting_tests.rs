//! Telemetry restoration (#526): regression coverage for the
//! `tokio::spawn(...).instrument(tracing::Span::current())` fix applied to
//! the coordinator's detached lifecycle pipelines
//! (`api/sessions.rs::create_session_core`'s boot spawn,
//! `api/snapshot.rs`'s manual-capture / resume / evict_local spawns).
//!
//! `tokio::spawn` severs the ambient tracing context: a span created
//! inside a spawned future has no parent unless the future is explicitly
//! `.instrument()`-ed with the span that was current at spawn time. This
//! module proves the fix works (instrumented → child of the request span)
//! and guards the regression the fix corrects (bare `tokio::spawn` → an
//! orphaned root — the exact "every trace is spans=1" symptom from the
//! issue's evidence pass).
//!
//! The test drives a capturing `tracing_subscriber::Layer` that records
//! each span's parent name via `on_new_span`, rather than standing up a
//! real OTel exporter — the parenting *decision* is what changed, and
//! that's entirely a `tracing`-crate-level property independent of the
//! export backend.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::span::{Attributes, Id};
use tracing::{Instrument, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Records `span name -> parent span name` for every span created while
/// this layer is the active subscriber.
#[derive(Clone, Default)]
struct ParentCapture(Arc<Mutex<HashMap<String, Option<String>>>>);

impl ParentCapture {
    fn snapshot(&self) -> HashMap<String, Option<String>> {
        self.0.lock().expect("capture mutex poisoned").clone()
    }
}

impl<S> Layer<S> for ParentCapture
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span_ref = ctx.span(id).expect("span just created must be resolvable");
        let name = span_ref.name().to_string();
        let parent_name = span_ref.parent().map(|p| p.name().to_string());
        self.0
            .lock()
            .expect("capture mutex poisoned")
            .insert(name, parent_name);
    }
}

/// Guards the coordinator's detached-pipeline span-parenting fix (#526).
///
/// Mirrors the exact production shape: capture `Span::current()` while
/// inside the "request span" (standing in for `session.create.grpc` /
/// `coord.finish_resume_to_active` / etc.), let that scope end (the
/// request handler returns while the spawned pipeline lives on — the
/// whole point of the #210/#213 detach pattern), then spawn two tasks:
///
/// - `detached.instrumented`: `.instrument(span_at_spawn_time)` — the
///   fix. Must parent under `test.request`.
/// - `detached.uninstrumented`: bare `tokio::spawn`, no `.instrument` —
///   the pre-fix shape. Must come up as an orphaned root (no parent),
///   reproducing the "every trace is spans=1" symptom the issue's
///   evidence pass found in prod.
#[tokio::test(flavor = "current_thread")]
async fn instrumented_spawn_parents_under_request_span_bare_spawn_orphans() {
    let capture = ParentCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _dispatch_guard = tracing::subscriber::set_default(subscriber);

    // Capture the span "current" at spawn time, then let its `.enter()`
    // guard drop — exactly like a request handler whose span scope ends
    // when it returns, while the spawned pipeline body it detached keeps
    // running (ADR 0019 / #526's whole premise).
    let span_at_spawn_time = {
        let request_span = tracing::info_span!("test.request");
        let _entered = request_span.enter();
        tracing::Span::current()
    };

    // The fix: `tokio::spawn(fut.instrument(Span::current()))`.
    let instrumented = tokio::spawn(
        async {
            let _child = tracing::info_span!("detached.instrumented");
        }
        .instrument(span_at_spawn_time.clone()),
    );

    // The regression this guards against: the identical body spawned
    // WITHOUT `.instrument` — the shape every detached pipeline had
    // before this fix.
    let uninstrumented = tokio::spawn(async {
        let _child = tracing::info_span!("detached.uninstrumented");
    });

    instrumented.await.expect("instrumented task panicked");
    uninstrumented.await.expect("uninstrumented task panicked");

    let captured = capture.snapshot();

    assert_eq!(
        captured.get("detached.instrumented").cloned(),
        Some(Some("test.request".to_string())),
        "a span created inside an `.instrument(Span::current())`-wrapped \
         tokio::spawn must parent under the enclosing request span — this \
         is the #526 fix (sessions.rs:701 / snapshot.rs:290,673,1735 shape)",
    );
    assert_eq!(
        captured.get("detached.uninstrumented").cloned(),
        Some(None),
        "a span created inside a bare tokio::spawn (no .instrument) must \
         come up as an orphaned root — regression guard: this is the \
         pre-fix 'every trace is spans=1' shape the issue's evidence pass \
         found in prod",
    );
}
