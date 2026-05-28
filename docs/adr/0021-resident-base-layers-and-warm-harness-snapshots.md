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

Design/direction document; phasing is sketched at the end and each phase lands with
its own prod validation per ADR 0020's ship loop.

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

### 1. An image is a self-contained *template*; harnesses are baked in

Drop the standalone "upload a harness" feature. An image is baked as a complete
**template = OS + tools + (optionally) one agent**, decided **at image-bake time**.

- **Out-of-the-box**: engram ships curated templates — `debian+claude`,
  `+opencode`, `+codex`, … — each pre-baked, warm-snapshotted, and resident. The DX
  is "sessions come with these agents, ready in ~hundreds of ms."
- **Custom**: bake your own template with your agent (the `engram-harness` SDK for
  the protocol adapter + the existing `engram image build`). The (image, agent)
  binding is fixed at bake time.
- **Harness-less templates are first-class** (answer to "just a dev machine"): a
  template with no agent is captured at **OS-ready** — exactly today's base snapshot
  (ADR 0020 P1). `harness: none` already exists (`HarnessSpec::None` → boot the VM,
  drive via shell / `engram exec`). Same restore mechanism, it just captures
  "whatever is idle" — OS-only or OS+agent.

This is the proven **template model** (E2B, Modal): build a template that includes
your agent, snapshot it, restore it per session. It retires an entire subsystem —
the `harness_packs` registry, `engram harness add/push`, harness OCI artifacts, the
harness ext4 **substrate + NBD path** (the ~2 s serial `chunk.fetch` — gone, the
agent is just in the rootfs), and the **option-D `swap_harness_drive`** machinery at
restore. The protocol **adapter** (the irreducible per-agent translation, today's
824-line `engram-harness-claude`) is still needed and still authored against the
`engram-harness` SDK — but it ships *inside the template*, not as a separate pack.

Tradeoff named honestly: you lose runtime "any image × any agent" mixing. But that
flexibility *was* the source of the coherence hazard above, agents are co-designed
with their environment in practice, and you can still bake multi-agent templates
(only the warmed one gets warm-start). Good trade for a bounded catalog.

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
  capture + the `engram-harness` SDK (adapter authoring, baked into templates).
  Net boot-path code is expected to *shrink*.
- **Risk**: storage amplification at large catalog (mitigated by per-pool pinning +
  content dedup); the `UFFDIO_CONTINUE`/FC unknown (gates only runtime-dedup); and the
  persistent-agent dependency for warm snapshots to pay off.

## Phasing (sketch — each lands + is prod-measured before the next)

- **P1 — Harnesses baked into templates; retire the standalone harness subsystem.**
  Bake the agent into the image; drop the harness pack/registry/substrate/NBD/option-D
  swap. Kills the ~2 s serial `chunk.fetch`. (Cheapest high-value; no new kernel
  mechanism.)
- **P2 — Residency for template chunks** (pin NVMe + stage-on-enable + host warmup
  gate). GCS off the boot path; retire the boot-path prefetch.
- **P3 — Warm snapshots per template** (capture at the agent-`idle` signal; restore
  into a ready agent), *gated on* moving the agent to a persistent model and the
  open-question-2 measurement (cheap-boot-vs-snapshot). Removes the ~7–9 s.
- **P4 — Runtime RAM dedup** (`UFFDIO_CONTINUE` + shared per-template backing) — gated
  on the FC spike (open question 1). Density, not latency.
- **P0 (parallel) — `engram-harness` SDK + a great `engram image build` DX** for
  baking agent templates (first- and third-party share one pipeline).

## References

- ADR 0007 (chunked immutable storage / canonical-memory UFFD) — the substrate.
- ADR 0014 (warm-pool / option-D / prefetch + working-set) — machinery this *retires*
  on the boot path.
- ADR 0019 (cold-boot tracing) — produced the trace evidence above.
- ADR 0020 (base-snapshot restore + chunk-native UFFD) — shipped; **Blocked on this
  ADR** for its remaining phases; its prod profile is the input here.
