import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { toast } from "sonner";
import { Copy } from "lucide-react";
import { useApiKeys, useCreateApiKey, useRevokeApiKey } from "../../hooks/useApiKeys";
import type { ApiKeyMeta } from "../../gen/engram/app/v1/api_key_pb";
import { PageHeading } from "../page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
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

// Global API keys (ADR 0086) — admin-minted programmatic credentials. A key
// carries its own role ('admin' | 'user') and authenticates requests via
// `x-api-key: engk_…` or `Authorization: Bearer engk_…`. The plaintext is
// shown exactly once at creation (only its hash is stored); the list shows a
// masked preview. Revoking deletes the key's service account — it stops
// authenticating immediately.
export function ApiKeysPanel() {
  const { data, isLoading, error } = useApiKeys();
  const rows = data?.keys ?? [];

  return (
    <div className="space-y-6">
      <PageHeading
        title="API keys"
        description="Programmatic credentials for CI and scripts. Each key acts with its assigned role — the same RBAC as a signed-in user. The key value is shown once at creation and never again."
        actions={<CreateKeyDialog />}
      />

      {error && <p className="text-sm text-destructive">could not load keys — {String(error)}</p>}

      {isLoading ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : rows.length === 0 ? (
        <Card>
          <CardContent className="py-10 text-center text-sm text-muted-foreground">
            No API keys yet. Create one to call the engrams API from CI or scripts.
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Role</TableHead>
              <TableHead>Key</TableHead>
              <TableHead>Expires</TableHead>
              <TableHead>Last used</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <KeyRow key={row.id} row={row} />
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

function KeyRow({ row }: { row: ApiKeyMeta }) {
  const revoke = useRevokeApiKey();
  return (
    <TableRow>
      <TableCell className="text-sm">{row.name}</TableCell>
      <TableCell>
        <Badge variant={row.role === "admin" ? "default" : "secondary"}>{row.role}</Badge>
      </TableCell>
      <TableCell className="font-mono text-xs text-muted-foreground">{row.start}…</TableCell>
      <TableCell
        className="text-xs text-muted-foreground"
        title={row.expiresAt ? new Date(row.expiresAt).toLocaleString() : undefined}
      >
        {row.expiresAt ? new Date(row.expiresAt).toLocaleDateString() : "Never"}
      </TableCell>
      <TableCell
        className="text-xs text-muted-foreground"
        title={row.lastUsedAt ? new Date(row.lastUsedAt).toLocaleString() : undefined}
      >
        {row.lastUsedAt ? timeAgo(row.lastUsedAt) : "Never"}
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end gap-1">
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" disabled={revoke.isPending}>
                Revoke
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Revoke {row.name}?</AlertDialogTitle>
                <AlertDialogDescription>
                  Anything using this key stops authenticating immediately. This cannot be undone —
                  mint a new key to restore access.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={() => revoke.mutate({ id: row.id })}>
                  Revoke key
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {revoke.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            could not revoke — {String(revoke.error)}
          </p>
        )}
      </TableCell>
    </TableRow>
  );
}

/** Tomorrow as yyyy-mm-dd (the <input type="date"> floor — the server's
 *  expiration minimum is 1 day out; revoke covers "kill it now"). */
function tomorrowIso(): string {
  const t = new Date(Date.now() + 24 * 3600_000);
  return t.toISOString().slice(0, 10);
}

const keySchema = z.object({
  name: z.string().trim().min(1, "name is required"),
  role: z.enum(["user", "admin"]),
  // Native date input yields "" (unset) or yyyy-mm-dd.
  expiresAt: z.string().refine((v) => v === "" || Date.parse(v) - Date.now() >= 23 * 3600_000, {
    message: "expiration must be at least a day out",
  }),
});
type KeyValues = z.infer<typeof keySchema>;

const EMPTY: KeyValues = { name: "", role: "user", expiresAt: "" };

function CreateKeyDialog() {
  const [open, setOpen] = useState(false);
  // The one-time reveal: set after a successful create, cleared on close.
  const [minted, setMinted] = useState<string | null>(null);
  const create = useCreateApiKey();
  const form = useForm<KeyValues>({ resolver: zodResolver(keySchema), defaultValues: EMPTY });

  const onSubmit = async (data: KeyValues) => {
    try {
      const r = await create.mutateAsync({
        name: data.name.trim(),
        role: data.role,
        // Send end-of-day local time so "expires on <date>" means the whole day.
        expiresAt:
          data.expiresAt === "" ? "" : new Date(`${data.expiresAt}T23:59:59`).toISOString(),
      });
      setMinted(r.key);
    } catch (e) {
      form.setError("root", { message: String(e) });
    }
  };

  const copyKey = async () => {
    if (!minted) return;
    await navigator.clipboard.writeText(minted);
    toast("Key copied to clipboard");
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (o) {
          form.reset(EMPTY);
          setMinted(null);
        }
      }}
    >
      <DialogTrigger asChild>
        <Button>Create key</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        {minted === null ? (
          <>
            <DialogHeader>
              <DialogTitle>New API key</DialogTitle>
              <DialogDescription>
                The key acts with the role you assign it — a user-role key can only touch what a
                member could; an admin-role key can do everything.
              </DialogDescription>
            </DialogHeader>

            {/* noValidate: the date input's `min` would otherwise trigger NATIVE
                constraint validation, silently blocking submit (no styled error)
                on a past date — zod owns validation; `min` stays as a picker
                affordance only. */}
            <form onSubmit={form.handleSubmit(onSubmit)} noValidate>
              <FieldGroup>
                <Controller
                  name="name"
                  control={form.control}
                  render={({ field, fieldState }) => (
                    <Field data-invalid={fieldState.invalid}>
                      <FieldLabel htmlFor={field.name}>Name</FieldLabel>
                      <Input
                        {...field}
                        id={field.name}
                        autoFocus
                        placeholder="ci-bot"
                        spellCheck={false}
                        autoCapitalize="off"
                        aria-invalid={fieldState.invalid}
                      />
                      {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                    </Field>
                  )}
                />
                <Controller
                  name="role"
                  control={form.control}
                  render={({ field }) => (
                    <Field>
                      <FieldLabel htmlFor={field.name}>Role</FieldLabel>
                      <Select value={field.value} onValueChange={field.onChange}>
                        <SelectTrigger id={field.name} aria-label="Role">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value="user">user</SelectItem>
                          <SelectItem value="admin">admin</SelectItem>
                        </SelectContent>
                      </Select>
                      <FieldDescription>
                        Prefer user unless the key must manage org-wide config.
                      </FieldDescription>
                    </Field>
                  )}
                />
                <Controller
                  name="expiresAt"
                  control={form.control}
                  render={({ field, fieldState }) => (
                    <Field data-invalid={fieldState.invalid}>
                      <FieldLabel htmlFor={field.name}>Expires (optional)</FieldLabel>
                      <Input
                        {...field}
                        id={field.name}
                        type="date"
                        min={tomorrowIso()}
                        aria-invalid={fieldState.invalid}
                      />
                      <FieldDescription>
                        Leave empty for a non-expiring key. Expired keys are deleted automatically.
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
                  disabled={create.isPending}
                >
                  Cancel
                </Button>
                <Button type="submit" disabled={create.isPending}>
                  {create.isPending ? "Creating…" : "Create key"}
                </Button>
              </DialogFooter>
            </form>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle>Copy your key now</DialogTitle>
              <DialogDescription>
                This is the only time the key is shown — only a hash is stored. Send it as{" "}
                <code className="font-mono">x-api-key</code> or{" "}
                <code className="font-mono">Authorization: Bearer</code>.
              </DialogDescription>
            </DialogHeader>
            <div className="flex items-center gap-2">
              <Input readOnly value={minted} className="font-mono text-xs" aria-label="API key" />
              <Button
                type="button"
                variant="outline"
                size="icon"
                onClick={copyKey}
                aria-label="Copy key"
              >
                <Copy className="size-4" />
              </Button>
            </div>
            <DialogFooter>
              <Button type="button" onClick={() => setOpen(false)}>
                Done
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function timeAgo(iso: string): string {
  const then = new Date(iso).getTime();
  const now = Date.now();
  if (Number.isNaN(then)) return iso;
  const seconds = Math.max(0, Math.floor((now - then) / 1000));
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(iso).toLocaleDateString();
}
