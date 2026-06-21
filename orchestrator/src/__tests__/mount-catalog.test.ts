/**
 * Native MountCatalogService tests (ADR 0055 P2) — injected deps, no network.
 *
 *   - listSkills/getSkill: any authed user; 401 anon
 *   - registerSkill/deleteSkill: admin-only (403 for members)
 *   - registerSkill stamps owner from the session, ignoring the request's owner
 */

import { expect, test, describe } from "bun:test";
import { ConnectError, Code, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";

import { buildServer } from "../server.ts";
import { registerMountCatalog } from "../rpc/mount-catalog.ts";
import type { MountCatalogDeps, GetSession } from "../rpc/mount-catalog.ts";
import type { MountCatalogClient, SkillRow } from "../skills/catalog.ts";
import { MountCatalogService } from "../gen/engram/app/v1/mount_catalog_pb.ts";

function makeGetSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role } } : null);
}

interface Recorded {
  registered: Array<{ name: string; description: string; owner: string; payloadTar: Uint8Array }>;
  deleted: string[];
}

function fakeCatalog(seed: SkillRow[] = []): { client: MountCatalogClient; rec: Recorded } {
  const rec: Recorded = { registered: [], deleted: [] };
  const client: MountCatalogClient = {
    async listSkills() {
      return { skills: seed };
    },
    async getSkill(req) {
      return { skill: seed.find((s) => s.name === req.name) };
    },
    async registerSkill(req) {
      rec.registered.push(req);
      return {
        skill: {
          id: "s1",
          owner: req.owner,
          name: req.name,
          description: req.description,
          sha256: "deadbeef",
          sizeBytes: 42n,
          createdAt: "2026-01-01T00:00:00Z",
        },
      };
    },
    async deleteSkill(req) {
      rec.deleted.push(req.name);
      return { deleted: true };
    },
  };
  return { client, rec };
}

async function spawn(deps: MountCatalogDeps) {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));
  const srv = buildServer(app, (router) => registerMountCatalog(router, deps));
  const url = await new Promise<string>((res) =>
    srv.listen(0, "127.0.0.1", () => res(`http://127.0.0.1:${(srv.address() as AddressInfo).port}`)),
  );
  return {
    client: createClient(
      MountCatalogService,
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

const uploaded: SkillRow = {
  id: "u1",
  owner: "alice",
  name: "my-linter",
  description: "lint the thing",
  sha256: "abc",
  sizeBytes: 100n,
  createdAt: "2026-01-02T00:00:00Z",
};

describe("MountCatalogService (native)", () => {
  test("anon ListSkills → Unauthenticated", async () => {
    const s = await spawn({ getSession: makeGetSession(null), mountCatalog: fakeCatalog().client });
    try {
      await expectErr(s.client.listSkills({}), Code.Unauthenticated);
    } finally {
      await s.close();
    }
  });

  test("member can list the org-shared catalog", async () => {
    const s = await spawn({
      getSession: makeGetSession("m"),
      mountCatalog: fakeCatalog([uploaded]).client,
    });
    try {
      const r = await s.client.listSkills({});
      expect(r.skills.map((x) => x.name)).toContain("my-linter");
    } finally {
      await s.close();
    }
  });

  test("member RegisterSkill → PermissionDenied", async () => {
    const s = await spawn({
      getSession: makeGetSession("m"),
      mountCatalog: fakeCatalog().client,
    });
    try {
      await expectErr(
        s.client.registerSkill({ name: "x", description: "", owner: "m", payloadTar: new Uint8Array([1]) }),
        Code.PermissionDenied,
      );
    } finally {
      await s.close();
    }
  });

  test("admin RegisterSkill stamps owner from the session, ignoring the request owner", async () => {
    const { client, rec } = fakeCatalog();
    const s = await spawn({ getSession: makeGetSession("admin1", "admin"), mountCatalog: client });
    try {
      const r = await s.client.registerSkill({
        name: "my-skill",
        description: "d",
        owner: "SPOOFED",
        payloadTar: new Uint8Array([1, 2, 3]),
      });
      expect(r.skill?.owner).toBe("admin1");
      expect(rec.registered).toHaveLength(1);
      expect(rec.registered[0].owner).toBe("admin1");
      expect(Array.from(rec.registered[0].payloadTar)).toEqual([1, 2, 3]);
    } finally {
      await s.close();
    }
  });

  test("member DeleteSkill → PermissionDenied; admin deletes", async () => {
    const { client, rec } = fakeCatalog();
    const member = await spawn({ getSession: makeGetSession("m"), mountCatalog: client });
    try {
      await expectErr(member.client.deleteSkill({ name: "my-linter" }), Code.PermissionDenied);
    } finally {
      await member.close();
    }
    const admin = await spawn({ getSession: makeGetSession("a", "admin"), mountCatalog: client });
    try {
      const r = await admin.client.deleteSkill({ name: "my-linter" });
      expect(r.deleted).toBe(true);
      expect(rec.deleted).toEqual(["my-linter"]);
    } finally {
      await admin.close();
    }
  });
});
