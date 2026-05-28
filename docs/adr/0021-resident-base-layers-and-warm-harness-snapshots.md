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
- [x] Publish pipeline: `bake-harness-claude.yml` emits a per-platform built-in
      artifact (`:v0.1.0-linux-x86_64`) with the new layer shape (rootfs subtree +
      `artifact.toml`); the baker's catalog resolver consumes it. *(this commit)*
      Still uses the about-to-be-retired `engram-cli harness push` for the OCI push
      itself — P1 swaps in the replacement publish surface.
- [ ] CLI ergonomics for `[harness]` (next pass; the bake itself already works
      through `engram image build`).
- [x] A worked `deploy/demo-claude/` example. *(this commit)*

**P1 — Harnesses baked into templates; retire the standalone harness subsystem.**
Kills the ~2 s serial `chunk.fetch`. (Cheapest high-value; no new kernel mechanism.)
- [ ] agentd launches the baked harness from the **rootfs path** (no drive mount, no
      NBD); `SpawnHarnessRequest` drops `harness_dev`/`harness_mount`.
- [ ] Re-home the egress-proxy CA cert off the (removed) harness drive → vsock/agentd
      RPC into the guest trust store.
- [ ] `HarnessSpec` → `SessionMode::{Agent, DevVm}`; coord reads the harness from the
      image manifest, not the session.
- [ ] Delete: `harness_packs` table (+ drop migration), coord `/api/harnesses`, CLI
      `harness add/push/list/rm`, registry types, host-agent
      `ensure_harness`/`ensure_harness_ext4`, `SandboxSpec.harness_substrate`/
      `harness_pack_uri`, and option-D `swap_harness_drive` (+ `option_d_*` tests).
- [ ] **Publish-surface handoff**: P0's `bake-harness-claude.yml` calls `engram-cli
      harness push` for the OCI push. That CLI subcommand is retired in this phase;
      introduce a replacement (e.g. `engram-cli builtin-harness publish` or a
      standalone publisher binary) and switch the workflow in the same pass so the
      CI doesn't break between commits.
- [ ] FC integration tests (`harness_loopback`, `e2e_harness`) green against the
      baked-in harness and wired into `ci.yml`'s `--test` list.

**P2 — Residency for template chunks** (pin NVMe + stage-on-enable + host warmup
gate). GCS off the boot path; retire the boot-path prefetch.
- [ ] Pin enabled-template chunks (+ warm snapshot) on NVMe; transactional
      stage-on-enable; host not `Ready` until its working set is staged.
- [ ] Retire the boot-path prefetch / working-set machinery (ADR 0014 M1.13/M1.14).

**P3 — Warm snapshots per template** (capture at the agent-`idle` signal; restore
into a ready agent). Removes the ~7–9 s. The big payoff.
- [ ] Move the agent to a **persistent model** (claude server/SDK mode; emits `Idle`)
      — supersedes the child-per-prompt `claude --print` loop.
- [ ] Open-question-2 measurement first: persistent-agent + cheap in-process warm
      start under ~1 s? If yes, skip VM warm snapshots.
- [ ] If not: capture the warm memory snapshot at `Idle` (incl. running agent);
      restore into a ready agent; per-session COW on top.

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
