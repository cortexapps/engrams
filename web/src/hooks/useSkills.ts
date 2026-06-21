/**
 * Skill catalog hooks (ADR 0055 P2).
 *
 * The orchestrator exposes the org-shared catalog over plain HTTP (file upload
 * is multipart, not Connect): GET /api/v1/skills lists builtins ∪ uploaded;
 * POST uploads (admin-only server-side). The profile editor renders the list as
 * toggles and offers the upload control.
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { API_BASE } from "../lib/base";

export interface CatalogSkill {
  name: string;
  /** Display label (builtins carry a friendly label; uploads use their name). */
  label: string;
  description: string;
  builtin: boolean;
  owner?: string;
  sizeBytes?: number;
  createdAt?: string;
}

/** GET /api/v1/skills — selectable skills (builtins ∪ uploaded). */
export function useSkills(enabled = true) {
  return useQuery({
    queryKey: ["skills"],
    queryFn: async (): Promise<CatalogSkill[]> => {
      const res = await fetch(`${API_BASE}/skills`, { credentials: "include" });
      if (!res.ok) throw new Error(`list skills → ${res.status}`);
      const body = (await res.json()) as { skills: CatalogSkill[] };
      return body.skills;
    },
    enabled,
    staleTime: 10_000,
  });
}

/** POST /api/v1/skills — upload a skill (multipart). Admin-only server-side. */
export function useUploadSkill() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: { name: string; description: string; file: File }) => {
      const fd = new FormData();
      fd.append("name", input.name);
      fd.append("description", input.description);
      fd.append("file", input.file);
      const res = await fetch(`${API_BASE}/skills`, {
        method: "POST",
        body: fd,
        credentials: "include",
      });
      if (!res.ok) {
        const err = (await res.json().catch(() => ({}))) as { error?: string };
        throw new Error(err.error ?? `upload → ${res.status}`);
      }
      return res.json();
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["skills"] }),
  });
}
