/**
 * SessionIngestWorkflow — the reverse-channel pump (ADR 0059 P1.3).
 *
 * Integration test against the real embedded DBOS engine (env-gated on a
 * reachable ORCHESTRATOR_DATABASE_URL, like the smoke test): a scripted fake
 * event source feeds two pages; a tiny collector workflow drains the "session"
 * mailbox. We assert the pump forwards only curated content, in idx order,
 * across pages, then sends a single terminal and exits.
 *
 * NOT a durability test (DBOS guarantees replay/recovery) — this exercises OUR
 * loop: curation, cursor advance, the terminal send, loop exit.
 */

import { expect, test, describe, afterAll } from "bun:test";
import { DBOS } from "@dbos-inc/dbos-sdk";
import { initDbos, shutdownDbos } from "../dbos.ts";
import { checkDb } from "../../db/client.ts";
import {
  sessionIngestWorkflow,
  __setIngestEventSourceForTests,
  SESSION_TOPIC,
  type SessionMessage,
} from "../session-ingest.ts";

// A collector workflow standing in for the SlackThreadWorkflow (P1.4): drains
// the "session" mailbox until the terminal message, returning what it saw.
const RECV_TIMEOUT_MS = 5_000;
const collectorWorkflow = DBOS.registerWorkflow(
  async (): Promise<SessionMessage[]> => {
    const msgs: SessionMessage[] = [];
    for (;;) {
      const m = await DBOS.recv<SessionMessage>(SESSION_TOPIC, RECV_TIMEOUT_MS);
      if (m === null) break;
      msgs.push(m);
      if (m.event.kind === "terminal") break;
    }
    return msgs;
  },
  { name: "test-collector-workflow" },
);

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

describe("SessionIngestWorkflow (requires ORCHESTRATOR_DATABASE_URL)", () => {
  afterAll(async () => {
    __setIngestEventSourceForTests(undefined);
    await shutdownDbos();
  });

  test.skipIf(!dbReachable)(
    "forwards curated content in order across pages, then one terminal, then exits",
    async () => {
      // Scripted RAW pages (the pump curates them). Page 1: content, no
      // terminal. Page 2: a curated event + a terminal status_changed.
      const pages = [
        {
          events: [
            { idx: 0n, kind: "run_started", payloadJson: "{}" },
            { idx: 1n, kind: "agent_message", payloadJson: "{}" }, // noise
            { idx: 2n, kind: "user_question", payloadJson: '{"tool_call_id":"t1"}' },
          ],
          nextAfterIdx: 2n,
        },
        {
          events: [
            { idx: 3n, kind: "file_shared", payloadJson: "{}" },
            { idx: 4n, kind: "status_changed", payloadJson: '{"to":"completed"}' },
          ],
          nextAfterIdx: 4n,
        },
      ];
      let call = 0;
      __setIngestEventSourceForTests(async () => pages[Math.min(call++, pages.length - 1)]!);

      await initDbos();

      const threadWfId = `test-thread-${Date.now()}`;
      const collector = await DBOS.startWorkflow(collectorWorkflow, {
        workflowID: threadWfId,
      })();
      await DBOS.startWorkflow(sessionIngestWorkflow, {
        workflowID: `test-ingest-${Date.now()}`,
      })({ sessionId: "sess-1", threadWfId });

      const msgs = await collector.getResult();

      // run_started, user_question, file_shared (agent_message + status_changed
      // filtered out), then the terminal.
      expect(msgs.map((m) => m.event.kind)).toEqual([
        "run_started",
        "user_question",
        "file_shared",
        "terminal",
      ]);
      expect(msgs[msgs.length - 1]!.event).toEqual({ kind: "terminal", ok: true });
    },
  );
});
