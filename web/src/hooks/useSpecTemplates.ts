import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { API_BASE } from "@/lib/base";

export type SpecTemplateStageMode = "on" | "suggested" | "off";

export interface SpecTemplateLayer {
  key: string;
  title: string;
  description?: string;
}

export interface SpecTemplateSection {
  key: string;
  title: string;
  layerKey: string;
  guidance: string;
  doneCriteria: string[];
  required: boolean;
  allowNa: boolean;
}

export interface SpecTemplateDefinition {
  name: string;
  description: string;
  layers: SpecTemplateLayer[];
  sections: SpecTemplateSection[];
  stageFlags: {
    alternatives: SpecTemplateStageMode;
    talkItThrough: SpecTemplateStageMode;
    gapCheck: SpecTemplateStageMode;
  };
}

export interface SpecTemplate extends SpecTemplateDefinition {
  id: string;
  builtIn: boolean;
  modifiedFromDefault: boolean;
  createdAt: string;
  updatedAt: string;
}

export class SpecTemplateRequestError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "SpecTemplateRequestError";
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${API_BASE}${path}`, {
    credentials: "include",
    headers: { Accept: "application/json", ...init?.headers },
    ...init,
  });
  if (!response.ok) {
    const raw = await response.text();
    let message = raw;
    try {
      const body = JSON.parse(raw) as { message?: unknown };
      if (typeof body.message === "string") message = body.message;
    } catch {
      // Hono HTTPException responses use plain text by default.
    }
    if (!message) message = `Request failed: ${response.status}`;
    throw new SpecTemplateRequestError(response.status, message);
  }
  return response.json() as Promise<T>;
}

export async function listSpecTemplates(): Promise<SpecTemplate[]> {
  return (await request<{ templates: SpecTemplate[] }>("/spec-templates")).templates;
}

export async function saveSpecTemplate(
  id: string | null,
  definition: SpecTemplateDefinition,
): Promise<SpecTemplate> {
  const path = id ? `/spec-templates/${encodeURIComponent(id)}` : "/spec-templates";
  return (
    await request<{ template: SpecTemplate }>(path, {
      method: id ? "PUT" : "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(definition),
    })
  ).template;
}

export async function cloneSpecTemplate(id: string): Promise<SpecTemplate> {
  return (
    await request<{ template: SpecTemplate }>(`/spec-templates/${encodeURIComponent(id)}/clone`, {
      method: "POST",
    })
  ).template;
}

export async function restoreSpecTemplate(id: string): Promise<SpecTemplate> {
  return (
    await request<{ template: SpecTemplate }>(`/spec-templates/${encodeURIComponent(id)}/restore`, {
      method: "POST",
    })
  ).template;
}

export function useSpecTemplates() {
  return useQuery({
    queryKey: ["spec-templates"],
    queryFn: listSpecTemplates,
    staleTime: 10_000,
  });
}

export function useSaveSpecTemplate() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ id, definition }: { id: string | null; definition: SpecTemplateDefinition }) =>
      saveSpecTemplate(id, definition),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["spec-templates"] });
    },
  });
}

export function useCloneSpecTemplate() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: cloneSpecTemplate,
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["spec-templates"] });
    },
  });
}

export function useRestoreSpecTemplate() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: restoreSpecTemplate,
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["spec-templates"] });
    },
  });
}
