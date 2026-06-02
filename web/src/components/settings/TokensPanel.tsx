// ADR 0031 user setting: "My Claude Code OAuth token". Saved once, sealed
// server-side under the deployment KEK, and auto-injected into every built-in
// Claude session — so we never prompt per session. The token is write-only:
// the API never returns it, we only know whether one is saved
// (`principal.has_claude_token`). This is also the screen the create-session
// flow routes to when a built-in-Claude image is selected with no token saved.

import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { saveClaudeToken } from '../../api';
import { useAuth } from '../../auth/AuthProvider';
import { Field, FormError, PressButton, SubHead } from './_form';

export function TokensPanel() {
  const { principal, refresh } = useAuth();
  const queryClient = useQueryClient();
  const [token, setToken] = useState('');

  const save = useMutation({
    mutationFn: (t: string) => saveClaudeToken(t),
    onSuccess: () => {
      setToken('');
      // Flip has_claude_token → unblocks the create-session button.
      void queryClient.invalidateQueries({ queryKey: ['me'] });
      refresh();
    },
  });

  const trimmed = token.trim();

  return (
    <section>
      <header className="mb-6 flex items-baseline justify-between">
        <h2
          className="font-mono smallcaps text-[0.7rem]"
          style={{ color: 'var(--color-ink-quiet)', letterSpacing: '0.18em' }}
        >
          Claude Code token
        </h2>
        <p
          className="font-display italic text-[0.8rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          saved once · sealed · auto-used for every session
        </p>
      </header>

      <p
        className="font-display italic text-[0.92rem] mb-6"
        style={{ color: 'var(--color-ink-faded)', maxWidth: '40rem' }}
      >
        {principal.has_claude_token
          ? 'A token is saved. Built-in Claude sessions use it automatically — you’ll never be prompted. Paste a new one below to replace it.'
          : 'Save your Claude Code OAuth token to launch built-in Claude sessions. It’s sealed under the deployment key and used automatically — you’re never prompted per session.'}
      </p>

      <form
        className="space-y-5"
        style={{ maxWidth: '40rem' }}
        onSubmit={(e) => {
          e.preventDefault();
          if (trimmed) save.mutate(trimmed);
        }}
      >
        <Field
          label="OAuth token"
          hint="From `claude setup-token`. Stored encrypted; never shown again."
        >
          <input
            type="password"
            value={token}
            autoComplete="off"
            spellCheck={false}
            placeholder={principal.has_claude_token ? '•••••••• (saved)' : 'sk-ant-oat…'}
            onChange={(e) => setToken(e.target.value)}
            className="font-mono text-[0.85rem] bg-transparent outline-none"
            style={{
              borderBottom: '1px solid var(--color-rule)',
              color: 'var(--color-ink)',
              padding: '0.15rem 0',
            }}
          />
        </Field>

        <div className="flex items-center gap-6">
          <PressButton
            type="submit"
            tone="primary"
            disabled={!trimmed || save.isPending}
          >
            {save.isPending
              ? 'sealing & saving…'
              : principal.has_claude_token
              ? 'replace token'
              : 'save token'}
          </PressButton>
          {save.isSuccess && (
            <SubHead>saved · sessions will use it</SubHead>
          )}
        </div>

        {save.isError && (
          <FormError message={`could not save — ${(save.error as Error).message}`} />
        )}
      </form>
    </section>
  );
}
