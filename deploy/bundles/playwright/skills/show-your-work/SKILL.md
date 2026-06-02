---
name: show-your-work
description: Capture screenshots and short screen recordings (with chapter markers) of a web UI you're building, by driving a real headless browser, and surface them in the session for the person who launched it. Use whenever there's something visual to SHOW rather than describe — to solicit mid-task feedback ("does this look right?"), to attach a repro of a bug, or for a final show-and-tell of the feature working. No tokens or setup needed.
---

# Show your work (screenshots + screen recordings)

When you've built or changed something with a visual surface — a page, a
component, a flow, a chart — **show it, don't just describe it.** This session
can drive a real headless browser against your running app, capture
screenshots and a short video (with chapter markers), and surface them in the
conversation the launcher is watching. Good for:

- **Feedback while you work** — "here's the new dashboard, does the layout look
  right?" Share a screenshot and keep going.
- **Bug repro** — record the exact steps that reproduce an issue.
- **Final show-and-tell** — a short video of the feature working end to end.

The tool is **`playwright-cli`** (Microsoft's browser-automation CLI). It's
pre-configured here — a headless Chromium is wired up; you just drive it. Run
`playwright-cli --help` (or `playwright-cli <command> --help`) for the full
reference; the common verbs are below.

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
