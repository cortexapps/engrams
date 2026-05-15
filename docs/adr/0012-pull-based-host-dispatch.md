# ADR 0012: Pull-based host dispatch (deferred)

Status: deferred, 2026-05-15
Phase: pre-design — captured here so the architectural intent
survives the operational workaround currently in production.

## Context

The coord today owns a long-lived WebSocket to every FC host
(`engrams-fc-host` MIG instances). Sandbox-create, exec, snapshot,
prompt — every control-plane verb dispatches as a wire Notify over
that WS. The WS handle is in-memory; the scheduler in
`engram-coordinator::host_registry::HostRegistry` calls
`HostClient::create(...)` against the handle.

The shape composes for `--mode=all` (one process, one WS, no
ambiguity) but breaks when the coord is horizontally scaled in
production. Each host's WS pins to *one* coord pod by L4-LB hash.
The other pods have no entry in their in-memory `host_registry`
for that host — so when `POST /sessions` round-robins to the
non-owning pod, the scheduler sees an empty registry and returns
`unavailable: "no hosts connected to the coordinator"`. With N
coord replicas and M hosts, the failure rate trends to
`(N-1)/N` for hosts whose WSes randomly land on a single pod.

The mirror added in `b56bce8` (persist host capacity to Postgres on
every heartbeat) papers over the **read** side — `/api/hosts`
returns a consistent view because both pods read the same row.
But the dispatch path can't be papered over the same way:
dispatching a `CreateSandbox` Notify requires the WS handle, and
the handle lives in exactly one pod's memory.

In production today the coord runs at `replicas=1` to dodge the
issue. That works for engrams' current scale but caps coord
throughput at one process and gives a ~30 s API-outage window
during rolling upgrades.

The architectural debt is real. The right fix is a protocol
change, not a routing layer.

## What other systems do

| System | Pattern | Notes |
|---|---|---|
| **AWS Lambda** | Centralized routing (Pattern B) | Frontend → WorkerManager → MicroManager → Firecracker. WorkerManager is a dedicated service whose only job is "given an invocation, find the worker that owns the slot." |
| **Fly.io** | Per-host autonomy, no centralized scheduler | `flyd` runs per-host; the Machines API talks to a specific `flyd` via the Fly Proxy + `fly-replay` header. Explicitly rejected centralized consensus. |
| **Replit Goval** | Shard + route-by-lookup | Infrastructure split per-region. Inside a region, `Eval` proxies WebSocket connections by asking a `Controlplane` service "which VM is this Repl on?" then forwards. |
| **Kubernetes, Nomad** | Pull-based agents (Pattern A) | API server is freely horizontal because it doesn't dispatch — it writes desired state to etcd; the kubelet on each node watches its slice and reacts locally. |
| **Slurm** | Active/passive controller | `slurmctld` is single-leader (or active/passive HA pair); the dispatch model needs the controller→node connection to be authoritative. Engrams' current shape, but Slurm acknowledges it and doesn't try to scale horizontally. |

## Decision

**Migrate to pull-based dispatch (Pattern A).** Hosts watch a
`sandbox_assignments` table (or `pg_notify` channel) keyed on
their `host_id`; the coord writes assignments rather than
dispatching them. Lifecycle events flow back via Postgres or a
thin event channel (existing WS becomes optional, scoped to
low-latency observability rather than control).

The decision is **deferred** — not implemented now. The current
prod deployment runs single-replica coord (see "Operational
workaround" below). This ADR captures the destination so the
next round of architectural work doesn't re-derive it from
scratch.

### Why Pattern A specifically

- **Coord scales freely.** No cross-pod routing infrastructure;
  no consensus; no shard rebalancing. Multiple coord replicas
  can all serve writes against the same Postgres-backed state.
- **Hosts gain autonomy.** A coord rolling restart doesn't drop
  control: hosts keep watching, in-flight assignments make
  progress. Today a coord restart kills every WS and the
  host-agent's reconnect loop has to repopulate.
- **Matches the K8s mental model.** Operators reasoning about
  the system in K8s terms (declarative spec in etcd, agents
  reconciling) need no special-case knowledge.
- **`pg_notify` is already a habit.** We use it for `host_dead`
  events and migration-cleanup signals; the pattern is familiar.

### Why not Pattern B (centralized routing layer)

Adds a new component (the routing service) without simplifying
anything. The cross-pod routing problem we'd solve is one we
created by pretending the coord can be horizontal in the first
place. Pattern A removes the problem; Pattern B works around it.

### Why not Pattern C (active/passive with leader election)

Acceptable — and is essentially what we're doing today (degenerate
to `replicas=1`). But it caps coord throughput at one process for
all time, and the K8s-native expression of active/passive (leader
election via Lease) adds runtime machinery that pull-based
doesn't need.

## Operational workaround in production

Until this lands:

- `hpa.enabled: false`, `pdb.enabled: false`, `replicaCount: 1`
  in `engrams-internal/values/engrams.yaml`.
- During coord rolling restart, hosts disconnect, the
  host-agent's reconnect loop with exponential backoff reattaches
  within seconds of the new pod coming up.
- API requests during the restart window return 5xx; the SPA's
  React Query retries cover most user-visible flake.

This is structurally identical to Slurm's controller HA model
(single active `slurmctld`, fast failover). Acceptable for
engrams' current scale (~10 hosts, low session-create rate).

## Open design questions for the migration

Captured here so they're not rediscovered when this is picked up:

1. **Watch transport.** `pg_notify` is the obvious choice but
   payload size is small (~8 KiB Postgres limit) and delivery is
   best-effort under load. The pattern would be "notify on
   change, host pulls the assignment row by id." Alternative:
   long-poll on a Postgres listen channel with the row id
   embedded.
2. **Idempotency on the host side.** A flaky watch must not
   create the same sandbox twice. The assignment row needs a
   stable id the host checks before acting; `sandbox_id` is the
   natural candidate, written before the dispatch.
3. **Cancellation.** Today the coord can synchronously decide
   "kill this sandbox." With pull-based, the assignment needs a
   `desired_state` column the host reconciles against (Active,
   Idle, Destroyed). Mirrors K8s pod spec.
4. **Exec / streaming.** Single-shot RPCs (create, snapshot)
   port cleanly to pull. Streaming RPCs (`exec_stream`, shell
   WS, SSE events) need a different channel — probably keep the
   WS for those, since they're inherently host→client and don't
   need cross-pod dispatch.
5. **Migration plan.** Big-bang vs. incremental. Likely
   incremental: keep the WS path operational, introduce
   pull-based for `create` first (the broken path today), move
   other verbs one at a time. Final cleanup removes the WS
   dispatcher entirely.

## Status field meaning

`deferred` — accepted as the right destination; implementation
intentionally postponed until the workaround stops being
acceptable. Re-status as `accepted` when work begins; `landed`
when migration completes.
