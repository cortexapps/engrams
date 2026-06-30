//! Shared readiness-polling helpers for the engram-host-agent integration
//! tests. These replace fixed `tokio::time::sleep` "settle" windows with
//! bounded polling so the serial CI suite isn't paying a blind margin on
//! every synchronization point (and so a too-tight margin can't flake).
//!
//! Each top-level file in `tests/` is its own crate, so only the helpers a
//! given test binary actually calls are reachable from it — hence the
//! module-wide `dead_code` allow.
#![allow(dead_code)]

use std::future::Future;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Poll `TcpStream::connect(addr)` on a 20 ms tick until it succeeds (the
/// listener is bound and accepting) or `timeout` elapses. Returns whether
/// the address became connectable. Modeled on the in-crate precedent in
/// `grpc_pause_resume.rs::boot_grpc_server` (and `boot.rs`'s socket poll).
pub async fn wait_tcp_bound(addr: SocketAddr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Generic bounded async poll: call `pred` every `interval` until it
/// resolves `true` or `timeout` elapses. Returns whether the predicate
/// became true in time. Intended for exec-based predicates (e.g. polling a
/// guest file via the test's `exec` helper) where each probe is itself
/// async.
pub async fn poll_until_async<F, Fut>(timeout: Duration, interval: Duration, mut pred: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if pred().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}
