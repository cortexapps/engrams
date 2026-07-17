---
name: share-file
description: Deliver an existing image or video into the engrams conversation when the user explicitly requested that artifact or another active skill explicitly requires delivery. Do not use merely because an internal screenshot, render, or recording exists.
---

# Sharing a file (image / video)

This engrams session can surface an existing image or video in the conversation
history. Delivery is separate from creation and inspection: do not share a file
merely because you rendered, recorded, or looked at it internally.

Use this skill only when the user explicitly asks to receive visual evidence or
the requested deliverable is itself an image/video. If another active skill
defines a stricter sharing policy, follow it. For browser work, share one final
screenshot by default and never share internal recovery screenshots.

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
