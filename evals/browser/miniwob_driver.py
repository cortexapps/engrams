#!/usr/bin/env python3
"""Run an official BrowserGym MiniWoB++ task over the evaluator's CDP page."""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any

from browsergym.miniwob import all as miniwob_tasks
from playwright.sync_api import sync_playwright


TASKS = {
    "click-button": miniwob_tasks.ClickButtonTask,
    "choose-list": miniwob_tasks.ChooseListTask,
    "enter-text": miniwob_tasks.EnterTextTask,
    "click-checkboxes": miniwob_tasks.ClickCheckboxesTask,
    "click-menu-2": miniwob_tasks.ClickMenu2Task,
    "use-autocomplete": miniwob_tasks.UseAutocompleteTask,
}


def emit(value: dict[str, Any]) -> None:
    print(json.dumps(value, separators=(",", ":")), flush=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cdp", required=True)
    parser.add_argument("--task", choices=TASKS, required=True)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--base-url", required=True)
    args = parser.parse_args()

    with sync_playwright() as playwright:
        browser = playwright.chromium.connect_over_cdp(args.cdp)
        context = browser.contexts[0]
        page = context.pages[0] if context.pages else context.new_page()
        task = TASKS[args.task](
            seed=args.seed,
            base_url=args.base_url.rstrip("/") + "/",
            episode_max_time=180_000,
        )
        page.set_viewport_size(task.viewport)
        goal, info = task.setup(page)
        emit({"type": "setup", "goal": goal, "info": info, "url": page.url})
        for line in sys.stdin:
            request = json.loads(line)
            if request.get("command") == "validate":
                reward, done, message, result_info = task.validate(page, [])
                emit(
                    {
                        "type": "grade",
                        "success": reward > 0,
                        "reward": reward,
                        "done": done,
                        "message": message,
                        "info": result_info,
                    }
                )
            elif request.get("command") == "close":
                task.teardown()
                return 0
            else:
                emit({"type": "error", "error": "unknown command"})
                return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
