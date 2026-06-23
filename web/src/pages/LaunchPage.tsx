/**
 * LaunchPage (redesign §H) — the developer's full-page new-session picker. Choose
 * a profile (each card shows its providers + power count), hand the agent a task,
 * and see the "this session will be able to" receipt — the same policy the editor
 * shows — before launching. Calls the same CreateTask path as the quick dialog.
 */

import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { useMutation, createConnectQueryKey } from "@connectrpc/connect-query";
import {
  CircleCheckIcon,
  EyeIcon,
  LockIcon,
  PencilIcon,
  ShieldCheckIcon,
  TerminalIcon,
} from "lucide-react";

import { createTask, listTasks } from "../gen/engram/app/v1/task-TaskService_connectquery";
import { useProfiles } from "../hooks/useProfiles";
import { useIntegrationCatalog } from "../hooks/useIntegrations";
import { useEnabledImages } from "../hooks/useEnabledImages";
import { catalogToViews } from "../components/integrations/useConnectorViews";
import { ProviderTile } from "../components/integrations/ProviderTile";
import { ProfileIcon } from "../components/profiles/ProfileIcon";
import { humanizeAction } from "../lib/connectorModel";
import { derivePolicy } from "../lib/profilePolicy";
import { PageHeading } from "../components/page-heading";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Text } from "@/components/ui/text";
import { cn } from "@/lib/utils";

export function LaunchPage() {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const { data: profilesData, isPending } = useProfiles(false);
  const { data: catalog } = useIntegrationCatalog();
  const { data: images } = useEnabledImages(true);
  const createTaskMutation = useMutation(createTask);

  const profiles = profilesData?.profiles ?? [];
  const views = catalogToViews(catalog?.providers ?? []);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [error, setError] = useState<string | null>(null);

  const selected = profiles.find((p) => p.id === selectedId) ?? profiles[0] ?? null;
  const policy = selected
    ? derivePolicy(
        {
          capabilities: selected.capabilities ?? [],
          network: {
            default: selected.network?.default === "allow" ? "allow" : "deny",
            allowHosts: selected.network?.allowHosts ?? [],
            allowHostPatterns: selected.network?.allowHostPatterns ?? [],
          },
          secrets: [],
        },
        views,
      )
    : null;
  const imageName = selected
    ? images?.find((i) => i.id === selected.imageId)?.image_uri
    : undefined;

  const launch = async () => {
    if (!selected || !prompt.trim()) return;
    setError(null);
    try {
      const res = await createTaskMutation.mutateAsync({
        type: "chat",
        profileId: selected.id,
        prompt: prompt.trim(),
      });
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listTasks, cardinality: "finite" }),
      });
      const sessionId = res.task?.sessions[0]?.sessionId;
      if (sessionId) navigate({ to: "/sessions/$id", params: { id: sessionId } });
      else setError("Task created but no session id returned.");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="mx-auto max-w-3xl">
      <PageHeading
        title="Start a session"
        eyebrow="Sessions · New"
        description="Pick a profile — it decides the image, the powers, and what the session can reach. Hand the agent a task and launch."
      />

      {isPending ? (
        <p className="mt-6 text-sm text-muted-foreground">Loading profiles…</p>
      ) : profiles.length === 0 ? (
        <p className="mt-6 text-sm text-muted-foreground">
          No profiles configured — contact an admin to set one up.
        </p>
      ) : (
        <>
          <div className="mt-6">
            <Text variant="label">Choose a profile</Text>
            <div
              className="mt-2.5 grid gap-3"
              style={{ gridTemplateColumns: "repeat(auto-fill, minmax(240px, 1fr))" }}
            >
              {profiles.map((p) => {
                const on = selected?.id === p.id;
                const pol = derivePolicy(
                  {
                    capabilities: p.capabilities ?? [],
                    network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
                    secrets: [],
                  },
                  views,
                );
                return (
                  <button
                    key={p.id}
                    type="button"
                    onClick={() => setSelectedId(p.id)}
                    aria-pressed={on}
                    className={cn(
                      "flex flex-col gap-2.5 rounded-lg border p-3.5 text-left transition-colors",
                      on
                        ? "border-primary bg-primary/[0.08] ring-1 ring-primary"
                        : "border-border bg-card shadow-xs",
                    )}
                  >
                    <div className="flex items-center gap-2.5">
                      <span className="flex size-8 shrink-0 items-center justify-center rounded-md bg-secondary">
                        <ProfileIcon name={p.icon} className="size-4" />
                      </span>
                      <span className="font-display text-[0.92rem] font-semibold">{p.name}</span>
                      {on && <CircleCheckIcon className="ml-auto size-4 text-primary" />}
                    </div>
                    <p className="min-h-8 text-[0.78rem] leading-snug text-muted-foreground">
                      {p.description}
                    </p>
                    <div className="flex items-center gap-2">
                      {pol.providers.length > 0 ? (
                        pol.providers.map((pr) => (
                          <ProviderTile
                            key={pr.view.provider}
                            {...pr.view.icon}
                            name={pr.view.name}
                            size={16}
                          />
                        ))
                      ) : (
                        <span className="text-[0.72rem] text-muted-foreground">sandboxed</span>
                      )}
                      <span className="ml-auto font-mono text-[0.7rem] text-muted-foreground">
                        {pol.capCount} powers
                      </span>
                    </div>
                  </button>
                );
              })}
            </div>
          </div>

          {selected && policy && (
            <div className="mt-6 grid items-start gap-5 lg:grid-cols-[minmax(0,1fr)_300px]">
              <div className="flex flex-col gap-2.5">
                <Text variant="label">Task</Text>
                <Textarea
                  rows={5}
                  value={prompt}
                  onChange={(e) => setPrompt(e.target.value)}
                  placeholder="Fix the flaky billing-gateway integration test and open a PR."
                  className="text-sm leading-relaxed"
                  aria-label="Task"
                />
                <div className="flex items-center gap-3">
                  <Button
                    disabled={!prompt.trim() || createTaskMutation.isPending}
                    onClick={launch}
                    data-testid="launch-session"
                  >
                    <TerminalIcon className="size-4" />
                    {createTaskMutation.isPending ? "Launching…" : "Launch session"}
                  </Button>
                  {imageName && (
                    <span className="font-mono text-[0.74rem] text-muted-foreground">
                      boots {imageName}
                    </span>
                  )}
                </div>
                {error && <p className="text-sm text-destructive">{error}</p>}
              </div>

              <aside className="overflow-hidden rounded-lg border bg-card shadow-xs">
                <div className="flex items-center gap-2 border-b bg-secondary px-3.5 py-2.5">
                  <ShieldCheckIcon className="size-3.5 text-instrument-nominal" />
                  <Text variant="label" className="text-[0.6rem]">
                    This session will be able to
                  </Text>
                </div>
                <div className="flex flex-col gap-3 px-3.5 py-3">
                  {policy.capCount === 0 ? (
                    <span className="inline-flex items-center gap-1.5 text-[0.8rem] text-instrument-nominal">
                      <LockIcon className="size-3.5" />
                      Run fully sandboxed — no outside reach.
                    </span>
                  ) : (
                    policy.providers.map((pr) => (
                      <div key={pr.view.provider} className="flex flex-col gap-1">
                        <div className="flex items-center gap-2">
                          <ProviderTile {...pr.view.icon} name={pr.view.name} size={16} />
                          <span className="text-[0.78rem] font-semibold">{pr.view.name}</span>
                        </div>
                        {pr.caps.map((c) => (
                          <div key={c.action} className="flex items-center gap-2 pl-6">
                            {c.access === "write" ? (
                              <PencilIcon className="size-3 text-instrument-caution" />
                            ) : (
                              <EyeIcon className="size-3 text-muted-foreground" />
                            )}
                            <span className="text-[0.76rem] text-muted-foreground">
                              {humanizeAction(c.action)}
                            </span>
                          </div>
                        ))}
                      </div>
                    ))
                  )}
                  {policy.reachable.length > 0 && (
                    <div className="flex flex-col gap-1.5 border-t pt-2.5">
                      <span className="text-[0.7rem] text-muted-foreground">Reaches</span>
                      <div className="flex flex-wrap gap-1.5">
                        {policy.reachable.map((h) => (
                          <span
                            key={h}
                            className="rounded-full border bg-secondary px-2 py-px font-mono text-[0.68rem]"
                          >
                            {h}
                          </span>
                        ))}
                      </div>
                    </div>
                  )}
                </div>
              </aside>
            </div>
          )}
        </>
      )}
    </div>
  );
}
