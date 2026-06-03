// ADR 0031 redesign: a per-service token ledger (not a single-service form).
// Each row: service name + italic "what it's for" left; status + actions right.
// Saved rows → replace / remove. Unsaved → "add →" reveals an inline password
// field + hint + save / cancel. Tokens are sealed on save and never shown again.

import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { saveClaudeToken, saveGithubToken, deleteToken } from '../../api';
import { useAuth } from '../../auth/AuthProvider';

interface Service {
  id: 'claude' | 'github';
  name: string;
  use: string;
  hint: string;
  placeholder: string;
}

const SERVICES: Service[] = [
  {
    id: 'claude',
    name: 'Claude Code',
    use: 'built-in Claude sessions authenticate with this',
    hint: 'from `claude setup-token` — stored encrypted, never shown again',
    placeholder: 'sk-ant-oat…',
  },
  {
    id: 'github',
    name: 'GitHub',
    use: 'clone private repositories into a session',
    hint: 'a fine-grained PAT with repo scope',
    placeholder: 'github_pat_…',
  },
];

export function TokensPanel() {
  const { principal, refresh } = useAuth();
  const queryClient = useQueryClient();

  // Which service row is in the editing state.
  const [editing, setEditing] = useState<'claude' | 'github' | null>(null);

  // Local "github saved" state (server doesn't surface it on /me yet).
  const [githubSaved, setGithubSaved] = useState(false);

  const savedState: Record<string, boolean> = {
    claude: principal.has_claude_token,
    github: githubSaved,
  };

  const saveMutation = useMutation({
    mutationFn: async ({ id, token }: { id: 'claude' | 'github'; token: string }) => {
      if (id === 'claude') {
        await saveClaudeToken(token);
      } else {
        await saveGithubToken(token);
      }
      return id;
    },
    onSuccess: (id) => {
      setEditing(null);
      if (id === 'claude') {
        void queryClient.invalidateQueries({ queryKey: ['me'] });
        refresh();
      } else {
        setGithubSaved(true);
      }
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (id: 'claude' | 'github') => deleteToken(id),
    onSuccess: (_, id) => {
      if (id === 'claude') {
        void queryClient.invalidateQueries({ queryKey: ['me'] });
        refresh();
      } else {
        setGithubSaved(false);
      }
    },
  });

  return (
    <section style={{ maxWidth: '46rem' }}>
      <header className="mb-6 flex items-baseline justify-between">
        <h2 className="section-label">Tokens</h2>
        <p className="font-display italic text-[0.8rem]" style={{ color: 'var(--color-ink-quiet)' }}>
          sealed · auto-used per session
        </p>
      </header>

      <div className="tokens-ledger">
        {SERVICES.map((svc) => (
          <ServiceRow
            key={svc.id}
            svc={svc}
            isSaved={savedState[svc.id] ?? false}
            isEditing={editing === svc.id}
            isMutating={saveMutation.isPending || deleteMutation.isPending}
            onEdit={() => setEditing(svc.id)}
            onCancel={() => setEditing(null)}
            onSave={(token) => saveMutation.mutate({ id: svc.id, token })}
            onRemove={() => deleteMutation.mutate(svc.id)}
          />
        ))}
      </div>

      <p className="ledger-note">
        every token is sealed under the deployment key the moment you save it —
        the plaintext never touches Postgres, and it's used automatically so
        you're never prompted per session. new services land here as their own row.
      </p>
    </section>
  );
}

function ServiceRow({
  svc,
  isSaved,
  isEditing,
  isMutating,
  onEdit,
  onCancel,
  onSave,
  onRemove,
}: {
  svc: Service;
  isSaved: boolean;
  isEditing: boolean;
  isMutating: boolean;
  onEdit: () => void;
  onCancel: () => void;
  onSave: (token: string) => void;
  onRemove: () => void;
}) {
  const [token, setToken] = useState('');
  const trimmed = token.trim();

  const handleSave = () => {
    if (trimmed) {
      onSave(trimmed);
      setToken('');
    }
  };
  const handleCancel = () => {
    setToken('');
    onCancel();
  };

  return (
    <div className="token-row">
      <div className="token-svc">
        <span className="token-name">{svc.name}</span>
        <span className="token-use">{svc.use}</span>
        {isEditing && (
          <div className="token-edit">
            <input
              type="password"
              className="ledger-input font-mono"
              autoFocus
              placeholder={svc.placeholder}
              value={token}
              onChange={(e) => setToken(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') handleSave();
                if (e.key === 'Escape') handleCancel();
              }}
              style={{ fontSize: '0.85rem' }}
            />
            <span className="token-hint">{svc.hint}</span>
            <div className="token-edit-actions">
              <button
                type="button"
                className="members-act"
                disabled={!trimmed || isMutating}
                onClick={handleSave}
                style={{ opacity: !trimmed || isMutating ? 0.4 : 1 }}
              >
                save
              </button>
              <button
                type="button"
                className="members-act act-quiet"
                onClick={handleCancel}
              >
                cancel
              </button>
            </div>
          </div>
        )}
      </div>

      <span className={`token-status${isSaved ? ' is-saved' : ''}`}>
        {isSaved ? 'saved · sealed' : 'not connected'}
      </span>

      <div className="token-row-actions">
        {isSaved ? (
          <>
            <button type="button" className="members-act" onClick={onEdit}>
              replace
            </button>
            <button type="button" className="members-act act-quiet" onClick={onRemove}>
              remove
            </button>
          </>
        ) : (
          <button type="button" className="members-act act-primary" onClick={onEdit}>
            add →
          </button>
        )}
      </div>
    </div>
  );
}
