---
name: record-demo
description: Record a screenshot or short video of a web UI you built (your dev server on guest localhost) and share it in the session conversation. Use to SHOW a frontend change visually — a page you styled, a flow you wired, a fix you verified. Drives a real headless Chromium via the `playwright` MCP, then surfaces the media with engram-share.
---

# Recording an image / video demo of your work

This session has a headless browser wired through the **`playwright` MCP
server** (chromium-headless-shell). Use it to capture your own work — a dev
server running on `localhost` inside this session — and then share the result
so the person who launched the session sees it inline.

## 1. Start your app

Bring up whatever you're demoing on a local port, e.g.:

```bash
npm run dev   # or: just dev, python -m http.server, etc.
```

Note the URL (e.g. `http://localhost:3000`).

## 2. Drive the browser via the `playwright` MCP tools

Use the MCP tools (no shell needed):

- `browser_navigate` to `http://localhost:<port>` — load your page.
- `browser_take_screenshot` — capture a PNG. Save it to a path you control,
  e.g. `/workspace/demo.png`.
- For a flow, interact (`browser_click`, `browser_type`, `browser_fill_form`)
  between navigations and take screenshots at each step. For a video, use the
  MCP's video-recording capability; it produces a `.webm`.

The browser reaches `localhost` because it runs **inside this session**,
alongside your dev server.

## 3. Share it

Surface the media in the conversation with the `share-file` skill:

```bash
engram-share --file /workspace/demo.png --caption "Dashboard after the redesign"
```

Supported: images (`png`, `jpeg`, `gif`, `webp`) and video (`mp4`, `webm`).
The shared file appears inline on the session's event stream.

## Tips

- Headless only — there's no visible window; screenshots/video are how you
  see the result.
- Set a viewport with `browser_resize` before capturing if you want a
  specific frame size.
- Keep videos short; large files count against the session's artifact quota.
