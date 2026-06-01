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

- New vsock port 1029 and a new agentd verb (`DownloadStream`) — both baked into the
  guest image, so enabling artifacts requires a session-image re-bake + re-enable
  (per `reference_engrams_deploy_auto_triggers`). Clean break, no compat shim (0
  users).
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

- _(this commit)_ — ADR 0026 Proposed.
- _(filled in as phases land: proto+helper, sink/relay wiring, coord core+storage+
  migration, env decouple+baked skill, serve+security+CSP, web UI, operator pull)._
- _(final)_ — flip to Accepted with prod-validation note.
