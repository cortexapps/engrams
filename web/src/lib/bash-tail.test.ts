import { describe, expect, it } from "vitest";
import { bashTailCommand, tailFileId } from "./bash-tail";

// The sanitizer is a two-sided contract with the harness hook bridge
// (`tail_file_id` in crates/engram-harness-claude/src/main.rs): both sides
// must map the same tool_call_id to the same filename, and hostile input
// must never survive into generated shell text.

describe("tailFileId", () => {
  it("passes real tool ids through unchanged", () => {
    expect(tailFileId("toolu_01AbC-x_9")).toBe("toolu_01AbC-x_9");
  });

  it("strips shell metacharacters and caps length like the harness", () => {
    expect(tailFileId("$(rm -rf /)'")).toBe("rm-rf");
    expect(tailFileId("a".repeat(200))).toHaveLength(128);
  });

  it("returns null when nothing survives", () => {
    expect(tailFileId("$('`\"")).toBeNull();
  });
});

describe("bashTailCommand", () => {
  it("targets the sanitized log and pid paths", () => {
    const cmd = bashTailCommand("toolu_01AB");
    expect(cmd).toContain("'/tmp/engram-bash/toolu_01AB.log'");
    expect(cmd).toContain("'/tmp/engram-bash/toolu_01AB.pid'");
    expect(cmd).toContain("--pid=");
  });

  it("yields no command for an unsanitizable id", () => {
    expect(bashTailCommand("$('`\"")).toBeNull();
  });
});
