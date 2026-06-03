// store.jsx — the shared mutable fake store (used by both the current kit and
// the four-surface redesign). Seeds hosts/sessions/images and exposes
// createSession / sendPrompt / enableImage / disableImage.
const { useRef: useStoreRef } = React;

const REPLIES = [
  'On it. Pulling the relevant crate and re-running the suite to confirm the current state before I touch anything.',
  'Done — change is in and the test passes locally. Want me to open a PR or keep iterating?',
  'Good call. I\'ll snapshot the session first so we can resume from here if the next step goes sideways.',
];

function useStore(forceUpdate) {
  const ref = useStoreRef(null);
  if (!ref.current) {
    ref.current = {
      hosts: HOSTS,
      sessions: seedSessions(),
      images: IMAGES,
      _replyN: 0,
      _pending: {},
      createSession(uri, mode, prompt) {
        const id = uid('');
        const ev = [];
        let i = 0;
        if (prompt) {
          ev.push({ idx: i++, event: { type: 'run_started', run_id: 'r', prompt_summary: prompt, at: new Date().toISOString() } });
        }
        const s = { id, image: uri, status: 'created', created_at: new Date().toISOString(),
          last_active_at: new Date().toISOString(), events: ev };
        this.sessions = [s, ...this.sessions];
        forceUpdate((x) => x + 1);
        setTimeout(() => {
          s.status = 'active'; s.last_active_at = new Date().toISOString();
          if (prompt) {
            s.events = [...s.events,
              { idx: s.events.length, event: { type: 'agent_message', role: 'assistant', at: new Date().toISOString(), text: REPLIES[0] } },
              { idx: s.events.length + 1, event: { type: 'harness_idle' } }];
          }
          forceUpdate((x) => x + 1);
        }, 1400);
        return id;
      },
      sendPrompt(id, text) {
        const s = this.sessions.find((x) => x.id === id);
        if (!s) return;
        const base = s.events.length;
        // the prompt is carried by run_started.prompt_summary → rendered as the
        // user turn; a process kicks off in-flight so the context-aware waiting
        // verb reads "running cargo check…" and a live process line forms.
        const xid = 'x' + base;
        s.events = [...s.events.filter((e) => e.event.type !== 'harness_idle'),
          { idx: base, event: { type: 'run_started', run_id: 'r' + base, prompt_summary: text, at: new Date().toISOString() } },
          { idx: base + 1, event: { type: 'exec_started', exec_id: xid, command: ['cargo', 'check', '--workspace'], at: new Date().toISOString() } }];
        s.last_active_at = new Date().toISOString();
        if (s.status === 'idle') s.status = 'active';
        forceUpdate((x) => x + 1);
        // generous delay so the harness-waiting state (and the stop control) is
        // comfortably visible; real resumes are faster.
        this._pending[id] = setTimeout(() => {
          delete this._pending[id];
          const reply = REPLIES[1 + (this._replyN++ % (REPLIES.length - 1))];
          s.events = [...s.events,
            { idx: s.events.length, event: { type: 'stdout', exec_id: xid, chunk: '    Checking engram-core v0.1.0\n    Finished dev [unoptimized + debuginfo] in 4.20s\n' } },
            { idx: s.events.length + 1, event: { type: 'exec_completed', exec_id: xid, exit_status: 0, rusage: { duration_ms: 4200 }, at: new Date().toISOString() } },
            { idx: s.events.length + 2, event: { type: 'agent_message', role: 'assistant', at: new Date().toISOString(), text: reply } },
            { idx: s.events.length + 3, event: { type: 'run_completed', run_id: 'done', ok: true, at: new Date().toISOString() } },
            { idx: s.events.length + 4, event: { type: 'harness_idle' } }];
          s.last_active_at = new Date().toISOString();
          forceUpdate((x) => x + 1);
        }, 2600);
      },
      // operator interrupt — cf. the Agent SDK's query.interrupt(): stop the
      // in-flight run but keep the session alive. Clears the pending reply and
      // records the interruption in the transcript.
      interrupt(id) {
        const s = this.sessions.find((x) => x.id === id);
        if (!s) return;
        if (this._pending[id]) { clearTimeout(this._pending[id]); delete this._pending[id]; }
        s.events = [...s.events,
          { idx: s.events.length, event: { type: 'agent_message', role: 'system', at: new Date().toISOString(), text: 'Interrupted by operator — the harness stopped the current run. The session stays live; send a new prompt to continue.' } },
          { idx: s.events.length + 1, event: { type: 'run_completed', run_id: 'int', ok: false } },
          { idx: s.events.length + 2, event: { type: 'harness_idle' } }];
        s.last_active_at = new Date().toISOString();
        forceUpdate((x) => x + 1);
      },
      disableImage(imgId) { this.images = this.images.filter((i) => i.id !== imgId); forceUpdate((x) => x + 1); },
      enableImage(uri) {
        this.images = [...this.images, { id: uid('img-'), image_uri: uri, manifest_name: stripImageHost(uri),
          manifest_description: 'Enabled just now — manifest cached.', manifest_digest: 'sha256:' + uid('').slice(0, 16),
          harness_name: uri.includes('jobs') ? null : 'claude', last_refreshed_at: new Date().toISOString() }];
        forceUpdate((x) => x + 1);
      },
    };
    ['createSession', 'sendPrompt', 'interrupt', 'disableImage', 'enableImage'].forEach((m) => { ref.current[m] = ref.current[m].bind(ref.current); });
  }
  return ref.current;
}

Object.assign(window, { useStore, REPLIES });
