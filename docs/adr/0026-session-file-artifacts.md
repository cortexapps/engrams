# ADR 0026: Session file artifacts — agent media sharing + operator file pull

Status: 2026-06-01 — **Proposed.**

We want autonomous agents to **show their work** — surface a screenshot or screen
recording in the session's conversation history in the web dashboard — and we want
operators to be able to pull **any** file out of a session for inspection. This ADR
introduces *session file artifacts*: immutable, forever-lived blobs stored under an
`artifacts/` prefix in object storage, recorded in an `artifacts` table, surfaced as
a `FileShared` session event, and rendered inline (image/video) or as a download
chip (everything else) in the conversation UI. There are **two creation surfaces
with different trust boundaries**, both converging on one storage + serving core.

The end goal is browser tooling (Playwright) for in-session agents; this ADR is the
storage/transport/UI substrate that "show me a screenshot" rides on.

## Context

- Sessions run as Firecracker microVMs. The guest's disk is an ext4 image served
  over NBD from a host-side chunked, content-addressed store (16 MiB chunks, sha256,
  GC'd under `chunks/sha256/`). Writes since the last snapshot live in the host
  `disk_daemon` dirty buffer, not yet hashed.
- The existing **git-forge seam** (ADR 0023) is the template for guest→coord
  capability brokering: the guest dials a vsock port (1028) with a per-session
  broker token; the host relays to the coord; the coord authorizes (constant-time)
  and acts. PR creation already emits a `SessionEvent::PullRequestOpened` that the
  web UI renders as a `PullRequestCard`.
- Conversation history is an append-only `session_events` table (JSONB payload,
  monotonic idx) streamed to the React/Vite dashboard over SSE. No binary/blob
  events exist today; the only "artifact" is the PR card.
- `BlobStorage` (local/gcs) already exists but only stores snapshots/manifests.
  Chunk GC enumerates `chunks/sha256/` + manifests + snapshot sidecars **only** — a
  fresh prefix is never swept.

**Stream the bytes, do not reference storage blocks.** The tempting idea — correlate
a guest file to byte offsets in the chunk store and persist *pointers* — was
rejected:

1. Fresh file data isn't hashed yet (it's in the `disk_daemon` dirty buffer), so
   "pointing" forces a flush that uploads 16 MiB-granular chunks — *more* bytes than
   the file for a small screenshot.
2. Chunks aren't file-granular; content-unique media dedups against nothing, and
   pinning a chunk pins unrelated neighbor blocks.
3. Forever-persistence still forces a copy: GC sweeps the chunks once the session
   dies unless we copy them to a safe prefix (= storing the bytes) or grow an orphan
   pin-set forever. The pointer never avoids the copy.
4. The 16 MiB frame cap that motivated the idea is a non-issue: a tiny header frame
   + a raw streaming body on the same connection keeps any single bincode message
   small. Video streams fine.

**Prior art — E2B (`e2b-dev/infra`), surveyed.** Both decisions match the leading
FC-sandbox platform: (a) their rootfs is a base layer + NBD COW overlay tracking
dirty 4 KiB block indices, exported as opaque block-range diffs — **zero**
FIEMAP/inode/extent introspection anywhere; (b) file egress is plain streaming HTTP
(`ServeContent`/`io.Copy`), unbounded on the leaving-sandbox path. Where E2B gives
no prior art: it has no direct-to-bucket persistence for user files and no
"artifact in conversation history" concept — user files survive only *implicitly*
as dirty blocks in a COW snapshot, fetched on demand. That cannot deliver a
forever-copy decoupled from the session that the UI renders without resuming the VM,
which is exactly our goal. So the bucket-copy + event + UI card is ours; E2B
confirms only the transfer half. (We also keep our vsock/forge seam over E2B's host
HTTP reverse-proxy-to-in-guest-server model — the latter is a larger subsystem
engrams lacks.)

## Decision

Add *session file artifacts* with two creation surfaces, one shared core.

**Shared core (`process_upload`).** Streams a body into
`BlobStorage::put_streaming` under a **server-generated** key
`artifacts/<session_id>/<uuid>`, enforces a per-file size cap (`MAX_ARTIFACT_BYTES`,
abort + delete partial on exceed) and a per-session count/byte quota, records an
`artifacts` row, and emits `SessionEvent::FileShared{ artifact_id, media_type,
size_bytes, caption, at }`. It takes a `Trust { Untrusted, Trusted }` flag that
gates **only** the content-type policy; the caller does auth.

**Surface 1 — in-guest push (Untrusted).** A baked built-in skill `share-file`
(`engram-share --file <path> [--caption ...]`), mirroring the forge `engram-pr`
wrapper, dials a new vsock port **1029** with the per-session broker token (reused
from the forge `git_broker_tokens` map but **decoupled from `[git]` config** — every
image gets it), sends an `UploadRequest` header frame, then streams the raw file
body. The coord **magic-byte-validates** the content against a strict allowlist
(PNG/JPEG/WebP/GIF, MP4/WebM) and **rejects SVG/HTML/everything else** — a
potentially malicious LLM can only share verified media. Never trusts the
guest-supplied MIME.

**Surface 2 — operator pull (Trusted).** A coordinator API
`POST /sessions/:id/artifacts/from-path { path, caption? }`, mounted behind the
bearer/IAP layer. It `ensure_active()`s the session (auto-resuming a recoverable
Idle session; idle-eviction reclaims it later), reads the path out of the guest via
a **new streaming agentd verb** `WireRequest::DownloadStream` (the existing
`Download` verb is 16 MiB-capped — insufficient for video parity), and stores it as
`Trusted` — **no MIME restriction**, same size cap and quota.

**Serving (MIME-agnostic, both surfaces).**
`GET /sessions/:id/artifacts/:artifact_id` streams from `BlobStorage` behind the
bearer/IAP layer (validates the artifact belongs to the path session) with hardened
headers: server-**detected** `Content-Type`, `X-Content-Type-Options: nosniff`,
`Content-Disposition: inline; filename=<uuid>.<ext>`, `Content-Security-Policy:
sandbox`, `Cache-Control: private, no-store`. The dashboard renders `<img>`/`<video>`
for media and a **download chip** for non-media, never `<iframe>`/inline-SVG/
navigation. A dashboard-wide CSP is added in the web nginx config. It is the serving
hardening — not the upload allowlist — that makes arbitrary-type artifacts safe to
serve.

**Persistence.** The `artifacts/` prefix is outside every GC enumeration path, so
copies live forever with no extra machinery. Deleting a session cascades the PG row
but leaves the blob (acceptable for v1; an artifacts-GC is deferred).

## Consequences

- New vsock port 1029 (+ the baked `engram-share` skill) live in the guest image, so
  enabling artifacts requires a session-image re-bake + re-enable (per
  `reference_engrams_deploy_auto_triggers`). Clean break, no compat shim (0 users).
  The operator pull adds **no** guest-side verb — it reads files by streaming `cat`
  over the existing exec channel (see divergence below).
- The forge broker token is generalized to a per-session "broker token" guarding
  both git and uploads, and is now injected for **every** image (not just `[git]`
  ones). `authorize_broker_token` drops the `state.forge` requirement; forge keeps
  its extra gate.
- `ApiError` gains `PayloadTooLarge` (413) and `TooManyRequests` (429) so quota/size
  failures are distinguishable from malformed requests.
- Storage cost grows unbounded by design (forever lifetime) — bounded per session by
  the count/byte quota; no global GC.
- Operator pull can resume a recoverable session (a real cost) to read one file;
  this reuses `ensure_active()` and the normal idle-eviction reclaim, no new state.
- Security posture: reads are authorized as "some operator," not per-owner — same as
  every other coord read endpoint; UUID unguessability is not the access control.
  Deferred hardening: separate sandbox origin, image re-encode (EXIF/polyglot strip,
  images only — never ffmpeg-in-coord), per-operator authz.

## Commit chain

- `9a50bf9` — ADR 0026 Proposed.
- `193fab6` — phase 1: proto upload bridge (port 1029, `MAX_ARTIFACT_BYTES`,
  `Upload{Request,Op,Response}`) + `engram-agentd share-file` guest helper.
- `522f89f` — phase 2: `UploadSink` trait plumbing, FC vsock listener (create +
  restore), host-agent pure-byte relay (`CoordClient::upload_artifact`).
- `e14de08` — phase 3: coord `process_upload` core (sniff/quota/cap), vsock +
  `/api/hosts/upload` transports, `UploadSink` registration, `artifacts` table
  (0045), `MetadataStore` artifact methods, `SessionEvent::FileShared`, shared
  `session_auth` factored out of forge.
- `60b4249` — phase 4: decouple the upload token from `[git]`
  (`get_or_mint_broker_token` + `inject_upload_env` at both forge sites); bake the
  `engram-share` wrapper + `share-file` skill into **every** image.
- `325df32` — phase 5: serve endpoint `GET /sessions/:id/artifacts/:artifact_id`
  with the MIME-agnostic hardened headers (nosniff + Content-Disposition + CSP
  sandbox + no-store); dashboard-wide CSP in the web nginx SPA block.
- `b8b6cb0` — phase 6: web `file_shared` event + `ArtifactCard` (media inline,
  non-media download chip) + Transcript wiring.
- `b6f05fb` — phase 7: trusted operator pull
  `POST /sessions/:id/artifacts/from-path`; `process_upload` → `Result<_, UploadError>`;
  `ApiError::PayloadTooLarge`/`TooManyRequests`.
- `71cdf59` — phase 8: FC `upload_loopback` e2e (wired into `ci.yml` +
  `run-boot-test.sh`); ADR commit chain.
- `33bfe1d` — phase 8 fixup: rustfmt + clippy `type_complexity` on the
  Linux-gated e2e (only surfaced on the Linux CI runner; see verification note).
- _(final, post-merge)_ — flip to **Accepted** once prod-validated (dogfood: an
  agent screenshots a page and it renders in the conversation UI).

**Notes / divergences.**
- Adding the three `MetadataStore` artifact methods fanned out to **7 impls** (the
  real Postgres one + 6 test/mock doubles across coordinator, chunk-store, oci-auth);
  the mocks get trivial stubs.
- **Operator pull reuses `exec_stream` + `cat`, not a dedicated agentd
  `DownloadStream` verb** (which the plan proposed). The only streaming `HostClient`
  method that crosses the coord↔host gRPC boundary is `exec_stream`; a new download
  verb would have needed a whole parallel gRPC streaming RPC for split-mode parity.
  `cat`'s stdout is byte-preserving (there's a proto test), works in-proc + split
  with zero new surface, and a missing/unreadable path surfaces as a non-zero exit →
  the body stream errors and the upload aborts. The `DownloadStream` verb was
  prototyped then reverted.
- The ProcessBackend loopback `POST /sessions/:id/artifacts` (dev `--mode=all`) is
  **deferred** — the production FC path is vsock + split-mode `/api/hosts/upload`,
  which is what the e2e exercises. `share-file` only dials the vsock port today; the
  HTTP-loopback fallback (+ `ENGRAM_UPLOAD_ENDPOINT`) can land with the serve work
  if `just dev` testing needs it.
- **Verification (closed).** The `upload_loopback` FC e2e is
  `#![cfg(target_os = "linux")]`, so it's invisible to macOS `just check` — which is
  exactly why its rustfmt + clippy slips only surfaced on the Linux CI runner (fixed
  in `33bfe1d`; lesson: run dev-vm fmt/clippy on cfg(linux) test files before
  pushing, per `feedback_linux_only_clippy_via_dev_vm`). Now fully verified: CI green
  across all jobs (incl. `tests (firecracker, Linux + KVM)`), and a **real dev-VM
  microVM boot** of `share_file_round_trips_over_vsock` passed in 27.79s — baked a
  rootfs, booted FC, ran `engram-agentd share-file` on a >16 MiB file, and confirmed
  the broker token + full body crossed the vsock intact (past the 16 MiB frame cap).
  The lone CI red along the way was an unrelated `e2e stack` postgres-bring-up race
  that passed on re-run. All non-gated code: `just check` (882 tests) + web `tsc`.
