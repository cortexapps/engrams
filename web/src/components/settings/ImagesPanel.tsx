import { useState } from 'react';
import {
  useDisableImage,
  useEnableImage,
  useEnabledImages,
  useRefreshEnabledImage,
} from '../../hooks/useEnabledImages';
import { useEnableProgress } from '../../hooks/useEnableProgress';
import type { EnabledImageSummary } from '../../types';
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

// Enabled-images panel — operators curate the OCI URIs sessions may reference.
// The list reads `GET /api/enabled-images` (Postgres-backed); enable hits the
// registry to fetch + cache the manifest.toml, so session-create has zero
// registry I/O on the hot path.
export function ImagesPanel() {
  const { data, isLoading, error } = useEnabledImages();
  const rows = data ?? [];

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Images</h1>
          <p className="text-sm text-muted-foreground">
            OCI URIs sessions may reference. The manifest is cached on enable; refresh when tags move.
          </p>
        </div>
        <EnableImageDialog />
      </div>

      {error && (
        <p className="text-sm text-destructive">could not load enabled images — {String(error)}</p>
      )}

      {isLoading ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : rows.length === 0 ? (
        <Card><CardContent className="py-10 text-center text-sm text-muted-foreground">
          No images enabled. Bake + push an image with{' '}
          <code className="font-mono">engram image build --push</code>, then enable its URI here.
        </CardContent></Card>
      ) : (
        <Table>
          <TableHeader><TableRow>
            <TableHead>Image</TableHead><TableHead>Manifest</TableHead>
            <TableHead>Digest</TableHead><TableHead>Refreshed</TableHead>
            <TableHead className="text-right">Actions</TableHead>
          </TableRow></TableHeader>
          <TableBody>
            {rows.map((row) => <ImageRow key={row.id} row={row} />)}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

function ImageRow({ row }: { row: EnabledImageSummary }) {
  const del = useDisableImage();
  const refresh = useRefreshEnabledImage();
  const shortDigest = row.manifest_digest.length > 19
    ? `${row.manifest_digest.slice(0, 19)}…` : row.manifest_digest;

  return (
    <TableRow>
      <TableCell className="font-mono text-sm whitespace-nowrap">{row.image_uri}</TableCell>
      <TableCell className="text-sm text-muted-foreground">
        {row.manifest_name || '—'}
        {row.manifest_description && (
          <span className="block text-xs">{row.manifest_description}</span>
        )}
      </TableCell>
      <TableCell>
        <Badge variant="outline" className="font-mono text-[0.65rem]" title={row.manifest_digest}>
          {shortDigest}
        </Badge>
      </TableCell>
      <TableCell className="font-mono text-xs text-muted-foreground"
        title={new Date(row.last_refreshed_at).toLocaleString()}>
        {timeAgo(row.last_refreshed_at)}
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={() => refresh.mutate(row.image_uri)} disabled={refresh.isPending}>
            {refresh.isPending ? 'Refreshing…' : 'Refresh'}
          </Button>
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" disabled={del.isPending}>Disable</Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Disable {row.image_uri}?</AlertDialogTitle>
                <AlertDialogDescription>
                  Sessions can no longer reference this URI. The artifact in the registry is untouched.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={() => del.mutate(row.image_uri)}>Disable image</AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {refresh.error && <p className="mt-1 text-right text-xs text-destructive">could not refresh — {String(refresh.error)}</p>}
        {del.error && <p className="mt-1 text-right text-xs text-destructive">could not disable — {String(del.error)}</p>}
      </TableCell>
    </TableRow>
  );
}

function EnableImageDialog() {
  const [open, setOpen] = useState(false);
  const [imageUri, setImageUri] = useState('');
  const [submitError, setSubmitError] = useState<string | null>(null);
  const enable = useEnableImage();
  const progress = useEnableProgress(enable.isPending);

  const submit = async () => {
    setSubmitError(null);
    if (!imageUri.trim()) { setSubmitError('image URI is required'); return; }
    try {
      await enable.mutateAsync(imageUri.trim());
      setImageUri('');
      setOpen(false);
    } catch (err) {
      setSubmitError(String(err));
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild><Button>Enable a new image</Button></DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Enable a new image</DialogTitle>
          <DialogDescription>
            Full OCI reference: <code className="font-mono">&lt;host&gt;[:port]/&lt;repo&gt;:&lt;tag&gt;</code>.
            The coordinator pulls the manifest layer on enable.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-2">
          <Label htmlFor="image-uri">Image URI</Label>
          <Input id="image-uri" autoFocus value={imageUri}
            onChange={(e) => setImageUri(e.target.value)}
            className="font-mono" placeholder="ghcr.io/cortex/api:warm-1"
            spellCheck={false} autoCapitalize="off" />
          {submitError && <p className="text-sm text-destructive">{submitError}</p>}
        </div>
        <DialogFooter className="sm:items-center">
          {progress && (
            <span className="mr-auto text-xs text-muted-foreground">{progress.label}…</span>
          )}
          <Button variant="ghost" onClick={() => setOpen(false)} disabled={enable.isPending}>Cancel</Button>
          <Button onClick={submit} disabled={enable.isPending}>
            {enable.isPending ? 'Enabling…' : 'Enable'}
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
