import { useState } from 'react';
import {
  useAddRegistry,
  useDeleteRegistry,
  useRegistries,
} from '../../hooks/useRegistries';
import type {
  AddRegistryAuth,
  RegistryAuthKind,
  RegistryCredentialSummary,
} from '../../types';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader,
  DialogTitle, DialogTrigger,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
  AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog';
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table';
import { cn } from '@/lib/utils';

// Registries panel — operators register Docker registries with either a static
// credential (sealed under the deployment KEK) or ambient GCP Workload
// Identity. AWS instance role is a "coming soon" stub so the UI matrix matches
// the RegistryAuthSpec enum's design intent.
export function RegistriesPanel() {
  const { data, isLoading, error } = useRegistries();
  const rows = data ?? [];

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Registries</h1>
          <p className="text-sm text-muted-foreground">
            Docker registries for image pulls. Passwords are sealed at rest, never returned to the browser.
          </p>
        </div>
        <AddRegistryDialog />
      </div>

      {error && (
        <p className="text-sm text-destructive">could not load registries — {String(error)}</p>
      )}

      {isLoading ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : rows.length === 0 ? (
        <Card><CardContent className="py-10 text-center text-sm text-muted-foreground">
          No registries yet. Register a Docker registry to enable image pulls from outside your local network.
        </CardContent></Card>
      ) : (
        <Table>
          <TableHeader><TableRow>
            <TableHead>Host</TableHead><TableHead>Auth</TableHead>
            <TableHead>Principal</TableHead><TableHead>Added</TableHead>
            <TableHead className="text-right">Actions</TableHead>
          </TableRow></TableHeader>
          <TableBody>
            {rows.map((row) => <RegistryRow key={row.id} row={row} />)}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

function RegistryRow({ row }: { row: RegistryCredentialSummary }) {
  const del = useDeleteRegistry();
  return (
    <TableRow>
      <TableCell className="font-mono text-sm">{row.registry_host}</TableCell>
      <TableCell>
        <Badge variant="outline">{row.auth_kind === 'static' ? 'static' : 'workload identity'}</Badge>
      </TableCell>
      <TableCell className="font-mono text-xs text-muted-foreground">{row.auth_principal || '—'}</TableCell>
      <TableCell className="font-mono text-xs text-muted-foreground"
        title={new Date(row.created_at).toLocaleString()}>
        {timeAgo(row.created_at)}
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end">
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" disabled={del.isPending}>Remove</Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Remove {row.registry_host}?</AlertDialogTitle>
                <AlertDialogDescription>
                  Sessions can no longer pull images from this registry. The sealed credential is deleted.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={() => del.mutate(row.registry_host)}>Remove registry</AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {del.error && <p className="mt-1 text-right text-xs text-destructive">could not remove — {String(del.error)}</p>}
      </TableCell>
    </TableRow>
  );
}

interface AuthKindCardSpec {
  kind: RegistryAuthKind | 'aws_instance_role';
  label: string;
  blurb: string;
  disabled?: boolean;
  hint?: string;
}

const KIND_CARDS: AuthKindCardSpec[] = [
  {
    kind: 'static',
    label: 'Static',
    blurb: 'Username + password sealed under the deployment KEK. DockerHub, GHCR, Quay, Harbor, GAR with a service-account JSON key.',
  },
  {
    kind: 'gcp_workload_identity',
    label: 'GCP Workload Identity',
    blurb: 'Ambient GCP identity exchanged for a short-lived token per pull. No stored secret material.',
  },
  {
    kind: 'aws_instance_role',
    label: 'AWS Instance Role',
    blurb: 'Ambient AWS IAM identity exchanged for an ECR token per pull. Same shape as GCP WI.',
    disabled: true,
    hint: 'coming soon',
  },
];

function AddRegistryDialog() {
  const [open, setOpen] = useState(false);
  const [host, setHost] = useState('');
  const [authKind, setAuthKind] = useState<RegistryAuthKind>('static');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [impersonateSa, setImpersonateSa] = useState('');
  const [submitError, setSubmitError] = useState<string | null>(null);
  const add = useAddRegistry();

  const submit = async () => {
    setSubmitError(null);
    if (!host.trim()) { setSubmitError('host is required'); return; }
    let auth: AddRegistryAuth;
    if (authKind === 'static') {
      if (!username.trim()) { setSubmitError('username is required'); return; }
      if (!password) { setSubmitError('password is required'); return; }
      auth = { kind: 'static', username: username.trim(), password };
    } else {
      auth = { kind: 'gcp_workload_identity', impersonate_sa: impersonateSa.trim() || undefined };
    }
    try {
      await add.mutateAsync({ host: host.trim(), auth });
      setOpen(false);
      setHost(''); setUsername(''); setPassword(''); setImpersonateSa('');
    } catch (e) {
      setSubmitError(String(e));
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild><Button>Register a new registry</Button></DialogTrigger>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>New registry</DialogTitle>
          <DialogDescription>
            Passwords are sealed under the deployment KEK before they touch Postgres; never returned by the API.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-5">
          <div className="space-y-2">
            <Label htmlFor="reg-host">Host</Label>
            <Input id="reg-host" autoFocus value={host} onChange={(e) => setHost(e.target.value)}
              className="font-mono" placeholder="ghcr.io" spellCheck={false} autoCapitalize="off" />
            <p className="text-xs text-muted-foreground">
              e.g. ghcr.io · gcr.io · us-east1-docker.pkg.dev · localhost:5001
            </p>
          </div>

          <div className="space-y-2">
            <Label>Auth model</Label>
            <div role="radiogroup" aria-label="Authentication model" className="grid gap-2">
              {KIND_CARDS.map((card) => {
                const selected = !card.disabled && card.kind === authKind;
                return (
                  <button
                    key={card.kind}
                    type="button"
                    role="radio"
                    aria-checked={selected}
                    disabled={card.disabled}
                    onClick={() => { if (!card.disabled) setAuthKind(card.kind as RegistryAuthKind); }}
                    className={cn(
                      'rounded-md border p-3 text-left transition-colors',
                      selected ? 'border-primary bg-accent' : 'border-border',
                      card.disabled ? 'cursor-not-allowed opacity-55' : 'cursor-pointer hover:bg-accent/50',
                    )}
                  >
                    <div className="flex items-center gap-2">
                      <span className="text-sm font-medium">{card.label}</span>
                      {card.hint && (
                        <Badge variant="secondary" className="ml-auto text-[0.62rem] uppercase">{card.hint}</Badge>
                      )}
                    </div>
                    <p className="mt-1 text-xs text-muted-foreground">{card.blurb}</p>
                  </button>
                );
              })}
            </div>
          </div>

          {authKind === 'static' ? (
            <div className="space-y-4">
              <div className="space-y-2">
                <Label htmlFor="reg-username">Username</Label>
                <Input id="reg-username" value={username} onChange={(e) => setUsername(e.target.value)}
                  className="font-mono" spellCheck={false} autoCapitalize="off" autoComplete="username"
                  placeholder="username or _json_key" />
                <p className="text-xs text-muted-foreground">
                  For GCP service-account JSON keys, the literal string <code className="font-mono">_json_key</code>.
                </p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="reg-password">Password</Label>
                <Input id="reg-password" type="password" value={password} onChange={(e) => setPassword(e.target.value)}
                  className="font-mono" autoComplete="new-password" placeholder="•••••" />
              </div>
            </div>
          ) : (
            <div className="space-y-4">
              <p className="text-sm text-muted-foreground">
                No password required. The host-agent's ambient GCP identity is exchanged for a short-lived
                OAuth token on every pull.
              </p>
              <div className="space-y-2">
                <Label htmlFor="reg-impersonate">Impersonate (optional)</Label>
                <Input id="reg-impersonate" value={impersonateSa} onChange={(e) => setImpersonateSa(e.target.value)}
                  className="font-mono" placeholder="engram@my-project.iam.gserviceaccount.com"
                  spellCheck={false} autoCapitalize="off" />
                <p className="text-xs text-muted-foreground">
                  Pull as a different service account via the IAM Credentials API. Leave empty to use the ambient identity.
                </p>
              </div>
            </div>
          )}

          {submitError && <p className="text-sm text-destructive">{submitError}</p>}
        </div>

        <DialogFooter>
          <Button variant="ghost" onClick={() => setOpen(false)} disabled={add.isPending}>Cancel</Button>
          <Button onClick={submit} disabled={add.isPending}>
            {add.isPending ? 'Sealing & saving…' : 'Register'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function timeAgo(iso: string): string {
  const then = new Date(iso).getTime();
  const now = Date.now();
  if (Number.isNaN(then)) return iso;
  const seconds = Math.max(0, Math.floor((now - then) / 1000));
  if (seconds < 60) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(iso).toLocaleDateString();
}
