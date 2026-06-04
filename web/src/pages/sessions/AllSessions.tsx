import { useSessions } from '../../hooks/useSessions';
import { PageHeading } from '../../components/page-heading';
import { SessionsTable } from './sessions-columns';

export function AllSessions() {
  const { data: sessions } = useSessions('all');
  return (
    <div className="flex-1 space-y-6 overflow-auto p-4 md:p-6">
      <PageHeading
        title="All sessions"
        description="Every session across the fleet — owner-attributed."
      />
      <SessionsTable sessions={sessions ?? []} showOwner
        emptyText="No active sessions across the fleet." />
    </div>
  );
}
