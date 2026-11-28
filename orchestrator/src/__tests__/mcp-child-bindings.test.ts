import { expect, test } from "bun:test";
import {
  compileChildSessionBindings,
  assertChildMcpCreationBlocked,
} from "../mcp/session-child.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import { bindingAuthority, bindingGrant } from "./mcp-binding-fixture.ts";
import { checkDb, getDb } from "../db/client.ts";
import { task, taskSession } from "../db/schema.ts";
import { eq } from "drizzle-orm";

test("child compiler reads actual spawning session and defaults to no MCP request", async () => {
  const parentAuthority = bindingAuthority();
  parentAuthority.profileGrants[0]!.mcp = {
    ...bindingGrant.mcp!,
    unrestricted: false,
    argumentConstraintDocuments: [
      {
        $schema: "http://json-schema.org/draft-07/schema#",
        definitions: { p: { const: "parent" } },
        properties: { project: { $ref: "#/definitions/p" } },
      },
    ],
  };
  const parent = compileSessionBindings({
    launchPolicy: {
      profileId: parentAuthority.profileId,
      integrationGrants: parentAuthority.profileGrants,
    },
    liveAuthority: parentAuthority,
    bindingEpoch: 0,
  });
  const child = bindingAuthority();
  child.profileId = "child-profile";
  let selected = "";
  const args = {
    spawningSessionId: "actual-parent-session",
    launchPolicy: { profileId: child.profileId, integrationGrants: child.profileGrants },
    liveAuthority: child,
    bindingEpoch: 0,
    readParentCurrent: async (id: string) => {
      selected = id;
      return parent;
    },
  };
  expect((await compileChildSessionBindings(args)).immutableBindings).toEqual([]);
  const explicit = await compileChildSessionBindings({ ...args, requestedGrants: [bindingGrant] });
  expect(selected).toBe("actual-parent-session");
  expect(explicit.immutableBindings[0]!.tools[0]!.grant.argumentConstraintDocuments).toEqual(
    parentAuthority.profileGrants[0]!.mcp!.argumentConstraintDocuments,
  );
  await expect(
    compileChildSessionBindings({
      ...args,
      readParentCurrent: async () => {
        throw new Error("parent revoked");
      },
    }),
  ).rejects.toThrow("parent revoked");
});
const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
test.skipIf(!reachable)(
  "existing child path checks actual session authority even when launch policy is empty",
  async () => {
    const db = getDb(),
      tid = crypto.randomUUID(),
      sid = crypto.randomUUID();
    try {
      await db.insert(task).values({ id: tid, type: "chat" });
      await db
        .insert(taskSession)
        .values({ taskId: tid, sessionId: sid, integrationGrants: [bindingGrant] });
      await expect(assertChildMcpCreationBlocked(tid, sid, [], db)).rejects.toThrow(
        "activation is not configured",
      );
      await db
        .update(taskSession)
        .set({ integrationGrants: [] })
        .where(eq(taskSession.sessionId, sid));
      await assertChildMcpCreationBlocked(tid, sid, [], db);
      await expect(assertChildMcpCreationBlocked(tid, "other-session", [], db)).rejects.toThrow();
    } finally {
      await db.delete(task).where(eq(task.id, tid));
    }
  },
);
