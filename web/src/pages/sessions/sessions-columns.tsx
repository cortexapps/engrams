import { Link, useNavigate } from '@tanstack/react-router';
import { Badge } from '@/components/ui/badge';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table';
import type { SessionListItem } from '../../types';
import {
  lifecycleOf, relativeTime, shortId, statusVariant, stripImageHost, type Lifecycle,
} from './session-format';

const ORDER: Lifecycle[] = ['ACTIVE', 'IDLE — RESUMABLE', 'ARCHIVED'];

export function SessionsTable({
  sessions, showOwner, emptyText,
}: {
  sessions: SessionListItem[];
  showOwner: boolean;
  emptyText: string;
}) {
  if (sessions.length === 0) {
    return <p className="py-8 text-sm text-muted-foreground">{emptyText}</p>;
  }
  const groups = ORDER.map((g) => ({
    group: g,
    rows: sessions
      .filter((s) => lifecycleOf(s.status) === g)
      .sort((a, b) => new Date(b.last_active_at).getTime() - new Date(a.last_active_at).getTime()),
  })).filter((g) => g.rows.length > 0 || g.group === 'ACTIVE');

  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>Session</TableHead>
          <TableHead>Image</TableHead>
          <TableHead>Status</TableHead>
          {showOwner && <TableHead>Owner</TableHead>}
          <TableHead className="text-right">Age</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {groups.map(({ group, rows }) => (
          <GroupBlock key={group} group={group} rows={rows} showOwner={showOwner} />
        ))}
      </TableBody>
    </Table>
  );
}

function GroupBlock({ group, rows, showOwner }: {
  group: Lifecycle; rows: SessionListItem[]; showOwner: boolean;
}) {
  const navigate = useNavigate();
  const cols = showOwner ? 5 : 4;
  return (
    <>
      <TableRow className="hover:bg-transparent">
        <TableCell colSpan={cols} className="border-y bg-muted/30 py-1.5 font-mono text-[0.65rem] uppercase tracking-[0.14em] text-muted-foreground">
          {group} <span className="text-muted-foreground/60">·</span> {rows.length}
        </TableCell>
      </TableRow>
      {rows.length === 0 ? (
        <TableRow className="hover:bg-transparent"><TableCell colSpan={cols} className="text-sm italic text-muted-foreground">none</TableCell></TableRow>
      ) : rows.map((s) => (
        <TableRow
          key={s.id}
          className="cursor-pointer hover:bg-accent/60"
          onClick={() => navigate({ to: '/sessions/$id', params: { id: s.id } })}
        >
          <TableCell className="font-mono text-sm">
            <Link
              to="/sessions/$id"
              params={{ id: s.id }}
              className="hover:underline"
              onClick={(e) => e.stopPropagation()}
            >
              {shortId(s.id)}
            </Link>
          </TableCell>
          <TableCell className="text-muted-foreground">{stripImageHost(s.image)}</TableCell>
          <TableCell><Badge variant={statusVariant(s.status)}>{s.status}</Badge></TableCell>
          {showOwner && (
            <TableCell>
              {s.owner_kind === 'system' ? (
                <span className="text-sm text-muted-foreground italic">system</span>
              ) : (
                <span className="flex items-center gap-2">
                  <Avatar className="size-5"><AvatarFallback className="text-[10px]">
                    {(s.owner_name || s.owner_email || '?').charAt(0).toUpperCase()}
                  </AvatarFallback></Avatar>
                  <span className="font-mono text-xs text-muted-foreground">{s.owner_email}</span>
                </span>
              )}
            </TableCell>
          )}
          <TableCell className="text-right font-mono text-xs text-muted-foreground">
            {relativeTime(s.last_active_at)}
          </TableCell>
        </TableRow>
      ))}
    </>
  );
}
