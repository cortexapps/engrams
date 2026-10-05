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

describe("thread descriptions", () => {
  it("names the channel when the picker knows it, and titles a thread by its label", async () => {
    const { channelNames, describeThread, threadHandleLabel, threadLabel } =
      await import("./slack-format");
    const names = channelNames([{ key: "C0123", label: "#alerts" }]);
    expect(threadLabel("T1:C0123:1700.1", names)).toEqual({ channel: "#alerts", thread: "1700.1" });
    expect(threadLabel("T1:C9:1700.1", names)).toEqual({ channel: "#C9", thread: "1700.1" });
    expect(describeThread({ key: "T1:C0123:1700.1", label: "deploy is red" }, names)).toEqual({
      title: "deploy is red",
      subtitle: "#alerts · thread 1700.1",
    });
    // No label (a bare mention opened it): the place is the title.
    expect(describeThread({ key: "T1:C0123:1700.1", label: undefined }, names).title).toBe(
      "#alerts · thread 1700.1",
    );
    // The thread's root and a reply in it are one place.
    expect(threadHandleLabel("slack:C0123:1700.1", names)).toBe("#alerts · thread");
    expect(threadHandleLabel("slack:C0123:1700.9", names)).toBe("#alerts · thread");
    expect(threadHandleLabel("slack:C0123", names)).toBe("#alerts");
    expect(threadHandleLabel("github:acme/x#1", names)).toBeUndefined();
  });
});
