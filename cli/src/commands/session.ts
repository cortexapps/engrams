/**
 * engrams session … — SessionService passthrough verbs.
 *
 * `create` is the admin escape hatch (raw image URI, allow-all egress —
 * the policy map gates it manage/all); the product-level create is
 * `engrams task create`, which starts from a profile. `logs` tails the
 * orchestrator's SSE route (the gRPC StreamEvents has a dedicated Hono
 * route, not a passthrough); `exec` rides the server-streaming passthrough.
 */

import type { Clients } from "../client.ts";
import { authHeaders } from "../client.ts";
import { detail, failWith, printJson, table, truncate } from "../output.ts";
import type { Session } from "../gen/engram/app/v1/session_pb.ts";

function sessionJson(s: Session) {
  return {
    id: s.id,
    status: s.status,
    host_id: s.hostId,
    sandbox_id: s.sandboxId,
    image: s.image,
    mode: s.mode,
    created_at: s.createdAt,
    last_active_at: s.lastActiveAt,
  };
}

export async function list(c: Clients, json: boolean): Promise<void> {
  const resp = await c.session.listSessions({}).catch(failWith);
  const sessions = resp.sessions.map((i) => i.session).filter((s): s is Session => !!s);
  if (json) {
    printJson({ sessions: sessions.map(sessionJson) });
    return;
  }
  if (sessions.length === 0) {
    console.log("(no sessions)");
    return;
  }
  table(
    ["ID", "STATUS", "MODE", "IMAGE"],
    sessions.map((s) => [s.id, s.status, s.mode, truncate(s.image, 48)]),
    [36, 10, 8, 48],
  );
}

export async function get(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.session.getSession({ sessionId: id }).catch(failWith);
  const s = resp.session;
  if (!s) failWith(new Error("response carried no session"));
  if (json) {
    printJson(sessionJson(s));
    return;
  }
  detail([
    ["id", s.id],
    ["status", s.status],
    ["image", s.image],
    ["mode", s.mode],
    ["host_id", s.hostId],
    ["sandbox_id", s.sandboxId],
    ["created_at", s.createdAt],
    ["last_active", s.lastActiveAt],
  ]);
}

export interface CreateOpts {
  image: string;
  devVm: boolean;
  harness?: string;
  prompt?: string;
}

export async function create(c: Clients, opts: CreateOpts, json: boolean): Promise<void> {
  const resp = await c.session
    .createSession({
      imageUri: opts.image,
      mode: opts.devVm ? "dev_vm" : "agent",
      prompt: opts.prompt,
      harness: opts.harness,
      // Admin debug create: allow-all egress (no profile to compile a policy
      // from) — same posture as the retired Rust CLI.
      integrationPolicyJson: '{"network":{"default":"allow"}}',
    })
    .catch(failWith);
  if (json) {
    printJson({
      session_id: resp.sessionId,
      status: resp.status,
      image_version: resp.imageVersion,
      kind: resp.kind,
    });
  } else {
    console.log(resp.sessionId);
  }
}

export async function remove(c: Clients, id: string): Promise<void> {
  await c.session.deleteSession({ sessionId: id }).catch(failWith);
  console.log("deleted");
}

export async function exec(
  c: Clients,
  id: string,
  cmd: string,
  timeoutSecs: number | undefined,
  json: boolean,
): Promise<void> {
  const stream = c.session.exec({
    sessionId: id,
    command: cmd,
    ...(timeoutSecs !== undefined ? { timeoutSecs: BigInt(timeoutSecs) } : {}),
  });
  const out: Buffer[] = [];
  const errBuf: Buffer[] = [];
  let exitStatus: number | undefined;
  let sawExit = false;
  try {
    for await (const msg of stream) {
      switch (msg.event.case) {
        case "stdout":
          if (json) out.push(Buffer.from(msg.event.value));
          else process.stdout.write(msg.event.value);
          break;
        case "stderr":
          if (json) errBuf.push(Buffer.from(msg.event.value));
          else process.stderr.write(msg.event.value);
          break;
        case "exit":
          sawExit = true;
          exitStatus = msg.event.value.exitStatus;
          break;
        default:
          break;
      }
    }
  } catch (e) {
    failWith(e);
  }
  if (json) {
    printJson({
      stdout: Buffer.concat(out).toString("utf8"),
      stderr: Buffer.concat(errBuf).toString("utf8"),
      exit_status: sawExit ? (exitStatus ?? null) : null,
    });
  }
  // Mirror the remote exit so shell pipelines compose (killed = failure).
  if (exitStatus !== 0) process.exitCode = 1;
}

/** Render one event envelope: `[{idx}] {kind}: {payload}` (Rust CLI shape). */
export function formatEventLine(idx: string | undefined, kind: string, payloadJson: string): string {
  let payload: string;
  try {
    payload = JSON.stringify(JSON.parse(payloadJson));
  } catch {
    payload = JSON.stringify(payloadJson);
  }
  return `[${(idx ?? "-").padStart(6)}] ${kind}: ${payload}`;
}

/**
 * Tail the persistent event log via the orchestrator's SSE route
 * (`GET /api/v1/sessions/:id/events`). Stays open; Ctrl-C to stop.
 */
export async function logs(c: Clients, id: string, since: number | undefined): Promise<void> {
  const qs = since !== undefined ? `?since=${since}` : "";
  const res = await fetch(`${c.host}/api/v1/sessions/${encodeURIComponent(id)}/events${qs}`, {
    headers: { ...authHeaders(c.auth), accept: "text/event-stream" },
  }).catch(failWith);
  if (!res.ok || !res.body) {
    failWith(new Error(`events stream failed: HTTP ${res.status}`));
  }

  // Minimal SSE parser: frames are blank-line separated; we consume the
  // orchestrator's `id:` (idx), `event:` (kind), and `data:` (payload JSON)
  // lines and skip the 15s `ping` keepalives.
  const decoder = new TextDecoder();
  let buf = "";
  let frame: { id?: string; event?: string; data: string[] } = { data: [] };
  const flush = () => {
    if (frame.event && frame.event !== "ping") {
      console.log(formatEventLine(frame.id, frame.event, frame.data.join("\n")));
    }
    frame = { data: [] };
  };
  for await (const chunk of res.body) {
    buf += decoder.decode(chunk as Uint8Array, { stream: true });
    let nl: number;
    while ((nl = buf.indexOf("\n")) !== -1) {
      const line = buf.slice(0, nl).replace(/\r$/, "");
      buf = buf.slice(nl + 1);
      if (line === "") flush();
      else if (line.startsWith("id:")) frame.id = line.slice(3).trim();
      else if (line.startsWith("event:")) frame.event = line.slice(6).trim();
      else if (line.startsWith("data:")) frame.data.push(line.slice(5).trimStart());
    }
  }
}

export async function log(
  c: Clients,
  id: string,
  limit: number | undefined,
  json: boolean,
): Promise<void> {
  const resp = await c.session
    .getLog({
      sessionId: id,
      kind: "conversation",
      ...(limit !== undefined ? { limit: BigInt(limit) } : {}),
    })
    .catch(failWith);
  if (json) {
    printJson({
      session_id: resp.sessionId,
      kind: resp.kind,
      events: resp.events.map((e) => ({
        idx: Number(e.idx),
        kind: e.kind,
        at: e.at,
        payload: parseOr(e.payloadJson),
      })),
    });
    return;
  }
  if (resp.events.length === 0) {
    console.log("(no events)");
    return;
  }
  table(
    ["IDX", "KIND", "AT", "PAYLOAD"],
    resp.events.map((e) => [
      String(e.idx),
      truncate(e.kind, 28),
      e.at,
      truncate(summarize(e.payloadJson), 80),
    ]),
    [6, 28, 25, 80],
  );
}

function parseOr(payloadJson: string): unknown {
  try {
    return JSON.parse(payloadJson);
  } catch {
    return payloadJson;
  }
}

function summarize(payloadJson: string): string {
  const v = parseOr(payloadJson);
  if (v === null || v === undefined) return "";
  if (typeof v !== "object") return shortValue(v);
  if (Array.isArray(v)) return `[${v.length} items]`;
  return Object.entries(v as Record<string, unknown>)
    .slice(0, 3)
    .map(([k, val]) => `${k}=${shortValue(val)}`)
    .join(" ");
}

function shortValue(v: unknown): string {
  if (typeof v === "string") return truncate(v, 24);
  if (v === null) return "null";
  if (Array.isArray(v)) return `[${v.length} items]`;
  if (typeof v === "object") return `{${Object.keys(v as object).length} keys}`;
  return String(v);
}

export async function resume(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.session.resume({ sessionId: id }).catch(failWith);
  if (json) {
    printJson({
      session_id: resp.sessionId,
      snapshot_id: resp.snapshotId,
      size_bytes: resp.sizeBytes !== undefined ? Number(resp.sizeBytes) : undefined,
      note: resp.note,
    });
    return;
  }
  console.log(resp.note || "resumed");
}

export async function prompt(c: Clients, id: string, text: string, json: boolean): Promise<void> {
  const resp = await c.session
    .sendPrompt({ sessionId: id, text, promptId: "" })
    .catch(failWith);
  if (json) printJson({ session_id: resp.sessionId, note: resp.note });
  else console.log("prompt forwarded");
}
