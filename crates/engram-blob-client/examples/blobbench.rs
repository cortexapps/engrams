//! blobbench — the manual before/after throughput harness for the
//! blob tier. NOT a CI lane (compiled by `--all-targets` so it can't
//! rot; never executed by tests): run it by hand against a real
//! bucket to measure where the client stands and what a transport or
//! layer change buys.
//!
//! The headline metric is **throughput-per-core** — bytes moved per
//! CPU-second (user+sys, whole process) — the same normalization
//! TigerBeetle quotes for their object-storage client (~5 GB/s/core
//! GET, ~20 GB/s/core PUT, in-datacenter). Wall-clock MB/s is also
//! printed but is NIC/uplink-bound: from a laptop it measures your
//! uplink, not the client. Run in-region on a GCE VM for absolute
//! numbers; the per-core number and the A/B deltas travel.
//!
//! ```text
//! # A/B the SDK-default transport vs the tuned one, through the full client:
//! cargo run --release -p engram-blob-client --example blobbench -- \
//!   --bucket <bucket> --transport untuned --layer raw
//! cargo run --release -p engram-blob-client --example blobbench -- \
//!   --bucket <bucket> --transport tuned --layer client
//! ```
//!
//! Objects land under `blobbench/<run-id>/…` and are deleted at the
//! end (`--keep` to skip). Auth is ADC (`gcloud auth
//! application-default login`) or `STORAGE_EMULATOR_HOST`.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use clap::{Parser, ValueEnum};
use engram_blob_client::BlobClient;
use engram_core::traits::BlobStorage;
use futures::stream::{self, StreamExt};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Transport {
    /// The SDK's default reqwest client (getaddrinfo DNS, default
    /// pool) — the pre-2026-07 baseline.
    Untuned,
    /// The production transport: bounded connect, sized keep-alive
    /// pool, pinned http1.
    Tuned,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Layer {
    /// The bare backend — isolates transport cost.
    Raw,
    /// Through `BlobClient` (retry + deadline + metrics) — what
    /// production runs; the delta vs `raw` is the client overhead.
    Client,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Via {
    /// The buffered ops (`put`/`get`) — the production chunk path:
    /// sized PUT body, one-copy (or zero-copy) GET collect.
    Buffered,
    /// The streaming ops driven the way the buffered ops used to be
    /// implemented: `put_streaming` over a one-frame stream (mpsc
    /// pump + chunked encoding) and `get_streaming` drained into a
    /// growth-doubling `BytesMut`. The A/B control for the buffered
    /// overrides.
    Streaming,
}

#[derive(Parser, Debug)]
#[command(about = "manual blob-tier throughput harness (not a CI lane)")]
struct Args {
    /// GCS bucket to bench against (objects go under blobbench/<run-id>/).
    #[arg(long)]
    bucket: String,
    #[arg(long, value_enum, default_value_t = Transport::Tuned)]
    transport: Transport,
    #[arg(long, value_enum, default_value_t = Layer::Client)]
    layer: Layer,
    #[arg(long, value_enum, default_value_t = Via::Buffered)]
    via: Via,
    /// Object size in MiB (16 = the disk-chunk size; 0.5 MiB memory
    /// chunks can be approximated with 1).
    #[arg(long, default_value_t = 16)]
    object_mib: usize,
    /// Objects uploaded per concurrency level.
    #[arg(long, default_value_t = 16)]
    count: usize,
    /// GET passes over the object set per concurrency level.
    #[arg(long, default_value_t = 4)]
    rounds: usize,
    /// Comma-separated in-flight-op levels.
    #[arg(long, default_value = "1,8,32,64")]
    concurrency: String,
    /// Leave the objects in place (skip the delete sweep).
    #[arg(long, default_value_t = false)]
    keep: bool,
}

/// Whole-process CPU seconds (user + sys) — the "per-core"
/// normalizer. Aggregates across all runtime threads, so
/// bytes / Δcpu is bytes-per-core-second regardless of parallelism.
fn cpu_seconds() -> f64 {
    cpu_time::ProcessTime::now().as_duration().as_secs_f64()
}

struct Sample {
    label: String,
    ops: usize,
    bytes: u64,
    wall_s: f64,
    cpu_s: f64,
}

impl Sample {
    fn print(&self) {
        let mib = self.bytes as f64 / (1024.0 * 1024.0);
        println!(
            "{:<28} {:>5} ops  {:>9.1} MiB  wall {:>7.2}s ({:>8.1} MiB/s)  cpu {:>6.2}s ({:>9.1} MiB/s/core)  {:>6.1} ms/op",
            self.label,
            self.ops,
            mib,
            self.wall_s,
            mib / self.wall_s,
            self.cpu_s,
            mib / self.cpu_s,
            1000.0 * self.wall_s / self.ops as f64,
        );
    }
}

async fn run_wave<F, Fut>(label: String, concurrency: usize, tasks: Vec<F>) -> Sample
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = u64>,
{
    let ops = tasks.len();
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    let bytes: u64 = stream::iter(tasks.into_iter().map(|t| t()))
        .buffer_unordered(concurrency)
        .fold(0u64, |acc, n| async move { acc + n })
        .await;
    Sample {
        label,
        ops,
        bytes,
        wall_s: t0.elapsed().as_secs_f64(),
        cpu_s: (cpu_seconds() - cpu0).max(1e-9),
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let object_bytes = args.object_mib * 1024 * 1024;

    let backend: Arc<dyn BlobStorage> = match args.transport {
        Transport::Untuned => Arc::new(
            engram_storage_gcs::GcsBlobStorage::connect_untuned(args.bucket.clone())
                .await
                .expect("gcs connect (untuned)"),
        ),
        Transport::Tuned => Arc::new(
            engram_storage_gcs::GcsBlobStorage::connect(args.bucket.clone())
                .await
                .expect("gcs connect (tuned)"),
        ),
    };
    let store: Arc<dyn BlobStorage> = match args.layer {
        Layer::Raw => backend,
        Layer::Client => Arc::new(BlobClient::wrap(backend)),
    };

    // A patterned, non-zero body; one allocation, refcount-shared by
    // every PUT (GCS doesn't dedup — each key is a real upload).
    let mut body = vec![0u8; object_bytes];
    let mut x: u64 = 0x243F_6A88_85A3_08D3;
    for chunk in body.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
    let body = Bytes::from(body);

    let run_id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let levels: Vec<usize> = args
        .concurrency
        .split(',')
        .map(|s| s.trim().parse().expect("concurrency level"))
        .collect();

    println!(
        "blobbench: bucket={} transport={:?} layer={:?} via={:?} object={}MiB count={} rounds={} run=blobbench/{run_id}/",
        args.bucket, args.transport, args.layer, args.via, args.object_mib, args.count, args.rounds,
    );

    let mut created: Vec<String> = Vec::new();
    for &c in &levels {
        // PUT wave: fresh keys per level so every op is a real upload.
        let tasks: Vec<_> = (0..args.count)
            .map(|i| {
                let store = store.clone();
                let body = body.clone();
                let key = format!("blobbench/{run_id}/c{c}/obj-{i:04}");
                created.push(key.clone());
                let via = args.via;
                move || async move {
                    match via {
                        Via::Buffered => store.put(&key, body).await.expect("put"),
                        Via::Streaming => store
                            .put_streaming(&key, engram_core::traits::ByteStream::from_bytes(body))
                            .await
                            .expect("put_streaming"),
                    }
                }
            })
            .collect();
        run_wave(format!("PUT c={c}"), c, tasks).await.print();
    }

    // GET waves read the *first* level's objects (identical content
    // everywhere; any set works) `rounds` times over.
    let get_keys: Vec<String> = (0..args.count)
        .map(|i| format!("blobbench/{run_id}/c{}/obj-{i:04}", levels[0]))
        .collect();
    for &c in &levels {
        let tasks: Vec<_> = (0..args.count * args.rounds)
            .map(|n| {
                let store = store.clone();
                let key = get_keys[n % get_keys.len()].clone();
                let via = args.via;
                move || async move {
                    match via {
                        Via::Buffered => store.get(&key).await.expect("get").len() as u64,
                        Via::Streaming => {
                            // The pre-2026-07 collect: growth-doubling
                            // BytesMut, no pre-size, no single-frame
                            // shortcut.
                            let mut stream =
                                store.get_streaming(&key).await.expect("get_streaming");
                            let mut buf = bytes::BytesMut::new();
                            while let Some(chunk) = stream.next().await {
                                buf.extend_from_slice(&chunk.expect("frame"));
                            }
                            buf.len() as u64
                        }
                    }
                }
            })
            .collect();
        run_wave(format!("GET c={c}"), c, tasks).await.print();
    }

    if args.keep {
        println!("--keep: leaving {} objects in place", created.len());
        return;
    }
    let deletes: Vec<_> = created
        .into_iter()
        .map(|key| {
            let store = store.clone();
            move || async move {
                store.delete(&key).await.expect("delete");
                0u64
            }
        })
        .collect();
    let n = deletes.len();
    run_wave(format!("DELETE (cleanup, {n} keys)"), 32, deletes)
        .await
        .print();
}
