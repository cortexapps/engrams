/** Slack smart routing: where no channel override decides, a decision model
 * (Jev, through OpenRouter) picks the profile from the thread and asks in
 * the thread when it is not sure. Offered only while OpenRouter has a key;
 * without one the section is absent, unless smart routing was already on —
 * then it says it is paused (threads use the default profile) and offers
 * the switch back. */

import { Link } from "@tanstack/react-router";
import { AlertTriangle } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { useSetInputValue } from "@/hooks/useAutomations";
import { useConnectedRouters } from "@/hooks/useConnectedRouters";
import { errorMessage } from "@/lib/errors";

export type SlackRouting = "default" | "smart";

export interface SmartRoutingProps {
  automationId: string | undefined;
  inputs: Record<string, unknown>;
  profiles: ReadonlyArray<{ id: string; name: string }>;
}

export function routingOf(inputs: Record<string, unknown>): SlackRouting {
  return inputs["routing"] === "smart" ? "smart" : "default";
}

export function SmartRouting({ automationId, inputs, profiles }: SmartRoutingProps) {
  const connected = useConnectedRouters().has("openrouter");
  const setInput = useSetInputValue();
  const routing = routingOf(inputs);
  const threshold =
    typeof inputs["smart_min_confidence"] === "number" ? inputs["smart_min_confidence"] : 0.8;
  const chosen = Array.isArray(inputs["smart_profiles"])
    ? inputs["smart_profiles"].filter((id): id is string => typeof id === "string")
    : [];
  // Empty = every active profile.
  const candidates = chosen.length === 0 ? profiles.map((p) => p.id) : chosen;

  const save = async (inputKey: string, value: unknown, message: string) => {
    if (!automationId) return;
    try {
      await setInput.mutateAsync({ automationId, inputKey, valueJson: JSON.stringify(value) });
      toast.success(message);
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  if (!connected && routing === "default") return null;

  if (!connected) {
    return (
      <section className="flex flex-col gap-3" aria-label="Routing">
        <h2 className="text-base font-semibold">Routing</h2>
        <div
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-instrument-caution/40 bg-instrument-caution/10 p-3 text-sm"
        >
          <AlertTriangle className="mt-0.5 size-4 shrink-0 text-instrument-caution" aria-hidden />
          <span className="flex-1">
            Smart routing is paused: there is no OpenRouter key, so threads use the default profile.{" "}
            <Link to="/settings/model-routers" className="underline underline-offset-2">
              Connect OpenRouter
            </Link>{" "}
            to turn it back on.
          </span>
          <Button
            size="sm"
            variant="outline"
            disabled={!automationId || setInput.isPending}
            onClick={() => save("routing", "default", "Routing set to the default profile")}
          >
            Use default routing
          </Button>
        </div>
      </section>
    );
  }

  return (
    <section className="flex flex-col gap-3" aria-label="Routing">
      <h2 className="text-base font-semibold">Routing</h2>
      <p className="text-sm text-muted-foreground">
        How a new thread picks its profile. A channel override always wins. Smart routing reads the
        thread and picks the profile; when it is not sure, it asks in the thread with a button per
        profile. Without an answer, it takes its best pick.
      </p>
      <Select
        value={routing}
        onValueChange={(next) =>
          save("routing", next, next === "smart" ? "Smart routing on" : "Default routing on")
        }
        disabled={!automationId || setInput.isPending}
      >
        <SelectTrigger className="w-72" aria-label="Routing">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="default">Default profile</SelectItem>
          <SelectItem value="smart">Smart routing</SelectItem>
        </SelectContent>
      </Select>

      {routing === "smart" && (
        <div className="flex flex-col gap-4 rounded-lg border p-4">
          <label className="flex flex-wrap items-center gap-3 text-sm">
            <span className="w-56">Pick without asking at confidence</span>
            <Input
              key={threshold}
              type="number"
              min={0}
              max={1}
              step={0.05}
              defaultValue={threshold}
              className="w-24"
              aria-label="Smart routing confidence"
              disabled={!automationId || setInput.isPending}
              onBlur={(e) => {
                const next = Number(e.target.value);
                if (Number.isFinite(next) && next >= 0 && next <= 1 && next !== threshold) {
                  void save("smart_min_confidence", next, "Confidence saved");
                }
              }}
            />
            <span className="text-xs text-muted-foreground">
              Lower asks less often and routes wrong more often.
            </span>
          </label>

          <div className="flex flex-col gap-2">
            <span className="text-sm">Profiles smart routing can pick</span>
            <ul className="flex flex-col gap-1.5" aria-label="Profiles smart routing can pick">
              {profiles.map((p) => {
                const on = candidates.includes(p.id);
                return (
                  <li key={p.id} className="flex items-center gap-2 text-sm">
                    <Switch
                      aria-label={`Smart routing can pick ${p.name}`}
                      checked={on}
                      disabled={
                        !automationId || setInput.isPending || (on && candidates.length === 1)
                      }
                      onCheckedChange={(next) => {
                        const list = next
                          ? [...candidates, p.id]
                          : candidates.filter((id) => id !== p.id);
                        // Every profile on = the empty list, so a new profile is a candidate too.
                        const value = list.length === profiles.length ? [] : list;
                        void save("smart_profiles", value, "Candidates saved");
                      }}
                    />
                    {p.name}
                  </li>
                );
              })}
            </ul>
            <span className="text-xs text-muted-foreground">
              A profile's description and repositories are what smart routing reads: keep them
              specific.
            </span>
          </div>
        </div>
      )}
    </section>
  );
}
