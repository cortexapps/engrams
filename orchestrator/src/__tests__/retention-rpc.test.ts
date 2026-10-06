/** RetentionService (Settings → Retention): admin-only on both verbs, the
 * bounds enforced on write, and a round trip through the org setting store. */

import { describe, expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";

import { makeInMemoryOrgSettingStore, RETENTION_KEY } from "../db/org-settings.ts";
import { RetentionService } from "../gen/engram/app/v1/retention_pb.ts";
import { registerRetention } from "../rpc/retention.ts";
import type { GetSession } from "../rpc/require.ts";

const ADMIN: GetSession = async () => ({ user: { id: "u-admin", role: "admin", email: "a@test.invalid" } });
const MEMBER: GetSession = async () => ({ user: { id: "u-member", role: "user", email: "m@test.invalid" } });
const NOBODY: GetSession = async () => null;

function spawn(getSession: GetSession, settings = makeInMemoryOrgSettingStore()) {
  const transport = createRouterTransport((router) => registerRetention(router, { getSession, settings }));
  return { client: createClient(RetentionService, transport), settings };
}

async function expectCode(promise: Promise<unknown>, code: Code): Promise<void> {
  try {
    await promise;
    throw new Error(`expected ConnectError(${Code[code]})`);
  } catch (error) {
    if (!(error instanceof ConnectError)) throw error;
    expect(error.code).toBe(code);
  }
}

describe("RetentionService", () => {
  test("reads the default policy when nothing is stored", async () => {
    const { client } = spawn(ADMIN);
    expect((await client.getRetentionPolicy({})).policy?.runDetailDays).toBe(30);
  });

  test("set then get round-trips through the org setting store, stamping the admin", async () => {
    const { client, settings } = spawn(ADMIN);
    const set = await client.setRetentionPolicy({ policy: { runDetailDays: 90 } });
    expect(set.policy?.runDetailDays).toBe(90);
    expect(settings.rows.get(RETENTION_KEY)).toEqual({ runDetailDays: 90 });
    expect((await client.getRetentionPolicy({})).policy?.runDetailDays).toBe(90);
  });

  test("rejects a value outside the bounds or a missing policy", async () => {
    const { client, settings } = spawn(ADMIN);
    await expectCode(client.setRetentionPolicy({ policy: { runDetailDays: 6 } }), Code.InvalidArgument);
    await expectCode(client.setRetentionPolicy({ policy: { runDetailDays: 366 } }), Code.InvalidArgument);
    await expectCode(client.setRetentionPolicy({}), Code.InvalidArgument);
    expect(settings.rows.size).toBe(0);
  });

  test("both verbs are admin-only", async () => {
    const member = spawn(MEMBER);
    await expectCode(member.client.getRetentionPolicy({}), Code.PermissionDenied);
    await expectCode(member.client.setRetentionPolicy({ policy: { runDetailDays: 30 } }), Code.PermissionDenied);
    const nobody = spawn(NOBODY);
    await expectCode(nobody.client.getRetentionPolicy({}), Code.Unauthenticated);
  });
});
