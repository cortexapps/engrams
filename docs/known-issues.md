# Known issues

Things that work today but are quietly wrong, or shortcuts taken under
deadline. Each item has the file/line of the offending code and a
sketch of the proper fix so a future visitor (you, me, an agent) can
land it without rediscovering the problem.

## 1. Warm-pool key ignores `repo`

**File**: `crates/engram-host-agent/src/pooled_backend.rs:56-65`

`pool_key` currently builds a `PoolKey` from `spec.image` for both the
`repo` and `image_version` fields:

```rust
PoolKey {
    repo: spec.image.clone(),
    image_version: spec.image.clone(),
}
```

Two repos sharing the same image version (e.g., `local://demo/warm-1`
and `local://claude-oauth/warm-1`) collide on the pool key and are
served interchangeably. Worse, `PooledBackend::create` configures the
pool with the *first* spec it sees for that key, so all subsequent
checkouts hand back clones of the original `rootfs_source` — even
when the new request is for a totally different image. We hit this
during the OAuth bring-up: a session for `local://claude-oauth`
received a sandbox cloned from `local://demo`'s 228 MiB rootfs,
booted into a non-Claude image, and silently dropped traffic because
`/sbin/engram-harness-claude` didn't exist in that rootfs.

**Fix**: include the original `repo` (not `spec.image`) in the
`PoolKey`, plumbed through from `PooledBackend::create`'s caller. Add
a regression test that creates two sessions with overlapping
`image_version` but distinct `repo` and asserts they get different
sandbox IDs. The comment on the existing `pool_key` ("`repo` is
duplicated as the image_version because the wire protocol's
`WarmPoolReport` carries both fields and the scheduler matches on
`image_version` only") is correct about the wire shape but masks the
correctness bug.

## 2. VZ disk-attach failures leak the cloned rootfs

**File**: `crates/engram-sandbox-vz/src/backend.rs` — the `create`
path around the `clone_or_copy → VzVm::new → vm.start` sequence
(approximately lines 240-280).

Cleanup is wired only on `vm.start().await` failure:

```rust
clone_or_copy(&bake_rootfs, &rootfs_path).await?;
let (vm, port_fds) = VzVm::new(vm_cfg)?;          // <-- failure here leaks rootfs_path
if let Err(e) = vm.start().await {
    let _ = tokio::fs::remove_file(&rootfs_path).await;
    return Err(e.into());
}
```

When `VzVm::new` returns `Err` (the disk-attach call we hit during
the macOS Tahoe sector-alignment investigation), the just-cloned
`rootfs_path` is left behind in `work_dir`. After a few failed
creates we accumulated 4× 1.79 GB orphans in `var/sandboxes/`. On a
production host with retries against a transient VZ flake this would
fill the disk.

**Fix**: wrap the `VzVm::new` + `vm.start` block so any error path
from either step removes `rootfs_path`. Equivalently, use a guard
struct with `Drop` that unlinks the clone unless explicitly
`disarm()`d after `vm.start()` succeeds.

## 3. Resume path drops the secret-augmented env

**File**: `crates/engram-coordinator/src/api/snapshot.rs` —
the `build_dev_agent` call inside the resume path (around line 280,
flagged with a `TODO(secrets-on-resume)` comment).

```rust
let resume_base_env: HashMap<String, String> = HashMap::new();
if let Some(agent) =
    crate::api::sessions::build_dev_agent(&state, id, None, &resume_base_env)
{ … }
```

When a session idle-evicts and is later resumed, the harness gets
re-launched with an empty base env. So `ANTHROPIC_API_KEY` /
`CLAUDE_CODE_OAUTH_TOKEN` / any other manifest-declared secret is
absent post-resume — the next prompt will fail with auth errors even
though create-time worked. The empty HashMap was a placeholder
introduced when `build_dev_agent` gained its `base_env` argument
(the fix that landed delivery to create-time). Resume needs the same
treatment.

**Fix**: at resume, look up the session's `repo` + `image_version`,
re-load the manifest via the image registry, re-resolve the secret
bundle through `state.services.secrets.resolve(...)` with the same
`SecretContext` shape used at create, build a `spec_env` via
`apply_secrets_to_env`, and pass that into `build_dev_agent` instead
of the empty map. Ideally factor the create-time secret-resolution
block out of `api/sessions.rs::create_session` into a helper so both
paths share it.

## 4. No system-wide event stream on the coordinator

The dashboard's Overview page only polls `GET /sessions` and
`GET /api/hosts` at 1Hz — no SSE. Live event streaming is reserved
for the SessionDetail page (one SSE per page-view, scoped to the
session being read). That's deliberate: the coordinator's only
event endpoint today is `GET /sessions/:id/events`, so a "live
ticker across all sessions" would mean N concurrent EventSource
connections from the browser, hitting the 6-per-origin HTTP/1 cap
and starving the rest of the app.

**Future work**, if a cross-session live feed is wanted: add
`GET /events` on the coordinator that fan-ins the per-session
broadcast buses into one SSE feed. That'd let a dashboard show
real-time activity across the whole system over a single connection.
Out of scope until there's a concrete need.
