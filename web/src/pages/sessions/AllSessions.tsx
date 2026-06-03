import { useSessions } from '../../hooks/useSessions';
import { SessionsTable } from './sessions-columns';

export function AllSessions() {
  const { data: sessions } = useSessions('all');
  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">All sessions</h1>
        <p className="text-sm text-muted-foreground">Every session across the fleet — owner-attributed.</p>
      </div>
      <SessionsTable sessions={sessions ?? []} showOwner
        emptyText="No active sessions across the fleet." />
    </div>
  );
}
