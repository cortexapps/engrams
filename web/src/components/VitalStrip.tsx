import type { HostView, Session } from '../types';

// One hairline-ruled row of vital signs for the Sessions surface:
// active · idle · hosts · snapshots, each a tabular value + a mono
// small-caps label. This intentionally demotes the old big
// stat-figures from the Overview — the machine-side counts have moved
// to their own surfaces (Fleet, Storage); here the driver just wants a
// glanceable pulse.

export function VitalStrip({
  hosts,
  sessions,
}: {
  hosts: HostView[] | undefined;
  sessions: Session[] | undefined;
}) {
  const s = sessions ?? [];
  const h = hosts ?? [];
  const count = (st: Session['status']) => s.filter((x) => x.status === st).length;
  const snapshots = h.reduce((acc, host) => acc + host.local_snapshots, 0);

  const items: [string, number][] = [
    ['active', count('active')],
    ['idle', count('idle')],
    ['hosts', h.length],
    ['snapshots', snapshots],
  ];

  return (
    <div className="vital-strip">
      {items.map(([label, value]) => (
        <span key={label} className="vital-cell">
          <span className="vital-num" data-tabular>
            {value}
          </span>
          <span className="vital-lbl">{label}</span>
        </span>
      ))}
    </div>
  );
}
