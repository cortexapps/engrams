import { describe, expect, it } from "vitest";
import { parseThreadKey, threadLabel } from "./slack-format";

describe("thread keys", () => {
  it("parses team:channel:thread_ts and labels the channel the way Slack URLs do", () => {
    expect(parseThreadKey("T1:C0123:1700.1")).toEqual({
      team: "T1",
      channel: "C0123",
      threadTs: "1700.1",
    });
    expect(parseThreadKey("junk")).toBeNull();
    expect(threadLabel("T1:C0123:1700.1")).toEqual({ channel: "#C0123", thread: "1700.1" });
    expect(threadLabel("junk")).toEqual({ channel: "junk", thread: "" });
  });
});
