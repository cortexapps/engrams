import { useNavigate } from '@tanstack/react-router';
import { useHosts } from '../../hooks/useHosts';
import { useSessions } from '../../hooks/useSessions';
import { useAuth } from '../../auth/AuthProvider';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { NewSessionDialog } from '../../components/NewSessionDialog';
import { SessionsTable } from './sessions-columns';

export function MySessions() {
  const { principal } = useAuth();
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions('mine');
  const navigate = useNavigate();
  const all = sessions ?? [];
  const showTokenNudge = !principal.is_admin && !principal.has_claude_token;

  const stats: [string, number][] = [
    ['Active', all.filter((s) => s.status === 'active').length],
    ['Idle', all.filter((s) => s.status === 'idle').length],
    ['Hosts', (hosts ?? []).length],
    ['Snapshots', (hosts ?? []).reduce((a, h) => a + h.local_snapshots, 0)],
  ];

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Sessions</h1>
          <p className="text-sm text-muted-foreground">Bounded units of agent work — launch, watch, resume.</p>
        </div>
        <NewSessionDialog onCreated={(id) => navigate({ to: '/sessions/$id', params: { id } })} />
      </div>

      {showTokenNudge && (
        <Card>
          <CardContent className="flex items-center justify-between gap-4 py-3">
            <span className="text-sm">No Claude Code token saved yet — built-in Claude sessions need one.</span>
            <Button variant="secondary" size="sm" onClick={() => navigate({ to: '/settings/tokens' })}>
              Add token
            </Button>
          </CardContent>
        </Card>
      )}

      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        {stats.map(([label, value]) => (
          <Card key={label}><CardContent className="py-4">
            <div className="font-mono text-2xl tabular-nums">{value}</div>
            <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
          </CardContent></Card>
        ))}
      </div>

      <SessionsTable sessions={all} showOwner={false}
        emptyText='No sessions yet — start one with "New session".' />
    </div>
  );
}
