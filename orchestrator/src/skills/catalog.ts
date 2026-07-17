/**
 * Shared skill-catalog helpers (ADR 0055 P2).
 *
 * The builtins list + the selectable-name union back ProfileService's skills
 * validation (`assertSkillsValid`), so a profile can never be saved selecting a
 * skill the editor wouldn't have offered. The web editor renders the same
 * builtins (mirrored in `web/src/hooks/useSkills.ts`) merged with the uploaded
 * catalog it reads via `MountCatalogService.ListSkills`. Keep the two builtin
 * lists in sync — a name present in one but not the other is either un-offerable
 * (missing here) or un-saveable (missing there).
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
    // ADR 0065: the single browser capability. Mounts the `browser` bundle
    // (Xvfb + full chromium + x11vnc + openbox) and the intent-aware
    // playwright-cli 0.1.17 wrapper pointed at that same Chromium over CDP. So the human
    // drives it over VNC in the BROWSER tab and the agent drives the SAME
    // browser programmatically — one selection, one shared browser. (The old
    // headless-only `playwright` skill is retired into this.)
    name: "browser",
    label: "Browser",
    description:
      "One shared Chromium: the human watches or takes over through VNC while the agent uses semantic browser control with visual fallback. Evidence is shared only on request. Use an image sized for a browser (≥1 GiB).",
  },
  {
    // ADR 0085: the in-guest IDE bundle (code-server), a human surface served
    // in the IDE tab via the session-scoped proxy (routes/ide.ts).
    name: "ide",
    label: "IDE",
    description:
      "VS Code in the session (code-server): browse and edit the workspace, with an integrated terminal.",
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
