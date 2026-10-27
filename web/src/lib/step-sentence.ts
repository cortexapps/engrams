/** One sentence per step for the activity trace: what the block did, in the
 * words of its kind, from the ledger row's rendered inputs, outputs, status
 * and error. A kind without a sentence falls back to a compact readout of
 * its scalar outputs, so no row is ever blank. */

export interface StepFacts {
  status: string;
  error?: string;
  inputs: Record<string, unknown>;
  outputs: Record<string, unknown>;
}

function str(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}

function num(value: unknown): number | undefined {
  return typeof value === "number" ? value : undefined;
}

function clip(text: string, max = 72): string {
  const line = text.split("\n")[0] ?? "";
  return line.length > max ? `${line.slice(0, max - 1)}…` : line;
}

function short(id: string | undefined): string {
  return id ? id.slice(0, 8) : "";
}

function plural(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}

/** The scalar outputs as `key value` pairs, three at most. */
export function outputsReadout(outputs: Record<string, unknown>): string | undefined {
  const parts = Object.entries(outputs)
    .filter(([, v]) => typeof v === "string" || typeof v === "number" || typeof v === "boolean")
    .slice(0, 3)
    .map(([k, v]) => `${k} ${String(v).slice(0, 40)}`);
  return parts.length > 0 ? parts.join(" · ") : undefined;
}

type Sentence = (facts: StepFacts) => string | undefined;

const SENTENCES: Record<string, Sentence> = {
  filter: ({ status, outputs }) =>
    outputs["pass"] === false || status === "skipped"
      ? "Did not pass; the run ended here as filtered"
      : "Passed",
  branch: ({ outputs }) =>
    outputs["taken"] === "else" ? "Took the else branch" : "Took the then branch",
  loop: ({ outputs }) => {
    const n = num(outputs["iterations"]);
    if (n === undefined) return undefined;
    return outputs["exhausted"] === true
      ? `Ran ${plural(n, "iteration")} and hit the cap`
      : `Ran ${plural(n, "iteration")}`;
  },
  code: ({ inputs, outputs }) => {
    if (inputs["mode"] === "boolean") {
      return outputs["value"] === false
        ? "Returned false; the run ended here as filtered"
        : "Returned true";
    }
    const value = outputs["value"];
    if (value === undefined) return undefined;
    const text = typeof value === "string" ? value : JSON.stringify(value);
    return `Computed ${clip(text ?? "", 56)}`;
  },
  create_session: ({ outputs }) => {
    const id = str(outputs["session_id"]);
    return id ? `Started session ${short(id)}` : undefined;
  },
  run_agent: ({ outputs }) => {
    const id = str(outputs["session_id"]);
    if (!id) return undefined;
    const relayed = outputs["relayed"] === true ? ", relayed to the thread" : "";
    if (outputs["started"] === true) return `Started session ${short(id)}${relayed}`;
    if (outputs["delivered"] === true) return `Sent the turn to session ${short(id)}${relayed}`;
    return `Session ${short(id)}${relayed}`;
  },
  send_prompt: ({ outputs }) => {
    const outcome = str(outputs["outcome"]);
    const signal = str(outputs["signal"]);
    if (signal) return `Prompt sent; the session signalled ${signal}`;
    if (!outcome) return "Prompt sent";
    if (outcome === "none") return "Prompt sent, no wait";
    return `Prompt sent; the turn ended as ${outcome.replaceAll("_", " ")}`;
  },
  wait_session: ({ outputs }) => {
    const outcome = str(outputs["outcome"]);
    return outcome ? `Waited; the session is ${outcome.replaceAll("_", " ")}` : undefined;
  },
  wait_event: ({ outputs }) => {
    const key = str(outputs["event_key"]);
    return key ? `Received ${key.replaceAll("_", " ")}` : "Received an event";
  },
  end_session: ({ outputs }) =>
    outputs["ended"] === false ? "The session was already gone" : "Ended the session",
  session_status: ({ outputs }) => {
    if (outputs["found"] === false) return "No session found";
    const status = str(outputs["status"]);
    return status ? `The session is ${status.replaceAll("_", " ")}` : undefined;
  },
  run_command: ({ outputs }) => {
    const code = num(outputs["exit_status"]);
    if (code === undefined) return undefined;
    return code === 0 ? "Exited 0" : `Exited ${code}`;
  },
  write_files: ({ outputs }) => {
    const files = outputs["files"];
    return Array.isArray(files) ? `Wrote ${plural(files.length, "file")}` : undefined;
  },
  state_get: ({ outputs }) =>
    outputs["found"] === false ? "No entry under that key" : "Read the entry",
  state_set: ({ outputs }) =>
    outputs["ok"] === false ? "The write lost its version check" : "Wrote the entry",
  state_delete: ({ outputs }) =>
    outputs["deleted"] === false ? "Nothing to delete" : "Deleted the entry",
  state_list: ({ outputs }) => {
    const count = num(outputs["count"]);
    return count === undefined ? undefined : `Listed ${plural(count, "entry")}`;
  },
  lookup_pr_session: ({ outputs }) =>
    outputs["found"] === true
      ? `Found the PR's session ${short(str(outputs["session_id"]))}`
      : "No session for this PR",
  lookup_instance_session: ({ outputs }) => {
    if (outputs["found"] === true)
      return `Found the agent's session ${short(str(outputs["session_id"]))}`;
    const reason = str(outputs["reason"]);
    return reason ? `No kept session (${reason.replaceAll("_", " ")})` : "No kept session";
  },
  instance_close: ({ outputs }) =>
    outputs["not_instanced"] === true
      ? "No agent to close"
      : outputs["closed"] === true
        ? "Closed the agent"
        : "The agent was already closed",
  claim_handle: ({ outputs }) => {
    const handle = str(outputs["handle"]);
    if (outputs["claimed"] === false)
      return handle ? `${handle} is owned elsewhere` : "Not claimed";
    return handle ? `Claimed ${handle}` : "Claimed the handle";
  },
  integration_action: ({ inputs, error }) => {
    const provider = str(inputs["provider"]);
    const action = str(inputs["actionId"]);
    const what = action ? action.replaceAll("_", " ") : "the action";
    if (error) return undefined;
    return provider ? `Ran ${what} on ${provider}` : `Ran ${what}`;
  },
  decide: ({ outputs }) => {
    if (outputs["decided"] === false) {
      const reason = str(outputs["reason"]);
      return reason ? `No decision: ${clip(reason, 56)}` : "No decision";
    }
    const answers = outputs["answers"];
    if (typeof answers === "object" && answers !== null) {
      const keys = Object.keys(answers as Record<string, unknown>);
      return `Decided ${plural(keys.length, "question")}`;
    }
    return "Decided";
  },
  list_profiles: ({ outputs }) => {
    const profiles = outputs["profiles"];
    return Array.isArray(profiles) ? `Listed ${plural(profiles.length, "profile")}` : undefined;
  },
  slack_choice: ({ outputs }) => {
    const label = str(outputs["label"]);
    if (outputs["outcome"] === "timed_out") return "Nobody clicked in time";
    return label ? `Picked ${label}` : undefined;
  },
  resolve_user: ({ outputs }) =>
    outputs["found"] === true ? "Matched the author to a user" : "The author is not a linked user",
  relay_session: ({ outputs }) =>
    outputs["created"] === true
      ? "Relaying the session into the thread"
      : outputs["installed"] === true
        ? "Pointed the relay at this message"
        : undefined,
  relay_close: ({ outputs }) =>
    outputs["posted"] === true ? "Posted the closing message" : "Nothing to post",
  review_open_pass: ({ outputs }) =>
    outputs["deduplicated"] === true
      ? "This head was already reviewed"
      : `Opened a review pass${outputs["superseded_review_id"] ? ", superseding the one in flight" : ""}`,
  review_stage: ({ outputs }) => {
    const phase = str(outputs["phase"]);
    return phase ? `Staged the ${phase}` : undefined;
  },
  review_settle: ({ outputs }) => {
    const post = num(outputs["to_post_count"]);
    const ui = num(outputs["ui_only_count"]);
    if (post === undefined) return undefined;
    return `Settled ${plural(post, "finding")} to post${ui ? `, ${ui} kept in the UI` : ""}`;
  },
  review_close_pass: ({ outputs }) => {
    const outcome = str(outputs["outcome"]);
    return outcome ? `Closed the pass as ${outcome}` : undefined;
  },
  review_record_post: ({ outputs }) =>
    outputs["recorded"] === true ? "Linked the pass to its GitHub review" : "Nothing to record",
};

/** The sentence for a step. The error wins when the step has one; a kind
 * with no sentence, or one that cannot say anything from these facts, gets
 * the scalar readout, then the status word. */
export function stepSentence(blockType: string | undefined, facts: StepFacts): string {
  if (facts.error) return facts.error;
  if (facts.status === "skipped" && blockType !== "filter") return "Skipped";
  const sentence = blockType ? SENTENCES[blockType]?.(facts) : undefined;
  return sentence ?? outputsReadout(facts.outputs) ?? facts.status.replaceAll("_", " ");
}
