import { AnimatePresence, motion } from 'framer-motion';
import { useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';

// "Inkstamp" — the section-sign rune (§) embossed like a typesetter's
// mark, not the gradient avatar of a generic SaaS. Click reveals a
// small popover anchored beneath it.
//
// Two placements: `inline` sits in the nav spine's right slot (the
// four-surface IA); the default keeps the legacy fixed top-right
// corner for any page rendered outside the spine.
//
// Auth isn't wired yet, so the popover just identifies the
// deployment ("Local development") and links to /settings. When a
// real user identity lands, the rune gets replaced with the user's
// initial and the popover gains email + "Sign out".

export function UserChip({ inline = false }: { inline?: boolean }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);

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
        {/* §  the section sign — a typesetter's mark, fits the
             notebook aesthetic without committing to an identity yet. */}
        <span style={{ fontStyle: 'italic', transform: 'translateY(-1px)' }}>
          §
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
                className="font-mono smallcaps text-[0.66rem]"
                style={{
                  color: 'var(--color-ink-quiet)',
                  letterSpacing: '0.18em',
                }}
              >
                Local development
              </div>
              <div
                className="font-display italic text-[0.85rem] mt-1"
                style={{ color: 'var(--color-ink-faded)' }}
              >
                no auth wired
              </div>
            </div>
            <hr />
            <ul className="py-1">
              <li>
                <Link
                  to="/settings"
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
              <li
                aria-disabled
                className="block px-4 py-2 italic cursor-not-allowed"
                style={{
                  color: 'var(--color-ink-quiet)',
                  fontSize: '0.92rem',
                }}
                title="Auth not wired in this deployment"
              >
                Sign out
              </li>
            </ul>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
