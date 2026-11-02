import { RunnerFailure } from "./context.ts";

export const HARNESS = ["claude", "codex"] as const;
export type Harness = (typeof HARNESS)[number];
export const SCENARIOS = [
  {
    id: "catalog-fidelity",
    assertions: [
      "reviewed-tool-only-exposure",
      "actual-alias-schema",
      "http-json-sse",
      "modern-legacy",
      "exact-text-structured-is-error",
    ],
  },
  {
    id: "grant-denials",
    assertions: [
      "missing-grant",
      "revoked-grant",
      "forged-grant",
      "forged-guest-completion",
      "no-effect",
    ],
  },
  {
    id: "interactive-approval",
    assertions: [
      "owner-one-use",
      "admin-one-use",
      "other-member-denied",
      "changed-context-denied",
      "replayed-decision-denied",
      "one-effect",
    ],
  },
  {
    id: "unattended-grant",
    assertions: [
      "api-key-principal",
      "default-deny",
      "explicit-admin-grant",
      "argument-constraint",
      "plan-approval-insufficient",
    ],
  },
  {
    id: "plan-child-intersection",
    assertions: [
      "plan-read-only",
      "plan-write-denied",
      "child-grant-intersection",
      "child-argument-intersection",
    ],
  },
  {
    id: "credential-network-isolation",
    assertions: [
      "no-credential-in-guest",
      "default-allow-managed-authority-denied",
      "worker-control-denied",
      "positive-network-denial",
      "guest-return",
    ],
  },
  {
    id: "disk-only-cold-rebind",
    assertions: [
      "disk-only-restore",
      "new-native-and-mcp-frozen-bytes",
      "old-target-retired-before-spawn",
      "no-old-ram",
      "no-old-execution",
    ],
  },
  {
    id: "revoke-resume",
    assertions: [
      "revocation",
      "actual-resume",
      "current-binding",
      "old-alias-denied",
      "old-call-denied",
    ],
  },
  {
    id: "held-write-restart",
    assertions: [
      "committed-held-barrier",
      "worker-restart",
      "orchestrator-restart",
      "outcome-unknown",
      "one-effect",
      "duplicate-id-restricted",
      "new-id-restricted",
    ],
  },
  {
    id: "oauth-authority",
    assertions: [
      "real-pkce",
      "exact-resource-issuer",
      "expiry-rotation",
      "same-identity-binding-stable",
      "account-change-invalidates",
      "scope-expansion-invalidates",
      "reconnect",
      "no-credential-leak",
    ],
  },
  {
    id: "kill-cancel",
    assertions: [
      "global-kill",
      "zone-kill",
      "connection-kill",
      "tool-kill",
      "cancel",
      "journal-history-preserved",
      "no-new-dispatch",
    ],
  },
  {
    id: "outbox-guest-ack",
    assertions: [
      "verified-persisted-result",
      "coordinator-outbox",
      "restart",
      "guest-proof",
      "guest-ack",
      "no-provider-replay",
      "target-gone-not-delivery",
    ],
  },
] as const;
export type ScenarioId = (typeof SCENARIOS)[number]["id"];
export const REQUIRED_CASES = HARNESS.flatMap((harness) =>
  SCENARIOS.map((scenario) => ({
    ...scenario,
    harness,
    key: `${harness}/${scenario.id}`,
  })),
);
export function selectMatrix(value: string) {
  if (value !== "all")
    throw new RunnerFailure("INVALID_CONTEXT", "matrix_all_required");
  if (
    REQUIRED_CASES.length !== 24 ||
    new Set(REQUIRED_CASES.map((v) => v.key)).size !== 24
  )
    throw new RunnerFailure("ASSERTION_FAILED", "required_case_manifest");
  return REQUIRED_CASES;
}
export type AssertionEvidence = {
  assertion: string;
  observationHashes: string[];
};
export type CaseResult = {
  key: string;
  status: "PASS" | "FAIL" | "NEEDS_CONTEXT";
  assertions: AssertionEvidence[];
  seam?: string;
};
export function verifyResults(results: readonly CaseResult[]): void {
  if (
    results.length !== REQUIRED_CASES.length ||
    new Set(results.map((v) => v.key)).size !== REQUIRED_CASES.length
  )
    throw new RunnerFailure("ASSERTION_FAILED", "missing_or_duplicate_case");
  for (const required of REQUIRED_CASES) {
    const result = results.find((v) => v.key === required.key);
    if (!result || result.status !== "PASS")
      throw new RunnerFailure("ASSERTION_FAILED", "matrix_incomplete");
    const actual = result.assertions.map((v) => v.assertion);
    if (
      actual.length !== required.assertions.length ||
      new Set(actual).size !== actual.length ||
      required.assertions.some((v) => !actual.includes(v)) ||
      result.assertions.some(
        (v) =>
          v.observationHashes.length === 0 ||
          v.observationHashes.some((h) => !/^[a-f0-9]{64}$/.test(h)),
      )
    )
      throw new RunnerFailure(
        "ASSERTION_FAILED",
        "missing_subassertion_evidence",
      );
  }
}
