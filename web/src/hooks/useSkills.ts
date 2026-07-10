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
    // ADR 0065: the single browser capability (mirror of the orchestrator's
    // BUILTIN_SKILLS — keep the two in sync). One shared Chromium: the human
    // drives it over VNC in the BROWSER tab, the agent drives the SAME browser
    // via playwright-cli + show-your-work. (The old headless-only `playwright`
    // skill is retired into this.)
    name: "browser",
    label: "Browser",
    description:
      "One shared Chromium the human drives over VNC (BROWSER tab) and the agent drives programmatically — same browser, so the human watches the agent live. Use an image sized for a browser (≥1 GiB).",
  },
  {
    // ADR 0085: code-server as a trusted, human-only IDE surface over the
    // session workspace (mirror of the orchestrator's BUILTIN_SKILLS — keep
    // the two in sync).
    name: "ide",
    label: "IDE",
    description:
      "VS Code in the session (code-server): browse and edit the workspace, with an integrated terminal.",
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
