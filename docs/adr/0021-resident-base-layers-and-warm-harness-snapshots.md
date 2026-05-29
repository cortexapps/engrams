# ADR 0021: Resident templates + warm snapshots — the from-scratch boot model

Status: 2026-05-27 — **Proposed.** ADR 0020 shipped chunk-native UFFD restore to
prod and **measured where the time goes now**: guest-memory restore is cheap
(~1.2 s, warms further), the Anthropic round-trip is negligible (~0.2 s), and the
two real costs are (a) the **harness substrate bind** — in the prod trace, ~24
*serial* `chunk.fetch @ ~83 ms` (GCS RTT) during `fc.spawn_harness` — and (b) the
**agent's own in-guest cold startup** (~7–9 s for Claude Code's runtime), which is
CPU-bound and does not warm. This ADR proposes the from-scratch model that attacks
both at the root: **(1) treat an image as a self-contained *template* — OS + tools
+ (optional) agent baked together — and keep its full working set *resident* on
each FC host (GCS off the boot path); (2) capture a per-template *warm snapshot* at
idle so a session restores into a ready-to-run state; (3) retire the standalone
harness-upload subsystem — harnesses are baked into templates.** It supersedes ADR
0020's P2 (memory working-set prefetch is now low-value) and reframes P3/P4. **ADR
0020 is marked Blocked on this ADR.**

Design/direction document; phasing is at the end as a **living checklist** — each
item is crossed off as it lands, and each phase carries its own prod validation per
ADR 0020's ship loop.

**DX decisions locked 2026-05-27** (drive Decision §1 and the P0/P1 phasing below):
one harness per image, baked at bake time (or none); built-ins opt in via a
declarative `[harness]` block in `engram.toml` and the baker downloads a published
per-platform harness artifact (x86_64-linux first) and injects it; custom harnesses
are COPY'd into the rootfs by the author's own Dockerfile and declared in
`engram.toml`; the per-session harness *selection* is retired — which harness an
image runs is an image property, and the session API keeps only a mode choice
(run the baked harness vs. boot a pure dev VM).

**No backwards compatibility.** This is a clean break, not a migration. We delete the
standalone harness subsystem outright — no compat shims, no dual-read of old
`harness_packs`/`HarnessSpec`, no support for pre-0021 images or session requests.
Enabled images and any in-flight sessions are re-baked / re-created against the new
model; the deploy is a cutover. (engram is pre-1.0 and operator-curated, so there's
no external contract to preserve — carrying compat would just bloat the very code
paths this ADR retires.)

## Context — what the prod profile told us

ADR 0020's prod run (Claude Code session, bogus key → Anthropic 401), cold (first
on a freshly-rolled host) vs warm, and the Cloud Trace waterfall for `session.create`:

| phase | cold | warm |
|---|---|---|
| enable image = base-snapshot capture (one-time) | ~70 s | — |
| POST → Active (UFFD restore + harness bind) | ~11 s | ~4.5 s |
| Active → Claude Code `run_started` (in-guest startup) | 7.2 s | 9.1 s |
| `run_started` → "Invalid API key" (Anthropic RTT) | 0.22 s | 0.24 s |

```
session.create                               3302 ms
├─ coord.restore_base_for_session            1184 ms   (UFFD restore — cheap)
│   └─ fc.restore_in_jail                     464 ms
│       └─ fc.spawn_uffd_handler              102 ms
└─ host.start_agent → fc.spawn_harness       2073 ms
    └─ chunk.fetch  ~83 ms  ×24  (SERIAL)    ~2000 ms   ← harness substrate from GCS, cold
```

Two conclusions:

1. **The harness bind is the same serial-`chunk.fetch` pattern that started ADR
   0020** (the original 25 s trace's "162 serial chunk.fetch"), relocated to the
   harness substrate. Parallelizing helps but still pays GCS RTT; the real fix is
   to not touch GCS on the boot path.
2. **The agent's startup (~7–9 s) is in-guest CPU, not I/O** — residency can't touch
   it; the only lever is to restore *past* it (a warm snapshot taken after the agent
   is idle). Prod NVMe also confirmed the dev-vm's ~104 s node startup was slow-PD
   pathology, not a real cost.

The current storage model (ADR 0007/0014) is "chunks live in GCS; local NVMe is an
LRU cache; prefetch + working-set traces exist to *minimize* on-demand GCS fetches."
For a **bounded, admin-curated catalog** with latency as the goal, you'd flip it:
**local NVMe is the pinned source of truth for the active set; GCS is durability +
transport, off the boot read path** — which lets us *retire* the prefetch machinery
rather than tune it.

This ADR also resolves a design knot we worked through explicitly: trying to share
*one* warm snapshot across many images fails on **memory↔rootfs coherence** (a
snapshot booted on image A has kernel page-cache / inode state describing A; COW'ing
image B underneath leaves cached memory inconsistent with the disk). The template
model sidesteps it entirely — see below.

## Decision

### 1. An image is a self-contained *template*; exactly one harness is baked in (or none)

Drop the standalone "upload a harness" feature. An image is baked as a complete
**template = OS + tools + (optionally) one agent**, decided **at image-bake time**.
**One harness per image** — this keeps the warm snapshot (§3) coherent and the boot
profiling clean.

**The author surface — a singular `[harness]` table in `engram.toml`.** (The old
`[[harness]]` *array* was retired and is still rejected by `deny_unknown_fields`
tests; this singular table is the new, expected shape. Its resolved launch contract
is rendered into the image's `manifest.toml`, the single source of truth coord /
host-agent / agentd / warm-capture all read.)

```toml
# Built-in: the baker downloads the published per-platform artifact + injects it.
# Both `builtin` and `version` are required — no rolling defaults, reproducible bakes.
[harness]
builtin = "claude"
version = "v1.2.3"               # explicit pin → GHCR OCI digest

# — or — Custom: the author's own Dockerfile already COPY'd the binary in.
# No `version` (the binary is whatever the author put in their rootfs).
[harness]
name = "my-agent"
exec = "/opt/my-agent/harness"   # path inside the rootfs
args = ["--serve"]               # optional

# — or — omit [harness] entirely → harness-less image (pure dev VM).
```

- **Out-of-the-box (built-ins)**: engram ships curated harnesses — `claude`, later
  `opencode`/`codex` — as **GHCR OCI artifacts** (one tar.gz layer per
  `(name, version, platform)`, e.g.
  `ghcr.io/cortexapps/engrams/harness-claude:v1.2.3-linux-x86_64`). `engram image
  build` resolves `(builtin, version)` via a hardcoded catalog in the CLI, pulls the
  layer via the existing `engram-oci` client, and injects the contents into the
  rootfs at the canonical path `/opt/engram/harness/` (binary + any bundled runtime,
  e.g. the `claude` CLI). The author **must pin the version explicitly** in
  `engram.toml` — no rolling defaults — so a given `engram.toml` bakes to a
  deterministic artifact digest, which the baker also records in the rendered
  manifest. The artifact is the *bake input* that replaces the old runtime OCI pack;
  the CI publish pipeline lives on, repurposed.
- **Custom**: author builds a harness binary against `engram-harness-proto` (the
  wire types — frame format, attach handshake, the event/command enums) directly,
  **COPYs the binary (+ any runtime) into the rootfs in their own Dockerfile**, and
  declares `[harness] name/exec/args`. The baker validates `exec` exists and records
  the contract. Docker-native, handles multi-file runtimes, no separate upload
  surface. **No SDK wrapper**: the contract is just "a binary that speaks the proto
  on vsock, plus how to launch it" — anything beyond that would be opinion the
  author may not want to inherit.
- **Harness-less templates are first-class** (answer to "just a dev machine"): omit
  `[harness]`; the template is captured at **OS-ready** — exactly today's base
  snapshot (ADR 0020 P1). Same restore mechanism; capture is "whatever is idle"
  (OS-only or OS+agent).

**The per-session harness *selection* is retired.** Which harness an image runs is an
image property, so `HarnessSpec::{None, Builtin{name}}` on the session goes away
(coord no longer resolves a name → pack URI). The session-create API keeps only a
**mode**: `SessionMode::{Agent, DevVm}` — run the baked harness, or boot the image as
a pure dev VM (shell only). A `DevVm` session of a harnessed template restores the
**same** warm snapshot and simply doesn't drive the resident agent (an idle process
in RAM is harmless), so no separate boot path is needed.

This is the proven **template model** (E2B, Modal): build a template that includes
your agent, snapshot it, restore it per session. It retires an entire subsystem —
the `harness_packs` registry, `engram harness add/push/list/rm`, harness OCI packs,
the harness ext4 **substrate + NBD path** (the ~2 s serial `chunk.fetch` — gone, the
agent is just in the rootfs), and the **option-D `swap_harness_drive`** machinery at
restore. The protocol **adapter** (the irreducible per-agent translation, today's
824-line `engram-harness-claude`) is still needed and still authored directly
against the existing `engram-harness-proto` wire crate — but it ships *inside the
template*, not as a separate pack. We deliberately **do not ship an SDK wrapper**
above the proto: the surface authors care about is "a binary + how to launch it,"
and any ergonomic layer above the wire is opinion they may not want.

**One migration detail to carry:** the egress-proxy CA cert is delivered today via
the harness *drive* (`<harness_mount>/.engram-host/ca.pem`). With the drive gone, the
per-host/per-session cert must reach the guest trust store another way (vsock/agentd
RPC). Tracked in P1.

Tradeoff named honestly: you lose runtime "any image × any agent" mixing. But that
flexibility *was* the source of the coherence hazard above, and agents are
co-designed with their environment in practice. Good trade for a bounded catalog.

### 2. Resident base layers (GCS off the boot path)

One content-addressed chunk store, two tiers:
- **Local NVMe** holds the **pinned** working set of every enabled template (+ its
  warm snapshot). Pinned ≠ LRU; while enabled it's guaranteed-present.
- **GCS** is the durable origin + cross-host transport. The boot/restore path issues
  **zero network reads**.

**The working set is symmetric over disk + memory + snapshot.** The cache is
*already* one shared content-addressed store on NVMe (the `chunk-cache`, read by
the NBD disk daemon, the UFFD memory handler, and the prefetchers alike) — so
residency is not a new store, it is *pinning the right chunks in the one we have*.
The prod profile (see Prod measurements) made the gap concrete: **memory** chunks
were prefetched and warm, but **disk/rootfs** chunks were paged in on demand from
GCS during resume, because only memory had a prefetch path. Residency closes that
asymmetry — pin a template's disk manifest, memory manifest, *and* warm snapshot as
one unit. (Chunk *sizes* stay split — 512 KiB memory / 16 MiB disk; unifying them
for cross-dedup is blocked by guest page-alignment and would 8× memory
write-amplification, so disk-size retune is a secondary, measure-driven knob, not
the residency lever.)

Eliminates GCS RTT from boot, removes the "freshly-rolled host's first session is
2–3× slower" cliff, and makes boot deterministic. Operationally:
- **Staging is part of enable** (transactional, like ADR 0020's enable): a template
  isn't "ready" until its chunks (+ warm snapshot) are **staged on all hosts**.
- **Host rolls**: a freshly-rolled host is **not `Ready` for scheduling until its
  working set is staged** (warmup gate) — cold cost off the session path.
- **Scheduling**: "every host has everything" needs no placement constraint; pin
  per-pool at larger catalog scale.

Content-addressing dedups the shared bytes: if 5 templates bundle Claude, its bundle
is stored once. So "bake the agent into every template" is not storage blowup.

### 3. Warm snapshots, per template, captured at idle — coherent by construction

Today's base snapshot is captured with a **stub harness** (ADR 0020 option-D), so
the restored VM has no agent — every session cold-starts it (~7–9 s). Instead,
capture the template's **canonical/base memory snapshot at the agent-idle point**
(the agent has booted and signaled ready). The snapshot therefore **includes the
running agent's memory** (Bun heap, loaded code) — that *is* its purpose. A session
UFFD-restores it into a ready agent; per-session memory writes diverge COW on top.

**Coherence is free here** precisely because the harness is baked in: the warm
snapshot is captured on a template and restores *that same template*. Memory and
rootfs were captured together and are restored together — no cross-image carry, no
swapping a different rootfs under warm memory, no cache-invalidation hazard. The
only thing COW'd on top is the session's own workspace + mutations. It's the
standard coherent base-snapshot model, captured later (agent-idle) instead of
earlier (stub-idle).

The capture trigger moves from "agentd ready" to the agent's **`idle`/ready signal**
(declared by the harness via the SDK). For harness-less templates, the trigger is
OS-ready (no agent).

**Hard dependency to call out:** the warm snapshot only *pays off* if the agent runs
**persistently** (boots once per session, serves many prompts). The current
`engram-harness-claude` is **child-per-prompt** (`claude --print` spawned + exits per
prompt) — at idle there is no `claude` process, so a snapshot would capture only the
adapter, not a warm agent. So this milestone **presupposes moving the agent to a
persistent model** (interactive / SDK / server mode that stays resident). The first
question is whether the agent boot can simply be made *cheap* in-process (a Bun/V8
startup snapshot baked into the template, or a persistent agent that boots fast) —
**if so, residency alone suffices and we skip VM warm snapshots entirely.** Warm
snapshots are the fallback for "the agent boot is genuinely expensive and persistent."

### 4. COW is orthogonal; storage dedup is free, runtime-RAM dedup is not

- **(a) Storage dedup — free.** Immutable, content-addressed. A session's disk =
  template manifest (shared) + divergent chunks (its writes, deduped by content).
  Per-session storage ≈ divergence. **Requires a correct refcount/GC** for
  unreferenced divergent chunks — currently a hole (chunk-GC pulled May 2026); the
  deduped store must treat GC as first-class.
- **(b) Memory at rest — free dedup.** Warm snapshots are content-addressed; they
  share kernel/base-OS/agent chunks across templates.
- **(c) Runtime RAM — NOT free.** `UFFDIO_COPY` installs a **private** page per
  guest, so K live sessions of one template cost K× the working set in host RAM. Fix:
  a **shared per-template backing** (memfd/tmpfs populated once from resident chunks)
  served via **`UFFDIO_CONTINUE`** (minor faults), so sessions share read-only base
  pages, COW on write → K sessions cost `base + K×dirty`. This is the one piece
  needing a new mechanism + FC support (open question 1). COW divergence is unaffected.

### 5. Resulting layering

| layer | model |
|---|---|
| **template (OS + tools + agent)** | one baked image; chunked-disk + COW for the rootfs (it mutates); resident |
| **canonical/warm memory** | chunked + UFFD, captured at idle (incl. the agent); resident; runtime-shared via `UFFDIO_CONTINUE` |
| **session workspace + mutations** | COW on top (disk divergence + private/`CONTINUE` memory pages) |
| **everything** | resident on NVMe (pinned); GCS = durable origin + transport, off the boot path |

This **retires** the on-demand-fetch / prefetch / working-set-trace machinery on the
boot path (ADR 0014 M1.13/M1.14) **and** the entire standalone-harness substrate.
Net "simplify via abstractions, retire code."

### 6. Security

The microVM is the boundary, and engram's TCB never executes untrusted build- or
run-time code in its own domain:
- **Build** = image build. First-party templates built by engram's CI; custom
  templates built in the author's pipeline; engram only ever **ingests opaque,
  content-addressed, optionally-signed image artifacts** and runs them in microVMs.
  No separate untrusted-harness-upload surface (the subsystem is gone). engram can
  verify Anthropic's signed release manifest (key `31DD … 1A7E CACE`) when baking
  the Claude template.
- **Runtime**: the agent execs **only** inside the per-session microVM; the host TCB
  (coord, host-agent) never execs agent code. The vsock harness protocol is the
  untrusted boundary (frame-validated; cf. `--dangerously-skip-permissions`: "the
  sandbox is the safety boundary").
- **Warm-snapshot capture is itself a sandboxed boot** (the capture VM is a microVM),
  so even capturing a third-party agent runs it only inside the sandbox.

## The endgame this composes into

Resident templates + warm snapshots are how the proven Firecracker-snapshot
platforms hit ~100 ms: **a session = UFFD-restore a *resident* warm snapshot of a
ready-agent template, COW on top.** Restore is local (no GCS), lands past the
agent's ~7–9 s startup, and runtime RAM is shared across same-template sessions. The
remaining per-session cost is the COW delta + the first-prompt workspace scan.

## Open questions

1. **FC shared-memfd / minor-fault support** for `UFFDIO_CONTINUE`. Stock FC backs
   guest RAM anonymous-private + MISSING-mode; runtime RAM dedup needs FC to back
   guest RAM with a shared memfd + register MINOR. **Unverified on our FC build** —
   gates the runtime-dedup half only (latency + storage wins don't depend on it).
   Worth a spike before committing P4.
2. **Cheap agent boot vs warm snapshots.** Before building the warm-snapshot capture
   pipeline, measure whether a persistent agent + in-process warm start (Bun/V8
   startup snapshot in the template) already gets first-token under ~1 s. If so, skip
   VM warm snapshots. This also requires moving the agent off child-per-prompt.
3. **Warm-snapshot freshness.** An agent/base update = re-bake + re-warm-snapshot the
   template (one artifact, one version, one pipeline — cleaner than the old split,
   but still churn). Trigger + transactionality (mirror enable: not ready until
   recaptured + staged).
4. **Workspace model.** Is the user's *workspace* (their code) a separate per-session
   mount on top of the template, or baked into the template? Separate-mount keeps
   templates reusable and warm snapshots small; needs a per-session volume + the
   first-prompt scan. (Likely separate-mount — confirm.)
5. **Storage GC.** Residency + COW divergence is only sound with a correct refcount/GC
   (currently a regression) — design it in, not bolt-on.

## Consequences

- **Supersedes ADR 0020 P2** (memory working-set prefetch) — memory restore is
  already cheap; the standalone harness substrate it might have applied to is retired.
- **Reframes ADR 0020 P3/P4** as minor tail-shaves under this model.
- **Retires** the boot-path prefetch/working-set machinery **and** the standalone
  harness subsystem (registry, add/push, OCI packs, substrate NBD, option-D swap).
- **New build surface**: residency staging + warmup gate + per-template warm-snapshot
  capture + the built-in harness artifact publish pipeline (custom harness authors
  stay on the existing `engram-harness-proto` wire crate; no SDK wrapper). Net
  boot-path code is expected to *shrink*.
- **Risk**: storage amplification at large catalog (mitigated by per-pool pinning +
  content dedup); the `UFFDIO_CONTINUE`/FC unknown (gates only runtime-dedup); and the
  persistent-agent dependency for warm snapshots to pay off.

## Phasing (living checklist — each lands + is prod-measured before the next)

Cross items off (`[x]`) as they land, with the commit SHA where useful.

**P0 — `[harness]` DX + built-in artifact pipeline** (no SDK wrapper; custom
authors target `engram-harness-proto` directly):
- [x] Singular `[harness]` table on `ImageManifest` + parse/validate in the baker's
      `engram.toml` reader (mutual-exclusion of `builtin` vs `name`/`exec`; replace
      the stale-`[[harness]]`-rejection tests with singular-form tests).
      *(d157e87, 32bcddf)*
- [x] Baker injects the built-in artifact / validates the custom `exec`; renders the
      launch contract into `manifest.toml`. *(1d73837)*
- [x] Publish pipeline: per-platform built-in artifact
      (`:v0.1.0-linux-x86_64`) with the new layer shape (rootfs subtree +
      `artifact.toml`); the baker's catalog resolver consumes it. *(this commit;
      originally in `bake-harness-claude.yml`, later folded into ci.yml's
      `bake-harness-claude-artifact` job — see P0 follow-up below.)*
      Still uses the about-to-be-retired `engram-cli harness push` for the OCI push
      itself — P1 swaps in the replacement publish surface.
- [ ] CLI ergonomics for `[harness]` (next pass; the bake itself already works
      through `engram image build`).
- [x] A worked `deploy/demo-claude/` example. *(this commit)*

**P1 — Harnesses baked into templates; retire the standalone harness subsystem.**
Kills the ~2 s serial `chunk.fetch`. (Cheapest high-value; no new kernel mechanism.)
- [x] agentd launches the baked harness from the **rootfs path** (no drive mount, no
      NBD); `SpawnHarnessRequest` drops `harness_dev`/`harness_mount`. *(95a4a63;
      legacy drive `inject_egress_proxy_ca` + `mount_harness` deleted; the
      CaCertInstaller paths feed the child's TLS env vars now.)*
- [x] Re-home the egress-proxy CA cert off the (removed) harness drive → vsock/agentd
      RPC into the guest trust store. *(60bfa89 P1.1: `InstallHostCa` RPC +
      cacerts installer; f328566 P1.2: FC backend calls it post-readiness;
      legacy drive path stays parallel until the drive itself is retired.)*
- [x] `HarnessSpec` → `SessionMode::{Agent, DevVm}`; coord reads the harness from the
      image manifest, not the session. *(cbe7f36; migration 0039 swaps
      `sessions.harness` JSONB for `sessions.mode` TEXT.)*
- [x] **Subsystem deletion P1.5a — harness_packs registry** *(97fef2f)*:
      `harness_packs` table (migration 0040 drops it), `HarnessPack` type, the
      four `MetadataStore` trait methods + postgres impls, coord `/api/harnesses`
      surface, CLI `harness add`/`list`/`rm` subcommands. `engram harness push`
      survives (CI still uses it; retires with the publish-surface handoff below).
- [x] **Subsystem deletion P1.5b+c** *(2117891, −3478 / +93 net)*: dropped
      `SandboxSpec.harness_substrate` + `harness_pack_uri`, `Sandbox::swap_harness_drive`,
      `restore_base_for_session`'s `harness_pack_uri`/`harness_name` params, host-agent
      `ImageCache::ensure_harness*` (+ types + helpers), FC's
      `repoint_harness_drive` + substrate canonical-symlink machinery, VZ's
      substrate virtio-blk attach. Substrate-dependent tests deleted
      (`option_d_*`, `harness_loopback`, `proxy_e2e`, `restore_chain`,
      `e2e_harness`, `e2e_shell`); CI workflow updated. P1.6 reintroduces
      baked-in-harness coverage.
- [x] **Publish-surface handoff** *(087eccd)*: new `crates/engram-publish-builtin-
      harness` binary; `bake-harness-claude.yml` switched; `engram-cli` `Harness`
      subcommand surface deleted entirely; `RestoreBaseForSessionRequest`'s
      `harness_pack_uri` / `harness_name` fields retired from the proto (numbers
      reserved).
- [x] **Coord-level test of the baked-harness path** *(d4d777e)*: a
      `mode = dev_vm` session against a harnessed image goes Active without
      spawning the harness — `tests/api.rs::
      create_session_dev_vm_mode_skips_harness_on_harnessed_image`.
- [x] **Real-FC baked-harness loopback test** *(7990a25)*:
      `tests/baked_harness_loopback.rs` bakes an image with `engram-harness-noop`
      COPY'd into `/opt/noop/harness`, boots it on real Firecracker, and drives
      the attach + RunStarted handshake. Verified on the dev VM (24.69 s). CI's
      unprivileged-FC step picks it up.
- [x] **Resurrect deleted e2e tests against the baked-harness model**
      *(cbf3ea6, 3eab5b9, 2c4f9e7)*: all four tests dropped in P1.5b+c are back —
      `proxy_e2e` + `restore_chain` in `engram-sandbox-firecracker`,
      `e2e_shell` + `e2e_harness` in `engram-host-agent`. The harness pack now
      travels in `/opt/engram/harness/` inside the rootfs (matching the bake-time
      `inject_builtin_harness` shape); the per-host egress-proxy CA flows in via
      `AgentSpec.host_ca_pem` → `InstallHostCa` vsock RPC instead of the retired
      `.engram-host/ca.pem` substrate smuggle. Wired into ci.yml under
      unprivileged FC (`restore_chain`), root FC (`proxy_e2e`), and a new
      true-e2e step (`e2e_shell`, `e2e_harness`). Cold paths all green on the
      dev VM (24–39 s each).
- [x] **Warm-restore vsock ECONNREFUSED — backend-level fix, not just a test
      tweak** *(1dfbb20 then a50dbd6)*: original hypothesis ("FC vsock UDS
      needs a settle window after `load_snapshot`") was wrong. Empirical chain
      (`ls -la` + `ss -lx` in root and per-VM netns + `lsof` + `ps` + full
      `firecracker.log`) showed the UDS file exists with no process holding
      it, FC in zombie state, FC log ending `Vmm is stopping. exit_code=0`
      exactly ~911 ms after `load_snapshot` returned — i.e. the guest kernel
      rebooted via `panic=1 reboot=k` → `KVM_EXIT_SHUTDOWN` → FC clean exit.
      Cause: `PooledBackend.snapshot` (ADR 0018 12m) calls `inner.pause` →
      flush → `inner.snapshot` → resume. If the guest is still in early boot
      at the pre-pause, agentd hasn't completed `bind(AF_VSOCK)` and the
      snapshot captures a half-initialised vsock driver. First test-side fix
      added a 2 s sleep before `snapshot`; second commit moved the gate into
      `PooledBackend.snapshot` itself via `inner.wait_agent_ready` *before*
      the pre-flush pause, so every caller is protected. The `connect_fc_vsock`
      retry bandage from `bd55c0c` was reverted to one-shot. Prod paths
      reachable without the gate: SIGTERM-during-cold-boot (`shutdown.rs`
      checkpoints all live sandboxes after a 5 s drain) and any future
      coord-driven `snapshot(id)` fired shortly after create.
      P2 unblocked.
- [x] **CI catch-22 on built-in harness publishing — structural fix**: the
      old `bake-harness-claude.yml` workflow only fired on push-to-main with
      path triggers, so a PR that bumped `HARNESS_VERSION` (or that just
      needed the artifact to exist for the first time) had no way to publish
      `:<version>-<platform>` before its own e2e_stack lane consumed it. Also
      meant PRs that changed the harness binary without bumping the version
      silently tested the *previous* GHCR tag.
      Fix: folded the bake into `ci.yml` as a job that always runs on every
      PR + push, uploads the staged artifact as a GitHub Actions workflow
      artifact, and only publishes to GHCR on push-to-main. Added an env
      override (`ENGRAM_BUILTIN_HARNESS_<NAME>_REPO`) on the in-baker
      `BuiltinCatalog` so the `test-e2e-stack` lane can download the
      workflow artifact, re-publish to `localhost:5001`, and have the
      baker resolve `[harness] builtin = "claude"` against that local
      registry. Production `bake-demo-image` on push-to-main keeps the
      default catalog → still pulls from GHCR. PR-built binaries never
      reach a production-consumed GHCR namespace. Closed
      `.github/workflows/bake-harness-claude.yml` (folded entirely).

**P1.8 — Soft-delete `enabled_images`; unblock resume after disable.**
Prod-found 2026-05-28 against session `cb8e4e35`: the existing
"disable image" endpoint **physically `DELETE`s the `enabled_images`
row**, so any idle session whose image was disabled while it was idle
fails resume on `resume_manifest_bundle` ("session image is no longer
enabled"). Combined with the dead-host detector marking those sessions
`Evacuating` after a MIG roll, this then thrashes the evac-resumer
through `evac_attempts=20` per stuck session.
- [x] Schema: `enabled_images.soft_deleted_at TIMESTAMPTZ NULL`
      (migration 0041). Re-enable via the same `upsert` clears the
      flag (operator-friendly undelete; chunks were never GC'd because
      the pin-set query treats soft-deleted rows as live).
- [x] API split: `list_enabled_images` + `get_enabled_image` are
      live-only (session-create + dashboard); new
      `get_enabled_image_any` returns the row regardless of
      soft-delete (resume path); `delete_enabled_image` is reserved
      for the future chunk-GC physical drop.
- [x] Guarded disable: `soft_delete_enabled_image` is transactional,
      takes a row-level `FOR UPDATE` lock on the image row, counts
      sessions in `{pending, created, active, evacuating}`, returns
      `Disabled` / `AlreadyDisabled` (idempotent) / `Blocked(Vec<…>)`.
      `POST /api/enabled-images/disable` maps to 204 on success +
      409 with a `DisableBlockedResponse` JSON body listing the
      blocking sessions when blocked.
- [x] Resume-path lookup: `resume_manifest_bundle`
      (`api/sessions.rs`) uses `get_enabled_image_any`. A
      soft-deleted-but-still-present row resumes cleanly with
      manifest env intact. A genuinely-missing row (chunk-GC ate it
      — not reachable today since chunk-GC is pulled) downgrades to
      warn + harness-less resume, which is good enough for forensics.
- [x] Audit: chunk-GC pin-set (`engram-postgres`'s
      `list_enabled_image_disk_manifest_ids`) is **already correct**
      because it doesn't filter `soft_deleted_at` — soft-deleted
      rows continue to pin their base-layer chunks until physically
      dropped. Comment added to lock the invariant.

**P1.8 follow-ups (deferred, not blocking this commit chain):**
- [ ] **Idle reaper.** Today an `Idle` session lives forever unless
      the operator nukes it manually. Once we ship a reaper that
      moves `Idle → Dead` after configurable inactivity, a
      soft-deleted image with zero remaining references becomes a
      candidate for physical delete + chunk reap.
- [ ] **Refcount-driven physical delete of soft-deleted images.**
      Pair with the cross-cutting GC effort (below). Trigger: 0
      sessions reference the image AND 0 recoverable snapshots
      derive from it. `delete_enabled_image` is the existing trait
      surface; the missing piece is the sweeper.
- [ ] **Force-override of `Blocked`.** Operator escape hatch — kill
      blocking sessions + soft-delete in one call. Not on the
      correctness path; defer until the routine flow is exercised.

## Prod measurements (2026-05-29) — what P2 + P3 are actually fixing

Measured against `demo-claude:warm-d94ed06`, post-`fbce5dc` deploy, two fresh
FC hosts (`engrams-fc-5cd6`, `engrams-fc-bdz6`), traced via Cloud Trace and
host Prometheus. These numbers should be the entry-point for P2 + P3 work.

### Substrate cost: created → active

> **Correction (re-traced 2026-05-29).** The first pass of this table blamed
> *UFFD memory pre-fault*. The authoritative span tree (`4bdd4903…`) shows that
> was wrong — and is internally contradicted by the "Agent cost" section below,
> which correctly calls the same spans *NBD* `chunk.fetch`. The ~2.5 s is
> **on-demand rootfs reads: 16 MiB *disk* chunks paged in over NBD, serial from
> GCS, while the guest resumes**; *memory* is already prefetched and warm.

DevVm session (no claude harness — pure substrate), host `engrams-fc-5cd6`:

| Phase                          | Cost        | Where it goes                                                                                                       |
| ------------------------------ | ----------- | ------------------------------------------------------------------------------------------------------------------- |
| memory prefetch (237 chunks)   | warm        | `prefetch_memory_chunks` 8-way into the shared `chunk-cache` *before* restore — already NVMe-warm, off the hot path |
| `fc.restore_in_jail` + uffd    | ~0.4 s      | spawn FC, configure jail, open sockets, spawn UFFD handler                                                          |
| `resume` (rootfs page-in)      | **~2.5 s**  | **30 × `chunk.fetch` @ ~84 ms serial from GCS, `bytes=16 MiB` (disk), `op=resume`** — guest reads its rootfs over NBD on a cold cache |
| `fc.install_host_ca`           | (idle wait) | ~2.45 s span but `busy≈95 µs` — *waiting* on agentd readiness, itself gated behind the rootfs reads above; not work |
| **Total substrate**            | **~2.7 s**  | `POST /sessions mode=dev_vm` + Cloud Trace `4bdd4903…` (session `49272389`)                                         |

Agent-mode session (same image, same host) adds `fc.spawn_harness` ~0.2 s
for the agentd→harness RPC, then *the claude bottleneck below*.

The dominant substrate cost is **30 serial GCS round-trips paging in the rootfs
during `resume`**: the disk daemon (`disk_daemon::backend`) has **no prefetch**, so
every uncached rootfs chunk pays ~84 ms. Memory got the full prefetch treatment
(ADR 0014 M1.13); disk did not. P2's residency retires this — but the lever is
**disk**, not the memory UFFD path the original draft named.

### Agent cost: harness attached → `run_started`

Same image, agent-mode: **2 m 44 s**. Two sessions on the same host
(`a8c6950f`) both measured 2 m 44 s ± 3 s — host warmth doesn't help.

What rules out disk I/O as the agent-mode bottleneck:

- Host metrics (`engram_chunk_cache_*` on `engrams-fc-5cd6`): 8,783 NVMe
  hits vs 297 GCS misses (97 % cache hit rate), 30.8 s total GCS time
  across the host's entire lifetime. Even if every GCS fetch on the host
  happened inside one slow session, it couldn't account for >31 s of
  the 160 s.
- Cloud Trace coverage gap: NBD `chunk.fetch` spans are only emitted while
  the `resume` operation scope is open (`trace_scope.rs:14-16`), and that
  scope closes at `start_agent` completion (`13:05:31` for trace
  `4bdd4903…`). The 2 m 40 s window after that is invisible to traces —
  not because nothing's happening, but because emitters are scope-gated.
  When investigating in-guest latency, lean on host Prometheus + guest-side
  evidence (disk-flush rate, run-loop hooks), not trace queries.
- `engram-harness-claude/src/main.rs:424` spawns `Command::new(claude_bin)`
  per prompt — fresh Bun runtime + JIT-compile of node_modules + config
  init on every cold start. The 67 MiB disk-write batches every 30 s in
  the host log are claude/Bun's startup writes, not large reads.

The dev_vm comparison is the proof: second `exec` on the same dev_vm
session round-trips in **10 ms**. The 150× cost of agent mode is entirely
inside claude/Bun — exactly what P3 (persistent agent + warm snapshot at
`Idle`) targets.

### E2B comparison (`~/test/infra` checkout, GCP-deployed)

E2B advertises ~150 ms sandbox starts. From their codebase the
architectural moves we don't have yet:

1. **Templates resident as first-class objects** on every orchestrator
   (`packages/orchestrator/pkg/sandbox/template/cache.go`, 25 h TTL).
   `Template{memfile, rootfs, snapfile, metafile}` kept in-process; cold-
   cache cost paid once per template per host, then every restore reuses
   the in-memory file handles directly. *Maps to P2.*
2. **4 MiB chunk size** (`shared/pkg/storage/storage.go:43`
   `MemoryChunkSize = 4 * 1024 * 1024`) — this is E2B's *memory* size.
   **Ours is 512 KiB memory / 16 MiB disk**; the earlier "we're on 16 MiB"
   conflated the two. Since the substrate bottleneck is *disk* page-in, the
   only relevant chunk-size lever is the *disk* size, and it is secondary to
   prefetch/residency. *Secondary knob, not a headline win.*
3. **Parallel memory prefetcher with worker pools**
   (`uffd/prefetch/prefetcher.go`, feature-flag-tunable
   `MemoryPrefetchMaxFetchWorkers` + `MemoryPrefetchMaxCopyWorkers`). Our
   UFFD pre-fault during `load_snapshot` is serial. *Cheap-win lever — can
   land pre-P2.*
4. **Peer-to-peer template transfer**
   (`template/peerclient` + `peerserver`). A fresh orchestrator pulls
   templates from another orchestrator on the same network instead of
   GCS. *Optional P2 enhancement; nice-to-have for cross-zone latency.*
5. **(Likely) `UFFDIO_CONTINUE` + shared memfd backing** — pages shared
   across same-template sandboxes in RAM. *Maps to P4 (gated on
   open question 1).*

### The real pre-P2 cheap win (corrected)

The two wins this section originally named both **miss the measured cost**:
- *Parallel UFFD memory pre-fault* — **moot**. Memory is already prefetched 8-way
  and warm before resume; the serial `prefault_from_trace` is off the hot path,
  and the slow spans are disk, not memory.
- *Shrink 16 MiB → 4 MiB* — **secondary**. 16 MiB is the *disk* size and is
  over-fetched, but the defect is that disk chunks are *un-prefetched and served
  serially from GCS*; shrinking adds RTTs unless prefetched. (Memory is already
  512 KiB.)

The real prelude: **prefetch the rootfs/disk working set.** Extend the host-boot
prefetch (`image_prefetch`) to warm the base **snapshot's** disk manifest — not
just the image's — mirroring what `prefetch_memory_chunks` already does for
memory. Small diff, on the residency path, collapses the 30 serial GCS reads.
Measure against the ~2.7 s baseline before the full residency machinery lands.

### Expected impact by phase

| After             | Substrate | Agent first-token | Notes                                       |
| ----------------- | --------- | ----------------- | ------------------------------------------- |
| Today             | ~2.7 s    | ~2 m 44 s         | the regression we measured                  |
| Pre-P2 cheap win  | ~330 ms   | ~2 m 44 s         | rootfs/disk prefetch (host-boot warm of the base snapshot's disk manifest) |
| P2 (residency)    | ~230 ms   | ~2 m 44 s         | NVMe-local disk + memory fetches @ ~1 ms + FC restore overhead |
| P3 (warm + persistent) | ~230 ms | <1 s           | restored into a hot Bun runtime             |
| P4 (RAM dedup)    | **sub-100 ms** | <1 s         | shared backing, minor-faults only           |

P2 is the biggest single move and is unblocked. P4 is what gets us *under*
100 ms but requires the FC build to support `UFFDIO_CONTINUE` (open
question 1). P3 is orthogonal to substrate latency and is what removes
the 2 m 40 s agent gap.

**P2 — Residency for template chunks** (pin NVMe + stage-on-enable + host warmup
gate), **symmetric over disk + memory + snapshot**. GCS off the boot path for both
NBD disk reads and UFFD memory faults; retire the boot-path prefetch.

Entry-point context: see "Prod measurements" above — the substrate cost is serial
on-demand **rootfs/disk** page-in during resume (memory is already warm). Target
after this phase: substrate ~230 ms (the 30 cold disk fetches collapse to NVMe @
~1 ms; remaining ~200 ms is FC restore overhead).

- [ ] **Pre-P2 cheap win (corrected): rootfs/disk prefetch.** Extend host-boot
      `image_prefetch` to warm the base **snapshot's** disk (+ memory) manifest,
      not just the image's — mirroring `prefetch_memory_chunks`. (The
      originally-named parallel-UFFD-prefault is moot — memory is warm; shrink-4 MiB
      is secondary — see the corrected cheap-win note above.) Measure vs the 2.7 s
      baseline.
- [ ] Pin enabled-template chunks — image disk + base snapshot disk + memory +
      warm snapshot — on NVMe; transactional stage-on-enable; host not `Ready`
      until its working set (disk *and* memory) is staged.
- [ ] **Cache invariant**: the UFFD handler must share the host's `chunk-cache`.
      Today only prod overrides the `uffd-chunk-cache` default — make the shared
      cache the default, not opt-in (`engram-sandbox-firecracker` lib.rs).
- [ ] Retire the now-subsumed prefetch / working-set machinery (per-restore
      `prefetch_memory_chunks`, image-only host-boot prefetch, ADR 0014
      M1.13/M1.14) — folded into the one residency mechanism.
- [ ] Verify on prod: re-measure substrate against a freshly-rolled host that
      staged at enable-time; the `chunk.fetch{bytes=16 MiB, op=resume}` spans
      should be gone. Target ~230 ms substrate.

**P3 — Warm snapshots per template** (capture at the agent-`idle` signal; restore
into a ready agent). Removes the ~7–9 s. The big payoff.

Entry-point context: the prod measurement of **2 m 44 s** for harness-attached
→ `run_started` (above) is the regression P3 retires. The original ADR plan
budgeted ~7–9 s — that was for a single Bun startup, not for
`Command::new(claude_bin).args(...)` spawning a fresh child *per prompt*.
Today's code path is at `engram-harness-claude/src/main.rs:424` inside
`run_one_claude_prompt`; each turn pays the full cold-start cost. The
base_snapshot captured by `build_base_snapshot` at `pooled_backend.rs:1971`
is taken at `wait_agent_ready` — *before* the harness binary is even
launched — so restoring it gives you agentd-ready, not claude-ready. P3
moves both ends of this:

- [ ] Move the agent to a **persistent model** (claude server/SDK mode; emits `Idle`)
      — supersedes the child-per-prompt `claude --print` loop at
      `engram-harness-claude/src/main.rs:424`. Open question whether
      claude's server mode is mature enough; alternative is keeping the
      child but warming Bun via V8 startup snapshots.
- [ ] Open-question-2 measurement first: persistent-agent + cheap in-process warm
      start under ~1 s? If yes, skip VM warm snapshots.
- [ ] If not: capture the warm memory snapshot at `Idle` (incl. running agent);
      restore into a ready agent; per-session COW on top. Concrete change:
      `build_base_snapshot` shifts capture from `wait_agent_ready` to
      `wait_harness_idle` (new primitive) — requires (a) host-side to know
      when the harness has emitted `Idle`, (b) the harness contract to
      guarantee `Idle` after first-prompt-ready, not just after attach.

**P4 — Runtime RAM dedup** (`UFFDIO_CONTINUE` + shared per-template backing) — gated
on the FC spike (open question 1). Density, not latency.
- [ ] FC spike: shared memfd + MINOR-mode on our FC build.
- [ ] If viable: shared per-template backing → K sessions cost `base + K×dirty`.

**Cross-cutting — storage refcount/GC** (open question 5): residency + COW divergence
is only sound with a correct GC (currently regressed — chunk-GC pulled May 2026).
- [ ] Design GC in as first-class, not bolt-on (`engram-chunk-store/src/gc.rs`).

## References

- ADR 0007 (chunked immutable storage / canonical-memory UFFD) — the substrate.
- ADR 0014 (warm-pool / option-D / prefetch + working-set) — machinery this *retires*
  on the boot path.
- ADR 0019 (cold-boot tracing) — produced the trace evidence above.
- ADR 0020 (base-snapshot restore + chunk-native UFFD) — shipped; **Blocked on this
  ADR** for its remaining phases; its prod profile is the input here.
