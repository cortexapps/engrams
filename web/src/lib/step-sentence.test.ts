import { describe, expect, it } from "vitest";

import { stepSentence, type StepFacts } from "./step-sentence";

function facts(partial: Partial<StepFacts>): StepFacts {
  return { status: "succeeded", inputs: {}, outputs: {}, ...partial };
}

describe("stepSentence", () => {
  it("says what each kind did", () => {
    expect(stepSentence("filter", facts({ status: "skipped", outputs: { pass: false } }))).toBe(
      "Did not pass; the run ended here as filtered",
    );
    expect(stepSentence("filter", facts({ outputs: { pass: true } }))).toBe("Passed");
    expect(stepSentence("branch", facts({ outputs: { taken: "else" } }))).toBe(
      "Took the else branch",
    );
    expect(stepSentence("loop", facts({ outputs: { iterations: 3, exhausted: true } }))).toBe(
      "Ran 3 iterations and hit the cap",
    );
    expect(
      stepSentence("code", facts({ inputs: { mode: "boolean" }, outputs: { value: false } })),
    ).toBe("Returned false; the run ended here as filtered");
    expect(
      stepSentence("create_session", facts({ outputs: { session_id: "se-abcdef12-x" } })),
    ).toBe("Started session se-abcde");
    expect(
      stepSentence(
        "run_agent",
        facts({
          outputs: { session_id: "se-abcdef12-x", started: false, delivered: true, relayed: true },
        }),
      ),
    ).toBe("Sent the turn to session se-abcde, relayed to the thread");
    expect(stepSentence("run_command", facts({ outputs: { exit_status: 0 } }))).toBe("Exited 0");
    expect(
      stepSentence(
        "integration_action",
        facts({ inputs: { provider: "github", actionId: "create_issue_comment" } }),
      ),
    ).toBe("Ran create issue comment on github");
    expect(stepSentence("resolve_user", facts({ outputs: { found: false } }))).toBe(
      "The author is not a linked user",
    );
    expect(
      stepSentence("review_settle", facts({ outputs: { to_post_count: 2, ui_only_count: 1 } })),
    ).toBe("Settled 2 findings to post, 1 kept in the UI");
  });

  it("the error wins; a skipped step says so; an unknown kind reads its outputs", () => {
    expect(stepSentence("run_command", facts({ status: "failed", error: "exit 2: no git" }))).toBe(
      "exit 2: no git",
    );
    expect(stepSentence("send_prompt", facts({ status: "skipped" }))).toBe("Skipped");
    expect(stepSentence(undefined, facts({ outputs: { exit_status: 0, stdout: "ok" } }))).toBe(
      "exit_status 0 · stdout ok",
    );
    expect(stepSentence("relay_session", facts({ outputs: {} }))).toBe("succeeded");
  });
});
