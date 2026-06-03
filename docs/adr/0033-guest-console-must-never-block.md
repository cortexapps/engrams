# ADR 0033: The guest console must never block guest processes

Status: 2026-06-02 — **Accepted.** Shipped as a single PR off `main`.

## Context: a prod session frozen at "thinking"

Prod session `5665bdd3` (2026-06-03 01:09 UTC) accepted its prompt and then
sat at "thinking" indefinitely: no assistant messages, no error event, no
`RunCompleted`. Inside the guest there was no `claude` child at all and
`/workspace/.engram` had never been created — the harness received nothing.
Its main thread's kernel stack told the whole story:

```
wait_woken → n_tty_write → redirected_tty_write → vfs_write   (fd 1 = /dev/console)
```

The harness was blocked **mid-`write(2)` on a `tracing` log line to
`/dev/console`**, frozen before its event loop could dequeue the prompt.
Confirmed live: `echo x > /dev/console` inside that guest hangs forever.

Mechanism: agentd runs as pid 1 with fds 0/1/2 on `/dev/console` (inherited
through `engram-init`'s `exec`), and the harness child inherited that stdio
from agentd's `Command::spawn`. The emulated 16550's output is drained during
cold boot (it lands in the host's per-sandbox `firecracker.log`), but **after
a snapshot restore Firecracker no longer drains the serial port** — an
upstream FC restore limitation. From that point the guest TTY output buffer
only fills. Whichever process pushes cumulative console output past the
buffer blocks forever on its next write. It's a lottery: most sessions never
log enough post-restore to hit it, which is why this looked like a one-off
"stuck session" rather than a systemic failure.

The blast radius is worse than one harness: agentd's own `tracing` output
goes to the same console, so agentd itself can freeze the same way — taking
`/exec`, eviction, and the flush path down with it.

## Decision

Guest processes must never have a *blocking* console as their log sink.
Two changes, both in `engram-agentd`:

1. **The harness child gets file stdio, never the console.**
   `harness_supervisor` spawns the harness with stdout/stderr appended to
   `/var/log/engram/harness.log` (`ENGRAM_HARNESS_LOG` overrides it; tests
   use a tempdir) and stdin null (input arrives over vsock, never the
   console). If the log file can't be opened, fall back to `Stdio::null()`:
   losing logs is acceptable, inheriting a blocking console is not. Bonus:
   harness logs are now readable in-guest via `/exec` — previously they
   vanished into the undrained console post-restore anyway.

2. **agentd's stdout goes `O_NONBLOCK` when it's a TTY.** First thing in
   `main`, before the first `tracing` line. Writes to a full console then
   fail with `EAGAIN` and the fmt layer drops the line — lossy exactly when
   the console is wedged, i.e. when the output was unreadable anyway.
   stderr stays blocking on purpose: `eprintln!` panics on write failure
   (pid-1 panic = dead guest), and agentd only writes stderr during startup
   while the console still drains.

## Alternatives considered

- **Redirect agentd entirely to a file in `engram-init`** — simplest, and
  fixes both processes at once, but loses agentd's cold-boot console lines in
  the host's `firecracker.log`, which are the only diagnostics when agentd
  fails before its vsock listener is up. The `O_NONBLOCK` approach keeps
  cold-boot logging byte-identical.
- **Fix the host side (drain serial after restore)** — it's an upstream FC
  restore limitation; even if patched, "the guest deadlocks if the host
  stops reading" is a failure mode we shouldn't carry. Guest-side hardening
  closes the class.

## Consequences

- Rolls out with the next image re-bake + base-snapshot re-bake (agentd and
  the spawn path are baked into the rootfs). Until then, restored sessions
  keep playing the buffer-fill lottery.
- `ttyd` (piped), `/exec` children (piped), and the claude child (piped by
  the harness) were already safe; the harness and agentd were the only two
  console-attached writers.
- The existing wedged-session signature for operators: session `active`,
  prompt `agent_message` present, zero assistant messages, no `claude`
  process in-guest, and a `n_tty_write` stack on the harness — if seen on a
  pre-fix snapshot, the session is unrecoverable in place (kill + recreate).
