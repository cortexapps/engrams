import { AnimatePresence, motion } from 'framer-motion';
import { useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { logout } from '../api';
import { useAuth } from '../auth/AuthProvider';

// The profile menu (ADR 0031). The chip shows the signed-in user's initial
// embossed like a typesetter's mark (not a gradient SaaS avatar); click
// reveals a popover with their display name + email, a link to *user*
// settings, and a working "Sign out".
//
// Two placements: `inline` sits in the nav spine's right slot (the
// four-surface IA); the default keeps the legacy fixed top-right corner for
// any page rendered outside the spine.

export function UserChip({ inline = false }: { inline?: boolean }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);
  const { principal } = useAuth();
  const initial = (principal.display_name || principal.email).charAt(0).toUpperCase();

  // Dismiss on outside click + Escape.
  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (!ref.current) return;
      if (!ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onDocClick);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDocClick);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  return (
    <div
      ref={ref}
      className={
        inline ? 'relative select-none' : 'fixed top-5 right-5 z-50 select-none'
      }
      style={{ fontFamily: 'var(--font-display)' }}
    >
      <button
        type="button"
        aria-label="Open user menu"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="grid place-items-center transition-colors duration-200"
        style={{
          width: '2.1rem',
          height: '2.1rem',
          backgroundColor: 'var(--color-paper-warm)',
          border: `1px solid ${open ? 'var(--color-ink)' : 'var(--color-rule)'}`,
          color: 'var(--color-ink-faded)',
          fontSize: '1.05rem',
          lineHeight: 1,
          letterSpacing: 0,
          cursor: 'pointer',
        }}
      >
        {/* The user's initial, set in the display face to match the
            notebook aesthetic. */}
        <span style={{ fontStyle: 'italic', transform: 'translateY(-1px)' }}>
          {initial}
        </span>
      </button>

      <AnimatePresence>
        {open && (
          <motion.div
            key="popover"
            role="menu"
            initial={{ opacity: 0, y: -4 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -4 }}
            transition={{ duration: 0.18, ease: 'easeOut' }}
            className="absolute right-0 mt-2"
            style={{
              minWidth: '14rem',
              backgroundColor: 'var(--color-paper)',
              border: '1px solid var(--color-rule)',
              boxShadow: '0 6px 20px -10px rgba(27, 22, 18, 0.25)',
            }}
          >
            <div className="px-4 pt-3 pb-3">
              <div
                className="font-display text-[0.95rem]"
                style={{ color: 'var(--color-ink)' }}
              >
                {principal.display_name || principal.email}
              </div>
              <div
                className="font-mono text-[0.76rem] mt-1"
                style={{ color: 'var(--color-ink-quiet)' }}
              >
                {principal.email}
              </div>
            </div>
            <hr />
            <ul className="py-1">
              <li>
                <Link
                  to="/settings/profile"
                  onClick={() => setOpen(false)}
                  className="block px-4 py-2 transition-colors"
                  style={{
                    color: 'var(--color-ink)',
                    fontSize: '0.92rem',
                  }}
                  onMouseEnter={(e) => {
                    (e.currentTarget as HTMLElement).style.backgroundColor =
                      'var(--color-paper-warm)';
                  }}
                  onMouseLeave={(e) => {
                    (e.currentTarget as HTMLElement).style.backgroundColor =
                      'transparent';
                  }}
                >
                  Settings
                </Link>
              </li>
              {/* ADR 0031: sign-out is only meaningful in OIDC mode (an
                  app-owned session cookie to revoke). Behind an edge proxy
                  (IAP) or in dev synthetic-admin, every request is
                  re-authenticated upstream, so hide the no-op control. */}
              {principal.can_sign_out && (
                <li>
                  <button
                    type="button"
                    onClick={() => {
                      setOpen(false);
                      void logout();
                    }}
                    className="block w-full text-left px-4 py-2 italic transition-colors"
                    style={{
                      color: 'var(--color-ink)',
                      fontSize: '0.92rem',
                      background: 'none',
                      border: 0,
                      cursor: 'pointer',
                    }}
                    onMouseEnter={(e) => {
                      (e.currentTarget as HTMLElement).style.backgroundColor =
                        'var(--color-paper-warm)';
                    }}
                    onMouseLeave={(e) => {
                      (e.currentTarget as HTMLElement).style.backgroundColor =
                        'transparent';
                    }}
                  >
                    Sign out
                  </button>
                </li>
              )}
            </ul>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
