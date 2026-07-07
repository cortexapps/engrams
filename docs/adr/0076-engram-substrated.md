# 0076 — engram-substrated: one host data-plane daemon (design only)

Status: Proposed (2026-07-06) — **implementation gated; see below**

Issue: #547 delivers this document as an artifact. Flipping it to
Accepted is NOT in that epic's scope.

## The end state

One long-lived host data-plane process, lifecycle-decoupled from
host-agent deploys, exclusively owning: the NVMe chunk cache (the ADR
0069 writer role moves here wholesale — socket, reader type, and
Hello/staging handshake port as-is), UFFD fault serving for ALL
sandboxes (one epoll over N uffds, replacing per-VM handler
processes), NBD serving for all sandboxes (host-agent rolls stop
parking guest disk I/O — the mechanism behind the 111 host_lost/14d
roll-day clusters), base-shm population + GC, staging/prefetch/pins,
and the migration page-server/peer. The host-agent shrinks to control
plane over a narrow UDS API. The no-tokio-in-fault-path discipline is
carried throughout.

## The gating open question: crash/upgrade fd retention

Today's facts, stated honestly:

- A substrated crash closes its userfaultfds. Closing a uffd
  de-registers the ranges; pending faults resolve as ordinary
  zero-fill anon faults — **silent memory corruption for every running
  guest on the host**. Today's per-VM handler bounds that blast radius
  to one VM, and that bound is load-bearing.
- `pidfd_getfd` recovery requires a **live donor**: `live_attach.rs`
  pidfd-steals fds from still-alive processes; nothing can resurrect
  fds from a dead one.

Candidate fd-retention mechanisms (to be evaluated and prototyped, not
decided here):

1. **FC-side retention patch (favored on current evidence):** a small
   vendored-Firecracker change making FC retain a dup of each uffd for
   the VM's lifetime, so a restarted daemon recovers every uffd via
   `pidfd_getfd` with FC as the always-live donor. We own the fork
   (`third_party/firecracker`, upstream-tracking cron); ADR 0045's v3
   patch was ~40 lines — precedent that small auditable fork features
   are in bounds.
2. **A minimal fd-escrow process:** near-zero code (accept SCM_RIGHTS
   dups, hold, re-emit) = near-zero crash surface; but it is one more
   lifecycle to operate and its own crash re-opens the question.
3. **systemd FDSTORE:** likely unavailable under the ADR 0044
   DaemonSet model (no systemd service manager per pod) — documented
   as a non-option unless the deployment model changes.

## The deployment contradiction to resolve

"Rolls rarely" as a node-image component means every substrate
iteration is a MIG node roll — which today discards the NVMe cache,
the very thing being protected. As a second DaemonSet it churns like
the first. The ADR must choose (candidate: DaemonSet with
fd-retention making rolls safe, plus #528's budget making cache loss
survivable) before implementation.

## Gate

**No implementation issue may be filed until the fd-retention design
is chosen and prototyped.** The ADR 0069 interim is deliberately
forward-compatible: the populate protocol, reader type, and readiness
handshake move owners without changing shape.
