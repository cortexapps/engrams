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
import {
  connectorOAuthAuthorizeUrl,
  useCancelOAuth,
  useConnectOAuth,
  useCredentials,
  useDeleteConnectorCredential,
  useDisconnectOAuth,
  useOAuthFlow,
  useSetConnectorCredential,
  type ConnectorCredentialEntry,
  type OAuthCredentialEntry,
} from "../../hooks/useCredentials";

/**
 * The user's harness credentials (ADR 0063 B3). Each registered harness declares
 * the env var it authenticates with (`auth.user_env`); this page is the union of
 * those across the catalog, one card each, with the value sealed under the
 * deployment key. There is no Claude-specific "claude token" any more.
 */
export function TokensPanel() {
  const { data: vars, isLoading } = useHarnessEnv(true);
  const { data: credentials, isLoading: credentialsLoading } = useCredentials(true);
  const oauth = credentials?.filter(
    (credential): credential is OAuthCredentialEntry => credential.kind === "oauth",
  );
  const connectors = credentials?.filter(
    (credential): credential is ConnectorCredentialEntry => credential.kind === "connector",
  );

  return (
    <div className="space-y-6">
      <PageHeading title="Credentials" />
      {(oauth ?? []).map((entry) => (
        <OAuthCard key={entry.provider} entry={entry} />
      ))}
      {isLoading || credentialsLoading ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : (!vars || vars.length === 0) && (!oauth || oauth.length === 0) ? (
        <Card>
          <CardContent className="py-8 text-center text-sm text-muted-foreground">
            No registered harness asks for a user credential. Nothing to set here.
          </CardContent>
        </Card>
      ) : (
        (vars ?? []).map((v) => <EnvVarCard key={v.envVar} entry={v} />)
      )}
      {connectors && connectors.length > 0 && (
        <>
          <div>
            <h2 className="text-base font-semibold">Integration credentials</h2>
            <p className="max-w-prose text-sm text-muted-foreground">
              Personal credentials for integrations a profile runs as you. Sessions you start from
              such a profile act with your identity instead of the shared org credential.
            </p>
          </div>
          {connectors.map((entry) => (
            <ConnectorCard key={entry.provider} entry={entry} />
          ))}
        </>
      )}
      <p className="max-w-prose text-sm text-muted-foreground">
        Credentials are sealed under the deployment key and selected automatically for each session.
        OAuth caches are delivered through the session control channel, never as model environment
        variables.
      </p>
    </div>
  );
}

/** ADR 0115: one personal-credential slot per connector — OAuth connect and/or
 *  a pasted personal access token; the newest write wins. */
function ConnectorCard({ entry }: { entry: ConnectorCredentialEntry }) {
  const [editing, setEditing] = useState(false);
  const remove = useDeleteConnectorCredential();
  const healthy = entry.connected && entry.status === "connected";
  const badge = healthy
    ? "connected"
    : entry.connected
      ? entry.status || "needs attention"
      : "not connected";

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
        <div>
          <CardTitle className="text-base">{entry.display.name}</CardTitle>
          <p className="text-sm text-muted-foreground">Your personal credential.</p>
          {entry.tokenHint && (
            <p className="mt-1 max-w-prose text-sm text-muted-foreground">{entry.tokenHint}</p>
          )}
        </div>
        <Badge variant={healthy ? "secondary" : "outline"}>{badge}</Badge>
      </CardHeader>
      <CardContent className="space-y-3">
        {entry.connected && entry.account && (
          <p className="text-sm text-muted-foreground">
            {[entry.account.displayName, entry.account.workspaceName].filter(Boolean).join(" · ")}
          </p>
        )}
        {editing ? (
          <ConnectorTokenForm provider={entry.provider} onDone={() => setEditing(false)} />
        ) : (
          <div className="flex gap-2">
            {entry.modes.oauth && (
              <Button size="sm" variant={healthy ? "outline" : "default"} asChild>
                <a href={connectorOAuthAuthorizeUrl(entry.provider, entry.connected)}>
                  {entry.connected ? "Reconnect" : "Connect"}
                </a>
              </Button>
            )}
            {entry.modes.token && (
              <Button
                size="sm"
                variant={entry.modes.oauth || healthy ? "outline" : "default"}
                onClick={() => setEditing(true)}
              >
                {entry.connected ? "Replace token" : "Add token"}
              </Button>
            )}
            {entry.connected && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => remove.mutate(entry.provider)}
                disabled={remove.isPending}
              >
                {remove.isPending ? "Disconnecting…" : "Disconnect"}
              </Button>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function ConnectorTokenForm({ provider, onDone }: { provider: string; onDone: () => void }) {
  const form = useForm<ValueForm>({
    resolver: zodResolver(valueSchema),
    defaultValues: { value: "" },
  });
  const save = useSetConnectorCredential();

  const onSubmit = (data: ValueForm) =>
    save.mutate(
      { provider, value: data.value },
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
                id={`${provider}-token`}
                type="password"
                autoFocus
                placeholder="paste token…"
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

function OAuthCard({ entry }: { entry: OAuthCredentialEntry }) {
  const connect = useConnectOAuth();
  const cancel = useCancelOAuth();
  const disconnect = useDisconnectOAuth();
  const [pending, setPending] = useState<{
    flowId: string;
    verificationUrl: string;
    userCode: string;
  } | null>(null);
  const flow = useOAuthFlow(pending?.flowId ?? null);
  const terminal = flow.data?.status && flow.data.status !== "pending";
  const askedBy = entry.harnesses.map((h) => h.label).join(", ");

  const begin = () =>
    connect.mutate(entry.provider, {
      onSuccess: (result) =>
        setPending({
          flowId: result.flow.id,
          verificationUrl: result.verificationUrl,
          userCode: result.userCode,
        }),
    });

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
        <div>
          <CardTitle className="text-base">
            {entry.provider === "openai-codex" ? "OpenAI ChatGPT" : entry.provider}
          </CardTitle>
          <p className="text-sm text-muted-foreground">
            {askedBy ? `Used by ${askedBy}.` : "Used by a registered harness."}
          </p>
          {entry.hint && (
            <p className="mt-1 max-w-prose text-sm text-muted-foreground">{entry.hint}</p>
          )}
        </div>
        <Badge variant={entry.connected ? "secondary" : "outline"}>
          {entry.connected ? "connected" : "not connected"}
        </Badge>
      </CardHeader>
      <CardContent className="space-y-3">
        {entry.connected && entry.account && (
          <p className="text-sm text-muted-foreground">
            {[entry.account.displayName, entry.account.workspaceName, entry.account.planType]
              .filter(Boolean)
              .join(" · ")}
          </p>
        )}
        {pending && !terminal && (
          <div className="rounded-md border bg-muted/40 p-3 text-sm">
            <p>
              Open{" "}
              <a
                className="font-medium underline"
                href={pending.verificationUrl}
                target="_blank"
                rel="noreferrer"
              >
                {pending.verificationUrl}
              </a>{" "}
              and enter:
            </p>
            <code className="mt-2 block select-all text-lg font-semibold tracking-widest">
              {pending.userCode}
            </code>
            <p className="mt-2 text-muted-foreground">Waiting for OpenAI…</p>
          </div>
        )}
        {flow.data && terminal && flow.data.status !== "succeeded" && (
          <p className="text-sm text-destructive">
            Connection {flow.data.status.replaceAll("_", " ")}. Try again.
          </p>
        )}
        <div className="flex gap-2">
          <Button
            size="sm"
            variant={entry.connected ? "outline" : "default"}
            onClick={begin}
            disabled={connect.isPending}
          >
            {connect.isPending ? "Starting…" : entry.connected ? "Reconnect" : "Connect"}
          </Button>
          {pending && !terminal && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => cancel.mutate(pending.flowId)}
              disabled={cancel.isPending}
            >
              Cancel
            </Button>
          )}
          {entry.connected && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => disconnect.mutate(entry.provider)}
              disabled={disconnect.isPending}
            >
              Disconnect
            </Button>
          )}
        </div>
      </CardContent>
    </Card>
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
