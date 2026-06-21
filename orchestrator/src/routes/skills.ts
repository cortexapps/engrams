/**
 * Skill catalog routes (ADR 0055 P2).
 *
 *   GET    /api/v1/skills        — list selectable skills (builtins ∪ uploaded)
 *   POST   /api/v1/skills        — upload a skill (multipart: name, description, file)
 *   DELETE /api/v1/skills/:name  — soft-delete an uploaded skill
 *
 * The catalog itself lives in the coordinator (MountCatalogService); these are
 * the browser-facing HTTP legs (file upload is multipart, not gRPC). Authz: any
 * authenticated user may read the org-shared catalog; upload/delete are
 * admin-only — the upload UX lives in the admin-only profile editor and skills
 * feed admin-curated profiles. (Per-user-owned uploads are the deferred
 * follow-up the catalog `owner` column seams.) The uploaded file is normalized
 * to a POSIX tar — a lone SKILL.md is wrapped, a .tar.gz is forwarded as-is (the
 * coordinator gunzips) — and the authenticated user.id rides as the
 * attribution `owner`.
 */

import { Hono } from "hono";
import type { Context } from "hono";
import { HTTPException } from "hono/http-exception";
import { ConnectError, Code } from "@connectrpc/connect";

import { auth } from "../auth/better-auth.ts";
import { BUILTIN_SKILLS, defaultCatalog, type MountCatalogClient, type SkillRow } from "../skills/catalog.ts";
import { tarSingleFile } from "./skill-tar.ts";

/** Mirror the coordinator skill_pack cap (markdown skills are KiB). */
const MAX_UPLOAD_BYTES = 2 * 1024 * 1024;

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

export interface SkillsDeps {
  mountCatalog?: MountCatalogClient;
  getSession?: GetSession;
}

export function makeSkillsRoute(deps?: SkillsDeps): Hono {
  const app = new Hono();
  const catalog: MountCatalogClient =
    (deps?.mountCatalog as MountCatalogClient | undefined) ?? defaultCatalog();
  const getSession: GetSession =
    deps?.getSession ??
    ((headers) => auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]));

  async function requireUser(c: Context): Promise<{ id: string; role: string }> {
    const session = await getSession(c.req.raw.headers);
    if (!session) throw new HTTPException(401, { message: "unauthenticated" });
    return { id: session.user.id, role: session.user.role ?? "user" };
  }
  async function requireAdmin(c: Context): Promise<{ id: string; role: string }> {
    const user = await requireUser(c);
    if (user.role !== "admin") throw new HTTPException(403, { message: "forbidden" });
    return user;
  }

  // List the selectable catalog — builtins first, then uploaded (newest first).
  app.get("/api/v1/skills", async (c) => {
    await requireUser(c);
    const resp = await catalog.listSkills({});
    const builtins = BUILTIN_SKILLS.map((b) => ({ ...b, builtin: true }));
    const uploaded = resp.skills.map((s) => ({
      name: s.name,
      label: s.name,
      description: s.description,
      builtin: false,
      owner: s.owner,
      sizeBytes: Number(s.sizeBytes),
      createdAt: s.createdAt,
    }));
    return c.json({ skills: [...builtins, ...uploaded] });
  });

  // Upload (admin): normalize → register. The coordinator validates the name +
  // SKILL.md + fleet-name collision and packs the squashfs.
  app.post("/api/v1/skills", async (c) => {
    const user = await requireAdmin(c);
    const body = await c.req.parseBody();
    const name = String(body.name ?? "").trim();
    const description = String(body.description ?? "").trim();
    const file = body.file;
    if (!name) return c.json({ error: "name is required" }, 400);
    if (!(file instanceof File)) return c.json({ error: "a skill file is required" }, 400);
    const bytes = new Uint8Array(await file.arrayBuffer());
    if (bytes.length > MAX_UPLOAD_BYTES) {
      return c.json({ error: "skill upload exceeds the 2 MiB cap" }, 413);
    }
    // A .tar(.gz) is forwarded verbatim; anything else is a lone SKILL.md.
    const fname = (file.name ?? "").toLowerCase();
    const isTarball =
      fname.endsWith(".tar.gz") || fname.endsWith(".tgz") || fname.endsWith(".tar");
    const payloadTar = isTarball ? bytes : tarSingleFile("SKILL.md", bytes);
    try {
      const resp = await catalog.registerSkill({ name, description, owner: user.id, payloadTar });
      return c.json({ skill: serializeSkill(resp.skill) }, 201);
    } catch (err) {
      if (err instanceof ConnectError) {
        const status =
          err.code === Code.InvalidArgument
            ? 400
            : err.code === Code.AlreadyExists || err.code === Code.FailedPrecondition
              ? 409
              : 502;
        return c.json({ error: err.message }, status);
      }
      throw err;
    }
  });

  // Soft-delete (admin) — idempotent; the coordinator's bundle GC reclaims the
  // blob after grace.
  app.delete("/api/v1/skills/:name", async (c) => {
    await requireAdmin(c);
    const name = c.req.param("name");
    const resp = await catalog.deleteSkill({ name });
    return c.json({ deleted: resp.deleted });
  });

  return app;
}

function serializeSkill(s: SkillRow | undefined) {
  if (!s) return null;
  return {
    id: s.id,
    owner: s.owner,
    name: s.name,
    description: s.description,
    sha256: s.sha256,
    sizeBytes: Number(s.sizeBytes),
    createdAt: s.createdAt,
  };
}

export default makeSkillsRoute();
