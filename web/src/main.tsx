import '@fontsource-variable/newsreader/opsz.css';
import '@fontsource-variable/newsreader/opsz-italic.css';
import '@fontsource-variable/jetbrains-mono/wght.css';
import '@fontsource-variable/jetbrains-mono/wght-italic.css';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import './theme.css';

// StrictMode intentionally double-mounts effects in dev. Useful in
// general, but it interacts badly with ghostty-web's WASM Terminal:
// the first mount opens a WS to ttyd, cleanup closes it, the second
// mount opens a fresh WS — bash sees two SIGWINCH-driven resize
// flurries on top of each other and the rendered output overlaps.
// Until we make TerminalPane fully StrictMode-idempotent, opt out
// at the root.
createRoot(document.getElementById('root')!).render(<App />);
