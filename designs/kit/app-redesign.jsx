// app-redesign.jsx — root + router for the FOUR-SURFACE IA redesign.
// Surfaces: Sessions (default) · Fleet · Storage · Settings, on the NavSpine.
// Session detail drills in under Sessions. Reuses SessionDetail / Settings /
// NewSessionForm / ImagesPanel from screens.jsx and the shared store.jsx.
const { useState: useRA, useEffect: useRAEffect } = React;

const RA_TWEAKS = /*EDITMODE-BEGIN*/{
  "markStatus": true,
  "fleetView": "strata",
  "bootLoader": true,
  "harnessVerb": "thinking",
  "viewAs": "admin",
  "authState": "app"
}/*EDITMODE-END*/;

// the two identities the demo can render as, so admin & member views are
// reviewable side-by-side from the Tweaks panel.
const VIEW_PRINCIPALS = {
  admin: { name: 'Nikhil Unni', email: 'nikhil.unni@cortex.io', role: 'admin', is_admin: true, has_claude_token: true, can_sign_out: true, source: 'claim' },
  member: { name: 'Theo Park', email: 'theo.park@cortex.io', role: 'member', is_admin: false, has_claude_token: false, can_sign_out: true, source: 'claim' },
};

function RedesignApp() {
  const [, forceUpdate] = useRA(0);
  const store = useStore(forceUpdate);
  const [t, setTweak] = useTweaks(RA_TWEAKS);
  const [route, setRoute] = useRA({ name: 'sessions' });
  const go = (r) => { window.scrollTo(0, 0); setRoute(typeof r === 'string' ? { name: r } : r); };

  // "View as": swap the live principal so admin & member surfaces both render.
  // Mutating PRINCIPAL in place keeps the components that read it by reference
  // (NavSpine, Settings, SessionsSurface, UserChip) in sync on re-render.
  useRAEffect(() => {
    Object.assign(PRINCIPAL, VIEW_PRINCIPALS[t.viewAs] || VIEW_PRINCIPALS.admin);
    if ((t.viewAs !== 'admin') && (route.name === 'fleet' || route.name === 'storage')) go('sessions');
  }, [t.viewAs]);

  // living-status: poll tick + boot resolve (same demo behaviour as the kit)
  const [tick, setTick] = useRA(0);
  useRAEffect(() => { const h = setInterval(() => setTick((x) => x + 1), 3200); return () => clearInterval(h); }, []);
  useRAEffect(() => {
    const h = setTimeout(() => {
      let changed = false;
      store.sessions.forEach((s) => { if (s.id === '7c2e0a91b8d4' && s.status === 'created') { s.status = 'active'; changed = true; } });
      if (changed) forceUpdate((x) => x + 1);
    }, 5200);
    return () => clearTimeout(h);
  }, []);

  const booting = store.sessions.some((s) => s.status === 'created');
  const markMode = booting ? 'loop' : 'pulse';
  const active = route.name === 'session' ? 'sessions' : route.name;
  const crumb = route.name === 'session' ? shortId(route.id) : null;

  // pass a synthetic "masthead on" tweak so reused pages render chromeless
  const pageT = { masthead: true, bootLoader: t.bootLoader, overviewWidth: 'wide', harnessVerb: t.harnessVerb };

  let page;
  if (route.name === 'session') {
    page = <SessionDetail store={store} id={route.id} t={pageT}
      onBack={() => go('sessions')} onSettings={() => go('settings')} />;
  } else if (route.name === 'fleet') {
    page = <FleetSurface store={store} t={t} />;
  } else if (route.name === 'storage') {
    page = <StorageSurface store={store} />;
  } else if (route.name === 'settings') {
    page = <Settings store={store} t={pageT} onBack={() => go('sessions')} onSettings={() => go('settings')} />;
  } else {
    page = <SessionsSurface store={store} t={t} onOpen={(id) => go({ name: 'session', id })} onSettings={() => go('settings')} />;
  }

  // auth states render INSTEAD of the app shell — the pre-login screens.
  if (t.authState && t.authState !== 'app') {
    return (
      <>
        <AuthScreen state={t.authState} onRetry={() => setTweak('authState', 'app')} onSignOut={() => setTweak('authState', 'app')} />
        <TweaksPanel>
          <TweakSection label="Review" />
          <TweakRadio label="View as" value={t.viewAs} options={['admin', 'member']} onChange={(v) => setTweak('viewAs', v)} />
          <TweakSelect label="Auth state" value={t.authState} options={['app', 'boot', 'error', 'not a member']} onChange={(v) => setTweak('authState', v)} />
        </TweaksPanel>
      </>
    );
  }

  return (
    <>
      <NavSpine active={active} crumb={crumb}
        onNavigate={(id) => go(id)} onHome={() => go('sessions')}
        markMode={markMode} pulseKey={tick} animate={t.markStatus} />
      {page}
      <TweaksPanel>
        <TweakSection label="Review" />
        <TweakRadio label="View as" value={t.viewAs} options={['admin', 'member']} onChange={(v) => setTweak('viewAs', v)} />
        <TweakSelect label="Auth state" value={t.authState} options={['app', 'boot', 'error', 'not a member']} onChange={(v) => setTweak('authState', v)} />
        <TweakSection label="Redesign" />
        <TweakToggle label="Mark as live status" value={t.markStatus} onChange={(v) => setTweak('markStatus', v)} />
        <TweakRadio label="Fleet view" value={t.fleetView} options={['strata', 'table']} onChange={(v) => setTweak('fleetView', v)} />
        <TweakToggle label="Inline boot loaders" value={t.bootLoader} onChange={(v) => setTweak('bootLoader', v)} />
        <TweakRadio label="Harness-waiting verb" value={t.harnessVerb} options={['thinking', 'recalling', 'working']} onChange={(v) => setTweak('harnessVerb', v)} />
      </TweaksPanel>
    </>
  );
}

ReactDOM.createRoot(document.getElementById('root')).render(<RedesignApp />);
