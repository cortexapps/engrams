import { useHosts } from '../hooks/useHosts';
import { useSessions } from '../hooks/useSessions';
import { useDrainHost } from '../hooks/useDrainHost';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Progress } from '@/components/ui/progress';
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
  AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog';
import type { HostStatus, HostView, Session } from '../types';

const statusVariant = (s: HostStatus) =>
  s === 'ready' ? 'default' : s === 'draining' ? 'secondary' : 'destructive';

export function Fleet() {
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions();
  const drain = useDrainHost();
  const h = hosts ?? [];
  const s = sessions ?? [];
  const totalSb = h.reduce((a, x) => a + x.running_sandboxes, 0);
  const usedGiB = (h.reduce((a, x) => a + x.capacity_used_mib, 0) / 1024).toFixed(0);
  const totGiB = (h.reduce((a, x) => a + x.capacity_total_mib, 0) / 1024).toFixed(0);
  const anyDraining = h.some((x) => x.status === 'draining');
  const liveByHost = (id: string) =>
    s.filter((x: Session) => x.host_id === id && x.status === 'active').length;

  return (
    <div className="space-y-6 p-4 md:p-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Fleet</h1>
        <p className="text-sm text-muted-foreground">Firecracker hosts and capacity.</p>
      </div>

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
        {([['Hosts', h.length], ['Sandboxes', totalSb], ['GiB used', `${usedGiB}/${totGiB}`]] as const).map(
          ([label, value]) => (
            <Card key={label}><CardContent className="py-4">
              <div className="font-mono text-2xl tabular-nums">{value}</div>
              <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
            </CardContent></Card>
          ))}
      </div>

      {h.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No hosts have registered yet. Hosts appear here once they boot and complete their first heartbeat.
        </p>
      ) : (
        <div className="space-y-3">
          {h.map((host) => (
            <HostCard key={host.id} host={host} live={liveByHost(host.id)}
              onDrain={() => drain.mutate(host.id)} draining={drain.isPending} />
          ))}
        </div>
      )}

      <p className="text-sm text-muted-foreground">
        Reconciler {anyDraining ? 'rebalancing — draining host, migrating sandboxes' : 'steady — desired state matches observed'}.
      </p>
    </div>
  );
}

function HostCard({ host, live, onDrain, draining }: {
  host: HostView; live: number; onDrain: () => void; draining: boolean;
}) {
  const pct = host.capacity_total_mib > 0
    ? Math.min(100, Math.round((host.capacity_used_mib / host.capacity_total_mib) * 100)) : 0;
  const totalGiB = (host.capacity_total_mib / 1024).toFixed(0);
  const usedGiB = (host.capacity_used_mib / 1024).toFixed(1);
  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
        <CardTitle className="font-mono text-base">{host.id}</CardTitle>
        <div className="flex items-center gap-3">
          <Badge variant={statusVariant(host.status)}>{host.status}</Badge>
          {host.status === 'ready' && (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="outline" size="sm" disabled={draining}>Drain</Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Drain {host.id}?</AlertDialogTitle>
                  <AlertDialogDescription>
                    The scheduler stops assigning new sessions to this host. In-flight sessions stay put.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={onDrain}>Drain</AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {host.running_sandboxes === 0 ? (
          <p className="text-sm italic text-muted-foreground">no sandboxes</p>
        ) : (
          <div className="flex flex-wrap gap-1" aria-label={`${host.running_sandboxes} sandboxes`}>
            {Array.from({ length: host.running_sandboxes }, (_, i) => (
              <span key={i} className={`size-3 ${i < live ? 'bg-primary' : 'bg-muted-foreground/40'}`} />
            ))}
          </div>
        )}
        <div className="flex items-center gap-3">
          <Progress value={pct} className="h-2" />
          <span className="font-mono text-xs tabular-nums text-muted-foreground">{pct}%</span>
        </div>
        <div className="font-mono text-xs text-muted-foreground">
          {host.running_sandboxes} sandboxes · {usedGiB}/{totalGiB} GiB · {host.local_snapshots} snapshots
        </div>
      </CardContent>
    </Card>
  );
}
