import { useHosts } from '../hooks/useHosts';
import { useSessions } from '../hooks/useSessions';
import { VitalSigns } from '../components/VitalSigns';
import { HostManifest } from '../components/HostManifest';
import { SessionManifest } from '../components/SessionManifest';

export function Overview() {
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions();

  return (
    <main className="book py-12">
      <Header />
      <VitalSigns hosts={hosts} sessions={sessions} />
      <HostManifest hosts={hosts} />
      <SessionManifest sessions={sessions} />
      <Footer />
    </main>
  );
}

function Header() {
  const now = new Date().toLocaleTimeString('en-GB', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
  return (
    <header className="mb-10">
      <h1
        className="font-display"
        style={{
          fontSize: '3.2rem',
          fontWeight: 400,
          letterSpacing: '-0.02em',
          lineHeight: 1.05,
          fontStyle: 'italic',
        }}
      >
        engrams
      </h1>
      <p
        className="font-mono smallcaps text-[0.7rem] mt-2"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        a live record · refreshing every second · {now}
      </p>
      <hr className="mt-6" />
    </header>
  );
}

function Footer() {
  return (
    <footer
      className="mt-20 mb-4 font-mono text-[0.7rem] smallcaps"
      style={{ color: 'var(--color-ink-quiet)' }}
    >
      <hr className="mb-6" />
      coordinator at 127.0.0.1:8090 · vite proxy
    </footer>
  );
}
