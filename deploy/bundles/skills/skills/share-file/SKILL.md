---
name: share-file
description: Share an image or video file so it appears in the engrams session's conversation history for the person who launched the session. Use when you want to SHOW your work visually — a screenshot of a page you built, a screen recording of a flow, a rendered chart. No tokens or setup needed.
---

# Sharing a file (image / video)

This engrams session can surface an image or video in the conversation history
the launcher is watching — use it to **show**, not just describe, your work
(e.g. a screenshot after a UI change, a short screen recording of a flow).

## Usage

```bash
engram-share --file /path/to/screenshot.png --caption "Dashboard after the fix"
```

- `--file` (required): path to the file in this session's filesystem.
- `--caption` (optional): a short human-readable caption.

Supported types: images (`png`, `jpeg`, `gif`, `webp`) and video (`mp4`,
`webm`). The file's content is verified — other file types are rejected. It
prints a confirmation with the stored artifact id. The shared file is surfaced
on the session's event stream, so whoever launched the session sees it inline.
