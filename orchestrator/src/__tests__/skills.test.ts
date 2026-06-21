/**
 * Skill catalog route tests (ADR 0055 P2) — injected deps, no DB / network.
 *
 *   - GET  list: builtins ∪ uploaded; 401 unauth
 *   - POST upload: admin-only (403 for member); lone-file wrapped to a tar,
 *     .tar.gz forwarded verbatim; owner = the authed user
 *   - DELETE: admin-only; idempotent
 */

import { expect, test, describe } from "bun:test";

import { makeSkillsRoute } from "../routes/skills.ts";
import type { GetSession, SkillsDeps } from "../routes/skills.ts";
import type { MountCatalogClient, SkillRow } from "../skills/catalog.ts";

function getSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
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

function app(deps: SkillsDeps) {
  return makeSkillsRoute(deps);
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

describe("GET /api/v1/skills", () => {
  test("lists builtins ∪ uploaded for an authed user", async () => {
    const { client } = fakeCatalog([uploaded]);
    const res = await app({ mountCatalog: client, getSession: getSession("u", "user") }).request(
      "/api/v1/skills",
    );
    expect(res.status).toBe(200);
    const body = (await res.json()) as { skills: { name: string; builtin: boolean }[] };
    const names = body.skills.map((s) => s.name);
    expect(names).toContain("skills");
    expect(names).toContain("playwright");
    expect(names).toContain("my-linter");
    expect(body.skills.find((s) => s.name === "my-linter")?.builtin).toBe(false);
    expect(body.skills.find((s) => s.name === "skills")?.builtin).toBe(true);
  });

  test("401 without a session", async () => {
    const { client } = fakeCatalog();
    const res = await app({ mountCatalog: client, getSession: getSession(null) }).request(
      "/api/v1/skills",
    );
    expect(res.status).toBe(401);
  });
});

describe("POST /api/v1/skills", () => {
  async function upload(deps: SkillsDeps, file: File, fields: Record<string, string>) {
    const fd = new FormData();
    for (const [k, v] of Object.entries(fields)) fd.append(k, v);
    fd.append("file", file);
    return app(deps).request("/api/v1/skills", { method: "POST", body: fd });
  }

  test("admin uploads a lone SKILL.md → wrapped into a tar, owner is the user", async () => {
    const { client, rec } = fakeCatalog();
    const file = new File([new TextEncoder().encode("# Hi\n")], "SKILL.md", {
      type: "text/markdown",
    });
    const res = await upload(
      { mountCatalog: client, getSession: getSession("admin1", "admin") },
      file,
      { name: "my-skill", description: "d" },
    );
    expect(res.status).toBe(201);
    expect(rec.registered).toHaveLength(1);
    expect(rec.registered[0].name).toBe("my-skill");
    expect(rec.registered[0].owner).toBe("admin1");
    // Wrapped into a ustar — "ustar" magic at offset 257.
    const magic = new TextDecoder().decode(rec.registered[0].payloadTar.subarray(257, 262));
    expect(magic).toBe("ustar");
  });

  test("a .tar.gz is forwarded verbatim", async () => {
    const { client, rec } = fakeCatalog();
    const gz = new Uint8Array([0x1f, 0x8b, 0x08, 0x00, 1, 2, 3]); // gzip magic + junk
    const file = new File([gz], "skill.tar.gz", { type: "application/gzip" });
    const res = await upload(
      { mountCatalog: client, getSession: getSession("admin1", "admin") },
      file,
      { name: "tgz-skill", description: "" },
    );
    expect(res.status).toBe(201);
    expect(Array.from(rec.registered[0].payloadTar)).toEqual(Array.from(gz));
  });

  test("a .zip is forwarded verbatim (coordinator sniffs + unpacks)", async () => {
    const { client, rec } = fakeCatalog();
    const zip = new Uint8Array([0x50, 0x4b, 0x03, 0x04, 9, 9, 9]); // PK\x03\x04 + junk
    const file = new File([zip], "skill.zip", { type: "application/zip" });
    const res = await upload(
      { mountCatalog: client, getSession: getSession("admin1", "admin") },
      file,
      { name: "zip-skill", description: "" },
    );
    expect(res.status).toBe(201);
    expect(Array.from(rec.registered[0].payloadTar)).toEqual(Array.from(zip));
  });

  test("rejects an unrecognized file type instead of mangling it into SKILL.md", async () => {
    const { client, rec } = fakeCatalog();
    const file = new File([new Uint8Array([1, 2, 3])], "evil.exe");
    const res = await upload(
      { mountCatalog: client, getSession: getSession("admin1", "admin") },
      file,
      { name: "x", description: "" },
    );
    expect(res.status).toBe(400);
    expect(rec.registered).toHaveLength(0);
  });

  test("403 for a non-admin", async () => {
    const { client } = fakeCatalog();
    const file = new File([new Uint8Array([1])], "SKILL.md");
    const res = await upload(
      { mountCatalog: client, getSession: getSession("u", "user") },
      file,
      { name: "x", description: "" },
    );
    expect(res.status).toBe(403);
  });

  test("400 without a name", async () => {
    const { client } = fakeCatalog();
    const file = new File([new Uint8Array([1])], "SKILL.md");
    const res = await upload(
      { mountCatalog: client, getSession: getSession("admin1", "admin") },
      file,
      { description: "" },
    );
    expect(res.status).toBe(400);
  });
});

describe("DELETE /api/v1/skills/:name", () => {
  test("admin soft-deletes", async () => {
    const { client, rec } = fakeCatalog();
    const res = await app({ mountCatalog: client, getSession: getSession("admin1", "admin") }).request(
      "/api/v1/skills/my-linter",
      { method: "DELETE" },
    );
    expect(res.status).toBe(200);
    expect(rec.deleted).toEqual(["my-linter"]);
  });

  test("403 for a non-admin", async () => {
    const { client } = fakeCatalog();
    const res = await app({ mountCatalog: client, getSession: getSession("u", "user") }).request(
      "/api/v1/skills/my-linter",
      { method: "DELETE" },
    );
    expect(res.status).toBe(403);
  });
});
