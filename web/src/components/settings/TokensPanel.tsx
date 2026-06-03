import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { saveClaudeToken } from '../../api';
import { useAuth } from '../../auth/AuthProvider';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';

export function TokensPanel() {
  const { principal, refresh } = useAuth();
  const qc = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [token, setToken] = useState('');
  const save = useMutation({
    mutationFn: (t: string) => saveClaudeToken(t),
    onSuccess: () => { setEditing(false); setToken(''); void qc.invalidateQueries({ queryKey: ['me'] }); refresh(); },
  });
  const saved = principal.has_claude_token;

  return (
    <div className="max-w-2xl space-y-6">
      <h1 className="text-2xl font-semibold tracking-tight">Tokens</h1>
      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
          <div>
            <CardTitle>Claude Code</CardTitle>
            <p className="text-sm text-muted-foreground">Built-in Claude sessions authenticate with this.</p>
          </div>
          <Badge variant={saved ? 'secondary' : 'outline'}>{saved ? 'saved · sealed' : 'not connected'}</Badge>
        </CardHeader>
        <CardContent className="space-y-3">
          {editing ? (
            <div className="space-y-2">
              <Input type="password" autoFocus placeholder="sk-ant-oat…" value={token}
                onChange={(e) => setToken(e.target.value)} className="font-mono" />
              <p className="text-xs text-muted-foreground">
                From <code className="font-mono">claude setup-token</code> — stored encrypted, never shown again.
              </p>
              <div className="flex gap-2">
                <Button size="sm" disabled={!token.trim() || save.isPending} onClick={() => save.mutate(token.trim())}>
                  {save.isPending ? 'Saving…' : 'Save'}
                </Button>
                <Button size="sm" variant="ghost" onClick={() => { setEditing(false); setToken(''); }}>Cancel</Button>
              </div>
              {save.error && <p className="text-sm text-destructive">{String(save.error)}</p>}
            </div>
          ) : (
            <Button size="sm" variant={saved ? 'outline' : 'default'} onClick={() => setEditing(true)}>
              {saved ? 'Replace' : 'Add token'}
            </Button>
          )}
        </CardContent>
      </Card>
      <p className="max-w-prose text-sm text-muted-foreground">
        Every token is sealed under the deployment key the moment you save it — the plaintext never touches
        Postgres, and it's used automatically so you're never prompted per session.
      </p>
    </div>
  );
}
