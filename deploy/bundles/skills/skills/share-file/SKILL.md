---
name: share-file
description: Deliver an existing file into the engrams conversation when the user explicitly requested that file or another active skill explicitly requires delivery. Any file type. Do not use merely because an internal screenshot, render, or intermediate file exists.
---

# Sharing a file

This engrams session can surface an existing file in the conversation
history. Any file type is accepted. The conversation shows a card with
the file name, size, and a download button; images and video render
inline.

Delivery is separate from creation and inspection: do not share a file
merely because you rendered, recorded, or looked at it internally. Use
this skill only when the user explicitly asks to receive a file or the
requested deliverable is itself a file. If another active skill defines
a stricter sharing policy, follow it. For browser work, share one final
screenshot by default and never share internal recovery screenshots.

Sharing is not publishing: a shared file lives in this conversation
only. To publish an HTML or Markdown document as a hosted, versioned
artifact with a stable URL (visible across sessions), use the `Artifact`
tool instead.

## Usage

```bash
engram-share --file /path/to/report.pdf --caption "Q3 capacity report"
```

- `--file` (required): path to the file in this session's filesystem.
- `--caption` (optional): a short human-readable caption.

The server stamps the stored media type from the file's content (with
the extension as a fallback for text formats) — a mislabeled extension
does not change what the file is served as. It prints a confirmation
with the stored artifact id. The shared file is surfaced on the
session's event stream, so whoever launched the session sees it inline.
