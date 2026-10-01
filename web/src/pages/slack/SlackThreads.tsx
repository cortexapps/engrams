import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { AlertTriangle } from "lucide-react";

import { toast } from "sonner";

import { PageHeading } from "../../components/page-heading";
import {
  useBuiltinAutomation,
  useSetAutomationEnabled,
  useSetInputValue,
  useSetMapInputEntry,
} from "../../hooks/useAutomations";
import { useInstanceList, useRecentDrops } from "../../hooks/useInstances";
import { useNow } from "../../hooks/useNow";
import { useProfiles } from "../../hooks/useProfiles";
import { errorMessage } from "../../lib/errors";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { WorkstreamDrops } from "@/pages/automations/workstreams/WorkstreamDrops";
import { WorkstreamsTable } from "@/pages/automations/workstreams/WorkstreamsTable";

/** The built-in behind this page. */
export const SLACK_BRAIN_BUILTIN_KEY = "slack_brain";

// The Slack threads product: whether engrams answers @-mentions, on which
// profile, and the threads it is (and was) in. It answers wherever the Slack
// app is a member and gets mentioned — the legacy behaviour; per-channel
// enrollment was the ADR 0119 window's gate and is gone. The switch is the
// Slack threads automation's own enabled flag; the default profile is its
// `default_profile` input; a channel override is one entry of its `channels`
// map. Every thread it answers in is a workstream of that automation, and
// every mention's run is on its Activity tab; the product is the front of the
// platform, the links below open the back.
export function SlackThreads() {
  const builtin = useBuiltinAutomation(SLACK_BRAIN_BUILTIN_KEY);
  const automation = builtin.data?.automation;
  const automationId = automation?.id;
  const inputs = inputsOf(automation?.inputsJson);
  const channels = channelsOf(automation?.inputsJson);
  const defaultProfile =
    typeof inputs["default_profile"] === "string" ? inputs["default_profile"] : "";
  const { data: profileData } = useProfiles();
  const profiles = profileData?.profiles ?? [];
  const setEnabled = useSetAutomationEnabled();
  const setDefault = useSetInputValue();
  const enabled = automation?.enabled === true;
  const answersSomewhere = defaultProfile !== "" || channels.length > 0;

  const onEnabledChange = async (next: boolean) => {
    if (!automationId) return;
    try {
      await setEnabled.mutateAsync({ id: automationId, enabled: next });
      toast.success(next ? "Answering @-mentions" : "Paused");
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  const onDefaultProfile = async (profileId: string) => {
    if (!automationId) return;
    try {
      // One key, atomically: a stale snapshot of the overrides map on this
      // page must never ride along and overwrite another admin's entry.
      await setDefault.mutateAsync({
        automationId,
        inputKey: "default_profile",
        valueJson: JSON.stringify(profileId === NO_PROFILE ? "" : profileId),
      });
      toast.success("Default profile saved");
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-8 overflow-auto p-4 md:p-6">
      <PageHeading
        title="Slack"
        count={
          automation ? (
            <label className="inline-flex items-center gap-2 text-xs">
              <Switch
                aria-label={enabled ? "Pause answering @-mentions" : "Answer @-mentions"}
                checked={enabled}
                disabled={setEnabled.isPending || (!enabled && !answersSomewhere)}
                onCheckedChange={onEnabledChange}
              />
              {enabled ? "Answering @-mentions" : "Paused"}
            </label>
          ) : undefined
        }
      />

      <p className="text-sm text-muted-foreground">
        @-mention engrams in any channel the Slack app is in and it answers in the thread, one
        session per thread, relayed both ways, by the{" "}
        {automationId ? (
          <Link
            to="/automations/$id"
            params={{ id: automationId }}
            search={{ tab: "activity" }}
            className="underline underline-offset-2"
          >
            Slack threads automation
          </Link>
        ) : (
          "Slack threads automation"
        )}
        . Every thread it answers in is a workstream; every mention is a run.
      </p>

      {builtin.error && (
        <EmptyState tone="error">
          Could not load the Slack threads automation — {errorMessage(builtin.error)}
        </EmptyState>
      )}

      <section className="flex flex-col gap-3" aria-label="Default profile">
        <h2 className="text-base font-semibold">Default profile</h2>
        <p className="text-sm text-muted-foreground">
          The session profile every thread runs on. A channel override below picks a different one
          for that channel. With no default, only channels with an override get answers.
        </p>
        {builtin.isPending ? (
          <SkeletonRows rows={1} columns={["minmax(12rem,1fr)"]} />
        ) : (
          <Select
            value={defaultProfile === "" ? NO_PROFILE : defaultProfile}
            onValueChange={onDefaultProfile}
            disabled={!automationId || setDefault.isPending}
          >
            <SelectTrigger className="w-72" aria-label="Default profile">
              <SelectValue placeholder="Pick a profile" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={NO_PROFILE}>No default profile</SelectItem>
              {profiles.map((p) => (
                <SelectItem key={p.id} value={p.id}>
                  {p.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
        {automation && !enabled && !answersSomewhere && (
          <p className="text-xs text-muted-foreground">
            Pick a default profile to turn answering on.
          </p>
        )}
        {automation && enabled && !answersSomewhere && (
          <div
            role="alert"
            className="flex items-start gap-2 rounded-lg border border-instrument-caution/40 bg-instrument-caution/10 p-3 text-sm"
          >
            <AlertTriangle className="mt-0.5 size-4 shrink-0 text-instrument-caution" aria-hidden />
            <span>
              Answering is on but no profile is set, so every mention is ignored. Pick a default
              profile.
            </span>
          </div>
        )}
      </section>

      <section className="flex flex-col gap-3" aria-label="Channel overrides">
        <div className="flex flex-wrap items-center gap-3">
          <h2 className="text-base font-semibold">Channel overrides</h2>
          {automationId && (
            <div className="ml-auto">
              <EnrollChannelDialog automationId={automationId} profiles={profiles} />
            </div>
          )}
        </div>
        {builtin.isPending ? (
          <SkeletonRows rows={2} columns={["minmax(12rem,1fr)", "minmax(8rem,1fr)", "6rem"]} />
        ) : channels.length === 0 ? (
          <EmptyState>No overrides. Every channel uses the default profile.</EmptyState>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Channel</TableHead>
                <TableHead>Profile</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {channels.map((row) => (
                <ChannelRow
                  key={row.channel}
                  automationId={automationId!}
                  row={row}
                  profiles={profiles}
                />
              ))}
            </TableBody>
          </Table>
        )}
      </section>

      {automationId && <ThreadsSection automationId={automationId} />}
    </div>
  );
}

const NO_PROFILE = "__none__";

/** The built-in's inputs, as stored. */
export function inputsOf(inputsJson: string | undefined): Record<string, unknown> {
  if (!inputsJson) return {};
  try {
    const parsed: unknown = JSON.parse(inputsJson);
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

export interface ChannelEntry {
  channel: string;
  profileId: string;
}

/** The `channels` map of the built-in's inputs, as rows. */
export function channelsOf(inputsJson: string | undefined): ChannelEntry[] {
  if (!inputsJson) return [];
  try {
    const parsed = JSON.parse(inputsJson) as { channels?: unknown };
    const map = parsed.channels;
    if (typeof map !== "object" || map === null || Array.isArray(map)) return [];
    return Object.entries(map as Record<string, unknown>)
      .map(([channel, profileId]) => ({
        channel,
        profileId: typeof profileId === "string" ? profileId : "",
      }))
      .sort((a, b) => a.channel.localeCompare(b.channel));
  } catch {
    return [];
  }
}

type ProfileLite = { id: string; name: string };

function profileLabel(profileId: string, profiles: ProfileLite[]): string {
  return profiles.find((p) => p.id === profileId)?.name ?? profileId;
}

function ChannelRow({
  automationId,
  row,
  profiles,
}: {
  automationId: string;
  row: ChannelEntry;
  profiles: ProfileLite[];
}) {
  const remove = useSetMapInputEntry();
  return (
    <TableRow>
      <TableCell className="font-mono text-xs">{row.channel}</TableCell>
      <TableCell className="text-xs text-muted-foreground">
        {profileLabel(row.profileId, profiles)}
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end gap-1">
          <EnrollChannelDialog automationId={automationId} profiles={profiles} existing={row} />
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" disabled={remove.isPending}>
                Remove
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Remove the override for {row.channel}?</AlertDialogTitle>
                <AlertDialogDescription>
                  Threads in this channel go back to the default profile from the next mention.
                  Threads already open keep their session.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction
                  onClick={() =>
                    remove.mutate({
                      automationId,
                      inputKey: "channels",
                      entryKey: row.channel,
                      enable: false,
                    })
                  }
                >
                  Remove
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {remove.error && (
          <EmptyState tone="error" inline className="mt-1 items-end text-right">
            Could not remove — {errorMessage(remove.error)}
          </EmptyState>
        )}
      </TableCell>
    </TableRow>
  );
}

const enrollSchema = z.object({
  channel: z
    .string()
    .trim()
    .regex(/^[CGD][A-Z0-9]{6,}$/, "must be a Slack channel ID (e.g. C0123456789)"),
  profileId: z.string().min(1, "pick a profile"),
});
type EnrollValues = z.infer<typeof enrollSchema>;

function EnrollChannelDialog({
  automationId,
  profiles,
  existing,
}: {
  automationId: string;
  profiles: ProfileLite[];
  existing?: ChannelEntry;
}) {
  const [open, setOpen] = useState(false);
  const enroll = useSetMapInputEntry();
  const isEdit = existing !== undefined;
  const defaults: EnrollValues = {
    channel: existing?.channel ?? "",
    profileId: existing?.profileId ?? profiles[0]?.id ?? "",
  };
  const form = useForm<EnrollValues>({
    resolver: zodResolver(enrollSchema),
    defaultValues: defaults,
  });

  const onSubmit = async (data: EnrollValues) => {
    try {
      // One entry, atomically. An override never turns answering on by itself.
      await enroll.mutateAsync({
        automationId,
        inputKey: "channels",
        entryKey: data.channel.trim(),
        valueJson: JSON.stringify(data.profileId),
        enable: false,
      });
      setOpen(false);
    } catch (e) {
      form.setError("root", { message: errorMessage(e) });
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (o) form.reset(defaults);
      }}
    >
      <DialogTrigger asChild>
        {isEdit ? (
          <Button variant="ghost" size="sm">
            Edit
          </Button>
        ) : (
          <Button variant="outline" size="sm">
            Add override
          </Button>
        )}
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>
            {isEdit ? `Edit ${existing.channel}` : "Add a channel override"}
          </DialogTitle>
          <DialogDescription>
            Threads in this channel run on the profile you pick instead of the default.
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={form.handleSubmit(onSubmit)} noValidate>
          <FieldGroup>
            <Controller
              name="channel"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Channel ID</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    autoFocus={!isEdit}
                    disabled={isEdit}
                    placeholder="C0123456789"
                    spellCheck={false}
                    autoCapitalize="off"
                    aria-invalid={fieldState.invalid}
                  />
                  <FieldDescription>
                    In Slack: open the channel, click its name, and copy the Channel ID at the
                    bottom of the About tab. Cannot be changed later.
                  </FieldDescription>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />
            <Controller
              name="profileId"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Profile</FieldLabel>
                  <Select value={field.value} onValueChange={field.onChange}>
                    <SelectTrigger id={field.name} aria-label="Profile">
                      <SelectValue placeholder="Pick a profile" />
                    </SelectTrigger>
                    <SelectContent>
                      {profiles.map((p) => (
                        <SelectItem key={p.id} value={p.id}>
                          {p.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <FieldDescription>
                    The session profile every thread in this channel runs on.
                  </FieldDescription>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />
            {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
          </FieldGroup>

          <DialogFooter className="mt-5">
            <Button
              type="button"
              variant="ghost"
              onClick={() => setOpen(false)}
              disabled={enroll.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={enroll.isPending}>
              {enroll.isPending ? "Saving…" : isEdit ? "Save" : "Add"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The threads: the automation's workstreams, open first. The table is the
 * platform's own workstream table — the same rows an admin sees under
 * Automations, which is the point. */
function ThreadsSection({ automationId }: { automationId: string }) {
  const [status, setStatus] = useState<"open" | "closed">("open");
  const list = useInstanceList(automationId, { includeClosed: true });
  const drops = useRecentDrops(automationId);
  const now = useNow();
  const all = list.data?.instances ?? [];
  const open = all.filter((instance) => instance.status === "open");
  const closed = all.filter((instance) => instance.status === "closed");
  const rows = status === "open" ? open : closed;

  return (
    <section className="flex flex-col gap-3" aria-label="Threads">
      <div className="flex flex-wrap items-center gap-3">
        <h2 className="text-base font-semibold">Threads</h2>
        <Tabs value={status} onValueChange={(value) => setStatus(value as "open" | "closed")}>
          <TabsList aria-label="Thread status">
            <TabsTrigger value="open">Open</TabsTrigger>
            <TabsTrigger value="closed">
              Closed
              <span className="font-mono text-2xs tabular-nums text-muted-foreground">
                {closed.length}
              </span>
            </TabsTrigger>
          </TabsList>
        </Tabs>
      </div>
      {list.error ? (
        <EmptyState tone="error">Couldn’t load threads. {errorMessage(list.error)}</EmptyState>
      ) : list.isPending ? (
        <div className="rounded-lg border bg-card px-4">
          <SkeletonRows
            rows={3}
            columns={["minmax(0,1.5fr)", "minmax(0,1.3fr)", "130px", "90px"]}
          />
        </div>
      ) : rows.length === 0 ? (
        <EmptyState>
          {status === "open"
            ? "No open threads. @-mention engrams in a channel the app is in to start one."
            : "No closed threads."}
        </EmptyState>
      ) : (
        <WorkstreamsTable instances={rows} automations={[]} showAutomation={false} now={now} />
      )}
      <WorkstreamDrops drops={drops.data?.drops ?? []} now={now} />
    </section>
  );
}
