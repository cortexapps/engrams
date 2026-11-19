/** TagService (Settings → Tags): admin-only on every verb, the tag rules and
 * their exact messages, and the store outcomes mapped to Connect codes. */

import { describe, expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";

import { TagService } from "../gen/engram/app/v1/tag_pb.ts";
import type { GetSession } from "../rpc/require.ts";
import { registerTags } from "../rpc/tags.ts";
import { makeMemoryTagStore } from "./support/memory-tag-store.ts";

const ADMIN: GetSession = async () => ({ user: { id: "u-admin", role: "admin", email: "a@test.invalid" } });
const MEMBER: GetSession = async () => ({ user: { id: "u-member", role: "user", email: "m@test.invalid" } });
const NOBODY: GetSession = async () => null;

function spawn(getSession: GetSession = ADMIN, store = makeMemoryTagStore()) {
  const transport = createRouterTransport((router) => registerTags(router, { getSession, store }));
  return { client: createClient(TagService, transport), store };
}

async function expectError(promise: Promise<unknown>, code: Code, message?: string): Promise<void> {
  try {
    await promise;
    throw new Error(`expected ConnectError(${Code[code]})`);
  } catch (error) {
    if (!(error instanceof ConnectError)) throw error;
    expect(error.code).toBe(code);
    if (message !== undefined) expect(error.rawMessage).toBe(message);
  }
}

const NAME_RULE = "a tag name must be 1 to 32 characters, with no control characters";
const DESCRIPTION_RULE = "a tag description must be at most 100 characters, on one line";
const COLOR_RULE = "a tag color must be a hex color like #1f6feb";

describe("TagService", () => {
  test("member and anonymous callers are refused on every verb", async () => {
    for (const [getSession, code] of [
      [MEMBER, Code.PermissionDenied],
      [NOBODY, Code.Unauthenticated],
    ] as const) {
      const { client } = spawn(getSession);
      await expectError(client.listTags({}), code);
      await expectError(client.createTag({ name: "Ops", color: "#1f6feb", description: "" }), code);
      await expectError(client.updateTag({ id: "tag-1", color: "#000000" }), code);
      await expectError(client.deleteTag({ id: "tag-1" }), code);
    }
  });

  test("create normalizes every field and returns the tag", async () => {
    const { client } = spawn();
    const { tag } = await client.createTag({ name: "  Team   Platform ", color: "#1F6FEB", description: " Owned " });
    expect(tag).toMatchObject({ name: "Team Platform", color: "#1f6feb", description: "Owned" });
    expect(tag?.id).not.toBe("");
  });

  test("each invalid field answers InvalidArgument with its rule", async () => {
    const { client, store } = spawn();
    await expectError(client.createTag({ name: " ", color: "#1f6feb", description: "" }), Code.InvalidArgument, NAME_RULE);
    await expectError(
      client.createTag({ name: "Ops", color: "#1f6feb", description: "a\nb" }),
      Code.InvalidArgument,
      DESCRIPTION_RULE,
    );
    await expectError(client.createTag({ name: "Ops", color: "blue", description: "" }), Code.InvalidArgument, COLOR_RULE);
    expect(store.tags.size).toBe(0);

    const { tag } = await client.createTag({ name: "Ops", color: "#1f6feb", description: "" });
    await expectError(client.updateTag({ id: tag!.id, name: "x".repeat(33) }), Code.InvalidArgument, NAME_RULE);
    await expectError(client.updateTag({ id: tag!.id, color: "#12345" }), Code.InvalidArgument, COLOR_RULE);
    await expectError(
      client.updateTag({ id: tag!.id, description: "d".repeat(101) }),
      Code.InvalidArgument,
      DESCRIPTION_RULE,
    );
  });

  test("a name taken without regard to case answers AlreadyExists", async () => {
    const { client } = spawn();
    await client.createTag({ name: "Ops", color: "#1f6feb", description: "" });
    const { tag: billing } = await client.createTag({ name: "Billing", color: "#1f6feb", description: "" });
    const taken = "a tag with this name already exists";
    await expectError(client.createTag({ name: "ops", color: "#1f6feb", description: "" }), Code.AlreadyExists, taken);
    await expectError(client.updateTag({ id: billing!.id, name: "OPS" }), Code.AlreadyExists, taken);
  });

  test("update changes only the fields it is given; an unknown id is NotFound", async () => {
    const { client } = spawn();
    const { tag } = await client.createTag({ name: "Ops", color: "#1f6feb", description: "On call" });
    const { tag: updated } = await client.updateTag({ id: tag!.id, color: "#8250DF" });
    expect(updated).toMatchObject({ id: tag!.id, name: "Ops", color: "#8250df", description: "On call" });
    await expectError(client.updateTag({ id: "nope", color: "#000000" }), Code.NotFound, "tag not found");
  });

  test("list carries each tag's automation count in name order", async () => {
    const store = makeMemoryTagStore();
    const { client } = spawn(ADMIN, store);
    const { tag: ops } = await client.createTag({ name: "ops", color: "#1f6feb", description: "" });
    await client.createTag({ name: "Billing", color: "#8250df", description: "Money" });
    await store.addToAutomation("a1", ops!.id, 10);
    await store.addToAutomation("a2", ops!.id, 10);

    const { tags } = await client.listTags({});
    expect(tags.map((s) => [s.tag?.name, s.tag?.color, s.tag?.description, s.automationCount])).toEqual([
      ["Billing", "#8250df", "Money", 0],
      ["ops", "#1f6feb", "", 2],
    ]);
  });

  test("delete removes the tag; deleting it again is NotFound", async () => {
    const { client } = spawn();
    const { tag } = await client.createTag({ name: "Ops", color: "#1f6feb", description: "" });
    await client.deleteTag({ id: tag!.id });
    expect((await client.listTags({})).tags).toEqual([]);
    await expectError(client.deleteTag({ id: tag!.id }), Code.NotFound, "tag not found");
  });
});
