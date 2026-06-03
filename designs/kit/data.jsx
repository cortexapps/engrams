// data.jsx — fake-but-realistic data store for the Engrams dashboard kit.
// Mirrors the shapes in cortexapps/engrams › web/src/types.ts, cosmetically.

// ---- helpers ---------------------------------------------------------------
function relativeTime(iso) {
  const t = new Date(iso).getTime();
  const dt = Math.max(0, (Date.now() - t) / 1000);
  if (dt < 60) return `${Math.floor(dt)}s`;
  if (dt < 3600) return `${Math.floor(dt / 60)}m`;
  if (dt < 86400) return `${Math.floor(dt / 3600)}h`;
  return `${Math.floor(dt / 86400)}d`;
}
function hms(iso) {
  return new Date(iso).toLocaleTimeString('en-GB', {
    hour: '2-digit', minute: '2-digit', second: '2-digit',
  });
}
function shortId(id) { return id.length <= 12 ? id : `${id.slice(0, 8)}…`; }
function stripImageHost(uri) {
  const slash = uri.indexOf('/');
  const colon = uri.lastIndexOf(':');
  const start = slash >= 0 ? slash + 1 : 0;
  const end = colon > start ? colon : uri.length;
  return uri.slice(start, end);
}
function ago(secs) { return new Date(Date.now() - secs * 1000).toISOString(); }
function fmtBytes(n) {
  if (n === 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let v = n, u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u += 1; }
  return `${v.toFixed(v >= 100 ? 0 : 1)} ${units[u]}`;
}
function uid(prefix) {
  const hex = Array.from({ length: 12 }, () =>
    '0123456789abcdef'[Math.floor(Math.random() * 16)]).join('');
  return `${prefix}${hex}`;
}

// ---- hosts -----------------------------------------------------------------
const HOSTS = [
  { id: 'fc-ord-a91c3f0e', status: 'ready', running_sandboxes: 9,
    capacity_used_mib: 38912, capacity_total_mib: 65536, local_snapshots: 21 },
  { id: 'fc-ord-7b2d0a14', status: 'ready', running_sandboxes: 3,
    capacity_used_mib: 14336, capacity_total_mib: 65536, local_snapshots: 18 },
  { id: 'fc-iad-3e88c220', status: 'draining', running_sandboxes: 0,
    capacity_used_mib: 0, capacity_total_mib: 32768, local_snapshots: 8 },
];

// ---- enabled images --------------------------------------------------------
const IMAGES = [
  { id: 'img-1', image_uri: 'ghcr.io/cortex/api:warm-1', manifest_name: 'cortex-api',
    manifest_description: 'Cortex API service — node 20, pnpm, prisma migrated.',
    manifest_digest: 'sha256:9f2a4c1b8e3d6072', harness_name: 'claude',
    last_refreshed_at: ago(60 * 14) },
  { id: 'img-2', image_uri: 'ghcr.io/cortex/web:warm-3', manifest_name: 'cortex-web',
    manifest_description: 'Cortex dashboard frontend — vite, tailwind v4.',
    manifest_digest: 'sha256:1c77e0d4a9b32f56', harness_name: 'claude',
    last_refreshed_at: ago(60 * 47) },
  { id: 'img-3', image_uri: 'ghcr.io/cortex/jobs:warm-2', manifest_name: 'cortex-jobs',
    manifest_description: 'Batch billing job runner.',
    manifest_digest: 'sha256:b830aa2f1d6c9e44', harness_name: null,
    last_refreshed_at: ago(60 * 60 * 5) },
];

// ---- a fully-built transcript for the seeded "active" session --------------
function seededEvents() {
  let i = 0;
  const e = (event) => ({ idx: i++, event });
  return [
    e({ type: 'run_started', run_id: 'r1', prompt_summary: 'fix the flaky snapshot-restore test under UFFD' }),
    e({ type: 'agent_message', role: 'assistant', at: ago(214),
        text: 'Looking at the failing test now. The flake is in snapshot_uffd — it asserts the restored guest sees the same memory.bin offset before the UFFD handler has finished prefaulting the working set.' }),
    e({ type: 'tool_call_started', tool_call_id: 'tc1', tool_name: 'read_file',
        args_summary: 'crates/engram-sandbox-firecracker/tests/snapshot_uffd.rs' }),
    e({ type: 'tool_call_completed', tool_call_id: 'tc1', tool_name: 'read_file',
        ok: true, duration_ms: 11, result_summary: '212 lines' }),
    e({ type: 'agent_message', role: 'assistant', at: ago(196),
        text: 'Confirmed. The test reads a page before restore() returns, but the UFFD handler populates lazily — so the first read races the copy. I\'ll await the prefault-complete signal the handler already emits on its ready UDS instead of sleeping a fixed 50ms.' }),
    e({ type: 'tool_call_started', tool_call_id: 'tc2', tool_name: 'edit_file',
        args_summary: 'snapshot_uffd.rs — replace sleep(50ms) with await ready_signal' }),
    e({ type: 'tool_call_completed', tool_call_id: 'tc2', tool_name: 'edit_file',
        ok: true, duration_ms: 7, result_summary: '1 hunk, +6 −2' }),
    e({ type: 'exec_started', exec_id: 'x1', at: ago(120),
        command: ['cargo', 'nextest', 'run', '-p', 'engram-sandbox-firecracker', 'snapshot_uffd'] }),
    e({ type: 'stdout', exec_id: 'x1', chunk: '    Finished test [optimized + debuginfo] target(s) in 0.71s\n    Starting 1 test across 1 binary\n' }),
    e({ type: 'stdout', exec_id: 'x1', chunk: 'PASS [  18.420s] engram-sandbox-firecracker snapshot_uffd::restores_under_uffd\n    Summary [  18.421s] 1 test run: 1 passed, 0 skipped\n' }),
    e({ type: 'exec_completed', exec_id: 'x1', exit_status: 0, rusage: { duration_ms: 18420 }, at: ago(101) }),
    e({ type: 'agent_message', role: 'assistant', at: ago(20),
        text: 'Green, and re-ran 20× with no flake. The fix:\n\n- replaced the fixed `sleep(50ms)` with an `await` on the handler\'s **prefault-complete** signal\n- the test now blocks on the ready UDS instead of racing the copy\n\n```rust\nlet ready = handler.ready_signal().await?;\nassert_eq!(guest.read_page(0)?, baseline);\n```\n\nOpening a PR against `main`.' }),
    e({ type: 'pull_request_opened', at: ago(8), url: '#', repo: 'cortex/api',
        title: 'Fix flaky snapshot-restore test under UFFD', number: 482,
        head_branch: 'engram/fix-flaky-restore', base_branch: 'main' }),
    e({ type: 'run_completed', run_id: 'r1', ok: true }),
    e({ type: 'snapshot_taken', snapshot_id: 'snap-1', size_bytes: 1287000000, at: ago(6) }),
    e({ type: 'harness_idle' }),
  ];
}

// ---- the session list ------------------------------------------------------
function seedSessions() {
  return [
    { id: 'a3f9c1b240e7', image: 'ghcr.io/cortex/api:warm-1', status: 'active',
      owner_kind: 'user', owner_name: 'Nikhil Unni', owner_email: 'nikhil.unni@cortex.io',
      created_at: ago(240), last_active_at: ago(8), events: seededEvents() },
    { id: '7c2e0a91b8d4', image: 'ghcr.io/cortex/web:warm-3', status: 'active',
      owner_kind: 'system', owner_label: 'automated',
      created_at: ago(14), last_active_at: ago(2), events: [] },
    { id: 'b8d4f33027ac', image: 'ghcr.io/cortex/api:warm-1', status: 'idle',
      owner_kind: 'system', owner_label: 'warm pool',
      created_at: ago(60 * 14), last_active_at: ago(19), events: [] },
    { id: 'd1e8c459a07b', image: 'ghcr.io/cortex/api:warm-1', status: 'idle',
      owner_kind: 'user', owner_name: 'Theo Park', owner_email: 'theo.park@cortex.io',
      created_at: ago(60 * 9), last_active_at: ago(60 * 4), events: [] },
    { id: '4e1a9c77f201', image: 'ghcr.io/cortex/web:warm-3', status: 'completed',
      owner_kind: 'user', owner_name: 'Nikhil Unni', owner_email: 'nikhil.unni@cortex.io',
      created_at: ago(60 * 38), last_active_at: ago(60 * 31), events: [] },
    { id: '1f6a9e22d3b8', image: 'ghcr.io/cortex/jobs:warm-2', status: 'dead',
      owner_kind: 'user', owner_name: 'Dana Schuman', owner_email: 'dana.schuman@cortex.io',
      created_at: ago(60 * 90), last_active_at: ago(60 * 61), events: [] },
  ];
}

Object.assign(window, {
  relativeTime, hms, shortId, stripImageHost, ago, fmtBytes, uid,
  HOSTS, IMAGES, seedSessions, seededEvents,
});
