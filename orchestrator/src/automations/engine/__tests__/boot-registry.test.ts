import { describe, expect, test } from "bun:test";

/** The registry is static and populated at boot by assertBlockRegistryComplete
 * (index.ts, next to the sweep-policy assertion). A cold process whose first
 * touch is an RPC validating a definition must already see every v1 block —
 * the e2e suite caught CreateAutomation rejecting `create_session` as unknown
 * when registration was left to the first interpreter run. This test loads
 * the registry through the boot entry point ONLY (no interpreter import) and
 * validates a definition against it, so a regression to lazy registration
 * fails here, not in the e2e lane. */
describe("block registry at boot", () => {
  test("assertBlockRegistryComplete registers every v1 block before any definition is validated", async () => {
    const { assertBlockRegistryComplete, V1_BLOCK_TYPES } = await import("../blocks/index.ts");
    const { getBlock } = await import("../blocks/registry.ts");
    assertBlockRegistryComplete();
    for (const type of V1_BLOCK_TYPES) {
      expect(getBlock(type), type).toBeDefined();
    }
    // The review system blocks ride the same registration.
    expect(getBlock("system.open_review_pass")).toBeDefined();
    expect(getBlock("system.review_policy_gate")).toBeDefined();

    const { validateDefinition } = await import("../definition.ts");
    const parsed = validateDefinition(
      {
        engine: 1,
        trigger: { kind: "manual" },
        blocks: [
          {
            id: "launch",
            type: "create_session",
            config: { profileId: "p", promptTemplate: "go" },
          },
        ],
        inputsSchema: [],
        settings: { endSessionsOnFinish: false },
      },
      { kind: "user" },
    );
    expect(parsed.blocks[0]!.type).toBe("create_session");
  });
});
