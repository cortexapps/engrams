# Browser capability evaluation

This directory contains the deterministic evaluation used by ADR 0097. It is
deliberately independent of the product web application and databases.

The Incident Console task exercises:

- filtering and selecting an incident in normal DOM controls;
- a rerender that invalidates old element references;
- an obstructing acknowledgement dialog;
- a topology drawn only on a canvas; and
- exact server-side validation of the selected remediation.

The task object exposes BrowserGym's `setup(page)` and
`validate(page, chat_messages)` shape. The black-box runner attaches Codex or
Claude to a shared Chrome through the candidate CLI and uses the same grader.
No LLM judges pass/fail.

```sh
just browser-eval-smoke
just browser-eval
```

CI runs only `just browser-eval-smoke`: deterministic Python contract tests and
the no-model grader smoke. It never launches Codex, Claude, Chrome, or a browser
CLI and requires no model credentials. The live matrix and MiniWoB calibration
are explicit, on-demand runs.

The full matrix invokes authenticated model CLIs and may consume quota. Use
`--dry-run` to print its 64 episodes without invoking a model. Results go to
`artifacts/browser-eval/<run-id>/`, which is intentionally untracked.
Independent episodes may be run with `--jobs 2` or `--jobs 4`; each worker has
its own Chrome profile, CDP port, task server, CLI state, and artifact directory.

The top-two calibration uses the official BrowserGym 0.14.3 task classes and
the BrowserGym-pinned MiniWoB++ revision
`7fd85d71a4b60325c6585396ec4f48377d049838`. Point the runner at an
environment containing `browsergym-miniwob==0.14.3` and that checkout, then
name the selected arms:

```sh
ENGRAM_BROWSERGYM_PYTHON=/path/to/venv/bin/python \
ENGRAM_MINIWOB_ROOT=/path/to/miniwob-plusplus \
python3 evals/browser/browser_eval.py miniwob --arm hybrid --arm pw-latest
```

The six task shapes are click-button, choose-list, enter-text,
click-checkboxes, click-menu-2, and use-autocomplete. Both harnesses run each
shape with the same seed and BrowserGym's exact grader.

Tool overrides are useful for offline development:

```sh
ENGRAM_EVAL_AGENT_BROWSER_COMMAND="/path/to/agent-browser" \
ENGRAM_EVAL_PLAYWRIGHT_017_COMMAND="/path/to/playwright-cli" \
python3 evals/browser/browser_eval.py smoke
```
