// screens.jsx — NewSessionForm, faux TerminalPane, ImagesPanel, and the
// three pages (Overview, SessionDetail, Settings).
const { useState: useS, useEffect: useE, useRef: useR } = React;

// ---- NewSessionForm --------------------------------------------------------
function NSField({ label, children, w = '7rem' }) {
  return (
    <label className="ns-field" style={{ gridTemplateColumns: `${w} 1fr` }}>
      <span className="section-label" style={{ letterSpacing: '0.06em' }}>{label}</span>
      {children}
    </label>
  );
}
function NewSessionForm({ images, onCancel, onCreated }) {
  const [uri, setUri] = useS(images[0]?.image_uri || '');
  const [mode, setMode] = useS('agent');
  const [prompt, setPrompt] = useS('');
  const [authMethod, setAuthMethod] = useS('oauth');
  const [token, setToken] = useS('');
  const [submitting, setSubmitting] = useS(false);
  const selected = images.find((i) => i.image_uri === uri);
  const harness = selected?.harness_name ?? null;
  const isClaude = harness === 'claude';
  const promptMeaningful = harness && mode === 'agent';
  const tokenName = authMethod === 'oauth' ? 'CLAUDE_CODE_OAUTH_TOKEN' : 'ANTHROPIC_API_KEY';
  const tokenMissing = isClaude && mode === 'agent' && token.trim().length === 0;
  const canSubmit = !!selected && !submitting && !tokenMissing;
  const submit = (e) => {
    e.preventDefault();
    if (!canSubmit) return;
    setSubmitting(true);
    setTimeout(() => onCreated(selected.image_uri, mode, promptMeaningful ? prompt.trim() : ''), 520);
  };
  return (
    <section className="ns mb-12">
      <div className="flex items-baseline justify-between mb-4">
        <h2 className="section-label">NEW SESSION</h2>
        <button type="button" onClick={onCancel} className="ns-close section-label">× close</button>
      </div>
      <form onSubmit={submit} className="space-y-6">
        <div className="space-y-3">
          <p className="section-label" style={{ fontSize: '0.65rem' }}>IMAGE</p>
          <NSField label="image">
            <select value={uri} onChange={(e) => setUri(e.target.value)} className="ledger-input" style={{ fontFamily: 'var(--font-display)' }}>
              {images.map((img) => <option key={img.image_uri} value={img.image_uri}>{img.image_uri}{img.manifest_name ? ` — ${img.manifest_name}` : ''}</option>)}
            </select>
          </NSField>
          {selected?.manifest_description && <p className="ns-hint">{selected.manifest_description}</p>}
          <p className="ns-hint">{harness ? `baked harness: ${harness}` : 'no baked harness — shell-only image'}</p>
        </div>
        <div className="space-y-3">
          <p className="section-label" style={{ fontSize: '0.65rem' }}>MODE</p>
          <NSField label="mode">
            <select value={mode} onChange={(e) => setMode(e.target.value)} className="ledger-input" style={{ fontFamily: 'var(--font-display)' }}>
              <option value="agent">agent — drive the image's baked harness</option>
              <option value="dev_vm">dev VM — shell-only, harness (if any) stays undriven</option>
            </select>
          </NSField>
          {promptMeaningful && (
            <NSField label="prompt">
              <textarea value={prompt} onChange={(e) => setPrompt(e.target.value)} rows={2} className="ledger-input"
                placeholder="optional opening prompt" style={{ fontFamily: 'var(--font-display)', resize: 'vertical' }} />
            </NSField>
          )}
        </div>
        {isClaude && mode === 'agent' && (
          <div className="space-y-3" style={{ paddingTop: '0.5rem' }}>
            <p className="section-label" style={{ fontSize: '0.65rem' }}>CLAUDE CREDENTIALS · paste once · never persisted</p>
            <NSField label="auth method">
              <select value={authMethod} onChange={(e) => { setAuthMethod(e.target.value); setToken(''); }} className="ledger-input" style={{ fontFamily: 'var(--font-display)' }}>
                <option value="oauth">OAuth token — `claude setup-token` (sk-ant-oat01-…)</option>
                <option value="api_key">API key — sk-ant-api03-…</option>
              </select>
            </NSField>
            <NSField label={tokenName} w="17rem">
              <input type="password" autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)}
                className="ledger-input font-mono" placeholder="••••••••" />
            </NSField>
          </div>
        )}
        <div className="flex items-center justify-end" style={{ paddingTop: '0.5rem' }}>
          <button type="submit" disabled={!canSubmit} className="ns-submit section-label"
            style={{ color: canSubmit ? 'var(--accent-now)' : 'var(--fg-quiet)', opacity: canSubmit ? 1 : 0.4, cursor: canSubmit ? 'pointer' : 'not-allowed' }}>
            {submitting ? 'starting…' : 'start →'}
          </button>
        </div>
      </form>
      <hr style={{ marginTop: '1.5rem' }} />
    </section>
  );
}

// ---- TerminalPane (faux Solarized-Light shell) -----------------------------
const SHELL_LINES = [
  { p: 'cortex@a3f9c1b2', d: '~/workspace', c: 'cargo nextest run -p engram-sandbox-firecracker snapshot_uffd' },
  { o: ['    Finished test [unoptimized + debuginfo] target(s) in 0.71s', '    Starting 1 test across 1 binary', 'PASS [  18.420s] engram-sandbox-firecracker snapshot_uffd::restores_under_uffd', '────────────', '    Summary [  18.421s] 1 test run: 1 passed, 0 skipped'] },
  { p: 'cortex@a3f9c1b2', d: '~/workspace', c: 'git status -s' },
  { o: [' M crates/engram-sandbox-firecracker/tests/snapshot_uffd.rs'] },
  { p: 'cortex@a3f9c1b2', d: '~/workspace', c: '' },
];
function TerminalPane() {
  return (
    <section className="mb-12">
      <div className="terminal-host">
        {SHELL_LINES.map((ln, i) => (
          ln.o ? (
            <div key={i} className="term-out">{ln.o.map((l, j) => <div key={j}>{l}</div>)}</div>
          ) : (
            <div key={i} className="term-prompt">
              <span className="term-user">{ln.p}</span><span className="term-sep">:</span><span className="term-dir">{ln.d}</span><span className="term-sep">$ </span>
              <span className="term-cmd">{ln.c}</span>{ln.c === '' && <span className="term-cursor">█</span>}
            </div>
          )
        ))}
      </div>
    </section>
  );
}

// ---- ImagesPanel (settings) ------------------------------------------------
function DigestChip({ digest }) {
  const s = digest.length > 19 ? `${digest.slice(0, 19)}…` : digest;
  return <span className="digest-chip font-mono" title={digest}>{s}</span>;
}
function PressButton({ children, tone, onClick, disabled }) {
  const color = tone === 'primary' ? 'var(--accent-now)' : tone === 'danger' ? 'var(--accent-now)' : 'var(--fg-muted)';
  return <button type="button" onClick={onClick} disabled={disabled} className="press-btn section-label" style={{ color, opacity: disabled ? 0.4 : 1 }}>{children}</button>;
}
function ImageRow({ row, onDisable }) {
  const [confirming, setConfirming] = useS(false);
  return (
    <li className="img-row">
      <div className="flex items-baseline gap-3" style={{ flexWrap: 'wrap' }}>
        <span className="glyph" style={{ color: 'var(--fg-muted)' }}>●</span>
        <span className="font-mono" style={{ fontSize: '0.95rem', color: 'var(--fg)', whiteSpace: 'nowrap' }}>{row.image_uri}</span>
        {row.manifest_name && <span className="italic" style={{ fontFamily: 'var(--font-display)', fontSize: '0.85rem', color: 'var(--fg-muted)' }}>{row.manifest_name}</span>}
        <DigestChip digest={row.manifest_digest} />
        <span className="img-actions">
          <span className="font-mono" style={{ fontSize: '0.72rem', color: 'var(--fg-quiet)' }}>refreshed {relativeTime(row.last_refreshed_at)} ago</span>
          <PressButton>refresh</PressButton>
          {!confirming ? <PressButton onClick={() => setConfirming(true)}>disable</PressButton> : (
            <span className="flex items-baseline gap-3">
              <span className="italic" style={{ fontFamily: 'var(--font-display)', color: 'var(--fg-muted)', fontSize: '0.85rem' }}>sure?</span>
              <PressButton tone="danger" onClick={() => onDisable(row.id)}>yes</PressButton>
              <PressButton onClick={() => setConfirming(false)}>no</PressButton>
            </span>
          )}
        </span>
      </div>
      {row.manifest_description && <p className="img-desc">{row.manifest_description}</p>}
    </li>
  );
}
function ImagesPanel({ images, onDisable, onEnable }) {
  const [addOpen, setAddOpen] = useS(false);
  const [uri, setUri] = useS('');
  return (
    <section>
      <header className="mb-6 flex items-baseline justify-between">
        <h2 className="section-label">Enabled Images</h2>
        <p className="italic" style={{ fontFamily: 'var(--font-display)', fontSize: '0.8rem', color: 'var(--fg-quiet)' }}>manifest cached on enable, refresh when tags move</p>
      </header>
      <ul>{images.map((r) => <ImageRow key={r.id} row={r} onDisable={onDisable} />)}</ul>
      {addOpen ? (
        <form className="enable-form" onSubmit={(e) => { e.preventDefault(); if (uri.trim()) { onEnable(uri.trim()); setUri(''); setAddOpen(false); } }}>
          <p className="section-label" style={{ fontSize: '0.65rem' }}>NEW ENABLED IMAGE</p>
          <NSField label="image uri" w="8rem">
            <input value={uri} autoFocus onChange={(e) => setUri(e.target.value)} className="ledger-input font-mono" placeholder="ghcr.io/cortex/api:warm-1" spellCheck={false} />
          </NSField>
          <div className="flex items-baseline gap-6" style={{ paddingTop: '0.5rem' }}>
            <PressButton tone="primary" onClick={(e) => { e.preventDefault(); if (uri.trim()) { onEnable(uri.trim()); setUri(''); setAddOpen(false); } }}>enable</PressButton>
            <PressButton onClick={() => setAddOpen(false)}>cancel</PressButton>
          </div>
        </form>
      ) : (
        <div className="mt-8 flex justify-center"><PressButton tone="primary" onClick={() => setAddOpen(true)}>+ enable a new image</PressButton></div>
      )}
    </section>
  );
}

// ---- Pages -----------------------------------------------------------------
function Overview({ store, onOpen, onSettings, t = {}, pollTick = 0 }) {
  const [creating, setCreating] = useS(false);
  const [now, setNow] = useS(new Date());
  useE(() => { const t = setInterval(() => setNow(new Date()), 1000); return () => clearInterval(t); }, []);
  const chrome = !t.masthead;                 // legacy in-page header + chip
  const wide = t.masthead && t.overviewWidth === 'wide';
  const clock = now.toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  return (
    <main className={wide ? 'book-wide' : 'book'} style={{ paddingTop: chrome ? '3rem' : '2rem', paddingBottom: '3rem' }}>
      {chrome && <UserChip onSettings={onSettings} />}
      {chrome ? (
        <header className="mb-10">
          <h1 className="wordmark">engrams</h1>
          <p className="section-label" style={{ display: 'block', marginTop: '0.5rem', fontSize: '0.7rem' }}>
            polling every second · {clock}
          </p>
          <hr style={{ marginTop: '1.5rem' }} />
        </header>
      ) : (
        <div className="page-eyebrow">
          <span className="section-label">overview</span>
          <span className="section-label" style={{ letterSpacing: '0.05em' }}>{clock}</span>
        </div>
      )}
      <VitalSigns hosts={store.hosts} sessions={store.sessions} />
      <HostManifest hosts={store.hosts} />
      {creating && <NewSessionForm images={store.images} onCancel={() => setCreating(false)}
        onCreated={(uri, mode, prompt) => { setCreating(false); onOpen(store.createSession(uri, mode, prompt)); }} />}
      <SessionManifest sessions={store.sessions} onOpen={onOpen} bootLoader={t.bootLoader}
        onNewClick={creating ? undefined : () => setCreating(true)} />
    </main>
  );
}

function SessionDetail({ store, id, onBack, onSettings, t = {} }) {
  const session = store.sessions.find((s) => s.id === id);
  const [tab, setTab] = useS('transcript');
  const [, force] = useS(0);
  const events = session?.events || [];
  const chrome = !t.masthead;
  const booting = session && (session.status === 'created' || session.status === 'guest_ready');
  const TABS = [{ id: 'transcript', label: 'TRANSCRIPT' }, { id: 'shell', label: 'SHELL' }, { id: 'raw', label: 'RAW' }];
  return (
    <main className="book" style={{ paddingTop: chrome ? '3rem' : '2rem', paddingBottom: '3rem' }}>
      {chrome && <UserChip onSettings={onSettings} />}
      {chrome && <a href="#" onClick={(e) => { e.preventDefault(); onBack(); }} className="back-link section-label">← back to overview</a>}
      <header style={{ marginTop: chrome ? '2rem' : '0.5rem', marginBottom: '3rem' }}>
        <div className="section-label">session</div>
        <h1 className="font-mono" style={{ fontSize: '1.4rem', color: 'var(--fg)', letterSpacing: '-0.01em' }}>{id}</h1>
        {session && (
          <div className="flex items-baseline gap-3" style={{ marginTop: '0.75rem', fontFamily: 'var(--font-display)', color: 'var(--fg-muted)' }}>
            {t.bootLoader && booting
              ? <span className="row-loader" title="booting"><EngramMark size={18} mode="loop" period={2200} /></span>
              : <StatusGlyph status={session.status} />}
            <span className="section-label" style={{ letterSpacing: '0.05em' }}>{booting ? 'restoring canonical memory…' : session.status}</span>
          </div>
        )}
        {session && (
          <div className="font-mono" style={{ marginTop: '0.25rem', fontSize: '0.78rem', color: 'var(--fg-quiet)' }}>
            image {session.image} · created {relativeTime(session.created_at)} ago · {events.length} events
          </div>
        )}
        <hr style={{ marginTop: '1.5rem' }} />
      </header>
      <TabRow tabs={TABS} active={tab} onChange={setTab} right={tab === 'transcript' ? `${events.length} events` : undefined} />
      {tab === 'transcript' && (
        <>
          <Transcript events={events} busyVerb={t.harnessVerb || 'thinking'} onStop={() => { store.interrupt(id); force((x) => x + 1); }} />
          <PromptComposer status={session?.status} onSend={(t) => { store.sendPrompt(id, t); force((x) => x + 1); }} />
        </>
      )}
      {tab === 'shell' && <TerminalPane />}
      {tab === 'raw' && (
        <div className="font-mono raw-log">
          {events.map((e) => (
            <div key={e.idx} className="raw-row">
              <span data-tabular style={{ color: 'var(--fg-quiet)' }}>{e.idx}</span>
              <span className="smallcaps" style={{ fontSize: '0.66rem' }}>{e.event.type}</span>
              <span className="raw-summary" title={JSON.stringify(e.event)}>{rawSummary(e.event)}</span>
            </div>
          ))}
        </div>
      )}
    </main>
  );
}
function rawSummary(ev) {
  return Object.entries(ev).filter(([k]) => k !== 'type' && k !== 'at').map(([k, v]) => `${k}=${JSON.stringify(v)}`).join(' ');
}

// Grouped Settings IA (ADR 0031): "You" (personal) split from "Deployment"
// (admin-only config) by a hairline. Members see only the You group.
const SETTINGS_HINTS = {
  profile: 'Your signed-in identity & what your role can do',
  tokens: 'Your service tokens — sealed & used automatically for every session',
  members: 'Everyone in this deployment — roles & access',
  images: 'Curated OCI image URIs sessions may reference',
  registries: 'Docker registry credentials, sealed under the deployment KEK',
};
function Settings({ store, onBack, onSettings, t = {} }) {
  const isAdmin = PRINCIPAL.is_admin;
  const [tab, setTab] = useS('profile');
  const chrome = !t.masthead;
  const GROUPS = [
    { label: 'you', tabs: [{ to: 'profile', label: 'Profile' }, { to: 'tokens', label: 'Tokens' }] },
  ];
  if (isAdmin) GROUPS.push({ label: 'deployment', tabs: [
    { to: 'members', label: 'Members' }, { to: 'images', label: 'Images' }, { to: 'registries', label: 'Registries' },
  ] });
  return (
    <main className={chrome ? 'book' : 'book-wide'} style={{ paddingTop: chrome ? '3rem' : '2rem', paddingBottom: '3rem' }}>
      {chrome && <UserChip onSettings={onSettings} />}
      {chrome ? (
        <header className="mb-8">
          <h1 className="engrams-h1">
            <a href="#" onClick={(e) => { e.preventDefault(); onBack(); }} className="breadcrumb-home" style={{ color: 'var(--fg-muted)', textDecoration: 'none' }}>engrams</a>
            <span aria-hidden style={{ color: 'var(--fg-quiet)', margin: '0 0.4em', fontStyle: 'normal' }}>›</span>settings
          </h1>
          <p className="section-label" style={{ display: 'block', marginTop: '0.5rem' }}>{SETTINGS_HINTS[tab]}</p>
          <hr style={{ marginTop: '1.5rem' }} />
        </header>
      ) : (
        <div className="surface-head" style={{ marginBottom: '1.75rem' }}>
          <div>
            <h1 className="surface-title">settings</h1>
            <p className="surface-sub">{SETTINGS_HINTS[tab]}</p>
          </div>
        </div>
      )}
      <nav className="settings-groups">
        {GROUPS.map((g, gi) => (
          <React.Fragment key={g.label}>
            {gi > 0 && <span className="settings-divider" aria-hidden />}
            <div className="settings-group">
              <span className="settings-group-label">{g.label}</span>
              <div className="settings-group-tabs">
                {g.tabs.map((x) => (
                  <a key={x.to} href="#" onClick={(e) => { e.preventDefault(); setTab(x.to); }} className="settings-tab"
                    style={{ color: tab === x.to ? 'var(--fg)' : 'var(--fg-quiet)', borderBottom: tab === x.to ? '1px solid var(--fg)' : '1px solid transparent' }}>{x.label}</a>
                ))}
              </div>
            </div>
          </React.Fragment>
        ))}
      </nav>
      <div style={{ marginTop: '2.5rem' }}>
        {tab === 'profile' && <ProfilePanel principal={PRINCIPAL} />}
        {tab === 'tokens' && <TokensPanel principal={PRINCIPAL} />}
        {tab === 'members' && isAdmin && <MembersPanel />}
        {tab === 'images' && isAdmin && <ImagesPanel images={store.images} onDisable={store.disableImage} onEnable={store.enableImage} />}
        {tab === 'registries' && isAdmin && <RegistriesStub />}
      </div>
    </main>
  );
}

function AccessLegend({ role }) {
  const admin = role === 'admin';
  const can = admin
    ? ['launch & manage your own sessions', 'oversee every session across the fleet',
       'inspect host capacity & drain hosts', 'read storage durability & snapshots',
       'curate images & registry credentials', 'manage members & their roles']
    : ['launch & manage your own sessions', 'save your own Claude Code token'];
  const cannot = admin ? [] : ['the fleet, storage & deployment settings — admin only'];
  return (
    <div className="access-legend">
      <span className="section-label">what your role can do</span>
      <ul className="access-list">
        {can.map((c) => <li key={c}><span className="glyph">✓</span>{c}</li>)}
        {cannot.map((c) => <li className="denied" key={c}><span className="glyph">✕</span>{c}</li>)}
      </ul>
    </div>
  );
}

function ProfilePanel({ principal }) {
  return (
    <section>
      <header className="mb-6 flex items-baseline justify-between">
        <h2 className="section-label">Profile</h2>
        <span className="font-display italic" style={{ fontSize: '0.8rem', color: 'var(--fg-quiet)' }}>signed in</span>
      </header>
      <div className="profile-head">
        <PersonMark name={principal.name} email={principal.email} size="md" />
        <div>
          <div className="font-display" style={{ fontSize: '1.2rem', color: 'var(--fg)' }}>{principal.name}</div>
          <div className="font-mono" style={{ fontSize: '0.82rem', color: 'var(--fg-quiet)' }}>{principal.email}</div>
        </div>
      </div>
      <dl style={{ maxWidth: '40rem' }}>
        <div className="profile-row">
          <dt>role</dt>
          <dd className="flex items-baseline gap-3"><RoleTag role={principal.role} /><Provenance source={principal.source} /></dd>
        </div>
        <div className="profile-row">
          <dt>tokens</dt>
          <dd>{principal.has_claude_token
            ? <span>Claude Code saved · <span style={{ color: 'var(--fg-quiet)' }}>managed under Tokens</span></span>
            : <span className="font-display italic" style={{ color: 'var(--accent-now)' }}>none saved — add one under Tokens →</span>}</dd>
        </div>
      </dl>
      <AccessLegend role={principal.role} />
    </section>
  );
}

function TokensPanel({ principal }) {
  // a ledger of per-service credentials — one tab that scales to N services,
  // each with its own "what it's for" line. Identity stays in Profile.
  const SERVICES = [
    { id: 'claude', name: 'Claude Code', use: 'built-in Claude sessions authenticate with this',
      hint: 'from `claude setup-token` — stored encrypted, never shown again', placeholder: 'sk-ant-oat…' },
    { id: 'github', name: 'GitHub', use: 'clone private repositories into a session',
      hint: 'a fine-grained PAT with repo scope', placeholder: 'github_pat_…' },
  ];
  const [saved, setSaved] = useS({ claude: !!principal.has_claude_token, github: false });
  const [editing, setEditing] = useS(null);
  return (
    <section style={{ maxWidth: '46rem' }}>
      <header className="mb-6 flex items-baseline justify-between">
        <h2 className="section-label">Tokens</h2>
        <span className="font-display italic" style={{ fontSize: '0.8rem', color: 'var(--fg-quiet)' }}>sealed · auto-used per session</span>
      </header>
      <div className="tokens-ledger">
        {SERVICES.map((s) => {
          const isSaved = saved[s.id];
          const isEditing = editing === s.id;
          return (
            <div key={s.id} className="token-row">
              <div className="token-svc">
                <span className="token-name">{s.name}</span>
                <span className="token-use">{s.use}</span>
                {isEditing && (
                  <div className="token-edit">
                    <input type="password" className="ledger-input font-mono" autoFocus placeholder={s.placeholder} style={{ fontSize: '0.85rem' }} />
                    <span className="token-hint">{s.hint}</span>
                    <div className="token-edit-actions">
                      <PressButton onClick={() => { setSaved((m) => ({ ...m, [s.id]: true })); setEditing(null); }}>save</PressButton>
                      <button className="members-act act-quiet" onClick={() => setEditing(null)}>cancel</button>
                    </div>
                  </div>
                )}
              </div>
              <span className={`token-status${isSaved ? ' is-saved' : ''}`}>{isSaved ? 'saved · sealed' : 'not connected'}</span>
              <div className="token-row-actions">
                {isSaved ? (
                  <>
                    <button className="members-act" onClick={() => setEditing(s.id)}>replace</button>
                    <button className="members-act act-quiet" onClick={() => setSaved((m) => ({ ...m, [s.id]: false }))}>remove</button>
                  </>
                ) : (
                  <button className="members-act act-primary" onClick={() => setEditing(s.id)}>add →</button>
                )}
              </div>
            </div>
          );
        })}
      </div>
      <p className="ledger-note">
        every token is sealed under the deployment key the moment you save it — the plaintext never touches Postgres, and it’s used automatically so you’re never prompted per session. new services land here as their own row.
      </p>
    </section>
  );
}

function MembersPanel() {
  const [people, setPeople] = useS(PEOPLE);
  const setRole = (id, role) => setPeople((ps) => ps.map((p) => p.id === id ? { ...p, role, source: 'manual' } : p));
  const setActive = (id, active) => setPeople((ps) => ps.map((p) => p.id === id ? { ...p, active } : p));
  const admins = people.filter((p) => p.role === 'admin' && p.active).length;
  const disabled = people.filter((p) => !p.active).length;
  return (
    <section>
      <header className="mb-6 flex items-baseline justify-between">
        <h2 className="section-label">Members</h2>
        <span className="font-display italic" style={{ fontSize: '0.8rem', color: 'var(--fg-quiet)' }}>everyone in this deployment</span>
      </header>
      <div className="people-rollup">
        <b>{people.length}</b> people <span className="dot">·</span>
        <b>{admins}</b> admins <span className="dot">·</span>
        <b>{disabled}</b> disabled
      </div>
      <div className="members-ledger">
        <div className="members-row members-head">
          <span className="section-label">person</span>
          <span className="section-label">role</span>
          <span className="section-label">source</span>
          <span className="section-label" style={{ textAlign: 'right' }}>status · actions</span>
        </div>
        {people.map((p) => (
          <div key={p.id} className={`members-row${p.active ? '' : ' is-off'}`}>
            <div className={`person${p.active ? '' : ' is-off'}`}>
              <PersonMark name={p.name} email={p.email} size="sm" off={!p.active} />
              <div className="person-id">
                <div className="person-name">{p.name}{p.you && <span className="you">you</span>}</div>
                <div className="person-email">{p.email}</div>
              </div>
            </div>
            <span><RoleTag role={p.role} /></span>
            <Provenance source={p.source} />
            <div className="members-actions">
              <MemberStatus active={p.active} />
              {p.you ? <span className="members-self">you</span> : (
                <>
                  {p.role === 'member'
                    ? <button className="members-act act-primary" onClick={() => setRole(p.id, 'admin')}>make admin</button>
                    : <button className="members-act act-quiet" onClick={() => setRole(p.id, 'member')}>revoke admin</button>}
                  {p.active
                    ? <button className="members-act act-quiet" onClick={() => setActive(p.id, false)}>deactivate</button>
                    : <button className="members-act act-primary" onClick={() => setActive(p.id, true)}>reactivate</button>}
                </>
              )}
            </div>
          </div>
        ))}
      </div>
      <p className="ledger-note">
        roles are provisioned from your identity provider on first sign-in and stay in sync over SCIM; promote or revoke here and the change is marked <em>set by an admin</em>. a deactivated member keeps their sessions but can’t sign in.
      </p>
    </section>
  );
}
// ADR 0031 auth states — the first screens every user sees, before the app
// shell exists. On warm paper, the engram mark, the lowercase em-dash voice.
function AuthScreen({ state, onRetry, onSignOut }) {
  if (state === 'boot') {
    return (
      <div className="auth-stage">
        <div className="auth-card">
          <span className="auth-mark"><EngramMark size={72} mode="loop" /></span>
          <div className="auth-line">authenticating…</div>
        </div>
      </div>
    );
  }
  if (state === 'error') {
    return (
      <div className="auth-stage">
        <div className="auth-card">
          <span className="auth-mark"><EngramMark size={72} mode="static" /></span>
          <div className="auth-strong">could not reach the coordinator —<br />retrying…</div>
          <div className="auth-detail">GET /api/v1/me → 503 service unavailable</div>
          <div className="auth-actions"><PressButton onClick={onRetry}>retry now</PressButton></div>
        </div>
      </div>
    );
  }
  // not-a-member
  return (
    <div className="auth-stage">
      <div className="auth-card">
        <span className="auth-mark"><EngramMark size={72} mode="static" /></span>
        <div className="auth-strong">you’re signed in — but not yet<br />a member of this deployment.</div>
        <div className="auth-detail">nikhil.unni@cortex.io</div>
        <div className="auth-line">ask an admin to add you, then reload.</div>
        <div className="auth-actions"><button className="members-act act-quiet" onClick={onSignOut}>sign out</button></div>
      </div>
    </div>
  );
}

function RegistriesStub() {
  return (
    <section>
      <header className="mb-6"><h2 className="section-label">Registry Credentials</h2></header>
      <div className="reg-row">
        <span className="glyph" style={{ color: 'var(--fg-muted)' }}>●</span>
        <span className="font-mono" style={{ fontSize: '0.95rem' }}>ghcr.io</span>
        <span className="digest-chip font-mono">sealed · key_id 3</span>
        <span className="img-actions"><PressButton>rotate</PressButton><PressButton>remove</PressButton></span>
      </div>
      <p className="img-desc" style={{ marginLeft: 0, marginTop: '1.5rem' }}>credentials are encrypted with the deployment KEK before they touch Postgres — the plaintext never leaves this request.</p>
    </section>
  );
}
function ProfileStub() {
  return (
    <section>
      <header className="mb-6"><h2 className="section-label">Profile</h2></header>
      <NSField label="deployment"><div className="ledger-input" style={{ fontFamily: 'var(--font-display)' }}>Local development</div></NSField>
      <p className="img-desc" style={{ marginLeft: 0, marginTop: '1.5rem' }}>auth is not wired in this deployment — once a real identity lands, the § mark gains an email and a sign-out.</p>
    </section>
  );
}

Object.assign(window, { NewSessionForm, TerminalPane, ImagesPanel, Overview, SessionDetail, Settings, AuthScreen });
