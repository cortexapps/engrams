import { useEffect, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { useNavigate } from '@tanstack/react-router';
import { createSession } from '../api';
import { useAuth } from '../auth/AuthProvider';
import { useEnabledImages } from '../hooks/useEnabledImages';
import type { SessionMode } from '../types';
import { Button } from '@/components/ui/button';
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader,
  DialogTitle, DialogTrigger,
} from '@/components/ui/dialog';
import { Label } from '@/components/ui/label';
import { Textarea } from '@/components/ui/textarea';
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select';

export function NewSessionDialog({ onCreated }: { onCreated: (id: string) => void }) {
  const [open, setOpen] = useState(false);
  const { data: images, isLoading } = useEnabledImages(true);
  const { principal } = useAuth();
  const qc = useQueryClient();
  const navigate = useNavigate();

  const [selectedUri, setSelectedUri] = useState('');
  const [mode, setMode] = useState<SessionMode>('agent');
  const [prompt, setPrompt] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const selected = images?.find((i) => i.image_uri === selectedUri);
  useEffect(() => {
    if (!selectedUri && images && images.length > 0) setSelectedUri(images[0].image_uri);
  }, [images, selectedUri]);

  const harnessName = selected?.harness_name ?? null;
  const hasHarness = harnessName !== null;
  const isClaude = harnessName === 'claude';
  const promptMeaningful = hasHarness && mode === 'agent';
  const needsToken = isClaude && mode === 'agent' && !principal.has_claude_token;
  const canSubmit = !!selected && !submitting && !needsToken;

  const submit = async () => {
    if (!canSubmit || !selected) return;
    setSubmitting(true); setError(null);
    try {
      const res = await createSession({
        image: selected.image_uri,
        mode: mode === 'dev_vm' ? 'dev_vm' : undefined,
        prompt: promptMeaningful ? (prompt.trim() || undefined) : undefined,
      });
      qc.invalidateQueries({ queryKey: ['sessions'] });
      setOpen(false); setPrompt('');
      onCreated(res.session_id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally { setSubmitting(false); }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild><Button>New session</Button></DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New session</DialogTitle>
          <DialogDescription>Launch a bounded unit of agent work.</DialogDescription>
        </DialogHeader>

        {isLoading && <p className="text-sm text-muted-foreground">Loading images…</p>}
        {images && images.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No images enabled. Enable one under Settings → Images.
          </p>
        )}

        {images && images.length > 0 && (
          <div className="space-y-4">
            <div className="space-y-2">
              <Label>Image</Label>
              <Select value={selectedUri} onValueChange={setSelectedUri}>
                <SelectTrigger><SelectValue /></SelectTrigger>
                <SelectContent>
                  {images.map((i) => (
                    <SelectItem key={i.image_uri} value={i.image_uri}>
                      {i.image_uri}{i.manifest_name ? ` — ${i.manifest_name}` : ''}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <p className="text-xs text-muted-foreground">
                {harnessName ? `Baked harness: ${harnessName}` : 'No baked harness — shell-only image'}
              </p>
            </div>

            <div className="space-y-2">
              <Label>Mode</Label>
              <Select value={mode} onValueChange={(v) => setMode(v as SessionMode)}>
                <SelectTrigger><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="agent">agent — drive the baked harness</SelectItem>
                  <SelectItem value="dev_vm">dev VM — shell-only</SelectItem>
                </SelectContent>
              </Select>
            </div>

            {promptMeaningful && (
              <div className="space-y-2">
                <Label htmlFor="ns-prompt">Prompt</Label>
                <Textarea id="ns-prompt" rows={2} value={prompt}
                  onChange={(e) => setPrompt(e.target.value)}
                  placeholder="optional opening prompt" />
              </div>
            )}

            {needsToken && (
              <p className="text-sm text-muted-foreground">
                This image runs built-in Claude, which uses your saved token — you don’t have one yet.
              </p>
            )}
            {error && <p className="text-sm text-destructive">{error}</p>}
          </div>
        )}

        <DialogFooter>
          {needsToken ? (
            <Button variant="secondary" onClick={() => { setOpen(false); navigate({ to: '/settings/tokens' }); }}>
              Save your Claude token
            </Button>
          ) : (
            <Button onClick={submit} disabled={!canSubmit}>
              {submitting ? 'Starting…' : 'Start'}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
