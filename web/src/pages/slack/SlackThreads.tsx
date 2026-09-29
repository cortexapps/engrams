import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { AlertTriangle } from "lucide-react";

import { PageHeading } from "../../components/page-heading";
import { useBuiltinAutomation, useSetMapInputEntry } from "../../hooks/useAutomations";
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

// The Slack threads product: which channels engrams answers @-mentions in,
// and the threads it is (and was) in. Enrolling a channel here is a write to
// the Slack threads automation — its `channels` input, one entry per channel,
// mapped to the session profile the thread runs on — and the automation is
// enabled on the first channel. Every thread it answers in is a workstream of
// that automation, and every mention's run is on its Activity tab; the
// product is the front of the platform, the links below open the back.
export function SlackThreads() {
  const builtin = useBuiltinAutomation(SLACK_BRAIN_BUILTIN_KEY);
  const automation = builtin.data?.automation;
  const automationId = automation?.id;
  const channels = channelsOf(automation?.inputsJson);
  const { data: profileData } = useProfiles();
  const profiles = profileData?.profiles ?? [];

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-8 overflow-auto p-4 md:p-6">
      <PageHeading
        title="Slack"
        count={
          channels.length > 0
            ? `${channels.length} ${channels.length === 1 ? "channel" : "channels"}`
            : undefined
        }
        actions={
          automationId ? (
            <EnrollChannelDialog automationId={automationId} profiles={profiles} />
          ) : undefined
        }
      />

      <p className="text-sm text-muted-foreground">
        @-mentions in these channels start a session per thread, relayed both ways, by the{" "}
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

      <section className="flex flex-col gap-3" aria-label="Channels">
        <h2 className="text-base font-semibold">Channels</h2>
        {builtin.isPending ? (
          <SkeletonRows rows={2} columns={["minmax(12rem,1fr)", "minmax(8rem,1fr)", "6rem"]} />
        ) : channels.length === 0 ? (
          <EmptyState>
            No channels enrolled yet. Enroll one to have engrams answer @-mentions there.
          </EmptyState>
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
        {automation && !automation.enabled && channels.length > 0 && (
          <div
            role="alert"
            className="flex items-start gap-2 rounded-lg border border-instrument-caution/40 bg-instrument-caution/10 p-3 text-sm"
          >
            <AlertTriangle className="mt-0.5 size-4 shrink-0 text-instrument-caution" aria-hidden />
            <span>
              The Slack threads automation is paused, so no channel answers. Turn it on under
              Automations, or enroll a channel again.
            </span>
          </div>
        )}
      </section>

      {automationId && <ThreadsSection automationId={automationId} />}
    </div>
  );
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
                <AlertDialogTitle>Stop answering in {row.channel}?</AlertDialogTitle>
                <AlertDialogDescription>
                  engrams will ignore @-mentions in this channel from the next message. Threads
                  already open keep their session. You can enroll it again any time.
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
      // One entry, atomically; the first channel turns the automation on.
      await enroll.mutateAsync({
        automationId,
        inputKey: "channels",
        entryKey: data.channel.trim(),
        valueJson: JSON.stringify(data.profileId),
        enable: true,
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
          <Button>Enroll channel</Button>
        )}
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{isEdit ? `Edit ${existing.channel}` : "Enroll a channel"}</DialogTitle>
          <DialogDescription>
            engrams answers @-mentions in enrolled channels with one session per thread. The Slack
            app must be a member of the channel.
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
              {enroll.isPending ? "Saving…" : isEdit ? "Save" : "Enroll"}
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
            ? "No open threads. @-mention engrams in an enrolled channel to start one."
            : "No closed threads."}
        </EmptyState>
      ) : (
        <WorkstreamsTable instances={rows} automations={[]} showAutomation={false} now={now} />
      )}
      <WorkstreamDrops drops={drops.data?.drops ?? []} now={now} />
    </section>
  );
}
