import { Outlet } from 'react-router-dom';
import { NavSpine } from '../components/NavSpine';
import { useSessions } from '../hooks/useSessions';

// The shell shared by every surface: the sticky nav spine on top, the
// active surface in the middle (via <Outlet/>), a quiet footer at the
// foot. The sessions poll lives here so the masthead mark can render
// live status globally — it loops while anything is booting and
// strikes once per refetch tick otherwise.

export function Layout() {
  const { data: sessions, dataUpdatedAt } = useSessions();
  const booting = (sessions ?? []).some((s) => s.status === 'created');

  return (
    <>
      <NavSpine booting={booting} tick={dataUpdatedAt} />
      <Outlet />
      <Footer />
    </>
  );
}

function Footer() {
  return (
    <footer
      className="book-wide mt-20 mb-4 font-mono text-[0.7rem] smallcaps"
      style={{ color: 'var(--color-ink-quiet)' }}
    >
      <hr className="mb-6" />
      coordinator at 127.0.0.1:8090 · vite proxy
    </footer>
  );
}
