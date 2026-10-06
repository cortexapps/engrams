import { useEffect, useState } from "react";
import { toast } from "sonner";

import { PageHeading } from "../../components/page-heading";
import { useRetentionPolicy, useSetRetentionPolicy } from "../../hooks/useRetention";
import { errorMessage } from "../../lib/errors";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Button } from "@/components/ui/button";
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";

export const RUN_DETAIL_DAYS_MIN = 7;
export const RUN_DETAIL_DAYS_MAX = 365;

// Retention policies for the workspace. Automation run details are the
// first: how long a finished run keeps its step-by-step record (each
// block's inputs and outputs, the relay's per-message records) and the
// engine's own bookkeeping of it. The run itself is kept: status, timing,
// trigger, the session it ran in. Session retention joins this page.
export function Retention() {
  const policy = useRetentionPolicy();
  const save = useSetRetentionPolicy();
  const stored = policy.data?.policy?.runDetailDays;
  const [days, setDays] = useState<string>("");
  useEffect(() => {
    if (stored !== undefined) setDays(String(stored));
  }, [stored]);

  const parsed = Number(days);
  const valid =
    days.trim() !== "" &&
    Number.isInteger(parsed) &&
    parsed >= RUN_DETAIL_DAYS_MIN &&
    parsed <= RUN_DETAIL_DAYS_MAX;
  const dirty = stored !== undefined && valid && parsed !== stored;

  const onSave = async () => {
    if (!valid) return;
    try {
      await save.mutateAsync({ policy: { runDetailDays: parsed } });
      toast.success("Retention policy saved");
    } catch (e) {
      toast.error(errorMessage(e));
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-6 overflow-auto p-4 md:p-6">
      <PageHeading title="Retention" />
      <p className="text-sm text-muted-foreground">
        How long the workspace keeps what its work leaves behind. Policies apply to every workspace
        member and are collected continuously, a batch at a time.
      </p>

      <section className="flex flex-col gap-3" aria-label="Automation runs">
        <h2 className="text-base font-semibold">Automation runs</h2>
        {policy.error ? (
          <EmptyState tone="error">
            Could not load the policy: {errorMessage(policy.error)}
          </EmptyState>
        ) : policy.isPending ? (
          <SkeletonRows rows={1} columns={["minmax(12rem,1fr)"]} />
        ) : (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void onSave();
            }}
            noValidate
            className="flex max-w-xl flex-col gap-4"
          >
            <FieldGroup>
              <Field data-invalid={!valid}>
                <FieldLabel htmlFor="run-detail-days">Keep run details for (days)</FieldLabel>
                <Input
                  id="run-detail-days"
                  inputMode="numeric"
                  value={days}
                  onChange={(e) => setDays(e.target.value)}
                  aria-invalid={!valid}
                  className="w-32"
                />
                <FieldDescription>
                  A finished run keeps its step-by-step record for this many days: each block’s
                  inputs and outputs, and the messages a Slack thread relayed. The run itself is
                  kept: its status, timing, trigger and the session it ran in. Between{" "}
                  {RUN_DETAIL_DAYS_MIN} and {RUN_DETAIL_DAYS_MAX} days.
                </FieldDescription>
                {!valid && (
                  <FieldError
                    errors={[
                      {
                        message: `Enter a whole number of days between ${RUN_DETAIL_DAYS_MIN} and ${RUN_DETAIL_DAYS_MAX}.`,
                      },
                    ]}
                  />
                )}
              </Field>
            </FieldGroup>
            <div>
              <Button type="submit" disabled={!dirty || save.isPending}>
                {save.isPending ? "Saving…" : "Save"}
              </Button>
            </div>
          </form>
        )}
      </section>

      <section className="flex flex-col gap-2" aria-label="Sessions">
        <h2 className="text-base font-semibold">Sessions</h2>
        <p className="text-sm text-muted-foreground">
          Session retention policies are not configurable yet; parked sessions follow the
          deployment’s idle limits.
        </p>
      </section>
    </div>
  );
}
