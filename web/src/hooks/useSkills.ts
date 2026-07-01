/**
 * Skill catalog hooks (ADR 0055 P2).
 *
 * Skills live on the orchestrator's native Connect `MountCatalogService` (like
 * profiles), so these are connect-query hooks — no bespoke HTTP. `useSkills`
 * merges the fleet's built-in bundles (a stable, tiny set) with the uploaded
 * catalog from `ListSkills`; `useUploadSkill` calls `RegisterSkill` with the
 * file bytes (the server stamps the owner from the session + gates admin).
 */

import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listSkills,
  registerSkill,
} from "../gen/engram/app/v1/mount_catalog-MountCatalogService_connectquery";

export interface SelectableSkill {
  name: string;
  /** Display label (builtins carry a friendly label; uploads use their name). */
  label: string;
  description: string;
  builtin: boolean;
}

/** The fleet's baked built-in skill bundles (resolved by name from the fleet
 * stamp at session create). A stable set the editor renders next to uploads. */
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
  {
    // ADR 0065: the opt-in human-driven browser (mirror of the orchestrator's
    // BUILTIN_SKILLS — keep the two in sync). Selecting it mounts the `browser`
    // bundle and lights up the session's BROWSER tab over noVNC.
    name: "browser",
    label: "Browser (interactive)",
    description:
      "Full Chromium UI you drive over VNC from the session's BROWSER tab (Xvfb + x11vnc + openbox). Use an image sized for a browser (≥1 GiB).",
  },
];

/** Selectable skills: builtins ∪ the uploaded catalog. */
export function useSkills() {
  return useQuery(
    listSkills,
    {},
    {
      select: (data): SelectableSkill[] => [
        ...BUILTIN_SKILLS.map((b) => ({ ...b, builtin: true })),
        ...data.skills.map((s) => ({
          name: s.name,
          label: s.name,
          description: s.description,
          builtin: false,
        })),
      ],
      staleTime: 10_000,
    },
  );
}

/** Upload (register) a skill — admin-only server-side; the file rides the
 * `payloadTar` bytes field. Invalidates the catalog so the new skill appears. */
export function useUploadSkill() {
  const qc = useQueryClient();
  return useMutation(registerSkill, {
    onSuccess: () => {
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listSkills, input: {}, cardinality: "finite" }),
      });
    },
  });
}
