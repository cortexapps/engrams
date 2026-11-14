import { createHash } from "node:crypto";
import { create, toBinary } from "@bufbuild/protobuf";
import { z } from "zod";
import { CreatePolicySessionRequestSchema } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import type { SessionCreateInput } from "../rpc/task-create.ts";
import type { ManagedCreationIdentity } from "./managed-lifecycle.ts";

export const bytesDigest = (bytes: Uint8Array) =>
  `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
export function managedCreateInputDigest(input: {
  protocol: number;
  nativeVersion: number;
  taskId: string;
  nativeDigest: string;
  creationRequest: Uint8Array;
}) {
  const hash = createHash("sha256").update("engram-managed-create-v1\0");
  for (const value of [input.protocol, input.nativeVersion]) {
    const bytes = Buffer.alloc(4);
    bytes.writeUInt32BE(value);
    hash.update(bytes);
  }
  for (const bytes of [
    Buffer.from(input.taskId),
    Buffer.from(input.nativeDigest),
    input.creationRequest,
  ]) {
    const length = Buffer.alloc(8);
    length.writeBigUInt64BE(BigInt(bytes.length));
    hash.update(length).update(bytes);
  }
  return `sha256:${hash.digest("hex")}`;
}
const descriptor = z
  .object({
    name: z
      .string()
      .min(1)
      .max(256)
      .refine((name) => !name.startsWith("mcp_")),
    description: z.string(),
    inputSchema: z.record(z.string(), z.unknown()),
    execution: z.enum(["sync", "deferred"]),
    nativeBindings: z.record(z.string(), z.string().nullable()).optional(),
    resultFormat: z.literal("json").optional(),
  })
  .strict();
export function frozenNativeManifest(raw: string | undefined) {
  const text = raw ?? "[]",
    bytes = new TextEncoder().encode(text);
  if (bytes.length > 1024 * 1024) throw Error("MCP native manifest exceeds limit");
  const descriptors = z.array(descriptor).max(128).parse(JSON.parse(text));
  if (new Set(descriptors.map((d) => d.name)).size !== descriptors.length)
    throw Error("MCP duplicate native tool");
  return { bytes, digest: bytesDigest(bytes), text };
}
/** Exact wire bytes are transient: they can contain native harness credentials. Persist
 * only the returned identity and native digest, never the request body. */
export function managedCreationInput(
  input: SessionCreateInput,
  source: {
    taskId: string;
    actorId: string;
    inputId: string;
    parentSessionId?: string;
  },
) {
  if (!input.requestedSessionId || ![undefined, "", "default", "plan"].includes(input.harnessMode))
    throw Error("MCP creation context unavailable");
  const native = frozenNativeManifest(input.harnessEnv?.ENGRAM_TOOLS);
  const {
    mcpBindingEnvelope: _binding,
    integrationConnections: _connections,
    integrationGrants: _grants,
    ...wire
  } = input;
  const creationRequest = toBinary(
    CreatePolicySessionRequestSchema,
    create(CreatePolicySessionRequestSchema, {
      protocol: 1,
      actorId: source.actorId,
      inputId: source.inputId,
      parentSessionId: source.parentSessionId,
      create: { ...wire, harnessEnv: { ...wire.harnessEnv, ENGRAM_TOOLS: native.text } },
    }),
  );
  if (creationRequest.length > 3 * 1024 * 1024) throw Error("MCP creation input exceeds limit");
  const request = {
    protocol: 1,
    nativeVersion: 1,
    taskId: source.taskId,
    nativeDigest: native.digest,
    creationRequest,
  };
  const inputDigest = managedCreateInputDigest(request);
  const identity: ManagedCreationIdentity = {
    sessionId: input.requestedSessionId,
    taskId: source.taskId,
    inputId: source.inputId,
    actorId: source.actorId,
    ...(source.parentSessionId ? { parentSessionId: source.parentSessionId } : {}),
    writeEligibility: input.harnessMode === "plan" ? "read_only" : "write_eligible",
    inputDigest,
  };
  return {
    request: { ...request, inputDigest },
    identity,
    nativeVersion: 1 as const,
    nativeDigest: native.digest,
  };
}
