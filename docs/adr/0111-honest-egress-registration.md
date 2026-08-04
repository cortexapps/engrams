# ADR 0111: Honest egress registration — applied policies survive process death

Status: Accepted (2026-08-03)

Commit chain: the ADR and the implementation + retirement land in one
PR (#992, branch `honest-egress-registration`, based on main after
ADR 0110's #973; ADR 0110's own retirement PR #987 lands
independently).

Terms used in this document:

- **Egress proxy**: our program on each node that filters and brokers a
  guest's outbound traffic. It runs inside the host-agent process.
- **Registry**: the proxy's in-memory map from a guest's IP to its
  policy (allowed hosts, resolved credentials, inject rules).
- **Policy**: one session's `SessionEgressPolicy` — the resolved,
  ready-to-apply form the coordinator sends the host.
- **Binding record**: the host-durable session→sandbox file
  (ADR 0073), one JSON per session under `<work_dir>/bindings/`. It
  survives host-agent restarts by design.

## Summary, in plain English

The host acks every egress-policy apply, but keeps the applied policy
only in process memory. A host-agent redeploy kills the process; every
surviving VM then loses ALL egress — the proxy refuses every packet as
`UnknownGuest`. The guest's agent sees `ConnectionRefused` on every
API call.

We built a coordinator-side watchdog to manage that damage: on host
re-register, re-derive each survivor's policy from PG + Secret
Manager + the mint providers, and push it back, with five retries. On
2026-08-03 (session 51fc6af7) the rebuild failed identically on all
five attempts — the session's persisted policy carries 77 inject
entries with an empty `secret_ref`, a permanent config defect that the
strict rebuild counted as a transient failure — and a healthy VM
stayed egress-less until an unrelated drain evicted it.

The fix is the ADR 0110 move: make the ack honest and delete the
watchdog. The host persists every policy it acks in a node-local file
beside the binding record — the record that already exists, already
survives rolls, and already has exactly the right lifecycle. A
restarted host-agent rebuilds its registry from a local read. There is
nothing left for the coordinator to re-push.

## Decision

**Rule: an acked egress-policy apply is on the node before the ack.**

1. The `start_agent` path (the one place a policy is applied — ADR
   0013 bundles it) writes the applied `SessionEgressPolicy` verbatim
   to `<work_dir>/bindings/<session_id>.egress` (atomic temp+rename,
   mode 0600) after the in-memory registration succeeds and before the
   ack returns.
2. At startup, after the reattach pass fixes the live set and before
   coordinator registration, the host-agent replays each persisted
   policy whose sandbox survived into the fresh registry
   (`rebuild_egress_from_policies` — the same `notify_session_policy`
   code path an apply uses). Policies for dead sandboxes are inert.
3. The file shares the binding record's lifecycle: removed at
   unbind/destroy. A file with no surviving VM is never replayed.
4. The coordinator's survivor re-push — the retry loop, the strict
   policy-rebuild face, and the `ApplyEgressPolicy` RPC it rode — is
   deleted. The RPC had no other production caller.

## What this defends against

Process death: deploys, panics, OOM kills. The registry dies with the
process; the files do not. Recovery is deterministic and local — no
coordinator round-trip, no secret resolution, no retry policy to get
wrong.

## What this does not defend against

- **Node death.** No VM survives it, so no registry entry is wanted.
  A stale or torn file after a reboot has no surviving owner; the
  rebuild skips what it cannot parse and replays only live sandboxes
  (the ADR 0110 reboot argument).
- **Credential staleness.** A rebuilt minted entry may carry an
  expired token. The proxy's existing refresh seam (`refresh_if_stale`)
  re-mints on first use. The file restores *reachability* — DNS,
  allow-lists, bypass, observe — which needs no fresh secret at all.

## Security posture

The persisted policy contains resolved credentials in plaintext. This
is a deliberate, stated posture: the same disk already holds the same
class of secrets (the sandbox spec sidecar persists `spec.env`
unredacted, and VM memory snapshot files contain whatever the guest
held). The policy file adds no new exposure class; it is written mode
0600 and removed with its binding. An operator who can read
`/var/lib/engram` already has everything this file contains.

## The config-defect half of the incident

An empty `secret_ref` reached GCP Secret Manager as
`projects/<p>/secrets//versions/latest` and got a 400 on every call —
77 entries, 77 WARN lines per resume, and a deterministic failure the
strict rebuild classified as transient. Two changes, independent of
the durability fix:

1. `resolve_explicit_secret_ref` treats an empty ref as "not
   configured" (`Ok(None)`) and never calls the store.
2. `resolve_inject_entries` skips structurally invalid entries (no
   `mint_source` AND an empty `secret_ref`) before any store call and
   reports them once, aggregated.

With the watchdog deleted, the strict/lossy split disappears: one
lossy build path remains (boot/resume, where the user is present and
the refresh seam self-heals), so a config defect can reduce a policy
but can never strand a session.

## What this deletes

| Code | Location |
|---|---|
| The survivor egress re-push (5×10s retry loop, exhaustion arm) | `coordinator/src/api/host_http.rs` |
| `build_survivor_egress_policy` (the STRICT face) + the resolution-failure counters threaded through `resolve_policy_secrets` / `resolve_inject_entries` | `coordinator/src/api/sessions.rs`, `session_boot.rs` |
| The `ApplyEgressPolicy` RPC: proto method + message, client, server arm, trait method, and every test mock | `engram-protocol`, `engram-core`, `engram-host-agent`, coordinator tests |

## Testing

- Unit: policy file round-trip through a fresh store (the restart
  shape); full-replace semantics; unbind removes the file; garbage
  files are skipped; mode 0600. (`bindings.rs` tests.)
- In-process: a fresh registry + fresh store over the same directory
  rebuilds the survivor's entry and skips dead sandboxes
  (`restart_rebuilds_egress_registry_from_persisted_policies`).
- Firecracker (CI KVM lane, `egress_survives_roll`): real VM, real
  reattach — apply, kill generation A, generation B rebuilds, the
  guest resolves with the same allow-list.
- Empty-ref regression: an empty ref never reaches the secret store.

## Rollout

Coordinator and hosts auto-roll from the same main push. The deleted
RPC's only caller is deleted in the same commit, so no cross-version
call exists. Surviving sessions from before the roll have no persisted
policy file yet — for exactly one roll they behave as before this ADR
(egress-less until resume); every policy applied after the roll is
persisted.

## Relationship to other ADRs

- **ADR 0110** (honest writes): the pattern this document applies to
  egress registration. Same split: process death is defended locally;
  node death falls back to the remote authority (here: the normal
  boot/resume policy build).
- **ADR 0073** (binding records): the carrier. The policy file lives
  beside the record and shares its lifecycle.
- **ADR 0013** (bundled `start_agent`): unchanged; the persist rides
  the same atomic point.
- **ADR 0006** (egress proxy): the registry semantics are unchanged;
  only their durability is new.
