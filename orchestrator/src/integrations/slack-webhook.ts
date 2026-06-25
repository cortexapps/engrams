/**
 * Slack Events API payload classification (ADR 0060 P2.8) — pure, DBOS-free.
 *
 * The route verifies the request with the SDK (`isValidSlackRequest`) and then
 * classifies the raw body here: a url_verification handshake, an `app_mention`
 * (the trigger) → SourceMention, or ignore. Typed against `@slack/types`'
 * `AppMentionEvent`; this is field mapping, not verification. Never throws.
 */

import type { AppMentionEvent } from "@slack/types";

import type { SourceMention } from "../workflows/thread-inbox.ts";

/** The Events API envelope (the bits we read), wrapping a typed event. */
interface EventCallbackEnvelope {
  type?: string;
  challenge?: string;
  team_id?: string;
  event_id?: string;
  event?: Partial<AppMentionEvent> & { type?: string };
}

export type ClassifiedSlackEvent =
  | { kind: "challenge"; challenge: string }
  | { kind: "mention"; mention: SourceMention }
  | { kind: "ignore" };

/** Classify a Slack Events API payload. `threadRoot` is `thread_ts` for an
 *  in-thread mention, else the message `ts` (a top-level mention opens its own
 *  thread). */
export function classifySlackEvent(rawBody: string): ClassifiedSlackEvent {
  let body: EventCallbackEnvelope;
  try {
    body = JSON.parse(rawBody);
  } catch {
    return { kind: "ignore" };
  }

  if (body.type === "url_verification" && typeof body.challenge === "string") {
    return { kind: "challenge", challenge: body.challenge };
  }

  if (body.type === "event_callback" && body.event?.type === "app_mention") {
    const e = body.event;
    const ts = str(e.ts);
    return {
      kind: "mention",
      mention: {
        team: str(body.team_id),
        channel: str(e.channel),
        threadRoot: str(e.thread_ts) || ts,
        user: str(e.user),
        ts,
        eventId: str(body.event_id),
      },
    };
  }

  return { kind: "ignore" };
}

const str = (v: unknown): string => (typeof v === "string" ? v : "");
