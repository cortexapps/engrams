//! The binding-writer ratchet (#896, ADR 0090 addendum — the
//! `driver_coverage_is_declared` pattern applied to ownership writes).
//!
//! `sessions.sandbox_id` is ownership truth (ADR 0090). Every surface
//! that writes it is inventoried HERE: a new call site — a new place
//! that can bind or release a sandbox — fails this test until it is
//! declared with a one-line justification. The store-level
//! `BindingDisposition` legality catches by-STATE violations; this
//! ratchet holds the by-SITE discipline (e.g. `Retain` into `Idle` is
//! authorized ONLY for the evac-exhaustion residue) that a store,
//! blind to its caller, cannot.
//!
//! Scan mechanics (deliberately textual, like `driver_coverage`):
//! `//`-comments are stripped first (a mention in prose must not
//! count), each file is truncated at its first `#[cfg(test)]` (test
//! fixtures walk rows freely and would swamp the counts), and only the
//! call shape `.method(` matches.

use std::collections::BTreeMap;
use std::path::Path;

/// Every surface that can write `sessions.sandbox_id`. The
/// `transition_with_fence*` pair carries Detach semantics post-#896, so
/// its call sites are ownership writes too.
const METHODS: &[&str] = &[
    "transition_session_created",
    "assign_session_sandbox",
    "assign_session_sandbox_guarded",
    "rebind_session_guarded",
    "fenced_assign_sandbox",
    "mark_host_dead_and_orphan_sessions",
    "transition_session",
    "fenced_transition_session",
    "fenced_transition_session_with_events",
    "transition_with_fence",
    "transition_with_fence_emitting",
];

/// The declared inventory: (module path, method, count, justification).
/// A new/undeclared site fails with instructions to add a row here —
/// with a justification a reviewer can hold you to.
const DECLARED: &[(&str, &str, usize, &str)] = &[
    ("api/host_http", "transition_session", 1, "Active→Unreachable (Retain: dead-guest VM stays owned)"),
    ("api/sessions", "transition_session", 1, "BootError::Started→Failed (Detach: boot pipeline unbound; idempotent)"),
    ("api/snapshot", "fenced_assign_sandbox", 1, "#896 resume gate: fenced clear after confirmed source teardown"),
    ("api/snapshot", "rebind_session_guarded", 1, "resume bind: fresh VM onto the Idle/unbound row (CAS Some(None))"),
    ("api/snapshot", "transition_session", 2, "no-recoverable-state / restore-failed →Dead (Detach authorizes orphan reap)"),
    ("api/snapshot", "transition_with_fence", 3, "ascent→Active + park→Created + resume-finish→Active (all Retain)"),
    ("dead_host", "assign_session_sandbox_guarded", 1, "straggler destroy's guarded clear before →Idle/Dead"),
    ("dead_host", "mark_host_dead_and_orphan_sessions", 1, "the bulk orphan: every non-terminal on a dead host →HostLost unbound"),
    ("dead_host", "transition_session", 2, "stage-2 HostLost→Idle|Dead after the bulk/guarded clears (RequireUnbound)"),
    ("evac_resumer", "fenced_assign_sandbox", 1, "confirmed-teardown clear before the peer restore"),
    ("evac_resumer", "transition_session", 2, "exhaustion→Idle (THE authorized Retain residue, #896) + structural →Idle/Dead (RequireUnbound)"),
    ("evacuation", "assign_session_sandbox", 1, "evac rebind: the new sandbox before →Created"),
    ("evacuation", "transition_session", 1, "→Created post-rebind (Retain)"),
    ("idle_detector", "transition_session", 1, "Active→Evicting nomination (Retain: VM untouched)"),
    ("idle_evictor", "transition_session", 3, "evict/park-reaper exhaustion →HostLost (Retain/RequireUnbound) + Parked→Evicting descent (Retain)"),
    ("idle_evictor", "transition_with_fence", 4, "park bookkeeping + unbound →HostLost + admin evict entry"),
    ("idle_evictor", "transition_with_fence_emitting", 1, "THE fused evict flip: Detach into Idle, Retain into Evacuating (#896)"),
    ("live_migration", "rebind_session_guarded", 1, "teleport commit: new sandbox CAS'd over the expected old one"),
    ("live_migration", "transition_session", 5, "parachute arms (Retain) + walk-back (Retain) + kill →Failed (Detach)"),
    ("queue_scanner", "transition_session", 4, "Queued→Idle/Failed settles (RequireUnbound: queued rows are unbound)"),
    ("reconcile", "assign_session_sandbox", 1, "strike-out unbind fallback"),
    ("reconcile", "assign_session_sandbox_guarded", 1, "strike-out guarded unbind"),
    ("reconcile", "transition_session", 2, "→HostLost + recovery_target after the clears (RequireUnbound)"),
    ("session_boot", "transition_session", 1, "Created→Active (Retain: the freshly-bound sandbox)"),
    ("session_boot", "transition_session_created", 1, "THE bind: Pending→Created fused with sandbox_id"),
    ("session_ops", "fenced_transition_session", 1, "transition_with_fence wrapper internals"),
    ("session_ops", "fenced_transition_session_with_events", 1, "transition_with_fence_emitting wrapper internals"),
    ("session_ops", "transition_session", 2, "the wrappers' epoch-0 fallbacks"),
    ("session_verbs", "fenced_assign_sandbox", 2, "unreachable-recovery unbind + destroy's host-affinity clear (sandbox already detached by the fused flip)"),
    ("session_verbs", "transition_with_fence", 7, "boot-failure Detach flips + evict exhaustion Retain + unreachable RequireUnbound + destroy Detach fusion"),
];

fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_at_cfg_test(source: &str) -> &str {
    match source.find("#[cfg(test)]") {
        Some(i) => &source[..i],
        None => source,
    }
}

fn scan(root: &Path, dir: &Path, inventory: &mut BTreeMap<(String, String), usize>) {
    for entry in std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()))
    {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            scan(root, &path, inventory);
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let source = strip_line_comments(truncate_at_cfg_test(&raw));
        let module = path
            .strip_prefix(root)
            .expect("under root")
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        for method in METHODS {
            // Call shapes only: `.method(` (method-call) and `::method(`
            // (the session_ops free-fn wrappers). The terminating `(` and
            // the leading separator make the match exact — a longer name
            // sharing a prefix never matches (its next char is not `(`),
            // and a shorter name never matches inside a longer one (the
            // preceding char is not `.`/`:`).
            let total = source.matches(&format!(".{method}(")).count()
                + source.matches(&format!("::{method}(")).count();
            if total > 0 {
                *inventory
                    .entry((module.clone(), method.to_string()))
                    .or_default() += total;
            }
        }
    }
}

#[test]
fn binding_writers_are_declared() {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut inventory: BTreeMap<(String, String), usize> = BTreeMap::new();
    scan(root, root, &mut inventory);
    assert!(
        !inventory.is_empty(),
        "the scan found no binding writers at all — bad root?",
    );

    let declared: BTreeMap<(String, String), usize> = DECLARED
        .iter()
        .map(|(m, f, c, _)| ((m.to_string(), f.to_string()), *c))
        .collect();

    let mut problems = Vec::new();
    for ((module, method), count) in &inventory {
        match declared.get(&(module.clone(), method.clone())) {
            Some(want) if want == count => {}
            Some(want) => problems.push(format!(
                "{module}: .{method}( count {count} != declared {want}"
            )),
            None => problems.push(format!("{module}: .{method}( x{count} UNDECLARED")),
        }
    }
    for (module, method) in declared.keys() {
        if !inventory.contains_key(&(module.clone(), method.clone())) {
            problems.push(format!(
                "{module}: .{method}( declared but no longer present"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "binding-writer inventory drift — every sandbox_id writer call site \
         must be declared in DECLARED with a one-line justification (or the \
         stale row removed):\n  {}",
        problems.join("\n  "),
    );
}
