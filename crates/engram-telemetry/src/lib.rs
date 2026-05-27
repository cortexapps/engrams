//! Shared tracing + OpenTelemetry init for Engram binaries (ADR 0019).
//!
//! Every binary calls [`init`] once at startup. It always installs the
//! existing `tracing_subscriber::fmt` layer (so logs are unchanged), and
//! *additionally* installs an OTLP span-export layer **iff**
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set. When the env var is unset this
//! crate is a no-op beyond the fmt layer — so production is untouched
//! until we explicitly opt in by pointing it at a collector.
//!
//! The boot path crosses four processes (coord → host-agent → spawned
//! firecracker/uffd-handler → in-guest agentd). To stitch their spans
//! into one trace we propagate W3C trace-context; see [`current_traceparent`]
//! and [`set_parent_from_traceparent`].
//!
//! Short-lived processes (uffd-handler, agentd) must call [`init`] from
//! *inside* a Tokio runtime (the batch exporter spawns a background task)
//! and must keep the returned [`TelemetryGuard`] alive until exit — its
//! `Drop` flushes pending spans. Dropping it outside a runtime is fine;
//! shutdown is synchronous.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Per-binary telemetry configuration.
pub struct Config {
    /// `service.name` resource attribute on every exported span, and the
    /// OTel tracer name. Use the binary's crate name, e.g. `"engram-coordinator"`.
    pub service_name: &'static str,
    /// Default `EnvFilter` directive when neither `RUST_LOG` nor
    /// `ENGRAM_LOG` is set. Long-lived services use `"info,engram=debug"`;
    /// the in-guest/short-lived binaries historically used `"info"`.
    pub default_filter: &'static str,
}

/// Holds the OTLP tracer provider so spans flush on shutdown. Drop it (or
/// let it drop at end of `main`) to flush. Inert when OTLP is disabled.
#[must_use = "hold the guard until program exit so spans are flushed"]
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Best-effort flush; we're tearing down, so a failed export
            // shouldn't mask the real exit path.
            if let Err(e) = provider.shutdown() {
                tracing::debug!(error = %e, "otel tracer provider shutdown");
            }
        }
    }
}

fn env_filter(default: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
}

/// Initialise the global tracing subscriber (fmt layer always; OTLP layer
/// when `OTEL_EXPORTER_OTLP_ENDPOINT` is set).
///
/// Honors `ENGRAM_LOG_FORMAT` (`pretty` default, or `json` for Cloud
/// Logging). Idempotent-unsafe: call exactly once per process.
pub fn init(cfg: Config) -> TelemetryGuard {
    let json = matches!(
        std::env::var("ENGRAM_LOG_FORMAT").as_deref(),
        Ok("json") | Ok("JSON")
    );

    // The OTLP layer is opt-in: present only when an endpoint is
    // configured. We build it first so we can early-return the
    // logs-only path with the simplest possible subscriber.
    let otlp = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .and_then(
            |endpoint| match build_provider(cfg.service_name, &endpoint) {
                Ok(p) => Some(p),
                Err(e) => {
                    // Never fail startup over telemetry — fall back to logs only.
                    eprintln!("engram-telemetry: OTLP export disabled (provider init failed): {e}");
                    None
                }
            },
        );

    // Type-erase the layers with `.boxed()` so the json/pretty choice and
    // the optional OTLP layer all collapse to one concrete subscriber type
    // built once. `Option<Layer>` is itself a `Layer` (a no-op when `None`),
    // which is how we make the OTLP layer conditional without duplicating
    // the whole subscriber stack.
    let fmt_layer = if json {
        tracing_subscriber::fmt::layer().json().boxed()
    } else {
        tracing_subscriber::fmt::layer().boxed()
    };

    let otel_layer = otlp.as_ref().map(|provider| {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );
        let tracer = provider.tracer(cfg.service_name);
        tracing_opentelemetry::layer().with_tracer(tracer).boxed()
    });

    tracing_subscriber::registry()
        .with(env_filter(cfg.default_filter))
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    if otlp.is_some() {
        tracing::info!(
            service = cfg.service_name,
            "OpenTelemetry OTLP span export enabled"
        );
    }
    TelemetryGuard { provider: otlp }
}

/// Build the OTLP gRPC exporter + batch-processing tracer provider. Reads
/// `OTEL_EXPORTER_OTLP_ENDPOINT` (and the rest of the standard `OTEL_*`
/// env) via the exporter builder. Must run inside a Tokio runtime.
fn build_provider(
    service_name: &'static str,
    _endpoint: &str,
) -> Result<SdkTracerProvider, Box<dyn std::error::Error>> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_attribute(KeyValue::new("service.name", service_name))
                .build(),
        )
        .build();
    Ok(provider)
}

/// Serialize the *current* span's trace-context to a W3C `traceparent`
/// string, for propagation across a process or VM boundary (gRPC metadata,
/// a spawn env var, a vsock handshake field). Returns `None` when there is
/// no active span context (e.g. OTLP disabled, or called outside any span).
pub fn current_traceparent() -> Option<String> {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let ctx = tracing::Span::current().context();
    let span = ctx.span();
    let sc = span.span_context();
    if !sc.is_valid() {
        return None;
    }
    // W3C trace-context: version-traceid-spanid-flags
    Some(format!(
        "00-{}-{}-{:02x}",
        sc.trace_id(),
        sc.span_id(),
        sc.trace_flags().to_u8()
    ))
}

/// Parse an inbound W3C `traceparent` and set it as the parent of the
/// given span, linking this process's spans to the upstream trace. No-op
/// on a malformed/empty header.
pub fn set_parent_from_traceparent(span: &tracing::Span, traceparent: &str) {
    use std::collections::HashMap;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    if traceparent.is_empty() {
        return;
    }
    let mut carrier = HashMap::new();
    carrier.insert("traceparent".to_string(), traceparent.to_string());
    let parent_cx = opentelemetry::global::get_text_map_propagator(|prop| prop.extract(&carrier));
    span.set_parent(parent_cx);
}
