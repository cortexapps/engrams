import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { saveClaudeToken } from "../../api";
import { useAuth } from "../../auth/AuthProvider";
import { PageHeading } from "../page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Field, FieldDescription, FieldError, FieldGroup } from "@/components/ui/field";
import { Input } from "@/components/ui/input";

const tokenSchema = z.object({
  token: z.string().trim().min(1, "Token is required"),
});
type TokenValues = z.infer<typeof tokenSchema>;

export function TokensPanel() {
  const { principal, refresh } = useAuth();
  const qc = useQueryClient();
  const [editing, setEditing] = useState(false);
  const saved = principal.has_claude_token;

  return (
    <div className="max-w-2xl space-y-6">
      <PageHeading title="Tokens" />
      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
          <div>
            <CardTitle>Claude Code</CardTitle>
            <p className="text-sm text-muted-foreground">
              Built-in Claude sessions authenticate with this.
            </p>
          </div>
          <Badge variant={saved ? "secondary" : "outline"}>
            {saved ? "saved · sealed" : "not connected"}
          </Badge>
        </CardHeader>
        <CardContent>
          {editing ? (
            <TokenForm
              onCancel={() => setEditing(false)}
              onSaved={() => {
                setEditing(false);
                void qc.invalidateQueries({ queryKey: ["me"] });
                refresh();
              }}
            />
          ) : (
            <Button
              size="sm"
              variant={saved ? "outline" : "default"}
              onClick={() => setEditing(true)}
            >
              {saved ? "Replace" : "Add token"}
            </Button>
          )}
        </CardContent>
      </Card>
      <p className="max-w-prose text-sm text-muted-foreground">
        Every token is sealed under the deployment key the moment you save it — the plaintext never
        touches Postgres, and it's used automatically so you're never prompted per session.
      </p>
    </div>
  );
}

function TokenForm({ onCancel, onSaved }: { onCancel: () => void; onSaved: () => void }) {
  const form = useForm<TokenValues>({
    resolver: zodResolver(tokenSchema),
    defaultValues: { token: "" },
  });
  const save = useMutation({
    mutationFn: (t: string) => saveClaudeToken(t),
    onSuccess: onSaved,
    onError: (e) => form.setError("root", { message: String(e) }),
  });

  const onSubmit = (data: TokenValues) => save.mutate(data.token);

  return (
    <form onSubmit={form.handleSubmit(onSubmit)}>
      <FieldGroup>
        <Controller
          name="token"
          control={form.control}
          render={({ field, fieldState }) => (
            <Field data-invalid={fieldState.invalid}>
              <Input
                {...field}
                id={field.name}
                type="password"
                autoFocus
                placeholder="sk-ant-oat…"
                className="font-mono"
                aria-invalid={fieldState.invalid}
              />
              <FieldDescription>
                From <code className="font-mono">claude setup-token</code> — stored encrypted, never
                shown again.
              </FieldDescription>
              {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
            </Field>
          )}
        />
        <Field orientation="horizontal">
          <Button type="submit" size="sm" disabled={save.isPending}>
            {save.isPending ? "Saving…" : "Save"}
          </Button>
          <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
        </Field>
        {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
      </FieldGroup>
    </form>
  );
}
