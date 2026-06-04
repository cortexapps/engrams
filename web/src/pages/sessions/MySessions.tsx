import { useNavigate } from '@tanstack/react-router';
import { useHosts } from '../../hooks/useHosts';
import { useSessions } from '../../hooks/useSessions';
import { useAuth } from '../../auth/AuthProvider';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { NewSessionDialog } from '../../components/NewSessionDialog';
import { PageHeading } from '../../components/page-heading';
import { StatReadout } from '../../components/stat-readout';
import { SessionsTable } from './sessions-columns';

export function MySessions() {
  const { principal } = useAuth();
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions('mine');
  const navigate = useNavigate();
  const all = sessions ?? [];
  const showTokenNudge = !principal.is_admin && !principal.has_claude_token;

  const stats = [
    { label: 'Active', value: all.filter((s) => s.status === 'active').length },
    { label: 'Idle', value: all.filter((s) => s.status === 'idle').length },
    { label: 'Hosts', value: (hosts ?? []).length },
    { label: 'Snapshots', value: (hosts ?? []).reduce((a, h) => a + h.local_snapshots, 0) },
  ];

  return (
    <div className="flex-1 space-y-6 overflow-auto p-4 md:p-6">
      <PageHeading
        title="Sessions"
        description="Bounded units of agent work — launch, watch, resume."
        actions={
          <NewSessionDialog onCreated={(id) => navigate({ to: '/sessions/$id', params: { id } })} />
        }
      />

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

      <StatReadout items={stats} />

      <SessionsTable sessions={all} showOwner={false}
        emptyText='No sessions yet — start one with "New session".' />
    </div>
  );
}
