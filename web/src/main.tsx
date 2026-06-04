// JetBrains Mono is a multi-axis variable font; Fontsource splits it into one
// CSS file per axis. Import the `wght` axis so the full weight range is
// available to anything using `--font-mono` (code, IDs, tabular numbers).
import '@fontsource-variable/jetbrains-mono/wght.css';
import '@fontsource-variable/jetbrains-mono/wght-italic.css';
// IBM Plex Serif is the serif identity voice (page/section titles, prose) —
// a transitional/slab register that reads "engineering instrument", not
// "magazine". Static weights: 400 for prose, 500 for titles (font-medium),
// plus 400 italic for the logbook register on notes.
import '@fontsource/ibm-plex-serif/400.css';
import '@fontsource/ibm-plex-serif/500.css';
import '@fontsource/ibm-plex-serif/400-italic.css';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { ThemeProvider } from './components/theme-provider';
// index.css pulls in Tailwind once and @imports theme.css (the Lab-Notebook
// partial kept for the not-yet-migrated SessionDetail transcript subtree).
import './index.css';

// StrictMode intentionally double-mounts effects in dev. Useful in
// general, but it interacts badly with ghostty-web's WASM Terminal:
// the first mount opens a WS to ttyd, cleanup closes it, the second
// mount opens a fresh WS — bash sees two SIGWINCH-driven resize
// flurries on top of each other and the rendered output overlaps.
// Until we make TerminalPane fully StrictMode-idempotent, opt out
// at the root.
createRoot(document.getElementById('root')!).render(
  <ThemeProvider>
    <App />
  </ThemeProvider>,
);
