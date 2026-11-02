import { z } from "zod";
import { requireThat, RunnerFailure, sha256, type Context } from "./context.ts";
import type { PublicClient } from "./transport.ts";

export function canonicalJson(value: unknown): string {
  if (
    value === null ||
    typeof value === "boolean" ||
    typeof value === "string" ||
    (typeof value === "number" && Number.isFinite(value))
  )
    return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (typeof value === "object" && value)
    return `{${Object.keys(value)
      .sort()
      .map(
        (key) =>
          `${JSON.stringify(key)}:${canonicalJson(Reflect.get(value, key))}`,
      )
      .join(",")}}`;
  throw new RunnerFailure("ASSERTION_FAILED", "canonical_json_value");
}

export async function readGuestFile(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
  path: string,
) {
  requireThat(
    path.startsWith(ctx.guest.stateDirectory + "/") &&
      /^[A-Za-z0-9_.-]+$/.test(path.slice(ctx.guest.stateDirectory.length + 1)),
    "guest_proof_path",
  );
  const chunks: Uint8Array[] = [];
  let bytes = 0;
  let expected: { size: bigint; hash: string } | undefined;
  for await (const response of client.sessions.readFile(
    { sessionId, path },
    { timeoutMs: ctx.deadlines.requestMs },
  )) {
    if (response.frame.case === "metadata") {
      requireThat(
        !expected &&
          response.frame.value.path === path &&
          response.frame.value.sizeBytes <= 1024n * 1024n,
        "guest_file_metadata",
      );
      expected = {
        size: response.frame.value.sizeBytes,
        hash: response.frame.value.sha256,
      };
    } else if (response.frame.case === "chunk") {
      requireThat(expected, "guest_metadata_first");
      bytes += response.frame.value.length;
      requireThat(bytes <= 1024 * 1024, "guest_proof_size");
      chunks.push(response.frame.value);
    } else throw new RunnerFailure("ASSERTION_FAILED", "guest_file_frame");
  }
  const value = Buffer.concat(chunks);
  requireThat(
    expected &&
      BigInt(value.length) === expected.size &&
      sha256(value) === expected.hash,
    "guest_file_integrity",
  );
  return value;
}

export const NativeProofSchema = z.object({
  callId: z.string().regex(/^[A-Za-z0-9_-]{1,64}$/),
  tool: z.string().regex(/^[A-Za-z0-9_]{1,128}$/),
  resultReceived: z.boolean().optional(),
  forgedRejected: z.boolean().optional(),
  delivery: z.enum(["available", "unavailable"]).optional(),
  outcome: z
    .enum(["succeeded", "failed", "canceled", "outcome_unknown"])
    .nullable()
    .optional(),
  resultUnavailable: z.boolean().nullable().optional(),
  isError: z.boolean().nullable().optional(),
  denialPlanMode: z.boolean().optional(),
  selectedToolName: z
    .string()
    .regex(/^[A-Za-z0-9_]{1,128}$/)
    .optional(),
  selectedSchemaSha256: z
    .string()
    .regex(/^sha256:[a-f0-9]{64}$/)
    .optional(),
  exposedMcpToolNamesSha256: z
    .string()
    .regex(/^sha256:[a-f0-9]{64}$/)
    .optional(),
  planSha256: z
    .string()
    .regex(/^sha256:[a-f0-9]{64}$/)
    .optional(),
  resultMatchesExpected: z.boolean().optional(),
  resultSha256: z
    .string()
    .regex(/^sha256:[a-f0-9]{64}$/)
    .optional(),
  expectedResultSha256: z
    .string()
    .regex(/^sha256:[a-f0-9]{64}$/)
    .optional(),
});

export function parseGuestProofRecords(bytes: Buffer) {
  requireThat(bytes.length <= 1024 * 1024, "guest_proof_size");
  const text = bytes.toString("utf8");
  // Each flushed record is one line. A writer may currently be appending the final line.
  const lines = text.split("\n");
  lines.pop();
  requireThat(lines.length <= 128, "guest_proof_record_bound");
  return lines.map((line) => {
    let raw: unknown;
    try {
      raw = JSON.parse(line);
    } catch {
      throw new RunnerFailure("ASSERTION_FAILED", "guest_proof_json");
    }
    const proof = NativeProofSchema.safeParse(raw);
    requireThat(proof.success, "guest_proof_shape");
    return { proof: proof.data, sha256: sha256(line + "\n") };
  });
}
export async function readGuestProofRecords(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
  path: string,
) {
  return parseGuestProofRecords(
    await readGuestFile(ctx, client, sessionId, path),
  );
}
export async function readGuestProof(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
  path: string,
) {
  return (
    (await readGuestProofRecords(ctx, client, sessionId, path)).at(-1) ?? null
  );
}

export async function guestExec(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
  argv: string[],
) {
  let started = false;
  let exited = false;
  let status: number | undefined;
  let bytes = 0;
  const output: Uint8Array[] = [];
  for await (const response of client.sessions.exec(
    {
      sessionId,
      argv,
      timeoutSecs: BigInt(
        Math.max(1, Math.floor(ctx.deadlines.requestMs / 1000)),
      ),
    },
    { timeoutMs: ctx.deadlines.requestMs },
  )) {
    requireThat(!exited, "guest_exec_after_exit");
    switch (response.event.case) {
      case "started":
        requireThat(!started, "guest_exec_single_start");
        started = true;
        break;
      case "stdout":
      case "stderr":
        requireThat(started, "guest_exec_start_first");
        bytes += response.event.value.length;
        requireThat(bytes <= 65536, "guest_exec_output_bound");
        if (response.event.case === "stdout") output.push(response.event.value);
        break;
      case "exit":
        requireThat(started, "guest_exec_start_first");
        exited = true;
        status = response.event.value.exitStatus;
        break;
      default:
        throw new RunnerFailure("ASSERTION_FAILED", "guest_exec_frame");
    }
  }
  requireThat(
    started && exited && status !== undefined,
    "guest_exec_completion",
  );
  return { status, stdout: Buffer.concat(output) };
}

export async function guestReady(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
) {
  const response = await guestExec(ctx, client, sessionId, [
    "/bin/sh",
    "-c",
    'test -d /tmp/mcp-stack-fixture && test ! -L /tmp/mcp-stack-fixture && test "$(stat -c %a /tmp/mcp-stack-fixture)" = 700',
  ]);
  requireThat(response.status === 0, "guest_fixture_directory_ready");
}

export async function proofPaths(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
  suffix: string,
) {
  requireThat(/^[a-z0-9-]{1,40}$/.test(suffix), "guest_proof_suffix");
  const result = await guestExec(ctx, client, sessionId, [
    "/bin/ls",
    "-1",
    ctx.guest.stateDirectory,
  ]);
  requireThat(result.status === 0, "guest_proof_listing");
  return result.stdout
    .toString("utf8")
    .trim()
    .split("\n")
    .filter((name) => new RegExp(`^${suffix}-[0-9]+\\.json$`).test(name))
    .map((name) => `${ctx.guest.stateDirectory}/${name}`);
}

/** Literal native response contract from engram-harness-proto/tool_result.rs. */
export function expectedCounterNative(
  harness: "claude" | "codex",
  count: number,
  isError: boolean,
): Record<string, unknown> {
  const output = {
    content: [{ type: "text", text: `count=${count}` }],
    structuredContent: { count },
    isError,
  };
  const metadata = {
    outcome: isError ? "failed" : "succeeded",
    delivery: "available",
    resultUnavailable: false,
    isError,
  };
  return harness === "claude"
    ? { ...output, _meta: { "engrams/toolResult": metadata } }
    : {
        success: !isError,
        contentItems: [
          { type: "inputText", text: `count=${count}` },
          {
            type: "inputText",
            text: canonicalJson({ structuredContent: { count } }),
          },
          { type: "inputText", text: canonicalJson(metadata) },
        ],
      };
}
