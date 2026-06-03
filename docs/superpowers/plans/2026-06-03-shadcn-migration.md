# Pure shadcn Migration — engrams-web Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild the `web/` frontend on pure shadcn/ui — stock components, default zinc theme with a dark-mode toggle, default sans typography, and a dual-sidebar IA (primary destinations rail + per-section second sidebar via nested TanStack layout routes). SessionDetail/transcript is excluded.

**Architecture:** Initialise shadcn for Vite + Tailwind v4. Add a class-based `ThemeProvider`. Replace the top `NavSpine` masthead with a shadcn `Sidebar` shell (`RootLayout`). Add nested **layout routes** `/sessions` and `/settings` (code-based router) that each render their own section sidebar + `<Outlet/>`; Fleet/Storage render full-bleed. Rebuild each in-scope surface with stock shadcn components, reusing all existing hooks/`api.ts`/`types.ts` unchanged. Keep `theme.css` imported so the untouched SessionDetail still renders (an accepted "old-paper island" until the later assistant-ui migration).

**Tech Stack:** Vite 6 · React 19 · Tailwind v4 (`@tailwindcss/vite`) · TanStack Router (code-based) · TanStack Query · TanStack Table · shadcn/ui · lucide-react · Vitest + Testing Library.

**Spec:** `docs/superpowers/specs/2026-06-03-shadcn-migration-design.md`

---

## Conventions for every task

- All commands run from `web/` unless noted. The repo blocks compound `cd`; run shadcn/npm commands with the shell already in `web/`.
- Package manager: this project uses **pnpm** (see `engram-web-pnpm` note / lockfile). Use `pnpm` / `pnpm dlx`, not `npm`.
- After each task: `pnpm exec tsc -b --noEmit` must pass and `pnpm test` must stay green, then commit.
- Existing hooks (`useSessions`, `useHosts`, `useStorageSummary`, `useDrainHost`, `useEnabledImages`, `useEnableProgress`, `useRegistries`), `api.ts`, `types.ts`, and `auth/AuthProvider.tsx` are **reused unchanged** unless a task says otherwise. Only presentation changes.
- "Excluded — do not touch": `pages/SessionDetail.tsx`, `components/Transcript.tsx`, `transcriptFmt.ts`, `ToolCall.tsx`, `Process.tsx`, `RunSummary.tsx`, `RunBoundary.tsx`, `UserTurn.tsx`, `Markdown.tsx`, `HarnessWaiting.tsx`, `PromptComposer.tsx`, `TerminalPane.tsx`, `ArtifactCard.tsx`, `PullRequestCard.tsx`, `DurabilityMarker.tsx`, `CowState.tsx`, `VitalSigns.tsx`, `Glyph.tsx`, and their tests.

---

## File Structure

**Created:**
- `web/components.json` — shadcn config
- `web/src/lib/utils.ts` — `cn()`
- `web/src/index.css` — shadcn zinc tokens (`:root`/`.dark`), `@theme inline`, `@import "tailwindcss"`, animate import, `--font-mono`
- `web/src/components/ui/*` — generated shadcn primitives
- `web/src/components/theme-provider.tsx` — class-based theme context
- `web/src/components/mode-toggle.tsx` — light/dark toggle button
- `web/src/components/app-sidebar.tsx` — primary destinations sidebar (`MainSidebar`)
- `web/src/components/user-menu.tsx` — sidebar-footer user dropdown (replaces `UserChip`)
- `web/src/pages/RootLayout.tsx` — `SidebarProvider` + `MainSidebar` + header + `<Outlet/>`
- `web/src/pages/sessions/SessionsLayout.tsx` — sessions section sidebar + `<Outlet/>`
- `web/src/pages/sessions/sessions-columns.tsx` — TanStack Table column defs + grouped table
- `web/src/pages/sessions/MySessions.tsx`, `web/src/pages/sessions/AllSessions.tsx`
- `web/src/components/NewSessionDialog.tsx` — replaces `NewSessionForm.tsx`
- `web/src/pages/settings/SettingsLayout.tsx` — settings section sidebar + `<Outlet/>`
- `web/src/test-setup.ts` — jsdom `matchMedia`/`ResizeObserver` mocks

**Modified:**
- `web/src/main.tsx`, `web/index.html`, `web/vite.config.ts`, `web/tsconfig.json`
- `web/src/router.tsx`, `web/src/pages/Layout.tsx` (→ replaced by RootLayout)
- `web/src/pages/Fleet.tsx`, `web/src/pages/Storage.tsx`, `web/src/pages/Settings.tsx`, `web/src/pages/Members.tsx`
- `web/src/components/settings/ProfilePanel.tsx`, `TokensPanel.tsx`, `ImagesPanel.tsx`, `RegistriesPanel.tsx`
- Tests: `NewSessionForm.test.tsx` → `NewSessionDialog.test.tsx`, `ImagesPanel.test.tsx`, `RegistriesPanel.test.tsx`, `UserChip.test.tsx` → `user-menu.test.tsx`

**Deleted (Phase 6, after their surfaces are rebuilt):** `components/NavSpine.tsx`, `VitalStrip.tsx`, `ManifestGroup.tsx`, `SessionManifest.tsx`, `TabRow.tsx`, `SectionHead.tsx`, `UserChip.tsx`, `settings/_form.tsx`, the now-unused exports of `Identity.tsx`, `pages/Sessions.tsx`, `pages/Layout.tsx`.

---

# Phase 0 — Foundation

### Task 1: Init shadcn (deps, alias, cn util)

**Files:**
- Modify: `web/tsconfig.json`, `web/vite.config.ts`
- Create: `web/src/lib/utils.ts`, `web/components.json`

- [ ] **Step 1: Add the `@/` path alias to tsconfig**

In `web/tsconfig.json`, add to `compilerOptions`:

```jsonc
"baseUrl": ".",
"paths": { "@/*": ["./src/*"] }
```

- [ ] **Step 2: Add the alias to Vite**

In `web/vite.config.ts`, add the import and `resolve.alias`:

```ts
import path from 'node:path';
// ...
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { '@': path.resolve(__dirname, './src') } },
  server: { /* unchanged */ },
  test: { /* unchanged */ },
});
```

- [ ] **Step 3: Install runtime deps**

Run (shell in `web/`):
```bash
pnpm add class-variance-authority clsx tailwind-merge lucide-react tw-animate-css @tanstack/react-table
```
Expected: deps added to `package.json`, no errors.

- [ ] **Step 4: Create the `cn` util**

`web/src/lib/utils.ts`:
```ts
import { clsx, type ClassValue } from 'clsx';
import { twMerge } from 'tailwind-merge';

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
```

- [ ] **Step 5: Create `components.json`**

`web/components.json` (Tailwind v4 uses `"css"` not `"config"`; `"cssVariables": true`):
```json
{
  "$schema": "https://ui.shadcn.com/schema.json",
  "style": "new-york",
  "rsc": false,
  "tsx": true,
  "tailwind": {
    "config": "",
    "css": "src/index.css",
    "baseColor": "zinc",
    "cssVariables": true,
    "prefix": ""
  },
  "iconLibrary": "lucide",
  "aliases": {
    "components": "@/components",
    "utils": "@/lib/utils",
    "ui": "@/components/ui",
    "lib": "@/lib",
    "hooks": "@/hooks"
  }
}
```

- [ ] **Step 6: Commit**
```bash
git add web/tsconfig.json web/vite.config.ts web/src/lib/utils.ts web/components.json web/package.json web/pnpm-lock.yaml
git commit -m "build(web): scaffold shadcn (alias, cn util, components.json)"
```

---

### Task 2: shadcn zinc theme CSS + main.tsx wiring

**Files:**
- Create: `web/src/index.css`
- Modify: `web/src/main.tsx`, `web/index.html`

- [ ] **Step 1: Write `web/src/index.css`**

This is the shadcn zinc token set (Tailwind v4 form). Keep `theme.css` separate and still imported (Task done in main.tsx). `--font-mono` keeps JetBrains Mono for code/IDs.

```css
@import "tailwindcss";
@import "tw-animate-css";

@custom-variant dark (&:is(.dark *));

:root {
  --radius: 0.625rem;
  --background: oklch(1 0 0);
  --foreground: oklch(0.141 0.005 285.823);
  --card: oklch(1 0 0);
  --card-foreground: oklch(0.141 0.005 285.823);
  --popover: oklch(1 0 0);
  --popover-foreground: oklch(0.141 0.005 285.823);
  --primary: oklch(0.21 0.006 285.885);
  --primary-foreground: oklch(0.985 0 0);
  --secondary: oklch(0.967 0.001 286.375);
  --secondary-foreground: oklch(0.21 0.006 285.885);
  --muted: oklch(0.967 0.001 286.375);
  --muted-foreground: oklch(0.552 0.016 285.938);
  --accent: oklch(0.967 0.001 286.375);
  --accent-foreground: oklch(0.21 0.006 285.885);
  --destructive: oklch(0.577 0.245 27.325);
  --border: oklch(0.92 0.004 286.32);
  --input: oklch(0.92 0.004 286.32);
  --ring: oklch(0.705 0.015 286.067);
  --sidebar: oklch(0.985 0 0);
  --sidebar-foreground: oklch(0.141 0.005 285.823);
  --sidebar-primary: oklch(0.21 0.006 285.885);
  --sidebar-primary-foreground: oklch(0.985 0 0);
  --sidebar-accent: oklch(0.967 0.001 286.375);
  --sidebar-accent-foreground: oklch(0.21 0.006 285.885);
  --sidebar-border: oklch(0.92 0.004 286.32);
  --sidebar-ring: oklch(0.705 0.015 286.067);
  --font-mono: "JetBrains Mono Variable", ui-monospace, monospace;
}

.dark {
  --background: oklch(0.141 0.005 285.823);
  --foreground: oklch(0.985 0 0);
  --card: oklch(0.21 0.006 285.885);
  --card-foreground: oklch(0.985 0 0);
  --popover: oklch(0.21 0.006 285.885);
  --popover-foreground: oklch(0.985 0 0);
  --primary: oklch(0.92 0.004 286.32);
  --primary-foreground: oklch(0.21 0.006 285.885);
  --secondary: oklch(0.274 0.006 286.033);
  --secondary-foreground: oklch(0.985 0 0);
  --muted: oklch(0.274 0.006 286.033);
  --muted-foreground: oklch(0.705 0.015 286.067);
  --accent: oklch(0.274 0.006 286.033);
  --accent-foreground: oklch(0.985 0 0);
  --destructive: oklch(0.704 0.191 22.216);
  --border: oklch(1 0 0 / 10%);
  --input: oklch(1 0 0 / 15%);
  --ring: oklch(0.552 0.016 285.938);
  --sidebar: oklch(0.21 0.006 285.885);
  --sidebar-foreground: oklch(0.985 0 0);
  --sidebar-primary: oklch(0.488 0.243 264.376);
  --sidebar-primary-foreground: oklch(0.985 0 0);
  --sidebar-accent: oklch(0.274 0.006 286.033);
  --sidebar-accent-foreground: oklch(0.985 0 0);
  --sidebar-border: oklch(1 0 0 / 10%);
  --sidebar-ring: oklch(0.552 0.016 285.938);
}

@theme inline {
  --color-background: var(--background);
  --color-foreground: var(--foreground);
  --color-card: var(--card);
  --color-card-foreground: var(--card-foreground);
  --color-popover: var(--popover);
  --color-popover-foreground: var(--popover-foreground);
  --color-primary: var(--primary);
  --color-primary-foreground: var(--primary-foreground);
  --color-secondary: var(--secondary);
  --color-secondary-foreground: var(--secondary-foreground);
  --color-muted: var(--muted);
  --color-muted-foreground: var(--muted-foreground);
  --color-accent: var(--accent);
  --color-accent-foreground: var(--accent-foreground);
  --color-destructive: var(--destructive);
  --color-border: var(--border);
  --color-input: var(--input);
  --color-ring: var(--ring);
  --color-sidebar: var(--sidebar);
  --color-sidebar-foreground: var(--sidebar-foreground);
  --color-sidebar-primary: var(--sidebar-primary);
  --color-sidebar-primary-foreground: var(--sidebar-primary-foreground);
  --color-sidebar-accent: var(--sidebar-accent);
  --color-sidebar-accent-foreground: var(--sidebar-accent-foreground);
  --color-sidebar-border: var(--sidebar-border);
  --color-sidebar-ring: var(--sidebar-ring);
  --font-mono: var(--font-mono);
  --radius-sm: calc(var(--radius) - 4px);
  --radius-md: calc(var(--radius) - 2px);
  --radius-lg: var(--radius);
  --radius-xl: calc(var(--radius) + 4px);
}

@layer base {
  * { @apply border-border outline-ring/50; }
  body { @apply bg-background text-foreground; }
}
```

- [ ] **Step 2: Rewrite `web/src/main.tsx` imports**

Drop Newsreader; keep JetBrains Mono (used by `--font-mono`). Import `index.css` first, then keep `theme.css` for the excluded SessionDetail island. Wrap `<App/>` in `ThemeProvider` (created next task — leave the import; Task 3 creates the file, so do Step 2 of this task *after* Task 3, OR temporarily render `<App/>` and add the provider in Task 3 Step 4). To keep tasks independently green, **here just fix the CSS/font imports**:

```tsx
import '@fontsource-variable/jetbrains-mono/wght.css';
import '@fontsource-variable/jetbrains-mono/wght-italic.css';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import './index.css';
import './theme.css';

createRoot(document.getElementById('root')!).render(<App />);
```

- [ ] **Step 3: Update `index.html` theme-color + title stays**

In `web/index.html`, change the meta theme-color from paper to a neutral that matches zinc light bg:
```html
<meta name="theme-color" content="#ffffff" />
```

- [ ] **Step 4: Verify dev build compiles**

Run: `pnpm exec tsc -b --noEmit`
Expected: PASS (no type errors). Run `pnpm test` — existing tests still green (theme.css still present).

- [ ] **Step 5: Commit**
```bash
git add web/src/index.css web/src/main.tsx web/index.html
git commit -m "feat(web): add shadcn zinc theme css; drop Newsreader, keep JetBrains mono"
```

---

### Task 3: ThemeProvider + mode toggle (TDD)

**Files:**
- Create: `web/src/components/theme-provider.tsx`, `web/src/components/mode-toggle.tsx`, `web/src/test-setup.ts`
- Modify: `web/vite.config.ts` (register setup file), `web/src/main.tsx`
- Test: `web/src/components/mode-toggle.test.tsx`

- [ ] **Step 1: Create the jsdom test-setup mocks**

shadcn `sidebar` and theme code touch `matchMedia`/`ResizeObserver`, absent in jsdom.

`web/src/test-setup.ts`:
```ts
import { afterEach } from 'vitest';
import { cleanup } from '@testing-library/react';

afterEach(() => cleanup());

if (!window.matchMedia) {
  window.matchMedia = (query: string) =>
    ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }) as unknown as MediaQueryList;
}

if (!('ResizeObserver' in window)) {
  // @ts-expect-error minimal stub
  window.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}
```

Register it in `web/vite.config.ts` `test` block:
```ts
test: {
  environment: 'jsdom',
  globals: false,
  setupFiles: ['./src/test-setup.ts'],
  include: ['src/**/*.{test,spec}.{ts,tsx}'],
},
```

- [ ] **Step 2: Write the failing test**

`web/src/components/mode-toggle.test.tsx`:
```tsx
import { expect, test } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ThemeProvider } from './theme-provider';
import { ModeToggle } from './mode-toggle';

test('toggles the dark class on the document root', async () => {
  document.documentElement.classList.remove('dark');
  render(
    <ThemeProvider>
      <ModeToggle />
    </ThemeProvider>,
  );
  const btn = screen.getByRole('button', { name: /toggle theme/i });
  expect(document.documentElement.classList.contains('dark')).toBe(false);
  await userEvent.click(btn);
  expect(document.documentElement.classList.contains('dark')).toBe(true);
});
```

- [ ] **Step 3: Run test to verify it fails**

Run: `pnpm exec vitest run src/components/mode-toggle.test.tsx`
Expected: FAIL — modules not found.

- [ ] **Step 4: Implement `theme-provider.tsx`**

```tsx
import { createContext, useContext, useEffect, useState, type ReactNode } from 'react';

type Theme = 'light' | 'dark';
interface ThemeCtx { theme: Theme; setTheme: (t: Theme) => void; toggle: () => void }
const STORAGE_KEY = 'engrams-theme';
const ThemeContext = createContext<ThemeCtx | null>(null);

function initialTheme(): Theme {
  if (typeof localStorage !== 'undefined') {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved === 'light' || saved === 'dark') return saved;
  }
  return 'light';
}

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [theme, setThemeState] = useState<Theme>(initialTheme);

  useEffect(() => {
    const root = document.documentElement;
    root.classList.toggle('dark', theme === 'dark');
    try { localStorage.setItem(STORAGE_KEY, theme); } catch { /* ignore */ }
  }, [theme]);

  const setTheme = (t: Theme) => setThemeState(t);
  const toggle = () => setThemeState((t) => (t === 'dark' ? 'light' : 'dark'));
  return (
    <ThemeContext.Provider value={{ theme, setTheme, toggle }}>
      {children}
    </ThemeContext.Provider>
  );
}

export function useTheme(): ThemeCtx {
  const ctx = useContext(ThemeContext);
  if (!ctx) throw new Error('useTheme must be used within ThemeProvider');
  return ctx;
}
```

- [ ] **Step 5: Implement `mode-toggle.tsx`** (uses shadcn Button added in Task 4 — for now use a plain button, swap to `Button` in Task 5)

```tsx
import { Moon, Sun } from 'lucide-react';
import { useTheme } from './theme-provider';

export function ModeToggle() {
  const { theme, toggle } = useTheme();
  return (
    <button
      type="button"
      aria-label="Toggle theme"
      onClick={toggle}
      className="inline-flex size-8 items-center justify-center rounded-md hover:bg-sidebar-accent"
    >
      {theme === 'dark' ? <Sun className="size-4" /> : <Moon className="size-4" />}
    </button>
  );
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `pnpm exec vitest run src/components/mode-toggle.test.tsx`
Expected: PASS.

- [ ] **Step 7: Wrap the app in ThemeProvider**

In `web/src/main.tsx`, wrap `<App/>`:
```tsx
import { ThemeProvider } from './components/theme-provider';
// ...
createRoot(document.getElementById('root')!).render(
  <ThemeProvider>
    <App />
  </ThemeProvider>,
);
```

- [ ] **Step 8: Commit**
```bash
git add web/src/components/theme-provider.tsx web/src/components/mode-toggle.tsx web/src/components/mode-toggle.test.tsx web/src/test-setup.ts web/vite.config.ts web/src/main.tsx
git commit -m "feat(web): class-based ThemeProvider + dark-mode toggle"
```

---

### Task 4: Add shadcn primitives

**Files:** Create: `web/src/components/ui/*`

- [ ] **Step 1: Pull components via CLI**

Run (shell in `web/`):
```bash
pnpm dlx shadcn@latest add sidebar button card table badge tabs dialog dropdown-menu select input label form progress tooltip separator skeleton sonner alert-dialog avatar breadcrumb
```
Expected: files written under `src/components/ui/`; CLI may also add `@radix-ui/*`, `react-hook-form`, `@hookform/resolvers`, `zod`, `cmdk`, `vaul`, `next-themes`(unused), `sonner`. Accept the writes. The `sidebar` add also creates `src/hooks/use-mobile.ts` and adds sidebar CSS vars (already present from Task 2 — keep ours).

- [ ] **Step 2: Verify compile**

Run: `pnpm exec tsc -b --noEmit`
Expected: PASS. If `ui/*` files import `@/lib/utils`, the alias resolves. Run `pnpm test` — green.

- [ ] **Step 3: Point ModeToggle at shadcn Button**

Update `web/src/components/mode-toggle.tsx` to use `Button`:
```tsx
import { Moon, Sun } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useTheme } from './theme-provider';

export function ModeToggle() {
  const { theme, toggle } = useTheme();
  return (
    <Button variant="ghost" size="icon" aria-label="Toggle theme" onClick={toggle}>
      {theme === 'dark' ? <Sun className="size-4" /> : <Moon className="size-4" />}
    </Button>
  );
}
```
Run the toggle test again: `pnpm exec vitest run src/components/mode-toggle.test.tsx` → PASS.

- [ ] **Step 4: Commit**
```bash
git add web/src/components/ui web/src/hooks/use-mobile.ts web/package.json web/pnpm-lock.yaml web/src/components/mode-toggle.tsx
git commit -m "build(web): add shadcn ui primitives"
```

---

# Phase 1 — Shell + routing

### Task 5: User menu (sidebar footer dropdown)

**Files:**
- Create: `web/src/components/user-menu.tsx`, `web/src/components/user-menu.test.tsx`
- Reuses: `useAuth`, `logout` (`api.ts`)

- [ ] **Step 1: Write the failing test** (port the intent of `UserChip.test.tsx`)

`web/src/components/user-menu.test.tsx`:
```tsx
import { expect, test } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { SidebarProvider } from '@/components/ui/sidebar';
import { renderWithProviders } from '../test-utils';
import { UserMenu } from './user-menu';

test('shows the signed-in email and a settings link when opened', async () => {
  renderWithProviders(
    <SidebarProvider>
      <UserMenu />
    </SidebarProvider>,
  );
  await userEvent.click(screen.getByRole('button', { name: /local admin/i }));
  expect(await screen.findByText('dev@engram.local')).toBeTruthy();
  expect(screen.getByRole('menuitem', { name: /settings/i })).toBeTruthy();
});
```
(`renderWithProviders` seeds the default admin principal `Local Admin` / `dev@engram.local`.)

- [ ] **Step 2: Run → FAIL** (`pnpm exec vitest run src/components/user-menu.test.tsx`) — module missing.

- [ ] **Step 3: Implement `user-menu.tsx`**

```tsx
import { ChevronsUpDown, LogOut, Settings } from 'lucide-react';
import { useNavigate } from '@tanstack/react-router';
import { logout } from '../api';
import { useAuth } from '../auth/AuthProvider';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem,
  DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  SidebarMenu, SidebarMenuButton, SidebarMenuItem,
} from '@/components/ui/sidebar';

export function UserMenu() {
  const { principal } = useAuth();
  const navigate = useNavigate();
  const label = principal.display_name || principal.email;
  const initial = label.charAt(0).toUpperCase();

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <SidebarMenuButton size="lg" aria-label={label}>
              <Avatar className="size-8 rounded-md">
                <AvatarFallback className="rounded-md">{initial}</AvatarFallback>
              </Avatar>
              <div className="grid flex-1 text-left text-sm leading-tight">
                <span className="truncate font-medium">{label}</span>
                <span className="truncate text-xs text-muted-foreground">
                  {principal.email}
                </span>
              </div>
              <ChevronsUpDown className="ml-auto size-4" />
            </SidebarMenuButton>
          </DropdownMenuTrigger>
          <DropdownMenuContent side="right" align="end" className="min-w-56">
            <DropdownMenuLabel className="font-normal">
              <div className="grid text-sm">
                <span className="font-medium">{label}</span>
                <span className="text-xs text-muted-foreground">{principal.email}</span>
              </div>
            </DropdownMenuLabel>
            <DropdownMenuSeparator />
            <DropdownMenuItem onClick={() => navigate({ to: '/settings/profile' })}>
              <Settings /> Settings
            </DropdownMenuItem>
            {principal.can_sign_out && (
              <DropdownMenuItem onClick={() => void logout()}>
                <LogOut /> Sign out
              </DropdownMenuItem>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}
```

- [ ] **Step 4: Run → PASS.** Then `pnpm exec tsc -b --noEmit` → PASS.

- [ ] **Step 5: Commit**
```bash
git add web/src/components/user-menu.tsx web/src/components/user-menu.test.tsx
git commit -m "feat(web): sidebar user menu (replaces UserChip popover)"
```

---

### Task 6: MainSidebar (primary destinations rail)

**Files:**
- Create: `web/src/components/app-sidebar.tsx`, `web/src/components/app-sidebar.test.tsx`
- Reuses: `useIsAdmin`, `useRouterState`, `EngramMark`, `UserMenu`, `ModeToggle`

- [ ] **Step 1: Write the failing test**

`web/src/components/app-sidebar.test.tsx`:
```tsx
import { expect, test } from 'vitest';
import { screen } from '@testing-library/react';
import { SidebarProvider } from '@/components/ui/sidebar';
import { renderWithProviders } from '../test-utils';
import { MainSidebar } from './app-sidebar';

test('admin sees all four destinations', () => {
  renderWithProviders(
    <SidebarProvider>
      <MainSidebar />
    </SidebarProvider>,
  );
  for (const label of ['Sessions', 'Fleet', 'Storage', 'Settings']) {
    expect(screen.getByRole('link', { name: new RegExp(label, 'i') })).toBeTruthy();
  }
});

test('member does not see Fleet or Storage', () => {
  renderWithProviders(
    <SidebarProvider>
      <MainSidebar />
    </SidebarProvider>,
    { principal: {
      email: 'm@e.local', display_name: 'Mem', role: 'member',
      is_admin: false, has_claude_token: true, can_sign_out: false,
    } },
  );
  expect(screen.queryByRole('link', { name: /Fleet/i })).toBeNull();
  expect(screen.getByRole('link', { name: /Sessions/i })).toBeTruthy();
});
```

- [ ] **Step 2: Run → FAIL.**

- [ ] **Step 3: Implement `app-sidebar.tsx`**

```tsx
import { Boxes, HardDrive, Layers, Settings } from 'lucide-react';
import { Link, useRouterState, type LinkProps } from '@tanstack/react-router';
import { useIsAdmin } from '../auth/AuthProvider';
import { EngramMark } from './EngramMark';
import { ModeToggle } from './mode-toggle';
import { UserMenu } from './user-menu';
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupContent,
  SidebarHeader, SidebarMenu, SidebarMenuButton, SidebarMenuItem,
} from '@/components/ui/sidebar';

interface Dest {
  to: LinkProps['to'];
  label: string;
  icon: typeof Boxes;
  adminOnly: boolean;
  match: (p: string) => boolean;
}

const DESTS: Dest[] = [
  { to: '/sessions', label: 'Sessions', icon: Layers, adminOnly: false, match: (p) => p === '/' || p.startsWith('/sessions') },
  { to: '/fleet', label: 'Fleet', icon: Boxes, adminOnly: true, match: (p) => p.startsWith('/fleet') },
  { to: '/storage', label: 'Storage', icon: HardDrive, adminOnly: true, match: (p) => p.startsWith('/storage') },
  { to: '/settings', label: 'Settings', icon: Settings, adminOnly: false, match: (p) => p.startsWith('/settings') },
];

export function MainSidebar() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const dests = DESTS.filter((d) => !d.adminOnly || isAdmin);

  return (
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton asChild size="lg">
              <Link to="/sessions" aria-label="engrams — sessions">
                <span className="flex aspect-square size-8 items-center justify-center">
                  <EngramMark size={26} mode="static" />
                </span>
                <span className="font-semibold">engrams</span>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              {dests.map((d) => (
                <SidebarMenuItem key={d.label}>
                  <SidebarMenuButton asChild isActive={d.match(pathname)} tooltip={d.label}>
                    <Link to={d.to}>
                      <d.icon />
                      <span>{d.label}</span>
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter>
        <div className="flex items-center justify-between gap-2 px-1 group-data-[collapsible=icon]:flex-col">
          <ModeToggle />
        </div>
        <UserMenu />
      </SidebarFooter>
    </Sidebar>
  );
}
```

- [ ] **Step 4: Run → PASS.** `pnpm exec tsc -b --noEmit` → PASS.

- [ ] **Step 5: Commit**
```bash
git add web/src/components/app-sidebar.tsx web/src/components/app-sidebar.test.tsx
git commit -m "feat(web): MainSidebar — primary destinations rail"
```

---

### Task 7: RootLayout (shell)

**Files:**
- Create: `web/src/pages/RootLayout.tsx`
- (replaces `pages/Layout.tsx`, deleted in Phase 6)

- [ ] **Step 1: Implement `RootLayout.tsx`**

```tsx
import { Outlet } from '@tanstack/react-router';
import { MainSidebar } from '../components/app-sidebar';
import { SidebarInset, SidebarProvider, SidebarTrigger } from '@/components/ui/sidebar';
import { Separator } from '@/components/ui/separator';

// The app shell: the primary destinations rail + the active surface. Section
// layouts (/sessions, /settings) render their own second sidebar INTO this
// inset's Outlet. Fleet/Storage render full-bleed in the inset.
export function RootLayout() {
  return (
    <SidebarProvider>
      <MainSidebar />
      <SidebarInset>
        <header className="flex h-12 shrink-0 items-center gap-2 border-b px-3">
          <SidebarTrigger className="-ml-1" />
          <Separator orientation="vertical" className="mr-2 h-4" />
        </header>
        <div className="flex flex-1 flex-col">
          <Outlet />
        </div>
      </SidebarInset>
    </SidebarProvider>
  );
}
```

- [ ] **Step 2: Verify compile** — `pnpm exec tsc -b --noEmit` → PASS (RootLayout not yet wired into the router; that is Task 9).

- [ ] **Step 3: Commit**
```bash
git add web/src/pages/RootLayout.tsx
git commit -m "feat(web): RootLayout shadcn shell (SidebarProvider + inset)"
```

---

### Task 8: Section sidebars (Sessions + Settings layouts)

**Files:**
- Create: `web/src/pages/sessions/SessionsLayout.tsx`, `web/src/pages/settings/SettingsLayout.tsx`

These render a **second** sidebar via a scoped `SidebarProvider`, then their own `<Outlet/>`.

**Responsive rule for both section layouts:** the vertical second sidebar shows only at `md+` (`hidden md:flex`); below `md` it is replaced by a horizontal, scrollable nav strip above the content. The primary rail keeps its native off-canvas behaviour (its `SidebarTrigger` is in the RootLayout header). Content padding steps down on mobile (`p-4 md:p-6`).

- [ ] **Step 1: Implement `sessions/SessionsLayout.tsx`**

```tsx
import { Layers, ListChecks } from 'lucide-react';
import { Link, Outlet, useRouterState, type LinkProps } from '@tanstack/react-router';
import { useIsAdmin } from '../../auth/AuthProvider';
import {
  Sidebar, SidebarContent, SidebarGroup, SidebarGroupContent, SidebarGroupLabel,
  SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarProvider,
} from '@/components/ui/sidebar';
import { cn } from '@/lib/utils';

export function SessionsLayout() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const items: { to: LinkProps['to']; label: string; icon: typeof Layers; active: boolean }[] = [
    { to: '/sessions', label: 'My sessions', icon: Layers, active: pathname === '/sessions' || pathname === '/sessions/' },
    ...(isAdmin
      ? [{ to: '/sessions/all' as LinkProps['to'], label: 'All sessions', icon: ListChecks, active: pathname.startsWith('/sessions/all') }]
      : []),
  ];

  return (
    <SidebarProvider className="min-h-0 flex-1">
      {/* desktop (md+): vertical second sidebar */}
      <Sidebar collapsible="none" className="hidden border-r md:flex">
        <SidebarContent>
          <SidebarGroup>
            <SidebarGroupLabel>Sessions</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                {items.map((it) => (
                  <SidebarMenuItem key={it.label}>
                    <SidebarMenuButton asChild isActive={it.active}>
                      <Link to={it.to}><it.icon /><span>{it.label}</span></Link>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
      </Sidebar>
      <div className="flex flex-1 flex-col overflow-auto">
        {/* mobile (<md): horizontal nav strip */}
        <nav className="flex gap-1 overflow-x-auto border-b p-2 md:hidden">
          {items.map((it) => (
            <Link key={it.label} to={it.to}
              className={cn('inline-flex items-center gap-2 whitespace-nowrap rounded-md px-3 py-1.5 text-sm',
                it.active ? 'bg-accent text-accent-foreground' : 'text-muted-foreground')}>
              <it.icon className="size-4" />{it.label}
            </Link>
          ))}
        </nav>
        <div className="flex-1 p-4 md:p-6"><Outlet /></div>
      </div>
    </SidebarProvider>
  );
}
```

- [ ] **Step 2: Implement `settings/SettingsLayout.tsx`**

```tsx
import { KeyRound, Users, Boxes, Database, UserCircle } from 'lucide-react';
import { Link, Outlet, useRouterState, type LinkProps } from '@tanstack/react-router';
import { useIsAdmin } from '../../auth/AuthProvider';
import {
  Sidebar, SidebarContent, SidebarGroup, SidebarGroupContent, SidebarGroupLabel,
  SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarProvider,
} from '@/components/ui/sidebar';

interface Item { to: LinkProps['to']; label: string; icon: typeof Users }

const YOU: Item[] = [
  { to: '/settings/profile', label: 'Profile', icon: UserCircle },
  { to: '/settings/tokens', label: 'Tokens', icon: KeyRound },
];
const DEPLOYMENT: Item[] = [
  { to: '/settings/members', label: 'Members', icon: Users },
  { to: '/settings/images', label: 'Images', icon: Boxes },
  { to: '/settings/registries', label: 'Registries', icon: Database },
];

export function SettingsLayout() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const group = (label: string, items: Item[]) => (
    <SidebarGroup key={label}>
      <SidebarGroupLabel>{label}</SidebarGroupLabel>
      <SidebarGroupContent>
        <SidebarMenu>
          {items.map((it) => (
            <SidebarMenuItem key={it.label}>
              <SidebarMenuButton asChild isActive={pathname.startsWith(it.to as string)}>
                <Link to={it.to}><it.icon /><span>{it.label}</span></Link>
              </SidebarMenuButton>
            </SidebarMenuItem>
          ))}
        </SidebarMenu>
      </SidebarGroupContent>
    </SidebarGroup>
  );

  const items = [...YOU, ...(isAdmin ? DEPLOYMENT : [])];

  return (
    <SidebarProvider className="min-h-0 flex-1">
      {/* desktop (md+): vertical second sidebar */}
      <Sidebar collapsible="none" className="hidden border-r md:flex">
        <SidebarContent>
          {group('You', YOU)}
          {isAdmin && group('Deployment', DEPLOYMENT)}
        </SidebarContent>
      </Sidebar>
      <div className="flex flex-1 flex-col overflow-auto">
        {/* mobile (<md): horizontal nav strip */}
        <nav className="flex gap-1 overflow-x-auto border-b p-2 md:hidden">
          {items.map((it) => (
            <Link key={it.label} to={it.to}
              className={cn('inline-flex items-center gap-2 whitespace-nowrap rounded-md px-3 py-1.5 text-sm',
                pathname.startsWith(it.to as string) ? 'bg-accent text-accent-foreground' : 'text-muted-foreground')}>
              <it.icon className="size-4" />{it.label}
            </Link>
          ))}
        </nav>
        <div className="flex-1 p-4 md:p-6"><Outlet /></div>
      </div>
    </SidebarProvider>
  );
}
```

Add `import { cn } from '@/lib/utils';` to `SettingsLayout.tsx`.

- [ ] **Step 3: Verify compile** — `pnpm exec tsc -b --noEmit` → PASS.

- [ ] **Step 4: Commit**
```bash
git add web/src/pages/sessions/SessionsLayout.tsx web/src/pages/settings/SettingsLayout.tsx
git commit -m "feat(web): section sidebars for Sessions and Settings"
```

---

### Task 9: Rewire the router (nested layout routes)

**Files:**
- Modify: `web/src/router.tsx`
- Create placeholder pages so the tree compiles: `web/src/pages/sessions/MySessions.tsx`, `AllSessions.tsx` (full versions in Phase 2)

- [ ] **Step 1: Create stub `MySessions.tsx` / `AllSessions.tsx`**

Temporary stubs so routing compiles before Phase 2 fills them:
```tsx
// web/src/pages/sessions/MySessions.tsx
export function MySessions() { return <div>My sessions</div>; }
```
```tsx
// web/src/pages/sessions/AllSessions.tsx
export function AllSessions() { return <div>All sessions</div>; }
```

- [ ] **Step 2: Rewrite `router.tsx`**

Keep the `RouterContext`/`requireAdmin` exactly. Swap `Layout`→`RootLayout`; make `/sessions` and `/settings` layout routes; redirect `/`→`/sessions`; keep `/sessions/$id` → existing `SessionDetail` (untouched).

```tsx
import {
  createRootRouteWithContext, createRoute, createRouter, redirect,
} from '@tanstack/react-router';
import type { AuthState } from './auth/AuthProvider';
import { RootLayout } from './pages/RootLayout';
import { SessionsLayout } from './pages/sessions/SessionsLayout';
import { MySessions } from './pages/sessions/MySessions';
import { AllSessions } from './pages/sessions/AllSessions';
import { SessionDetail } from './pages/SessionDetail';
import { Fleet } from './pages/Fleet';
import { Storage } from './pages/Storage';
import { SettingsLayout } from './pages/settings/SettingsLayout';
import { Members } from './pages/Members';
import { ImagesPanel } from './components/settings/ImagesPanel';
import { ProfilePanel } from './components/settings/ProfilePanel';
import { RegistriesPanel } from './components/settings/RegistriesPanel';
import { TokensPanel } from './components/settings/TokensPanel';

export interface RouterContext { auth: AuthState; }

function requireAdmin({ context }: { context: RouterContext }) {
  if (!context.auth.isAdmin) throw redirect({ to: '/settings/profile' });
}

const createRootRoute = createRootRouteWithContext<RouterContext>();
const rootRoute = createRootRoute({ component: RootLayout });

const indexRoute = createRoute({
  getParentRoute: () => rootRoute, path: '/',
  beforeLoad: () => { throw redirect({ to: '/sessions' }); },
});

// /sessions layout route (second sidebar) ----------------------------------
const sessionsLayoutRoute = createRoute({
  getParentRoute: () => rootRoute, path: '/sessions', component: SessionsLayout,
});
const mySessionsRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute, path: '/', component: MySessions,
});
const allSessionsRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute, path: 'all',
  beforeLoad: requireAdmin, component: AllSessions,
});
// Session detail is OUTSIDE the sessions layout (full-bleed, no 2nd sidebar,
// keeps its old styling). It is a child of root at /sessions/$id.
const sessionDetailRoute = createRoute({
  getParentRoute: () => rootRoute, path: '/sessions/$id', component: SessionDetail,
});

const fleetRoute = createRoute({
  getParentRoute: () => rootRoute, path: '/fleet',
  beforeLoad: requireAdmin, component: Fleet,
});
const storageRoute = createRoute({
  getParentRoute: () => rootRoute, path: '/storage',
  beforeLoad: requireAdmin, component: Storage,
});

// /settings layout route (second sidebar) ----------------------------------
const settingsLayoutRoute = createRoute({
  getParentRoute: () => rootRoute, path: '/settings', component: SettingsLayout,
});
const settingsIndexRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute, path: '/',
  beforeLoad: () => { throw redirect({ to: '/settings/profile' }); },
});
const profileRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'profile', component: ProfilePanel });
const tokensRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'tokens', component: TokensPanel });
const membersRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'members', beforeLoad: requireAdmin, component: Members });
const imagesRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'images', beforeLoad: requireAdmin, component: ImagesPanel });
const registriesRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'registries', beforeLoad: requireAdmin, component: RegistriesPanel });

const routeTree = rootRoute.addChildren([
  indexRoute,
  sessionsLayoutRoute.addChildren([mySessionsRoute, allSessionsRoute]),
  sessionDetailRoute,
  fleetRoute,
  storageRoute,
  settingsLayoutRoute.addChildren([
    settingsIndexRoute, profileRoute, tokensRoute, membersRoute, imagesRoute, registriesRoute,
  ]),
]);

export const router = createRouter({
  routeTree,
  context: { auth: undefined! as AuthState },
});

declare module '@tanstack/react-router' {
  interface Register { router: typeof router; }
}
```

- [ ] **Step 3: Verify compile + run app**

Run: `pnpm exec tsc -b --noEmit` → PASS. Run `pnpm dev`, open `http://localhost:5173`:
- `/` redirects to `/sessions`; the primary rail + a second "Sessions" sidebar show; "My sessions"/"All sessions" stubs render in the inset.
- `/settings` shows the second settings sidebar; sub-panels still render (old-styled, fine).
- `/fleet`, `/storage` render full-bleed (old-styled, fine).
- The dark-mode toggle in the rail footer flips the shell.

- [ ] **Step 4: Commit**
```bash
git add web/src/router.tsx web/src/pages/sessions/MySessions.tsx web/src/pages/sessions/AllSessions.tsx
git commit -m "feat(web): nested layout routes — dual sidebar shell wired"
```

---

# Phase 2 — Sessions surface

### Task 10: Session table (TanStack Table column defs + grouped table)

**Files:**
- Create: `web/src/pages/sessions/sessions-columns.tsx`
- Reuses: `SessionListItem`, `stripImageHost`/`relativeTime` (keep these exported helpers — re-export from a small util to avoid importing the doomed `SessionManifest.tsx`).

- [ ] **Step 1: Extract the row helpers into a util that survives cleanup**

Create `web/src/pages/sessions/session-format.ts`:
```ts
import type { SessionState } from '../../types';

export function shortId(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 8)}…`;
}
export function stripImageHost(uri: string): string {
  const slash = uri.indexOf('/');
  const colon = uri.lastIndexOf(':');
  const start = slash >= 0 ? slash + 1 : 0;
  const end = colon > start ? colon : uri.length;
  return uri.slice(start, end);
}
export function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  const dt = Math.max(0, (Date.now() - t) / 1000);
  if (dt < 60) return `${Math.floor(dt)}s`;
  if (dt < 3600) return `${Math.floor(dt / 60)}m`;
  if (dt < 86400) return `${Math.floor(dt / 3600)}h`;
  return `${Math.floor(dt / 86400)}d`;
}
const ACTIVEISH = new Set<SessionState>(['active','created','guest_ready','pending','host_lost']);
const ARCHIVED = new Set<SessionState>(['completed','dead','failed']);
export type Lifecycle = 'ACTIVE' | 'IDLE — RESUMABLE' | 'ARCHIVED';
export function lifecycleOf(s: SessionState): Lifecycle {
  if (ACTIVEISH.has(s)) return 'ACTIVE';
  if (s === 'idle') return 'IDLE — RESUMABLE';
  return 'ARCHIVED';
}
const STATUS_VARIANT: Record<SessionState, 'default' | 'secondary' | 'outline' | 'destructive'> = {
  active: 'default', created: 'secondary', guest_ready: 'secondary', pending: 'secondary',
  host_lost: 'destructive', idle: 'outline', completed: 'outline', failed: 'destructive', dead: 'destructive',
};
export const statusVariant = (s: SessionState) => STATUS_VARIANT[s];
```

- [ ] **Step 2: Implement `sessions-columns.tsx`** (a self-contained grouped table component)

```tsx
import { Link } from '@tanstack/react-router';
import { Badge } from '@/components/ui/badge';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table';
import type { SessionListItem } from '../../types';
import {
  lifecycleOf, relativeTime, shortId, statusVariant, stripImageHost, type Lifecycle,
} from './session-format';

const ORDER: Lifecycle[] = ['ACTIVE', 'IDLE — RESUMABLE', 'ARCHIVED'];

export function SessionsTable({
  sessions, showOwner, emptyText,
}: {
  sessions: SessionListItem[];
  showOwner: boolean;
  emptyText: string;
}) {
  if (sessions.length === 0) {
    return <p className="py-8 text-sm text-muted-foreground">{emptyText}</p>;
  }
  const groups = ORDER.map((g) => ({
    group: g,
    rows: sessions
      .filter((s) => lifecycleOf(s.status) === g)
      .sort((a, b) => new Date(b.last_active_at).getTime() - new Date(a.last_active_at).getTime()),
  })).filter((g) => g.rows.length > 0 || g.group === 'ACTIVE');

  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>Session</TableHead>
          <TableHead>Image</TableHead>
          <TableHead>Status</TableHead>
          {showOwner && <TableHead>Owner</TableHead>}
          <TableHead className="text-right">Age</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {groups.map(({ group, rows }) => (
          <GroupBlock key={group} group={group} rows={rows} showOwner={showOwner} />
        ))}
      </TableBody>
    </Table>
  );
}

function GroupBlock({ group, rows, showOwner }: {
  group: Lifecycle; rows: SessionListItem[]; showOwner: boolean;
}) {
  const cols = showOwner ? 5 : 4;
  return (
    <>
      <TableRow className="hover:bg-transparent">
        <TableCell colSpan={cols} className="bg-muted/40 py-1.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">
          {group} · {rows.length}
        </TableCell>
      </TableRow>
      {rows.length === 0 ? (
        <TableRow><TableCell colSpan={cols} className="text-sm text-muted-foreground">none</TableCell></TableRow>
      ) : rows.map((s) => (
        <TableRow key={s.id} className="cursor-pointer">
          <TableCell className="font-mono text-sm">
            <Link to="/sessions/$id" params={{ id: s.id }} className="hover:underline">
              {shortId(s.id)}
            </Link>
          </TableCell>
          <TableCell className="text-muted-foreground">{stripImageHost(s.image)}</TableCell>
          <TableCell><Badge variant={statusVariant(s.status)}>{s.status}</Badge></TableCell>
          {showOwner && (
            <TableCell>
              {s.owner_kind === 'system' ? (
                <span className="text-sm text-muted-foreground italic">system</span>
              ) : (
                <span className="flex items-center gap-2">
                  <Avatar className="size-5"><AvatarFallback className="text-[10px]">
                    {(s.owner_name || s.owner_email || '?').charAt(0).toUpperCase()}
                  </AvatarFallback></Avatar>
                  <span className="font-mono text-xs text-muted-foreground">{s.owner_email}</span>
                </span>
              )}
            </TableCell>
          )}
          <TableCell className="text-right font-mono text-xs text-muted-foreground">
            {relativeTime(s.last_active_at)}
          </TableCell>
        </TableRow>
      ))}
    </>
  );
}
```

(Note: `@tanstack/react-table` was installed for future column features; this grouped view is simpler hand-rolled — keep the dep, it is also used by any future sortable table. If a reviewer objects to the unused dep, the import can be deferred until a sortable table is needed.)

- [ ] **Step 3: Verify compile** — `pnpm exec tsc -b --noEmit` → PASS.

- [ ] **Step 4: Commit**
```bash
git add web/src/pages/sessions/session-format.ts web/src/pages/sessions/sessions-columns.tsx
git commit -m "feat(web): grouped sessions table (shadcn Table + Badge)"
```

---

### Task 11: MySessions + AllSessions pages

**Files:**
- Modify: `web/src/pages/sessions/MySessions.tsx`, `AllSessions.tsx`
- Reuses: `useSessions`, `useHosts`, `useAuth`, `NewSessionDialog` (Task 12 — create the dialog first OR stub the trigger). Sequence: do Task 12 before this, then wire the dialog here.

- [ ] **Step 1: Implement `MySessions.tsx`**

```tsx
import { useNavigate } from '@tanstack/react-router';
import { useHosts } from '../../hooks/useHosts';
import { useSessions } from '../../hooks/useSessions';
import { useAuth } from '../../auth/AuthProvider';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { NewSessionDialog } from '../../components/NewSessionDialog';
import { SessionsTable } from './sessions-columns';

export function MySessions() {
  const { principal } = useAuth();
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions('mine');
  const navigate = useNavigate();
  const all = sessions ?? [];
  const showTokenNudge = !principal.is_admin && !principal.has_claude_token;

  const stats: [string, number][] = [
    ['Active', all.filter((s) => s.status === 'active').length],
    ['Idle', all.filter((s) => s.status === 'idle').length],
    ['Hosts', (hosts ?? []).length],
    ['Snapshots', (hosts ?? []).reduce((a, h) => a + h.local_snapshots, 0)],
  ];

  return (
    <div className="space-y-6">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Sessions</h1>
          <p className="text-sm text-muted-foreground">Bounded units of agent work — launch, watch, resume.</p>
        </div>
        <NewSessionDialog onCreated={(id) => navigate({ to: '/sessions/$id', params: { id } })} />
      </div>

      {showTokenNudge && (
        <Card>
          <CardContent className="flex items-center justify-between gap-4 py-3">
            <span className="text-sm">No Claude Code token saved yet — built-in Claude sessions need one.</span>
            <Button variant="secondary" size="sm" onClick={() => navigate({ to: '/settings/tokens' })}>
              Add token
            </Button>
          </CardContent>
        </Card>
      )}

      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        {stats.map(([label, value]) => (
          <Card key={label}><CardContent className="py-4">
            <div className="font-mono text-2xl tabular-nums">{value}</div>
            <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
          </CardContent></Card>
        ))}
      </div>

      <SessionsTable sessions={all} showOwner={false}
        emptyText='No sessions yet — start one with "New session".' />
    </div>
  );
}
```

- [ ] **Step 2: Implement `AllSessions.tsx`** (admin, owner-attributed)

```tsx
import { useSessions } from '../../hooks/useSessions';
import { SessionsTable } from './sessions-columns';

export function AllSessions() {
  const { data: sessions } = useSessions('all');
  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">All sessions</h1>
        <p className="text-sm text-muted-foreground">Every session across the fleet — owner-attributed.</p>
      </div>
      <SessionsTable sessions={sessions ?? []} showOwner
        emptyText="No active sessions across the fleet." />
    </div>
  );
}
```

- [ ] **Step 3: Verify** — `pnpm exec tsc -b --noEmit` → PASS; `pnpm dev` shows the grouped table on `/sessions` and `/sessions/all`.

- [ ] **Step 4: Commit**
```bash
git add web/src/pages/sessions/MySessions.tsx web/src/pages/sessions/AllSessions.tsx
git commit -m "feat(web): My/All sessions pages on shadcn table"
```

---

### Task 12: NewSessionDialog (Dialog + form)

**Files:**
- Create: `web/src/components/NewSessionDialog.tsx`, `web/src/components/NewSessionDialog.test.tsx`
- Delete (Phase 6): `web/src/components/NewSessionForm.tsx` + `NewSessionForm.test.tsx`
- Reuses: `useEnabledImages`, `useAuth`, `createSession`, `useQueryClient`

- [ ] **Step 1: Write the failing test** (port behaviour from `NewSessionForm.test.tsx` — read it first to mirror its mocks/assertions; key behaviour: lists enabled images, gates submit when a Claude image needs a token, calls `createSession` and `onCreated` on success).

`web/src/components/NewSessionDialog.test.tsx`:
```tsx
import { expect, test, vi, beforeEach } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders } from '../test-utils';
import { NewSessionDialog } from './NewSessionDialog';
import * as api from '../api';
import * as imagesHook from '../hooks/useEnabledImages';

beforeEach(() => {
  vi.restoreAllMocks();
  vi.spyOn(imagesHook, 'useEnabledImages').mockReturnValue({
    data: [{
      id: '1', image_uri: 'ghcr.io/x/api:warm', manifest_digest: 'sha256:abc',
      manifest_name: 'api', manifest_description: null, harness_name: 'claude',
      last_refreshed_at: new Date().toISOString(), created_at: new Date().toISOString(),
    }],
    isLoading: false, error: null,
  } as unknown as ReturnType<typeof imagesHook.useEnabledImages>);
});

test('creates a session and reports the new id', async () => {
  const onCreated = vi.fn();
  vi.spyOn(api, 'createSession').mockResolvedValue({
    session_id: 'sess-1', status: 'created', image_version: 'v1',
  });
  renderWithProviders(<NewSessionDialog onCreated={onCreated} />);
  await userEvent.click(screen.getByRole('button', { name: /new session/i }));
  await userEvent.click(await screen.findByRole('button', { name: /^start$/i }));
  await waitFor(() => expect(onCreated).toHaveBeenCalledWith('sess-1'));
});
```

- [ ] **Step 2: Run → FAIL.**

- [ ] **Step 3: Implement `NewSessionDialog.tsx`**

Preserves the original logic (image select → `harness_name`-driven mode/prompt/token gating), in a `Dialog`:
```tsx
import { useEffect, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { useNavigate } from '@tanstack/react-router';
import { createSession } from '../api';
import { useAuth } from '../auth/AuthProvider';
import { useEnabledImages } from '../hooks/useEnabledImages';
import type { SessionMode } from '../types';
import { Button } from '@/components/ui/button';
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader,
  DialogTitle, DialogTrigger,
} from '@/components/ui/dialog';
import { Label } from '@/components/ui/label';
import { Textarea } from '@/components/ui/textarea';
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select';

export function NewSessionDialog({ onCreated }: { onCreated: (id: string) => void }) {
  const [open, setOpen] = useState(false);
  const { data: images, isLoading } = useEnabledImages(true);
  const { principal } = useAuth();
  const qc = useQueryClient();
  const navigate = useNavigate();

  const [selectedUri, setSelectedUri] = useState('');
  const [mode, setMode] = useState<SessionMode>('agent');
  const [prompt, setPrompt] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const selected = images?.find((i) => i.image_uri === selectedUri);
  useEffect(() => {
    if (!selectedUri && images && images.length > 0) setSelectedUri(images[0].image_uri);
  }, [images, selectedUri]);

  const harnessName = selected?.harness_name ?? null;
  const hasHarness = harnessName !== null;
  const isClaude = harnessName === 'claude';
  const promptMeaningful = hasHarness && mode === 'agent';
  const needsToken = isClaude && mode === 'agent' && !principal.has_claude_token;
  const canSubmit = !!selected && !submitting && !needsToken;

  const submit = async () => {
    if (!canSubmit || !selected) return;
    setSubmitting(true); setError(null);
    try {
      const res = await createSession({
        image: selected.image_uri,
        mode: mode === 'dev_vm' ? 'dev_vm' : undefined,
        prompt: promptMeaningful ? (prompt.trim() || undefined) : undefined,
      });
      qc.invalidateQueries({ queryKey: ['sessions'] });
      setOpen(false); setPrompt('');
      onCreated(res.session_id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally { setSubmitting(false); }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild><Button>New session</Button></DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New session</DialogTitle>
          <DialogDescription>Launch a bounded unit of agent work.</DialogDescription>
        </DialogHeader>

        {isLoading && <p className="text-sm text-muted-foreground">Loading images…</p>}
        {images && images.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No images enabled. Enable one under Settings → Images.
          </p>
        )}

        {images && images.length > 0 && (
          <div className="space-y-4">
            <div className="space-y-2">
              <Label>Image</Label>
              <Select value={selectedUri} onValueChange={setSelectedUri}>
                <SelectTrigger><SelectValue /></SelectTrigger>
                <SelectContent>
                  {images.map((i) => (
                    <SelectItem key={i.image_uri} value={i.image_uri}>
                      {i.image_uri}{i.manifest_name ? ` — ${i.manifest_name}` : ''}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <p className="text-xs text-muted-foreground">
                {harnessName ? `Baked harness: ${harnessName}` : 'No baked harness — shell-only image'}
              </p>
            </div>

            <div className="space-y-2">
              <Label>Mode</Label>
              <Select value={mode} onValueChange={(v) => setMode(v as SessionMode)}>
                <SelectTrigger><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="agent">agent — drive the baked harness</SelectItem>
                  <SelectItem value="dev_vm">dev VM — shell-only</SelectItem>
                </SelectContent>
              </Select>
            </div>

            {promptMeaningful && (
              <div className="space-y-2">
                <Label htmlFor="ns-prompt">Prompt</Label>
                <Textarea id="ns-prompt" rows={2} value={prompt}
                  onChange={(e) => setPrompt(e.target.value)}
                  placeholder="optional opening prompt" />
              </div>
            )}

            {needsToken && (
              <p className="text-sm text-muted-foreground">
                This image runs built-in Claude, which uses your saved token — you don’t have one yet.
              </p>
            )}
            {error && <p className="text-sm text-destructive">{error}</p>}
          </div>
        )}

        <DialogFooter>
          {needsToken ? (
            <Button variant="secondary" onClick={() => { setOpen(false); navigate({ to: '/settings/tokens' }); }}>
              Save your Claude token
            </Button>
          ) : (
            <Button onClick={submit} disabled={!canSubmit}>
              {submitting ? 'Starting…' : 'Start'}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
```

Note: if `Textarea` was not pulled in Task 4, run `pnpm dlx shadcn@latest add textarea` and commit it with this task.

- [ ] **Step 4: Run → PASS.** `pnpm exec tsc -b --noEmit` → PASS.

- [ ] **Step 5: Commit**
```bash
git add web/src/components/NewSessionDialog.tsx web/src/components/NewSessionDialog.test.tsx web/src/components/ui/textarea.tsx
git commit -m "feat(web): NewSessionDialog (shadcn Dialog + Select/Textarea)"
```

---

# Phase 3 — Fleet

### Task 13: Rebuild Fleet

**Files:**
- Modify: `web/src/pages/Fleet.tsx`
- Reuses: `useHosts`, `useSessions`, `useDrainHost`, `HostView`, `HostStatus`

- [ ] **Step 1: Rewrite `Fleet.tsx`**

Rollup `Card`s; per-host `Card` with `Progress` capacity, `Badge` status, drain via `AlertDialog`; sandbox cells kept as small squares using theme tokens; reconciler note.

```tsx
import { useHosts } from '../hooks/useHosts';
import { useSessions } from '../hooks/useSessions';
import { useDrainHost } from '../hooks/useDrainHost';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Progress } from '@/components/ui/progress';
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
  AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog';
import type { HostStatus, HostView, Session } from '../types';

const statusVariant = (s: HostStatus) =>
  s === 'ready' ? 'default' : s === 'draining' ? 'secondary' : 'destructive';

export function Fleet() {
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions();
  const drain = useDrainHost();
  const h = hosts ?? [];
  const s = sessions ?? [];
  const totalSb = h.reduce((a, x) => a + x.running_sandboxes, 0);
  const usedGiB = (h.reduce((a, x) => a + x.capacity_used_mib, 0) / 1024).toFixed(0);
  const totGiB = (h.reduce((a, x) => a + x.capacity_total_mib, 0) / 1024).toFixed(0);
  const anyDraining = h.some((x) => x.status === 'draining');
  const liveByHost = (id: string) =>
    s.filter((x: Session) => x.host_id === id && x.status === 'active').length;

  return (
    <div className="space-y-6 p-4 md:p-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Fleet</h1>
        <p className="text-sm text-muted-foreground">Firecracker hosts and capacity.</p>
      </div>

      <div className="grid grid-cols-3 gap-3">
        {([['Hosts', h.length], ['Sandboxes', totalSb], ['GiB used', `${usedGiB}/${totGiB}`]] as const).map(
          ([label, value]) => (
            <Card key={label}><CardContent className="py-4">
              <div className="font-mono text-2xl tabular-nums">{value}</div>
              <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
            </CardContent></Card>
          ))}
      </div>

      {h.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No hosts have registered yet. Hosts appear here once they boot and complete their first heartbeat.
        </p>
      ) : (
        <div className="space-y-3">
          {h.map((host) => (
            <HostCard key={host.id} host={host} live={liveByHost(host.id)}
              onDrain={() => drain.mutate(host.id)} draining={drain.isPending} />
          ))}
        </div>
      )}

      <p className="text-sm text-muted-foreground">
        Reconciler {anyDraining ? 'rebalancing — draining host, migrating sandboxes' : 'steady — desired state matches observed'}.
      </p>
    </div>
  );
}

function HostCard({ host, live, onDrain, draining }: {
  host: HostView; live: number; onDrain: () => void; draining: boolean;
}) {
  const pct = host.capacity_total_mib > 0
    ? Math.min(100, Math.round((host.capacity_used_mib / host.capacity_total_mib) * 100)) : 0;
  const totalGiB = (host.capacity_total_mib / 1024).toFixed(0);
  const usedGiB = (host.capacity_used_mib / 1024).toFixed(1);
  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
        <CardTitle className="font-mono text-base">{host.id}</CardTitle>
        <div className="flex items-center gap-3">
          <Badge variant={statusVariant(host.status)}>{host.status}</Badge>
          {host.status === 'ready' && (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="outline" size="sm" disabled={draining}>Drain</Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Drain {host.id}?</AlertDialogTitle>
                  <AlertDialogDescription>
                    The scheduler stops assigning new sessions to this host. In-flight sessions stay put.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={onDrain}>Drain</AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {host.running_sandboxes === 0 ? (
          <p className="text-sm italic text-muted-foreground">no sandboxes</p>
        ) : (
          <div className="flex flex-wrap gap-1" aria-label={`${host.running_sandboxes} sandboxes`}>
            {Array.from({ length: host.running_sandboxes }, (_, i) => (
              <span key={i} className={`size-3 ${i < live ? 'bg-primary' : 'bg-muted-foreground/40'}`} />
            ))}
          </div>
        )}
        <div className="flex items-center gap-3">
          <Progress value={pct} className="h-2" />
          <span className="font-mono text-xs tabular-nums text-muted-foreground">{pct}%</span>
        </div>
        <div className="font-mono text-xs text-muted-foreground">
          {host.running_sandboxes} sandboxes · {usedGiB}/{totalGiB} GiB · {host.local_snapshots} snapshots
        </div>
      </CardContent>
    </Card>
  );
}
```

- [ ] **Step 2: Verify** — `pnpm exec tsc -b --noEmit` → PASS; `pnpm test` green; `pnpm dev` `/fleet` renders the shadcn cards + drain confirm.

- [ ] **Step 3: Commit**
```bash
git add web/src/pages/Fleet.tsx
git commit -m "feat(web): rebuild Fleet on shadcn (cards, progress, drain confirm)"
```

---

# Phase 4 — Storage

### Task 14: Rebuild Storage

**Files:**
- Modify: `web/src/pages/Storage.tsx`
- Reuses: `useStorageSummary`, `fmtAgo`/`fmtBytes`/`secondsSince`/`shortId` (`format.ts`), `DurabilityRow`

- [ ] **Step 1: Rewrite `Storage.tsx`**

```tsx
import { useStorageSummary } from '../hooks/useStorageSummary';
import { fmtAgo, fmtBytes, secondsSince, shortId } from '../format';
import { Card, CardContent } from '@/components/ui/card';
import { Progress } from '@/components/ui/progress';
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table';
import type { DurabilityRow } from '../types';

export function Storage() {
  const { data, isPending, error } = useStorageSummary();
  const rows = data?.rows ?? [];
  const rollups: [string, string | number][] = [
    ['Snapshots', data?.snapshots ?? 0],
    ['Snapshot bytes', fmtBytes(data?.snapshot_bytes ?? 0)],
    ['Tracked sandboxes', data?.tracked_sandboxes ?? 0],
    ['Unflushed', fmtBytes(data?.unflushed_bytes ?? 0)],
    ['Avg locality', `${data?.avg_locality_pct ?? 0}%`],
    ['GC pending', data?.gc_pending ?? 0],
  ];

  return (
    <div className="space-y-6 p-4 md:p-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Storage</h1>
        <p className="text-sm text-muted-foreground">
          Content-addressed chunk store, snapshots, and copy-on-write durability.
        </p>
      </div>

      <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
        {rollups.map(([label, value]) => (
          <Card key={label}><CardContent className="py-4">
            <div className="font-mono text-xl tabular-nums">{value}</div>
            <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
          </CardContent></Card>
        ))}
      </div>

      <div>
        <div className="mb-2 flex items-baseline justify-between">
          <h2 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Durability ledger · per-sandbox copy-on-write
          </h2>
          <span className="font-mono text-xs tabular-nums text-muted-foreground">{rows.length}</span>
        </div>
        <Table>
          <TableHeader><TableRow>
            <TableHead>Session</TableHead><TableHead>Host</TableHead>
            <TableHead className="text-right">Dirty</TableHead>
            <TableHead className="text-right">Unflushed</TableHead>
            <TableHead>Base locality</TableHead>
            <TableHead className="text-right">RPO</TableHead>
          </TableRow></TableHeader>
          <TableBody>
            {error ? (
              <TableRow><TableCell colSpan={6} className="text-sm text-destructive">{(error as Error).message}</TableCell></TableRow>
            ) : rows.length === 0 ? (
              <TableRow><TableCell colSpan={6} className="text-sm text-muted-foreground">
                {isPending ? 'Loading…' : 'No chunk-tracked sandboxes'}
              </TableCell></TableRow>
            ) : rows.map((r) => <LedgerRow key={r.sandbox_id} row={r} />)}
          </TableBody>
        </Table>
        <p className="mt-3 max-w-prose text-sm text-muted-foreground">
          Dirty chunks flush to the content-addressed store on the snapshot cadence; base locality is the
          share of a sandbox’s base chunks resident on its host. RPO is time since the last flush.
        </p>
      </div>
    </div>
  );
}

function LedgerRow({ row }: { row: DurabilityRow }) {
  const pct = row.base_chunks > 0 ? Math.round((row.base_chunks_local / row.base_chunks) * 100) : null;
  const rpoHot = secondsSince(row.last_flush_at) <= 10;
  return (
    <TableRow>
      <TableCell className="font-mono text-sm">{shortId(row.session_id ?? row.sandbox_id)}</TableCell>
      <TableCell className="font-mono text-xs text-muted-foreground">{shortId(row.host_id)}</TableCell>
      <TableCell className="text-right font-mono tabular-nums">{row.dirty_chunks}</TableCell>
      <TableCell className="text-right font-mono tabular-nums">{fmtBytes(row.dirty_bytes)}</TableCell>
      <TableCell>
        {pct === null ? '—' : (
          <span className="flex items-center gap-2">
            <Progress value={pct} className="h-1.5 w-16" />
            <span className="font-mono text-xs tabular-nums">{pct}%</span>
          </span>
        )}
      </TableCell>
      <TableCell className={`text-right font-mono text-xs tabular-nums ${rpoHot ? 'text-destructive' : 'text-muted-foreground'}`}>
        {fmtAgo(row.last_flush_at)}
      </TableCell>
    </TableRow>
  );
}
```

- [ ] **Step 2: Verify** — `pnpm exec tsc -b --noEmit` → PASS; `pnpm dev` `/storage` renders.

- [ ] **Step 3: Commit**
```bash
git add web/src/pages/Storage.tsx
git commit -m "feat(web): rebuild Storage on shadcn (rollup cards, ledger table)"
```

---

# Phase 5 — Settings panels

### Task 15: Settings page shell removal + ProfilePanel

**Files:**
- Modify: `web/src/components/settings/ProfilePanel.tsx`; Delete (Phase 6) `web/src/pages/Settings.tsx` (the section nav now lives in `SettingsLayout`).
- Reuses: `useAuth`

- [ ] **Step 1: Rewrite `ProfilePanel.tsx`** (Card + definition list + access legend, shadcn `Badge` for role)

```tsx
import { Link } from '@tanstack/react-router';
import { Check, X } from 'lucide-react';
import { useAuth } from '../../auth/AuthProvider';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export function ProfilePanel() {
  const { principal } = useAuth();
  const label = principal.display_name || principal.email;
  const isAdmin = principal.role === 'admin';
  const can = isAdmin
    ? ['Launch & manage your own sessions', 'Oversee every session across the fleet',
       'Inspect host capacity & drain hosts', 'Read storage durability & snapshots',
       'Curate images & registry credentials', 'Manage members & their roles']
    : ['Launch & manage your own sessions', 'Save your own Claude Code token'];
  const cannot = isAdmin ? [] : ['The fleet, storage & deployment settings — admin only'];

  return (
    <div className="max-w-2xl space-y-6">
      <h1 className="text-2xl font-semibold tracking-tight">Profile</h1>

      <Card>
        <CardHeader className="flex flex-row items-center gap-3 space-y-0">
          <Avatar className="size-12 rounded-md"><AvatarFallback className="rounded-md text-lg">
            {label.charAt(0).toUpperCase()}
          </AvatarFallback></Avatar>
          <div>
            <CardTitle>{label}</CardTitle>
            <p className="font-mono text-sm text-muted-foreground">{principal.email}</p>
          </div>
        </CardHeader>
        <CardContent className="space-y-3">
          <Row label="Role">
            <Badge variant={isAdmin ? 'default' : 'secondary'}>{principal.role}</Badge>
            {principal.role_source && (
              <span className="text-sm text-muted-foreground">· set by {principal.role_source}</span>
            )}
          </Row>
          <Row label="Tokens">
            {principal.has_claude_token ? (
              <span className="text-sm">Claude Code saved · managed under{' '}
                <Link to="/settings/tokens" className="underline">Tokens</Link></span>
            ) : (
              <Link to="/settings/tokens" className="text-sm underline">None saved — add one under Tokens →</Link>
            )}
          </Row>
        </CardContent>
      </Card>

      <Card>
        <CardHeader><CardTitle className="text-sm">What your role can do</CardTitle></CardHeader>
        <CardContent>
          <ul className="space-y-1.5 text-sm">
            {can.map((c) => (
              <li key={c} className="flex items-center gap-2">
                <Check className="size-4 text-primary" /> {c}
              </li>
            ))}
            {cannot.map((c) => (
              <li key={c} className="flex items-center gap-2 text-muted-foreground">
                <X className="size-4" /> {c}
              </li>
            ))}
          </ul>
        </CardContent>
      </Card>
    </div>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="grid grid-cols-[8rem_1fr] items-baseline gap-2">
      <span className="text-xs uppercase tracking-wide text-muted-foreground">{label}</span>
      <span className="flex flex-wrap items-baseline gap-2">{children}</span>
    </div>
  );
}
```

- [ ] **Step 2: Verify + Commit**
```bash
pnpm exec tsc -b --noEmit
git add web/src/components/settings/ProfilePanel.tsx
git commit -m "feat(web): rebuild ProfilePanel on shadcn"
```

---

### Task 16: TokensPanel

**Files:** Modify `web/src/components/settings/TokensPanel.tsx`. Reuses `useAuth`, `saveClaudeToken`, `useMutation`.

- [ ] **Step 1: Rewrite `TokensPanel.tsx`** (a `Card` per service; inline edit with `Input type=password` + Save/Cancel; `Badge` status)

```tsx
import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { saveClaudeToken } from '../../api';
import { useAuth } from '../../auth/AuthProvider';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';

export function TokensPanel() {
  const { principal, refresh } = useAuth();
  const qc = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [token, setToken] = useState('');
  const save = useMutation({
    mutationFn: (t: string) => saveClaudeToken(t),
    onSuccess: () => { setEditing(false); setToken(''); void qc.invalidateQueries({ queryKey: ['me'] }); refresh(); },
  });
  const saved = principal.has_claude_token;

  return (
    <div className="max-w-2xl space-y-6">
      <h1 className="text-2xl font-semibold tracking-tight">Tokens</h1>
      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
          <div>
            <CardTitle>Claude Code</CardTitle>
            <p className="text-sm text-muted-foreground">Built-in Claude sessions authenticate with this.</p>
          </div>
          <Badge variant={saved ? 'secondary' : 'outline'}>{saved ? 'saved · sealed' : 'not connected'}</Badge>
        </CardHeader>
        <CardContent className="space-y-3">
          {editing ? (
            <div className="space-y-2">
              <Input type="password" autoFocus placeholder="sk-ant-oat…" value={token}
                onChange={(e) => setToken(e.target.value)} className="font-mono" />
              <p className="text-xs text-muted-foreground">
                From <code className="font-mono">claude setup-token</code> — stored encrypted, never shown again.
              </p>
              <div className="flex gap-2">
                <Button size="sm" disabled={!token.trim() || save.isPending} onClick={() => save.mutate(token.trim())}>
                  {save.isPending ? 'Saving…' : 'Save'}
                </Button>
                <Button size="sm" variant="ghost" onClick={() => { setEditing(false); setToken(''); }}>Cancel</Button>
              </div>
              {save.error && <p className="text-sm text-destructive">{String(save.error)}</p>}
            </div>
          ) : (
            <Button size="sm" variant={saved ? 'outline' : 'default'} onClick={() => setEditing(true)}>
              {saved ? 'Replace' : 'Add token'}
            </Button>
          )}
        </CardContent>
      </Card>
      <p className="max-w-prose text-sm text-muted-foreground">
        Every token is sealed under the deployment key the moment you save it — the plaintext never touches
        Postgres, and it's used automatically so you're never prompted per session.
      </p>
    </div>
  );
}
```

- [ ] **Step 2: Verify + Commit**
```bash
pnpm exec tsc -b --noEmit
git add web/src/components/settings/TokensPanel.tsx
git commit -m "feat(web): rebuild TokensPanel on shadcn"
```

---

### Task 17: Members

**Files:** Modify `web/src/pages/Members.tsx`. Reuses `fetchUsers`, `updateUser`, `useAuth`, `AdminUser`, `Role`.

- [ ] **Step 1: Rewrite `Members.tsx`** (shadcn `Table`; `Badge` role; per-row `DropdownMenu` with role + active actions, destructive ones behind `AlertDialog`)

```tsx
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { MoreHorizontal } from 'lucide-react';
import { fetchUsers, updateUser } from '../api';
import { useAuth } from '../auth/AuthProvider';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table';
import type { AdminUser, Role } from '../types';

export function Members() {
  const { principal } = useAuth();
  const qc = useQueryClient();
  const { data: users = [], isLoading, error } = useQuery({ queryKey: ['admin', 'users'], queryFn: fetchUsers });
  const mutation = useMutation({
    mutationFn: ({ id, patch }: { id: string; patch: { role?: Role; active?: boolean } }) => updateUser(id, patch),
    onSuccess: (u) => qc.setQueryData<AdminUser[]>(['admin', 'users'], (prev) =>
      prev ? prev.map((x) => (x.id === u.id ? u : x)) : [u]),
  });

  if (isLoading) return <p className="p-6 text-sm text-muted-foreground">Loading…</p>;
  if (error) return <p className="p-6 text-sm text-destructive">Could not load members — {(error as Error).message}</p>;

  const admins = users.filter((u) => u.role === 'admin' && u.active).length;
  const disabled = users.filter((u) => !u.active).length;

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Members</h1>
        <p className="text-sm text-muted-foreground">
          {users.length} people · {admins} admins · {disabled} disabled
        </p>
      </div>
      <Table>
        <TableHeader><TableRow>
          <TableHead>Person</TableHead><TableHead>Role</TableHead>
          <TableHead>Source</TableHead><TableHead>Status</TableHead>
          <TableHead className="w-10" />
        </TableRow></TableHeader>
        <TableBody>
          {users.map((u) => {
            const isYou = u.email === principal.email;
            return (
              <TableRow key={u.id} className={u.active ? '' : 'opacity-60'}>
                <TableCell>
                  <span className="flex items-center gap-2">
                    <Avatar className="size-7"><AvatarFallback className="text-xs">
                      {(u.display_name || u.email).charAt(0).toUpperCase()}
                    </AvatarFallback></Avatar>
                    <span>
                      <span className="block text-sm">{u.display_name || u.email}{isYou && ' (you)'}</span>
                      <span className="block font-mono text-xs text-muted-foreground">{u.email}</span>
                    </span>
                  </span>
                </TableCell>
                <TableCell><Badge variant={u.role === 'admin' ? 'default' : 'secondary'}>{u.role}</Badge></TableCell>
                <TableCell className="text-sm text-muted-foreground">{u.role_source}</TableCell>
                <TableCell className="text-sm text-muted-foreground">{u.active ? 'active' : 'disabled'}</TableCell>
                <TableCell>
                  {!isYou && (
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button variant="ghost" size="icon" aria-label="Member actions"><MoreHorizontal className="size-4" /></Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end">
                        {u.role === 'member' ? (
                          <DropdownMenuItem onClick={() => mutation.mutate({ id: u.id, patch: { role: 'admin' } })}>Make admin</DropdownMenuItem>
                        ) : (
                          <DropdownMenuItem onClick={() => mutation.mutate({ id: u.id, patch: { role: 'member' } })}>Revoke admin</DropdownMenuItem>
                        )}
                        {u.active ? (
                          <DropdownMenuItem variant="destructive" onClick={() => mutation.mutate({ id: u.id, patch: { active: false } })}>Deactivate</DropdownMenuItem>
                        ) : (
                          <DropdownMenuItem onClick={() => mutation.mutate({ id: u.id, patch: { active: true } })}>Reactivate</DropdownMenuItem>
                        )}
                      </DropdownMenuContent>
                    </DropdownMenu>
                  )}
                </TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>
      <p className="max-w-prose text-sm text-muted-foreground">
        Roles are provisioned from your identity provider on first sign-in and stay in sync over SCIM;
        promote or revoke here and the change is marked “set by an admin”. A deactivated member keeps
        their sessions but can't sign in.
      </p>
    </div>
  );
}
```

- [ ] **Step 2: Verify + Commit**
```bash
pnpm exec tsc -b --noEmit
git add web/src/pages/Members.tsx
git commit -m "feat(web): rebuild Members on shadcn (table, role menu)"
```

---

### Task 18: ImagesPanel (+ update its test)

**Files:** Modify `web/src/components/settings/ImagesPanel.tsx`, `ImagesPanel.test.tsx`. Reuses the `useEnabledImages`/`useEnableImage`/`useDisableImage`/`useRefreshEnabledImage`/`useEnableProgress` hooks unchanged.

- [ ] **Step 1: Read `ImagesPanel.test.tsx`** to learn its current assertions (what text/roles it queries) so the rebuild keeps them satisfiable. Preserve: list rows show `image_uri`; an "enable a new image" affordance reveals an input; submitting calls the enable hook.

- [ ] **Step 2: Rewrite `ImagesPanel.tsx`** — list as a shadcn `Table` (URI, manifest name, digest, refreshed-ago, actions); "Enable a new image" as a `Dialog` containing an `Input` + Enable/Cancel; disable behind an `AlertDialog`. Use `Button`/`Badge`/`Table`/`Dialog`/`Input`/`Label`/`AlertDialog`. Keep `useEnableProgress` copy as muted text inside the dialog footer. (Mirror the data wiring in the current file exactly; only swap presentation.)

- [ ] **Step 3: Update `ImagesPanel.test.tsx`** to match new roles/labels (e.g. open the Dialog via the "Enable a new image" button before asserting the input is present). Run `pnpm exec vitest run src/components/settings/ImagesPanel.test.tsx` → PASS.

- [ ] **Step 4: Verify + Commit**
```bash
pnpm exec tsc -b --noEmit
git add web/src/components/settings/ImagesPanel.tsx web/src/components/settings/ImagesPanel.test.tsx web/src/components/ui/textarea.tsx
git commit -m "feat(web): rebuild ImagesPanel on shadcn (table + enable dialog)"
```

---

### Task 19: RegistriesPanel (+ update its test)

**Files:** Modify `web/src/components/settings/RegistriesPanel.tsx`, `RegistriesPanel.test.tsx`. Reuses `useRegistries` + add/delete hooks and the `AddRegistryAuth` discriminated union from `types.ts`.

- [ ] **Step 1: Read the current `RegistriesPanel.tsx` + its test** to capture the exact form fields (host + `auth_kind` select → static username/password | gcp impersonate_sa | anonymous) and assertions.
- [ ] **Step 2: Rewrite** — list as `Table` (host, auth kind `Badge`, principal, actions); "Add registry" `Dialog` with `Select` for `auth_kind` and conditional `Input`s matching the `AddRegistryAuth` variants; delete behind `AlertDialog`. Preserve the exact request body shapes from `addRegistry`.
- [ ] **Step 3: Update `RegistriesPanel.test.tsx`** to the new roles/labels. Run → PASS.
- [ ] **Step 4: Verify + Commit**
```bash
pnpm exec tsc -b --noEmit
git add web/src/components/settings/RegistriesPanel.tsx web/src/components/settings/RegistriesPanel.test.tsx
git commit -m "feat(web): rebuild RegistriesPanel on shadcn"
```

---

### Task 20: Auth-state screens (boot / not-member / error) on shadcn

**Files:** Modify `web/src/auth/AuthProvider.tsx` (only the three screen components at the bottom; the query logic stays).

- [ ] **Step 1: Rewrite `BootScreen`, `AuthErrorScreen`, `NotMemberScreen`** to use shadcn `bg-background`/`text-foreground` + `Card` + `Button` instead of the `auth-stage`/`auth-card`/`members-act` classes (which live in `theme.css`). Keep `EngramMark`. Example BootScreen:
```tsx
function BootScreen() {
  return (
    <div className="grid min-h-svh place-items-center bg-background p-8">
      <div className="flex flex-col items-center gap-3 text-center">
        <EngramMark size={72} mode="loop" />
        <p className="text-sm italic text-muted-foreground">authenticating…</p>
      </div>
    </div>
  );
}
```
Apply the same shell to the error and not-member screens, with `Button` for retry / sign-out.

- [ ] **Step 2: Verify + Commit**
```bash
pnpm exec tsc -b --noEmit && pnpm test
git add web/src/auth/AuthProvider.tsx
git commit -m "feat(web): auth-state screens on shadcn"
```

---

# Phase 6 — Cleanup

### Task 21: Delete dead components + trim theme.css body rule

**Files:** Delete the now-unused bespoke files; neutralise the global `body` background in `theme.css` so the shadcn shell owns the app background while the transcript subtree keeps its own classes.

- [ ] **Step 1: Confirm no live imports remain**

Run:
```bash
cd web && for f in NavSpine VitalStrip ManifestGroup SessionManifest TabRow SectionHead UserChip; do
  echo "== $f =="; grep -rn "$f" src --include=*.tsx --include=*.ts | grep -v "src/components/$f" || true
done
```
Expected: no references outside their own files (and outside excluded transcript files). `SessionManifest`'s helpers were re-homed in `session-format.ts` (Task 10), so confirm nothing imports from `SessionManifest` except (possibly) excluded files — if `SessionDetail` or a transcript file imports `relativeTime`/`stripImageHost` from `SessionManifest`, leave `SessionManifest.tsx` in place and skip deleting it.

- [ ] **Step 2: Delete confirmed-dead files**

```bash
cd web && git rm \
  src/components/NavSpine.tsx \
  src/components/VitalStrip.tsx \
  src/components/ManifestGroup.tsx \
  src/components/TabRow.tsx \
  src/components/SectionHead.tsx \
  src/components/UserChip.tsx src/components/UserChip.test.tsx \
  src/components/NewSessionForm.tsx src/components/NewSessionForm.test.tsx \
  src/components/settings/_form.tsx \
  src/pages/Sessions.tsx \
  src/pages/Layout.tsx \
  src/pages/Settings.tsx
```
(Only delete `SessionManifest.tsx` if Step 1 proved nothing imports it. Only delete `_form.tsx` after confirming ImagesPanel/RegistriesPanel no longer import it.)

- [ ] **Step 3: Neutralise the global body background in `theme.css`**

In `web/src/theme.css`, in the `@layer base { body { … } }` rule, remove the `background-color: var(--color-paper);` and `color: var(--color-ink);` lines (the shadcn `@layer base body` rule from `index.css` now sets `bg-background`/`text-foreground`). Leave the rest (font-feature-settings etc. are harmless; the transcript classes still reference `--fg`/`--bg`/`--color-*` which remain defined). This makes the shell zinc while the still-old SessionDetail renders as a paper "island" inside it.

- [ ] **Step 4: Verify the whole app**

Run:
```bash
cd web && pnpm exec tsc -b --noEmit && pnpm test && pnpm build
```
Expected: type-check PASS, all tests PASS, production build succeeds. Then `pnpm dev` and click through `/sessions`, `/sessions/all`, `/fleet`, `/storage`, `/settings/*`, open a session (`/sessions/$id` shows the old transcript, intentionally paper-styled), toggle dark mode.

- [ ] **Step 5: Commit**
```bash
git add -A web
git commit -m "chore(web): remove bespoke components; let shadcn own app background"
```

---

### Task 22: Mobile responsiveness pass & verification

**Files:** touch-ups only, across the surfaces built above.

shadcn's primary `Sidebar` handles mobile natively (off-canvas sheet + the `SidebarTrigger` in the RootLayout header). The section sidebars are already `hidden md:flex` with a mobile nav strip (Task 8). This task confirms the rest holds up at phone width.

- [ ] **Step 1: Confirm table overflow.** shadcn `Table` renders inside a `div.overflow-x-auto`, so the Sessions/Storage/Members tables scroll horizontally rather than overflow the viewport. Verify in DevTools at 375px — no horizontal page scroll, only the table scrolls. If any page added its own wrapper that clips this, remove it.

- [ ] **Step 2: Confirm stat-card grids reflow.** `MySessions` (`grid-cols-2 sm:grid-cols-4`), `Storage` (`grid-cols-2 sm:grid-cols-3 lg:grid-cols-6`), `Fleet` (`grid-cols-3`) all hold at 375px. If `Fleet`'s three rollup cards feel cramped, change to `grid-cols-1 sm:grid-cols-3`.

- [ ] **Step 3: Confirm dialogs/headers.** `NewSessionDialog`, drain `AlertDialog`, and the page headers (`flex items-start justify-between`) don't overflow at 375px — the New-session button wraps below the title if needed (`flex-wrap` on the header row if it clips).

- [ ] **Step 4: Manual sweep.** Run `pnpm dev`, open DevTools device toolbar at iPhone SE (375×667). Walk: open the primary rail via the header trigger; `/sessions` (strip nav + table scroll); `/sessions/all`; `/fleet`; `/storage`; `/settings/*` (strip nav); open a session (`/sessions/$id`); toggle dark mode. No element should cause horizontal page scroll.

- [ ] **Step 5: Commit any fixes.**
```bash
pnpm exec tsc -b --noEmit && pnpm test
git add -A web
git commit -m "fix(web): mobile responsiveness pass"
```

---

### Task 23: Update the redesign memory

**Files:** none in repo — update the assistant memory.

- [ ] **Step 1:** Update `engram-web-redesign` memory to record that the "discard default theme, keep Lab Notebook tokens" decision was **superseded** on 2026-06-03 by a pure-shadcn migration (zinc + dark mode, dual sidebar via nested layout routes, default fonts), SessionDetail deferred to assistant-ui. Point to this plan + the spec.

---

## Self-Review

**Spec coverage:**
- Foundation (deps, zinc tokens, dark mode, fonts, lucide) → Tasks 1–4 ✓
- Shell + nested layout routes + dual sidebar → Tasks 5–9 ✓
- Sessions (table, My/All via 2nd sidebar routes, New-session dialog, vital cards, token nudge) → Tasks 10–12 ✓
- Fleet → Task 13 ✓; Storage → Task 14 ✓
- Settings shell + Profile/Tokens/Members/Images/Registries → Tasks 8, 15–19 ✓
- UserChip → user menu → Task 5 ✓
- Excluded SessionDetail + theme.css coexistence (old-paper island) → Task 21 Step 3 ✓
- Auth screens → Task 20 ✓
- Mobile responsiveness (section sidebars → mobile nav strip; table overflow; grid reflow; verification sweep) → Tasks 8, 22 ✓
- Testing (matchMedia/ResizeObserver setup, updated component tests) → Tasks 3, 12, 18, 19 ✓
- Memory correction → Task 23 ✓

**Placeholder scan:** Tasks 18 & 19 intentionally describe the rewrite at a higher level ("mirror the data wiring exactly, swap presentation") rather than transcribing every line, because their data/hook wiring is already complete in the existing files and must be preserved verbatim — the instruction is to read the current file and keep its logic. Every other task has complete code.

**Type/name consistency:** `MainSidebar`, `UserMenu`, `ModeToggle`, `ThemeProvider`/`useTheme`, `SessionsTable`, `NewSessionDialog`, `session-format.ts` helpers (`shortId`/`stripImageHost`/`relativeTime`/`lifecycleOf`/`statusVariant`) are referenced consistently across tasks. Router uses `RootLayout`/`SessionsLayout`/`SettingsLayout` matching the files created in Tasks 7–8. Hooks/api/types names verified against the live source.

**Known sequencing note:** Task 11 imports `NewSessionDialog`; do Task 12 before Task 11 (or land the dialog stub first). Flagged inline in Task 11.
