import type { ReactNode } from 'react';

// Shared form primitives for the settings panels. Mirrors the
// SubHead/Field shapes used inside NewSessionForm. They live here
// (rather than as exports off NewSessionForm) because the settings
// surface is going to grow and a focused helper module keeps the
// session-create form's locals from leaking into reusable surface.

export function SubHead({ children }: { children: ReactNode }) {
  return (
    <p
      className="font-mono smallcaps text-[0.65rem]"
      style={{ color: 'var(--color-ink-quiet)', letterSpacing: '0.18em' }}
    >
      {children}
    </p>
  );
}

export function Field({
  label,
  children,
  hint,
}: {
  label: string;
  children: ReactNode;
  hint?: ReactNode;
}) {
  return (
    <div>
      <label
        className="grid items-baseline gap-x-4"
        style={{ gridTemplateColumns: '7rem 1fr' }}
      >
        <span
          className="font-mono smallcaps text-[0.7rem]"
          style={{
            color: 'var(--color-ink-quiet)',
            letterSpacing: '0.12em',
          }}
        >
          {label}
        </span>
        {children}
      </label>
      {hint && (
        <p
          className="font-display italic text-[0.78rem] mt-1"
          style={{
            marginLeft: '7rem',
            paddingLeft: '1rem',
            color: 'var(--color-ink-quiet)',
          }}
        >
          {hint}
        </p>
      )}
    </div>
  );
}

/** A pill-less button styled as typeset prose: italic display face,
 * underline on hover, ink color at rest. The "primary" tone uses
 * the amber accent for write actions; "ghost" stays in ink for
 * secondary / cancel. */
export function PressButton({
  children,
  onClick,
  type = 'button',
  tone = 'ghost',
  disabled = false,
}: {
  children: ReactNode;
  onClick?: () => void;
  type?: 'button' | 'submit';
  tone?: 'primary' | 'ghost' | 'danger';
  disabled?: boolean;
}) {
  const color =
    tone === 'primary'
      ? 'var(--color-amber)'
      : tone === 'danger'
      ? 'var(--color-amber)'
      : 'var(--color-ink)';
  return (
    <button
      type={type}
      onClick={onClick}
      disabled={disabled}
      className="font-display italic transition-opacity"
      style={{
        color,
        opacity: disabled ? 0.4 : 1,
        cursor: disabled ? 'not-allowed' : 'pointer',
        background: 'none',
        border: 0,
        padding: 0,
        textUnderlineOffset: '0.18em',
        textDecorationThickness: '1px',
        fontSize: '0.95rem',
      }}
      onMouseEnter={(e) => {
        if (!disabled) {
          (e.currentTarget as HTMLElement).style.textDecoration = 'underline';
        }
      }}
      onMouseLeave={(e) => {
        (e.currentTarget as HTMLElement).style.textDecoration = 'none';
      }}
    >
      {children}
    </button>
  );
}

/** Inline error message — italic, amber. Used by every settings
 * form on submit failure. */
export function FormError({ message }: { message: string }) {
  return (
    <p
      className="font-display italic text-[0.88rem]"
      style={{ color: 'var(--color-amber)' }}
      role="alert"
    >
      {message}
    </p>
  );
}
