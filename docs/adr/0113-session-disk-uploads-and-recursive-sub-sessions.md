# ADR 0113: Session-disk uploads and recursive sub-sessions

Status: Proposed

Date: 2026-08-08

## Context

A prompt can refer to text, but it cannot carry a browser-selected file into a
session. Agent sessions also cannot create and supervise child sessions through
the generic tool protocol. A durable attachment library would add ownership,
retention, garbage collection, and object-store policy that this use case does
not need. The session disk already has the required lifetime: it survives
snapshot, eviction, and resume, and it disappears when the session is deleted.

The existing `WriteFiles` RPC is not suitable for composer uploads. It is a
unary setup operation with a 3 MiB aggregate limit. The host-to-guest upload
verb also carries one buffered payload with a 16 MiB frame limit.

## Decision

### Session files

Browser uploads are ordinary files under this canonical directory:

```text
/tmp/uploads/<upload-uuid>/<sanitized-file-name>
```

The client chooses the UUID and file name. The control plane validates both and
derives the path. A caller cannot supply another destination. One file is at
most 512 MiB. The session disk limit bounds aggregate storage.

`SessionService` exposes authenticated `UploadFile`, `ReadFile`, and
`CopyFiles` operations. Upload and read use streaming RPCs. The orchestrator
serves dedicated authenticated HTTP upload and download routes and does not put
these methods on its generic Connect pass-through surface.

An upload writes to a temporary sibling, checks the declared length and SHA-256
while it streams, sets non-executable permissions, and renames the file
atomically. A retry with the same upload UUID, name, length, and digest succeeds
without changing the file. Different content for an existing canonical path
fails. A reader never observes a partial file.

`ReadFile` and `CopyFiles` accept any normalized absolute guest file path.
`/tmp/uploads` is a composer convention, not a file-transfer policy. This lets
an agent copy a file that it created elsewhere on its disk to a child session.
`CopyFiles` keeps each source path unchanged. It streams bytes from the source
guest through the source host, coordinator, target host, and target guest. It
does not use object storage and does not buffer the whole file. The coordinator
resumes a parked destination before it starts a transfer.

The composer stores ordered text and upload tokens. A token renders as a chip
but serializes as its literal canonical path. Send stays disabled until every
upload is complete. For a new task, `defer_initial_prompt` creates the task and
session without sending the prompt. The orchestrator still uses the prompt for
the initial title. The browser uploads files and then uses the normal
`SendPrompt` path. Prompt and event schemas do not carry attachment metadata.

### Task tree and coordination

Every child session owns a real `type = 'subsession'` task. A task stores its
parent and root task IDs, immutable local and root-relative canonical names, the
spawning session ID, and a versioned non-secret launch-policy snapshot. A root
task has no local or canonical name. Canonical segments match
`[a-z0-9][a-z0-9_-]{0,63}` and are never reused inside a task tree.

The launch snapshot fixes the image, skills, environment policy, network
policy, integration authority, and other non-secret launch inputs. Spawn reads
current credential bytes through the existing credential paths. A child may
override only harness, model, and effort. Legacy tasks without a launch snapshot
cannot spawn.

The generic tool registry provides `spawn_session`, `send_session_message`,
`read_session`, `interrupt_session`, `terminate_session`, `list_sessions`, and
`wait_sessions`. Child manifests omit tools that require direct human input.
The caller can inspect and control only proper descendants. A root session can
control the full tree. The default limits are depth four and eight live
descendants per root.

Coordination mutations use a durable ledger keyed by caller session, operation,
and caller idempotency key. The row stores a canonical request hash, reserved
IDs, status, and result. Reusing a key with different arguments fails. Spawn
reserves task and session IDs, creates a promptless child, copies files, and
sends one stable prompt ID. Message send copies files before it uses the normal
prompt path and stable prompt ID. Interrupted children return to `open`;
terminated children become `cancelled`.

Reads use the coordinator's append-only session event log. Cursors are opaque
to tool callers. `wait_sessions` waits for the first relevant update, defaults
to all descendants and 30 seconds, and has a 120-second hard cap.

Deleting one child terminates only that task. Deleting a root recursively
terminates its descendants. `ListTasks` hides sub-sessions unless requested;
`GetTask` returns descendants so the root page can render the tree.

## Consequences

- Uploaded files have session lifetime. They are not user-library objects.
- A source session must exist when a file is copied.
- The file path in a prompt is the real guest path, so no prompt-event schema
  change or attachment resolver is required.
- Coordinator, host-agent, and guest-agent rollout must precede UI enablement.
- The guest agent wire changes and every base image must be refreshed before
  uploads are enabled.
- Task trees add a schema and authorization boundary. The task row, session
  authorization snapshot, and operation ledger remain the durable source of
  truth; in-memory tool execution is not a workflow engine.

## Rollout

1. Land the inactive app, host, and guest file-transfer wire.
2. Deploy the coordinator and host fleet and refresh the guest agent in every
   enabled image.
3. Enable composer uploads and measure upload latency, failure rate, byte rate,
   and retry outcomes.
4. Land task-tree persistence and coordination tools.
5. Enable the child-task UI and coordination tools, then measure spawn latency,
   copy failures, depth and fan-out rejections, and wait duration.
6. Change this ADR to `Accepted` after the grouped UI and production metrics
   ship.
