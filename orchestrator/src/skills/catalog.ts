/**
 * Shared skill-catalog helpers (ADR 0055 P2).
 *
 * The builtins list + the selectable-name union are shared by the
 * `/api/v1/skills` route and ProfileService's skills validation, so a profile
 * can never be saved selecting a skill the editor wouldn't have offered. Kept out
 * of the route module so importing the helpers doesn't instantiate a Hono app.
 */

import { mountCatalog as defaultMountCatalog } from "../control-plane/client.ts";

/**
 * The fleet's baked built-in skill bundles (the coordinator resolves these names
 * from the fleet stamp). Their names are the single source the profile editor +
 * validation share; uploaded skills come from the coordinator catalog.
 */
export const BUILTIN_SKILLS: { name: string; label: string; description: string }[] = [
  {
    name: "skills",
    label: "Built-in skills",
    description: "share-file and the git credential wiring.",
  },
  {
    name: "playwright",
    label: "Browser (Playwright)",
    description:
      "chromium-headless-shell + the playwright-cli powering the show-your-work skill. Use an image sized for a browser (≥1 GiB).",
  },
];

export const BUILTIN_NAMES = new Set(BUILTIN_SKILLS.map((s) => s.name));

export interface SkillRow {
  id: string;
  owner: string;
  name: string;
  description: string;
  sha256: string;
  sizeBytes: bigint;
  createdAt: string;
}

/** Subset of MountCatalogService the orchestrator handler + validation use. */
export interface MountCatalogClient {
  listSkills(req: Record<string, never>): Promise<{ skills: SkillRow[] }>;
  getSkill(req: { name: string }): Promise<{ skill?: SkillRow }>;
  registerSkill(req: {
    name: string;
    description: string;
    owner: string;
    payloadTar: Uint8Array;
    // ADR 0058 uploaded-binary arm: PATH binaries the bundle declares.
    bins: string[];
  }): Promise<{ skill?: SkillRow }>;
  deleteSkill(req: { name: string }): Promise<{ deleted: boolean }>;
}

/** The real singleton client (typed down to the subset used here). */
export function defaultCatalog(): MountCatalogClient {
  return defaultMountCatalog as unknown as MountCatalogClient;
}

/**
 * The union of selectable skill names (builtins ∪ live catalog). Used by the
 * list endpoint and by profile-save validation.
 */
export async function selectableSkillNames(catalog: MountCatalogClient): Promise<Set<string>> {
  const resp = await catalog.listSkills({});
  const names = new Set(BUILTIN_NAMES);
  for (const s of resp.skills) names.add(s.name);
  return names;
}
