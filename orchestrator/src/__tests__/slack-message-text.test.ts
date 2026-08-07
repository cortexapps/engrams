/**
 * `replyText` — every content surface of a Slack message reaches the prompt.
 *
 * The regression that motivated this: the Linear app posts a one-line summary
 * as `text` and the ticket itself in an attachment. The old rule ("read
 * attachments only when `text` is empty") silently dropped the ticket, so a
 * session started from that thread saw the summary alone.
 */

import { expect, test, describe } from "bun:test";
import { replyText } from "../integrations/slack-message-text.ts";

describe("replyText()", () => {
  test("an app that summarizes in `text` and details in an attachment keeps BOTH", () => {
    // The shape the Linear Slack app posts.
    expect(
      replyText({
        text: "Madison Unell added an issue to the Product Feedback team",
        attachments: [
          {
            title: "PFR-412 Session loses the ticket body",
            title_link: "https://linear.app/acme/issue/PFR-412",
            text: "Starting a session from a Linear unfurl gives the agent no ticket.",
            fields: [
              { title: "Status", value: "Triage" },
              { title: "Priority", value: "Urgent" },
            ],
          },
        ],
      }),
    ).toBe(
      "Madison Unell added an issue to the Product Feedback team\n" +
        "<https://linear.app/acme/issue/PFR-412|PFR-412 Session loses the ticket body>\n" +
        "Starting a session from a Linear unfurl gives the agent no ticket.\n" +
        "Status: Triage\n" +
        "Priority: Urgent",
    );
  });

  test("a human's text plus a link unfurl keeps the unfurl (it is content, not noise)", () => {
    expect(
      replyText({
        text: "<@BOT> is this true? <https://linear.app/acme/issue/PFR-412|PFR-412>",
        attachments: [{ title: "PFR-412", text: "Users report the button does nothing." }],
      }),
    ).toBe(
      "<@BOT> is this true? <https://linear.app/acme/issue/PFR-412|PFR-412>\n" +
        "PFR-412\nUsers report the button does nothing.",
    );
  });

  test("an attachments-only post (empty text) still folds in", () => {
    expect(
      replyText({ text: "", attachments: [{ title: "Deploy failed", text: "step `build` exited 1" }] }),
    ).toBe("Deploy failed\nstep `build` exited 1");
  });

  test("`fallback` is read only when the attachment renders nothing else", () => {
    expect(replyText({ attachments: [{ fallback: "CI red on main" }] })).toBe("CI red on main");
    // …and never doubles a body it merely restates.
    expect(replyText({ attachments: [{ title: "CI red on main", fallback: "CI red on main" }] })).toBe(
      "CI red on main",
    );
  });

  test("Block Kit sections, headers, fields, and context all render", () => {
    expect(
      replyText({
        text: "",
        blocks: [
          { type: "header", text: { type: "plain_text", text: "Incident 42" } },
          {
            type: "section",
            text: { type: "mrkdwn", text: "p99 over budget for 12m" },
            fields: [
              { type: "mrkdwn", text: "*Service*\ncheckout" },
              { type: "mrkdwn", text: "*Region*\nus-west2" },
            ],
          },
          { type: "divider" },
          { type: "context", elements: [{ type: "mrkdwn", text: "paged by datadog" }] },
          {
            type: "actions",
            elements: [{ type: "button", text: { type: "plain_text", text: "Runbook" }, url: "https://rb/42" }],
          },
        ],
      }),
    ).toBe(
      "Incident 42\n" +
        "p99 over budget for 12m\n*Service*\ncheckout\n*Region*\nus-west2\n" +
        "paged by datadog\n" +
        "Runbook\nhttps://rb/42",
    );
  });

  test("an attachment whose payload is Block Kit renders (the modern unfurl shape)", () => {
    expect(
      replyText({
        text: "New issue",
        attachments: [
          {
            blocks: [
              { type: "section", text: { type: "mrkdwn", text: "*PFR-9* Login button dead" } },
              { type: "context", elements: [{ type: "mrkdwn", text: "assigned to Madison" }] },
            ],
          },
        ],
      }),
    ).toBe("New issue\n*PFR-9* Login button dead\nassigned to Madison");
  });

  test("a rich_text body does not double the `text` fallback that restates it", () => {
    expect(
      replyText({
        text: "hey <@BOT> please look :eyes:",
        blocks: [
          {
            type: "rich_text",
            elements: [
              {
                type: "rich_text_section",
                elements: [
                  { type: "text", text: "hey " },
                  { type: "user", user_id: "BOT" },
                  { type: "text", text: " please look " },
                  { type: "emoji", name: "eyes" },
                ],
              },
            ],
          },
        ],
      }),
    ).toBe("hey <@BOT> please look :eyes:");
  });

  test("a rich_text block with content the `text` fallback omits keeps both", () => {
    const out = replyText({
      text: "see the list",
      blocks: [
        {
          type: "rich_text",
          elements: [
            {
              type: "rich_text_list",
              elements: [
                { type: "rich_text_section", elements: [{ type: "text", text: "retry the job" }] },
                { type: "rich_text_section", elements: [{ type: "text", text: "then page oncall" }] },
              ],
            },
          ],
        },
      ],
    });
    // The contract is content, not layout: the walk keeps every word a block
    // carries, and drops Block Kit's presentation (bullets, inline runs).
    expect(out).toBe("see the list\nretry the job\nthen page oncall");
  });

  test("a rich_text link keeps its url", () => {
    expect(
      replyText({
        blocks: [
          {
            type: "rich_text",
            elements: [
              {
                type: "rich_text_section",
                elements: [
                  { type: "text", text: "ticket: " },
                  { type: "link", url: "https://linear.app/acme/issue/PFR-9", text: "PFR-9" },
                ],
              },
            ],
          },
        ],
      }),
      // Label and target both survive, unwrapped — the agent gets the raw URL.
    ).toBe("ticket:\nPFR-9\nhttps://linear.app/acme/issue/PFR-9");
  });

  test("shared files reach the prompt as named references", () => {
    expect(
      replyText({
        text: "look at this",
        files: [{ title: "trace.png", mimetype: "image/png", permalink: "https://files.slack.com/t.png" }],
      }),
    ).toBe("look at this\n[file: trace.png image/png https://files.slack.com/t.png]");
  });

  test("a blocks-only post with no `text` fallback still reaches the prompt", () => {
    // The old rule dropped this message entirely. Inline runs each land on
    // their own line and emoji/mention runs are lost — degraded, but present.
    expect(
      replyText({
        blocks: [
          {
            type: "rich_text",
            elements: [
              {
                type: "rich_text_section",
                elements: [
                  { type: "text", text: "deploy " },
                  { type: "text", text: "blocked on migration 0142" },
                ],
              },
            ],
          },
        ],
      }),
    ).toBe("deploy\nblocked on migration 0142");
  });

  test("an unknown block type still yields its text", () => {
    expect(replyText({ blocks: [{ type: "some_future_block", text: { type: "mrkdwn", text: "still read" } }] })).toBe(
      "still read",
    );
  });

  test("a message with no content anywhere renders empty", () => {
    expect(replyText({})).toBe("");
    expect(replyText({ text: "   ", attachments: [{}], blocks: [{ type: "divider" }] })).toBe("");
  });
});
