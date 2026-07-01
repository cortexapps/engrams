---
name: show-your-work
description: Drive a real browser with `playwright-cli` — open pages, click, fill, snapshot the accessibility tree, screenshot, record video — against a web UI you're building. This is the SAME live browser the person who launched the session can open and watch (and take over) in the BROWSER tab, so they see your actions in real time. Use whenever there's something visual to SHOW or verify rather than describe — mid-task feedback ("does this look right?"), a bug repro, a final show-and-tell, or just to check your own UI. No tokens or setup needed.
---

# Drive the browser (`playwright-cli`) — screenshots, recordings, and checking your work

When you've built or changed something with a visual surface — a page, a
component, a flow, a chart — **drive the browser and show it, don't just
describe it.** The tool is **`playwright-cli`** (Microsoft's browser-automation
CLI), and it's already wired up here: just run it.

**Key thing to know:** `playwright-cli` drives the session's **one shared, live
Chromium** — the *same* browser the person who launched you can open in the
**BROWSER tab** and watch you drive (they can even grab the mouse/keyboard).
It's not a hidden headless instance. So when you `open` a URL or click around,
your actions are visible to them in real time. Good for:

- **Feedback while you work** — "here's the new dashboard, does the layout look
  right?" Screenshot it (or tell them to watch the BROWSER tab) and keep going.
- **Bug repro** — record the exact steps that reproduce an issue.
- **Final show-and-tell** — a short video of the feature working end to end.
- **Checking your own work** — snapshot the page to verify what actually rendered.

Run `playwright-cli --help` (or `playwright-cli <command> --help`) for the full
reference; the common verbs are below. (The first command may take a moment
while the shared browser starts up.)

## 1. Point it at your running app

Start your app the normal way (e.g. `npm run dev`, `just dev`) and note the URL
it serves on — it's reachable on this session's **localhost**:

```bash
playwright-cli open http://localhost:3000      # your dev server's URL/port
```

## 2. Explore + record

`playwright-cli` keeps the browser open across commands, so you can navigate,
look, and react — exactly like a person clicking around:

```bash
playwright-cli video-start                       # begin recording the session
playwright-cli video-chapter "Open the dashboard"
playwright-cli snapshot                           # accessibility tree w/ element refs (e1, e2…)
playwright-cli click e7                           # click an element by its ref (or a CSS/text selector)
playwright-cli video-chapter "Apply a filter"
playwright-cli fill e12 "search term"
playwright-cli screenshot --filename /workspace/dashboard.png   # capture a key state
playwright-cli video-stop                         # stops + saves the .webm
playwright-cli close
```

- `snapshot` gives you a text view of the page (with refs) to decide what to do
  next — use it to *discover* what's on the page, then act on the refs.
- Use `video-chapter "<title>"` before each meaningful step so the recording is
  navigable.
- `screenshot --filename <path>` saves a PNG of the current view
  (`--full-page` for the whole scrollable page); without `--filename` it
  auto-names one.

## 3. Share it

Surface the image or video in the conversation with `engram-share`:

```bash
engram-share --file /workspace/dashboard.png --caption "Dashboard after the redesign"
engram-share --file <the .webm path printed by video-stop> --caption "Filtering flow"
```

Supported: images (`png`, `jpeg`, `gif`, `webp`) and video (`mp4`, `webm`).
Whoever launched the session sees it inline.

## Tips

- **Keep recordings short and chaptered.** Long videos are large and count
  against the session's artifact quota; a focused 10–30s clip per flow is
  ideal. Take a screenshot for a single state instead of a video.
- It's headless — there's no visible window; screenshots/video are how you (and
  the launcher) see the result.
- The browser reaches **only this session's localhost + whatever the network
  policy allows** — it can't see the public internet unless the image permits it.
- `playwright-cli` also does `type`, `hover`, `select`, `press`, `go-back`,
  `pdf`, and more — see `playwright-cli --help`.
