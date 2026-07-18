//! Targeted runtime invariants (ADR 0099 H6).
//!
//! Two macros, both carrying `#[track_caller]` provenance so a violation
//! points at the assertion site, not at this module:
//!
//! - [`invariant!`] — **always-on**, panics on violation. This is cheap
//!   here precisely because the recovery machinery already exists: a
//!   coordinator pod is stateless over Postgres (ADR 0047) and host
//!   state machines are redrive-safe (ADR 0028/0034/0079), so a panic is
//!   a loud restart, not data loss. ADR 0083 (fail-closed bind) is the
//!   precedent. Use it where a violated condition means the surrounding
//!   code has already lost its footing — continuing would corrupt state.
//!
//! - [`soft_invariant!`] — requires an explicit name slug and logs at
//!   `error!` with the stable, greppable prefix `soft-invariant violated:`
//!   plus that slug in a `name` field, and does
//!   **not** panic. For reconciler-class sites whose *job* is repairing
//!   anomalies: panicking the checker on detection would prevent the
//!   repair. A counter can ride on the log pipeline (alert on the
//!   `soft_invariant` field / the message prefix) — engram-core keeps no
//!   metrics registry, so the log line is the alerting seam.
//!
//! engram-core is the no-I/O crate; the only dependency this adds is the
//! `tracing` *facade* (no subscriber, no I/O of its own — the binary
//! crates install the subscriber), which is the idiomatic choice over a
//! callback the caller has to thread through.

/// Always-on runtime invariant. Panics (with `#[track_caller]`
/// provenance) if `cond` is false.
///
/// ```ignore
/// invariant!(epoch >= 1);
/// invariant!(epoch >= 1, "minted binding epoch must be positive, got {epoch}");
/// ```
#[macro_export]
macro_rules! invariant {
    ($cond:expr $(,)?) => {{
        if !$cond {
            $crate::invariant::__invariant_panic(::core::stringify!($cond), ::core::option::Option::None);
        }
    }};
    ($cond:expr, $($arg:tt)+) => {{
        if !$cond {
            $crate::invariant::__invariant_panic(
                ::core::stringify!($cond),
                ::core::option::Option::Some(::core::format_args!($($arg)+)),
            );
        }
    }};
}

/// Non-fatal runtime invariant. Logs at `error!` with the stable prefix
/// `soft-invariant violated:` and the explicit name slug in a `name`
/// field if `cond` is false; never panics. For reconciler-class sites
/// that must go on to repair the anomaly they just detected.
///
/// ```ignore
/// soft_invariant!("sandbox-cached-under-two-hosts", prev_host == host_id, "sandbox {sb} cached under {prev_host}, not {host_id}");
/// ```
#[macro_export]
macro_rules! soft_invariant {
    ($name:literal, $cond:expr, $($arg:tt)+) => {{
        if !$cond {
            $crate::invariant::__soft_invariant_violated(
                $name,
                ::core::format_args!($($arg)+),
            );
        }
    }};
}

/// Panic helper for [`invariant!`]. `#[track_caller]` so the reported
/// location is the macro invocation site, not this function. Not part of
/// the public API — call the macro.
#[doc(hidden)]
#[track_caller]
pub fn __invariant_panic(cond: &'static str, detail: Option<core::fmt::Arguments<'_>>) -> ! {
    match detail {
        Some(d) => panic!("invariant violated: {cond}: {d}"),
        None => panic!("invariant violated: {cond}"),
    }
}

/// Error-log helper for [`soft_invariant!`]. `#[track_caller]` so the
/// captured `location` is the macro invocation site. Not part of the
/// public API — call the macro.
#[doc(hidden)]
#[track_caller]
pub fn __soft_invariant_violated(name: &'static str, detail: core::fmt::Arguments<'_>) {
    let location = core::panic::Location::caller();
    tracing::error!(
        soft_invariant = true,
        name = name,
        %location,
        "soft-invariant violated: {detail}",
    );
}

#[cfg(test)]
mod tests {
    // The macros are `#[macro_export]`, so within this crate they resolve
    // at the crate root (`crate::invariant!`).

    #[test]
    fn invariant_holds_is_a_noop() {
        crate::invariant!(1 + 1 == 2);
        crate::invariant!(1 + 1 == 2, "arithmetic still works: {}", 2);
    }

    #[test]
    #[should_panic(expected = "invariant violated: 1 + 1 == 3")]
    fn invariant_violation_panics_with_condition() {
        crate::invariant!(1 + 1 == 3);
    }

    #[test]
    #[should_panic(expected = "invariant violated: false: epoch 7 must exceed 9")]
    fn invariant_violation_panics_with_message() {
        let (a, b) = (7, 9);
        crate::invariant!(false, "epoch {a} must exceed {b}");
    }

    #[test]
    fn soft_invariant_holds_is_a_noop() {
        crate::soft_invariant!("test-condition-holds", true, "should never log");
    }

    #[test]
    fn soft_invariant_violation_does_not_panic() {
        // The whole point: a violated soft invariant logs and returns so
        // the reconciler can proceed to repair. Reaching past the macro
        // proves control flow continued past the violation.
        let reached_before = true;
        crate::soft_invariant!(
            "test-anomaly-detected",
            false,
            "detected an anomaly worth an alert: {}",
            42
        );
        let reached_after = true;
        assert!(
            reached_before && reached_after,
            "soft_invariant! must not divert control flow"
        );
    }
}
