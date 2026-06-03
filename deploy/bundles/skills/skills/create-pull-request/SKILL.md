---
name: create-pull-request
description: Open a pull request (GitHub) / merge request (GitLab) for your changes from inside an engrams session. Use after you've committed and pushed a branch and the task is ready for review. No git tokens needed — the engrams platform brokers credentials.
---

# Creating a pull request

This engrams session can push to git and open a change request **without any
tokens of your own**. The platform brokers a short-lived, repo-scoped
credential per git operation, so there is nothing to configure or paste.

## 1. Sync with the base branch first

The repo checkout baked into this session's image may be **behind the
remote** (and may even carry stray uncommitted files from the bake). A PR
cut from a stale base opens with phantom diffs and merge conflicts — and a
conflicted PR can't render its image previews at all. Always re-anchor on
the live base branch before you start committing:

```bash
git fetch origin
git status --short          # stray uncommitted files from the bake? don't commit them
git checkout -b my-feature origin/main
```

If you already committed on a stale base, `git merge origin/main` (or
rebase) and resolve conflicts *before* opening the PR.

## 2. Commit and push your branch

Work on a branch and push it the normal way:

```bash
git add -A && git commit -m "Describe the change"
git push -u origin my-feature
```

Authentication happens automatically: git's `GIT_ASKPASS` helper fetches a
fresh credential from the host for each operation. **Do not** set
`GITHUB_TOKEN` or edit git credentials — it's already wired.

## 3. Open the change request

Run `engram-pr` — it is provider-agnostic (GitHub PR, GitLab MR, …) and
routes through the platform. **Do not** use `gh` or `glab`.

```bash
engram-pr --repo owner/name \
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

## Images in the PR body

Relative repo paths (`docs/assets/foo.png`) do **not** render in a PR
description — only in markdown files inside the repo tree. A PR body has no
base path to resolve against, so relative refs show as broken images. If
you committed screenshots and want them in the body, reference them by
absolute URL anchored to your commit SHA (survives branch deletion after
merge):

```markdown
![Sessions page](https://github.com/owner/name/raw/<commit-sha>/docs/assets/foo.png)
```

Get the SHA with `git rev-parse HEAD` after your final push.
