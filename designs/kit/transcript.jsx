// transcript.jsx — the agent transcript renderer + prompt composer.
// Cosmetic recreation of Transcript.tsx / ToolCall.tsx / PullRequestCard.tsx
// / RunBoundary.tsx / PromptComposer.tsx.
const { useState: useStateT, useMemo: useMemoT } = React;

// Minimal Markdown → HTML for FINAL assistant output (LLMs emit Markdown).
// Block level: fenced code, ATX headings, bullet/ordered lists, blockquote, hr,
// paragraphs. Inline: `code`, **bold**, *italic*, [text](url). Everything is
// HTML-escaped first, so this is safe to set as innerHTML. (During streaming
// you'd render plain text and only markdown-render once the message completes.)
function mdToHtml(src) {
  const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  const inline = (s) => esc(s)
    .replace(/`([^`]+)`/g, '<code class="md-code">$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^*])\*([^*\n]+)\*/g, '$1<em>$2</em>')
    .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, '<a class="md-link" href="$2" target="_blank" rel="noopener">$1</a>');
  const fences = [];
  src = src.replace(/```(\w+)?\n?([\s\S]*?)```/g, (_m, _l, code) => {
    fences.push(`<pre class="md-pre"><code>${esc(code.replace(/\n$/, ''))}</code></pre>`);
    return `\u0000F${fences.length - 1}\u0000`;
  });
  const lines = src.split('\n');
  const html = [];
  let list = null; // 'ul' | 'ol'
  const closeList = () => { if (list) { html.push(`</${list}>`); list = null; } };
  for (let raw of lines) {
    const line = raw.trimEnd();
    const fence = line.match(/^\u0000F(\d+)\u0000$/);
    if (fence) { closeList(); html.push(fences[+fence[1]]); continue; }
    if (!line.trim()) { closeList(); continue; }
    let m;
    if ((m = line.match(/^(#{1,6})\s+(.*)$/))) { closeList(); const n = m[1].length; html.push(`<h${n} class="md-h">${inline(m[2])}</h${n}>`); continue; }
    if (/^\s*([-*])\s+/.test(line)) { if (list !== 'ul') { closeList(); html.push('<ul class="md-ul">'); list = 'ul'; } html.push(`<li>${inline(line.replace(/^\s*[-*]\s+/, ''))}</li>`); continue; }
    if ((m = line.match(/^\s*\d+\.\s+(.*)$/))) { if (list !== 'ol') { closeList(); html.push('<ol class="md-ol">'); list = 'ol'; } html.push(`<li>${inline(m[1])}</li>`); continue; }
    if (/^\s*>\s?/.test(line)) { closeList(); html.push(`<blockquote class="md-quote">${inline(line.replace(/^\s*>\s?/, ''))}</blockquote>`); continue; }
    if (/^(---|\*\*\*|___)\s*$/.test(line)) { closeList(); html.push('<hr class="md-hr" />'); continue; }
    closeList();
    html.push(`<p>${inline(line)}</p>`);
  }
  closeList();
  return html.join('\n');
}

// ---- ToolCall: a bracketed aside, never a chat bubble ----------------------
function ToolCall({ toolName, argsSummary, completion }) {
  const [expanded, setExpanded] = useStateT(false);
  const status = completion ? (completion.ok ? 'ok' : 'err') : '…';
  const statusColor = completion ? (completion.ok ? 'var(--accent-archived)' : 'var(--accent-now)') : 'var(--accent-now)';
  const shortArgs = (a) => (!a ? '' : expanded || a.length <= 64 ? a : a.slice(0, 64) + '…');
  return (
    <div className="tool-call tool-rule font-mono">
      <button type="button" onClick={() => setExpanded((e) => !e)} className="tool-line">
        <span className="q">[ </span>
        <span className="tn">{toolName}</span>
        {argsSummary && (<><span className="q"> → </span><span>{shortArgs(argsSummary)}</span></>)}
        <span className="q"> · </span>
        <span style={{ color: statusColor }}>{status}</span>
        {completion && <span className="q" data-tabular> · {completion.durationMs}ms</span>}
        <span className="q"> ]</span>
      </button>
      {expanded && (
        <div className="tool-detail">
          {argsSummary && <Detail label="args" value={argsSummary} />}
          {completion?.resultSummary && <Detail label="ok " value={completion.resultSummary} />}
        </div>
      )}
    </div>
  );
}
function Detail({ label, value }) {
  return (
    <div className="tool-detail-row">
      <span className="smallcaps" style={{ color: 'var(--fg-quiet)', fontSize: '0.7rem' }}>{label}</span>
      <pre className="tool-pre">{value}</pre>
    </div>
  );
}

// ---- PullRequestCard: the durable artifact (verdigris) ---------------------
function PullRequestCard({ url, repo, title, number, headBranch, baseBranch, at }) {
  return (
    <div className="pr-card">
      <div className="flex items-baseline justify-between gap-3">
        <div className="font-mono" style={{ fontSize: '0.72rem', color: 'var(--fg-quiet)' }}>
          <span className="glyph" style={{ color: 'var(--accent-archived)' }}>↳</span>{' '}
          <span className="smallcaps" style={{ color: 'var(--accent-archived)' }}>pull request</span>
          {' · '}{repo} #{number}
        </div>
        <span className="font-mono" data-tabular style={{ fontSize: '0.72rem', color: 'var(--fg-quiet)' }}>{hms(at)}</span>
      </div>
      <a href={url} onClick={(e) => e.preventDefault()} className="pr-title">
        {title}<span style={{ color: 'var(--fg-quiet)' }}> ↗</span>
      </a>
      <div className="font-mono" style={{ fontSize: '0.78rem', color: 'var(--fg-quiet)', marginTop: '0.25rem' }}>
        {headBranch}<span style={{ color: 'var(--fg-muted)' }}> → </span>{baseBranch}
      </div>
    </div>
  );
}

// ---- Run boundaries + idle marker ------------------------------------------
function RunBoundary({ prompt }) {
  if (!prompt) return <hr className="run-rule" />;
  return (
    <div className="run-boundary">
      <hr className="flex-1" />
      <span className="run-prompt" title={prompt}>“{prompt}”</span>
      <hr className="flex-1" />
    </div>
  );
}
function IdleMarker() {
  return (
    <div className="idle-marker">
      <div className="glyph" style={{ fontSize: '1.1rem' }}>◌</div>
      <div className="section-label" style={{ marginTop: '0.25rem', letterSpacing: '0.08em' }}>awaiting prompt</div>
    </div>
  );
}

// ---- buildBlocks: collapse the event stream into renderable blocks ---------
function buildBlocks(events) {
  const out = [];
  const openTools = new Map();
  const openExecs = new Map();
  let activeMsg = null;
  let run = null; // per-run tally for the run summary
  const bump = (k) => { if (run) run[k] = (run[k] || 0) + 1; };
  for (const { idx, event: ev } of events) {
    switch (ev.type) {
      case 'run_started':
        run = { reads: 0, edits: 0, ran: 0, other: 0, at: ev.at };
        out.push({ kind: 'run-start', key: `rs${idx}`, prompt: ev.prompt_summary, at: ev.at }); activeMsg = null; break;
      case 'run_completed':
        out.push({ kind: 'run-end', key: `re${idx}`, summary: run, endAt: ev.at, ok: ev.ok }); run = null; activeMsg = null; break;
      case 'agent_message':
        if (activeMsg && activeMsg.role === ev.role) {
          out[activeMsg.idx].texts.push(ev.text);
        } else {
          out.push({ kind: 'message', key: `m${idx}`, role: ev.role, texts: [ev.text], at: ev.at });
          activeMsg = { role: ev.role, idx: out.length - 1 };
        }
        break;
      case 'tool_call_started':
        if (/^(read_file|read|grep|list|search|glob|cat)/.test(ev.tool_name)) bump('reads');
        else if (/^(edit_file|write_file|create_file|write|apply_patch|edit)/.test(ev.tool_name)) bump('edits');
        else bump('other');
        out.push({ kind: 'tool', key: `t${ev.tool_call_id}`, toolName: ev.tool_name, argsSummary: ev.args_summary });
        openTools.set(ev.tool_call_id, out.length - 1); activeMsg = null; break;
      case 'tool_call_completed': {
        const i = openTools.get(ev.tool_call_id);
        if (i != null) {
          out[i].completion = { ok: ev.ok, durationMs: ev.duration_ms, resultSummary: ev.result_summary };
          openTools.delete(ev.tool_call_id);
        }
        break;
      }
      case 'exec_started':
        bump('ran');
        out.push({ kind: 'exec', key: `x${ev.exec_id}`, command: (ev.command || []).join(' '), output: '' });
        openExecs.set(ev.exec_id, out.length - 1); activeMsg = null; break;
      case 'stdout': case 'stderr': {
        const i = openExecs.get(ev.exec_id);
        if (i != null) out[i].output += ev.chunk;
        break;
      }
      case 'exec_completed': {
        const i = openExecs.get(ev.exec_id);
        if (i != null) {
          out[i].completion = { exit: ev.exit_status, durationMs: ev.rusage && ev.rusage.duration_ms };
          openExecs.delete(ev.exec_id);
        }
        break;
      }
      case 'snapshot_taken':
        out.push({ kind: 'durability', key: `snap${idx}`, mark: 'snapshot', sizeBytes: ev.size_bytes, at: ev.at }); activeMsg = null; break;
      case 'resumed':
        out.push({ kind: 'durability', key: `res${idx}`, mark: 'resumed', at: ev.at }); activeMsg = null; break;
      case 'harness_idle': out.push({ kind: 'idle', key: `i${idx}` }); activeMsg = null; break;
      case 'pull_request_opened':
        out.push({ kind: 'pr', key: `pr${idx}`, url: ev.url, repo: ev.repo, title: ev.title,
          number: ev.number, headBranch: ev.head_branch, baseBranch: ev.base_branch, at: ev.at });
        activeMsg = null; break;
      default: break;
    }
  }
  return out;
}

// Derive the harness-waiting verb from whatever's actually in flight, so the
// indicator reads "running cargo nextest…" / "reading snapshot_uffd.rs…" rather
// than a generic gerund (cf. this chat's "Shelling…"). Falls back to `generic`
// between turns (just after a prompt, before any tool/exec starts).
function contextVerb(blocks, generic) {
  for (let i = blocks.length - 1; i >= 0; i--) {
    const b = blocks[i];
    if (b.kind === 'run-end' || b.kind === 'durability') continue;
    if (b.kind === 'exec' && !b.completion) {
      const head = b.command.split(/\s+/).slice(0, 2).join(' ');
      return `running ${head}`;
    }
    if (b.kind === 'tool' && !b.completion) {
      const file = b.argsSummary ? b.argsSummary.split(/[\s—]/)[0].split('/').pop() : null;
      if (/^(read_file|read|cat)/.test(b.toolName)) return file ? `reading ${file}` : 'reading';
      if (/^(edit_file|write_file|create_file|write|apply_patch|edit)/.test(b.toolName)) return file ? `editing ${file}` : 'editing';
      if (/^(grep|search|glob|list)/.test(b.toolName)) return 'searching';
      if (/^(bash|exec|shell|run)/.test(b.toolName)) { const head = (b.argsSummary || '').split(/\s+/).slice(0, 2).join(' '); return head ? `running ${head}` : 'running'; }
      return b.toolName.replace(/_/g, ' ');
    }
    break; // last meaningful block is a message/run-start → generic
  }
  return generic;
}

function Message({ role, texts, at, fresh }) {
  return (
    <section className="msg">
      <span className="margin-note margin-left smallcaps">{role}.</span>
      <span className="margin-note margin-right" data-tabular style={{ marginTop: '0.45rem' }}>{hms(at)}</span>
      <div className={`prose md ${fresh ? 'ink-settle' : ''}`}
        dangerouslySetInnerHTML={{ __html: mdToHtml(texts.join('\n\n')) }} />
    </section>
  );
}

// The harness is working — shown where the assistant's reply will land, in the
// transcript flow, while we wait for it to speak. A looping engram trace + a
// gerund (cf. this very chat's "Shelling…"). The trace forming = a thought
// forming. Unmounts the instant the real message arrives (which ink-settles in).
function HarnessWaiting({ verb = 'thinking', onStop }) {
  return (
    <section className="msg harness-waiting">
      <span className="margin-note margin-left smallcaps">assistant.</span>
      <div className="thinking">
        <span className="thinking-mark"><EngramMark size={20} mode="loop" period={2200} /></span>
        <span className="thinking-verb section-label">{verb}…</span>
        {onStop && <button type="button" className="stop-btn section-label" onClick={onStop}>✕ stop</button>}
      </div>
    </section>
  );
}

function Transcript({ events, busyVerb, onStop }) {
  const blocks = useMemoT(() => buildBlocks(events), [events]);
  if (blocks.length === 0) {
    return (
      <div className="space-y-1">
        <HarnessWaiting verb={busyVerb} onStop={onStop} />
      </div>
    );
  }
  let lastMsgKey = null;
  for (let i = blocks.length - 1; i >= 0; i--) { if (blocks[i].kind === 'message') { lastMsgKey = blocks[i].key; break; } }
  let trailingIdle = null;
  for (let i = blocks.length - 1; i >= 0; i--) {
    const k = blocks[i].kind;
    if (k === 'idle') { trailingIdle = blocks[i].key; break; }
    if (k === 'message' || k === 'tool' || k === 'exec' || k === 'run-start') break;
  }
  let busy = false;
  if (!trailingIdle) {
    for (let i = blocks.length - 1; i >= 0; i--) {
      const b = blocks[i];
      if (b.kind === 'run-end' || b.kind === 'durability') continue;
      if (b.kind === 'message') { busy = b.role === 'user'; break; }
      if (b.kind === 'tool') { busy = !b.completion; break; }
      if (b.kind === 'exec') { busy = !b.completion; break; }
      if (b.kind === 'run-start') { busy = true; break; }
      break;
    }
  }
  return (
    <div className="space-y-1">
      {blocks.map((b) => {
        switch (b.kind) {
          case 'run-start': return b.prompt ? <UserTurn key={b.key} prompt={b.prompt} at={b.at} /> : <RunBoundary key={b.key} prompt={null} />;
          case 'run-end': return <RunSummary key={b.key} summary={b.summary} startAt={b.summary && b.summary.at} endAt={b.endAt} ok={b.ok} />;
          case 'idle': return b.key === trailingIdle ? <IdleMarker key={b.key} /> : null;
          case 'tool': return <ToolCall key={b.key} toolName={b.toolName} argsSummary={b.argsSummary} completion={b.completion} />;
          case 'exec': return <Process key={b.key} command={b.command} output={b.output} completion={b.completion} />;
          case 'durability': return <DurabilityMarker key={b.key} mark={b.mark} sizeBytes={b.sizeBytes} at={b.at} />;
          case 'message': return b.role === 'user'
            ? <UserTurn key={b.key} prompt={b.texts.join('\n')} at={b.at} />
            : <Message key={b.key} role={b.role} texts={b.texts} at={b.at} fresh={b.key === lastMsgKey} />;
          case 'pr': return <PullRequestCard key={b.key} {...b} />;
          default: return null;
        }
      })}
      {busy && <HarnessWaiting verb={contextVerb(blocks, busyVerb)} onStop={onStop} />}
    </div>
  );
}

// A run's "receipt" — a faint one-line tally of what the agent did this run
// (read N · edited N · ran N · duration), so long sessions are scannable
// without reading every block. Renders at the run's close.
function RunSummary({ summary, startAt, endAt, ok }) {
  if (!summary) return null;
  const parts = [];
  if (summary.reads) parts.push(`read ${summary.reads}`);
  if (summary.edits) parts.push(`edited ${summary.edits}`);
  if (summary.ran) parts.push(`ran ${summary.ran}`);
  if (summary.other) parts.push(`${summary.other} ${summary.other === 1 ? 'tool' : 'tools'}`);
  if (startAt && endAt) {
    const ms = new Date(endAt) - new Date(startAt);
    if (ms > 0) parts.push(fmtDur(ms));
  }
  if (parts.length === 0 && ok !== false) return null;
  return (
    <div className="run-summary">
      <span className="run-summary-mark" aria-hidden>↳</span>
      <span className="run-summary-text font-mono">
        {ok === false ? 'interrupted' : parts.join(' · ')}
      </span>
    </div>
  );
}

// The human's turn — a contained, square paper-warm panel marked with the §
// user mark. Clear turn-taking without resorting to chat bubbles (the system
// deliberately avoids them); the contrast of contained-prompt vs open-prose
// reads the dialogue as clearly as ChatGPT/Claude bubbles do.
function UserTurn({ prompt, at }) {
  return (
    <section className="user-turn">
      <div className="user-bubble">
        <div className="user-bubble-head">
          <span className="user-turn-label section-label"><span className="user-turn-sigil">§</span> you</span>
          {at && <span className="user-bubble-time font-mono" data-tabular>{hms(at)}</span>}
        </div>
        <p className="user-turn-text">{prompt}</p>
      </div>
    </section>
  );
}

// A process the harness ran — shell vocabulary ($ command), exit dot, duration,
// expandable stdout/stderr. Distinct from the bracketed tool-call asides:
// tools are [ … ], processes are $ … . Surfaces exec/stdout events the current
// transcript drops on the floor.
function Process({ command, output, completion }) {
  const [open, setOpen] = useStateT(false);
  const running = !completion;
  const failed = completion && completion.exit !== 0 && completion.exit != null;
  const dot = running ? '◐' : failed ? '!' : '●';
  const dotColor = running ? 'var(--accent-now)' : failed ? 'var(--accent-now)' : 'var(--accent-archived)';
  const hasOutput = output && output.trim().length > 0;
  return (
    <div className="process">
      <button type="button" className="process-line" onClick={() => hasOutput && setOpen((o) => !o)} style={{ cursor: hasOutput ? 'pointer' : 'default' }}>
        <span className="glyph" style={{ color: dotColor, fontSize: '0.8rem' }}>{dot}</span>
        <span className="process-dollar">$</span>
        <span className="process-cmd">{command}</span>
        {completion && completion.exit != null && (
          <span className="process-meta" data-tabular>
            exit {completion.exit}{completion.durationMs != null ? ` · ${fmtDur(completion.durationMs)}` : ''}
          </span>
        )}
        {running && <span className="process-meta" style={{ color: 'var(--accent-now)' }}>running…</span>}
        {hasOutput && <span className="process-toggle">{open ? '▾' : '▸'}</span>}
      </button>
      {open && hasOutput && <pre className="process-output">{output.trimEnd()}</pre>}
    </div>
  );
}

// Durability rhythm — the Engrams signature. A faint centered marker tying the
// conversation to the snapshot/resume lifecycle.
function DurabilityMarker({ mark, sizeBytes, at }) {
  const label = mark === 'snapshot'
    ? `snapshotted${sizeBytes ? ` · ${fmtBytes(sizeBytes)}` : ''}`
    : 'resumed from snapshot';
  return (
    <div className="durability-marker">
      <span className="glyph" style={{ color: 'var(--accent-archived)', fontSize: '0.85rem' }}>⌑</span>
      <span className="durability-label section-label">{label}</span>
      {at && <span className="durability-time font-mono">{hms(at)}</span>}
    </div>
  );
}

function fmtDur(ms) {
  if (ms == null) return '';
  if (ms < 1000) return `${ms}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  return `${Math.floor(s / 60)}m ${Math.round(s % 60)}s`;
}

// ---- PromptComposer --------------------------------------------------------
function PromptComposer({ status, onSend }) {
  const [text, setText] = useStateT('');
  const [pending, setPending] = useStateT(false);
  if (status === 'dead') return <TerminalBanner text="this session is dead — fork it to continue." />;
  if (status === 'completed') return <TerminalBanner text="this session is completed — fork it to continue." />;
  if (status === 'failed') return <TerminalBanner text="this session failed during create — start a new one." />;
  const submit = () => {
    const t = text.trim();
    if (!t || pending) return;
    setPending(true);
    setTimeout(() => { onSend(t); setText(''); setPending(false); }, 480);
  };
  const onKeyDown = (e) => { if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') { e.preventDefault(); submit(); } };
  const idle = !text.trim() || pending;
  return (
    <div className="composer">
      <span className="margin-note margin-left smallcaps" style={{ color: pending ? 'var(--accent-now)' : 'var(--fg-quiet)' }}>you.</span>
      <div className="flex items-start gap-3">
        <span className={`glyph ${pending ? 'glyph-heartbeat' : ''}`} style={{ marginTop: '0.25rem', color: pending ? 'var(--accent-now)' : 'var(--fg-muted)' }}>▸</span>
        <textarea value={text} onChange={(e) => setText(e.target.value)} onKeyDown={onKeyDown} rows={2}
          disabled={pending} placeholder="type a reply — ⌘↵ to send" className="composer-input" />
        <button type="button" onClick={submit} disabled={idle} className="composer-send section-label"
          style={{ color: idle ? 'var(--fg-quiet)' : 'var(--accent-now)', cursor: idle ? 'not-allowed' : 'pointer' }}>
          {pending ? 'sending…' : 'send →'}
        </button>
      </div>
      {status === 'idle' && <p className="composer-hint">session is idle — sending will resume it.</p>}
      {(status === 'created' || status === 'guest_ready') && <p className="composer-hint">session is still starting up — agentd will be ready in a moment.</p>}
    </div>
  );
}
function TerminalBanner({ text }) { return <div className="terminal-banner">{text}</div>; }

Object.assign(window, { Transcript, ToolCall, PullRequestCard, RunBoundary, IdleMarker, PromptComposer, buildBlocks });
