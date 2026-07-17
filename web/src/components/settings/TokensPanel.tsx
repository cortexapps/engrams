import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import {
  useHarnessEnv,
  useSetHarnessEnv,
  useDeleteHarnessEnv,
  type HarnessEnvVar,
} from "../../hooks/useHarnessEnv";
import { PageHeading } from "../page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Field, FieldDescription, FieldError, FieldGroup } from "@/components/ui/field";
import { Input } from "@/components/ui/input";

/**
 * The user's harness credentials (ADR 0063 B3). Each registered harness declares
 * the env var it authenticates with (`auth.user_env`); this page is the union of
 * those across the catalog, one card each, with the value sealed under the
 * deployment key. There is no Claude-specific "claude token" any more.
 */
export function TokensPanel() {
  const { data: vars, isLoading } = useHarnessEnv(true);

  return (
    <div className="space-y-6">
      <PageHeading title="Tokens" />
      {isLoading ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : !vars || vars.length === 0 ? (
        <Card>
          <CardContent className="py-8 text-center text-sm text-muted-foreground">
            No registered harness asks for a user credential. Nothing to set here.
          </CardContent>
        </Card>
      ) : (
        vars.map((v) => <EnvVarCard key={v.envVar} entry={v} />)
      )}
      <p className="max-w-prose text-sm text-muted-foreground">
        Every value is sealed under the deployment key the moment you save it — the plaintext never
        touches Postgres, and it's used automatically so you're never prompted per session.
      </p>
    </div>
  );
}

function EnvVarCard({ entry }: { entry: HarnessEnvVar }) {
  const [editing, setEditing] = useState(false);
  const remove = useDeleteHarnessEnv();
  // "Claude Code, OpenCode" — the harnesses that authenticate with this env var.
  const askedBy = entry.harnesses.map((h) => h.label).join(", ");

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
        <div>
          <CardTitle className="font-mono text-base">{entry.envVar}</CardTitle>
          <p className="text-sm text-muted-foreground">
            {askedBy ? `Used by ${askedBy}.` : "Used by a registered harness."}
          </p>
          {entry.hint && (
            <p className="mt-1 max-w-prose text-sm text-muted-foreground">{entry.hint}</p>
          )}
        </div>
        <Badge variant={entry.present ? "secondary" : "outline"}>
          {entry.present ? "saved · sealed" : "not set"}
        </Badge>
      </CardHeader>
      <CardContent>
        {editing ? (
          <EnvVarForm envVar={entry.envVar} onDone={() => setEditing(false)} />
        ) : (
          <div className="flex gap-2">
            <Button
              size="sm"
              variant={entry.present ? "outline" : "default"}
              onClick={() => setEditing(true)}
            >
              {entry.present ? "Replace" : "Add value"}
            </Button>
            {entry.present && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => remove.mutate(entry.envVar)}
                disabled={remove.isPending}
              >
                {remove.isPending ? "Removing…" : "Remove"}
              </Button>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

const valueSchema = z.object({ value: z.string().trim().min(1, "A value is required") });
type ValueForm = z.infer<typeof valueSchema>;

function EnvVarForm({ envVar, onDone }: { envVar: string; onDone: () => void }) {
  const form = useForm<ValueForm>({
    resolver: zodResolver(valueSchema),
    defaultValues: { value: "" },
  });
  const save = useSetHarnessEnv();

  const onSubmit = (data: ValueForm) =>
    save.mutate(
      { envVar, value: data.value },
      {
        onSuccess: onDone,
        onError: (e) => form.setError("root", { message: String(e) }),
      },
    );

  return (
    <form onSubmit={form.handleSubmit(onSubmit)}>
      <FieldGroup>
        <Controller
          name="value"
          control={form.control}
          render={({ field, fieldState }) => (
            <Field data-invalid={fieldState.invalid}>
              <Input
                {...field}
                id={field.name}
                type="password"
                autoFocus
                placeholder="paste value…"
                className="font-mono"
                aria-invalid={fieldState.invalid}
              />
              <FieldDescription>Stored encrypted, never shown again.</FieldDescription>
              {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
            </Field>
          )}
        />
        <Field orientation="horizontal">
          <Button type="submit" size="sm" disabled={save.isPending}>
            {save.isPending ? "Saving…" : "Save"}
          </Button>
          <Button type="button" size="sm" variant="ghost" onClick={onDone}>
            Cancel
          </Button>
        </Field>
        {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
      </FieldGroup>
    </form>
  );
}
