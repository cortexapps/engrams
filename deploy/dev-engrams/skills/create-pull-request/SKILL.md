---
name: create-pull-request
description: Open a pull request (GitHub) / merge request (GitLab) for your changes from inside an engrams session. Use after you've committed and pushed a branch and the task is ready for review. No git tokens needed — the engrams platform brokers credentials.
---

# Creating a pull request

This engrams session can push to git and open a change request **without any
tokens of your own**. The platform brokers a short-lived, repo-scoped
credential per git operation, so there is nothing to configure or paste.

## 1. Commit and push your branch

Work on a branch and push it the normal way:

```bash
git checkout -b my-feature
git add -A && git commit -m "Describe the change"
git push -u origin my-feature
```

Authentication happens automatically: git's `GIT_ASKPASS` helper fetches a
fresh credential from the host for each operation. **Do not** set
`GITHUB_TOKEN` or edit git credentials — it's already wired.

## 2. Open the change request

Run `engram-pr` — it is provider-agnostic (GitHub PR, GitLab MR, …) and
routes through the platform. **Do not** use `gh` or `glab`.

```bash
engram-pr --repo cortexapps/engrams \
  --head my-feature \
  --base main \
  --title "Short, imperative summary" \
  --body "What changed and why. Reference any issue."
```

It prints the URL of the created PR/MR. Notes:

- `--repo` is `owner/name`; any repo the platform's installation can access
  works (you are not pinned to one repo).
- `--base` defaults to `main`.
- add `--draft` to open a draft.

The opened PR is also surfaced on the session's event stream, so whoever
launched the session sees the link.
