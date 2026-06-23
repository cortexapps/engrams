/**
 * Native MintService proxy tests (ADR 0057 C3) — admin-only; forwards the
 * coordinator's mint-kind registry verbatim.
 */

import { expect, test, describe } from "bun:test";
import { ConnectError, Code, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";

import { buildServer } from "../server.ts";
import { registerMint } from "../rpc/mint.ts";
import type { MintDeps, GetSession, MintClient } from "../rpc/mint.ts";
import { MintService, type MintKind } from "../gen/engram/app/v1/mint_pb.ts";

function makeGetSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role } } : null);
}

const KINDS = [
  {
    kind: "github_app",
    provider: "github",
    displayName: "GitHub App",
    fields: [
      { name: "app_id", label: "App ID", fieldKind: 1, required: true },
      { name: "private_key_pem", label: "Private key (PEM)", fieldKind: 2, required: true },
    ],
  },
] as unknown as MintKind[];

const fakeMint: MintClient = {
  async listMintKinds() {
    return { mintKinds: KINDS };
  },
};

async function spawn(deps: MintDeps) {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));
  const srv = buildServer(app, (router) => registerMint(router, deps));
  const url = await new Promise<string>((res) =>
    srv.listen(0, "127.0.0.1", () => res(`http://127.0.0.1:${(srv.address() as AddressInfo).port}`)),
  );
  return {
    client: createClient(
      MintService,
      createConnectTransport({ baseUrl: `${url}/rpc`, httpVersion: "1.1" }),
    ),
    close: () => new Promise<void>((res, rej) => srv.close((e) => (e ? rej(e) : res()))),
  };
}

async function expectErr(p: Promise<unknown>, code: Code) {
  try {
    await p;
    throw new Error(`expected ${Code[code]}`);
  } catch (e) {
    if (!(e instanceof ConnectError)) throw e;
    expect(e.code).toBe(code);
  }
}

describe("MintService (native proxy)", () => {
  test("anon → Unauthenticated; member → PermissionDenied", async () => {
    const anon = await spawn({ getSession: makeGetSession(null), mint: fakeMint });
    try {
      await expectErr(anon.client.listMintKinds({}), Code.Unauthenticated);
    } finally {
      await anon.close();
    }
    const mem = await spawn({ getSession: makeGetSession("m"), mint: fakeMint });
    try {
      await expectErr(mem.client.listMintKinds({}), Code.PermissionDenied);
    } finally {
      await mem.close();
    }
  });

  test("admin gets the coordinator's mint kinds", async () => {
    const s = await spawn({ getSession: makeGetSession("a", "admin"), mint: fakeMint });
    try {
      const r = await s.client.listMintKinds({});
      expect(r.mintKinds.map((k) => k.kind)).toEqual(["github_app"]);
      expect(r.mintKinds[0]!.fields.map((f) => f.name)).toEqual(["app_id", "private_key_pem"]);
    } finally {
      await s.close();
    }
  });
});
