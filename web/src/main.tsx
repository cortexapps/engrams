// Newsreader and JetBrains Mono are both multi-axis variable fonts.
// Fontsource splits each into one CSS file per axis and *pins* the
// other axes, so importing Newsreader's `opsz` (optical-size) axis
// registers the face at `font-weight: 400` only — every serif element
// asked for at another weight (`.smallcaps` is 500, bold headings,
// `<strong>`) has no matching `@font-face` and falls down the stack to
// a generic serif. Import the `wght` axis on both faces so the full
// weight range is available (mirrors the JetBrains Mono imports).
import '@fontsource-variable/newsreader/wght.css';
import '@fontsource-variable/newsreader/wght-italic.css';
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
