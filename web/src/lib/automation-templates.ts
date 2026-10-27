/** Starting points for a new automation: a definition the editor opens
 * prefilled and editable. Each one is the common shape — a way in, a filter
 * where one is needed, and the Agent block — so the person changes the
 * profile and the prompt and turns it on. Nothing here names a profile: the
 * Agent block's profile is the first thing to pick. */

import type { AutomationDefinition } from "./automation-blocks";

export interface AutomationTemplate {
  id: string;
  name: string;
  /** One line under the name on the card. */
  description: string;
  /** What one agent is per, when the template scopes its agents. */
  scope?: string;
  definition: AutomationDefinition;
}

const SLACK_THREAD_KEY =
  '${{ event.raw.team_id }}:${{ event.raw.event.channel }}:${{ event.raw.event | coalesce: "thread_ts", "ts" }}';

const SLACK_RELAY = {
  provider: "slack",
  team: "${{ event.raw.team_id }}",
  channel: "${{ event.raw.event.channel }}",
  threadTs: "${{ event.raw.event.thread_ts | default: event.raw.event.ts }}",
  mentionTs: "${{ event.raw.event.ts }}",
  userId: "${{ event.raw.event.user }}",
  eventId: "${{ event.raw.event_id }}",
};

export const AUTOMATION_TEMPLATES: readonly AutomationTemplate[] = [
  {
    id: "slack-answer-bot",
    name: "Slack answer bot",
    description: "Answers when mentioned; one session per thread, replying in the thread.",
    scope: "per thread",
    definition: {
      engine: 1,
      trigger: {
        kind: "integration",
        provider: "slack",
        connectionId: "",
        eventKeys: ["app_mention"],
      },
      blocks: [
        {
          id: "agent",
          type: "run_agent",
          config: {
            profileId: "",
            promptTemplate: "${{ event.raw.event.text | strip_mentions }}",
            titleTemplate: "${{ event.raw.event.text | strip_mentions | truncate: 80 }}",
            keepOnFinish: true,
            reuse: "workstream",
            relay: SLACK_RELAY,
          },
        },
      ],
      inputsSchema: [],
      settings: {
        instance: {
          keyTemplate: SLACK_THREAD_KEY,
          labelTemplate: "${{ event.raw.event.text | strip_mentions | truncate: 80 }}",
        },
        concurrency: { keyTemplate: SLACK_THREAD_KEY, policy: "queue" },
        endSessionsOnFinish: false,
      },
    },
  },
  {
    id: "pr-reviewer",
    name: "PR reviewer",
    description: "Follows a pull request across pushes; one session per PR.",
    scope: "per PR",
    definition: {
      engine: 1,
      trigger: {
        kind: "integration",
        provider: "github",
        connectionId: "",
        eventKeys: [
          "pull_request.opened",
          "pull_request.synchronize",
          "pull_request.ready_for_review",
        ],
      },
      blocks: [
        {
          id: "not_draft",
          type: "filter",
          config: {
            conditions: {
              mode: "all",
              conditions: [{ path: "event.raw.pull_request.draft", op: "is_false" }],
            },
          },
        },
        {
          id: "agent",
          type: "run_agent",
          config: {
            profileId: "",
            promptTemplate:
              "Review pull request ${{ event.raw.pull_request.html_url }} at ${{ event.raw.pull_request.head.sha }}. Leave findings as a review on the pull request.",
            titleTemplate:
              "Review ${{ event.raw.repository.full_name }}#${{ event.raw.pull_request.number }}",
            keepOnFinish: true,
            reuse: "workstream",
          },
        },
      ],
      inputsSchema: [],
      settings: {
        instance: {
          keyTemplate: "${{ event.raw.repository.full_name }}#${{ event.raw.pull_request.number }}",
        },
        concurrency: {
          keyTemplate: "${{ event.raw.repository.full_name }}#${{ event.raw.pull_request.number }}",
          policy: "supersede",
        },
        endSessionsOnFinish: false,
      },
    },
  },
  {
    id: "standing-sre",
    name: "Standing SRE",
    description: "One agent per channel that reads every message and acts when it should.",
    scope: "per channel",
    definition: {
      engine: 1,
      trigger: {
        kind: "integration",
        provider: "slack",
        connectionId: "",
        eventKeys: ["message"],
      },
      blocks: [
        {
          id: "from_people",
          type: "filter",
          config: {
            conditions: {
              mode: "all",
              conditions: [{ path: "event.raw.event.bot_id", op: "is_empty" }],
            },
          },
        },
        {
          id: "agent",
          type: "run_agent",
          config: {
            profileId: "",
            promptTemplate:
              "New message in the channel from <@${{ event.raw.event.user }}>:\n\n${{ event.raw.event.text }}\n\nAct only when it concerns an incident you own or are asked to look at.",
            titleTemplate: "SRE in ${{ event.raw.event.channel }}",
            keepOnFinish: true,
            reuse: "workstream",
            relay: {
              ...SLACK_RELAY,
              threadTs: "${{ event.raw.event.thread_ts | default: event.raw.event.ts }}",
            },
          },
        },
      ],
      inputsSchema: [],
      settings: {
        instance: { keyTemplate: "${{ event.raw.team_id }}:${{ event.raw.event.channel }}" },
        concurrency: {
          keyTemplate: "${{ event.raw.team_id }}:${{ event.raw.event.channel }}",
          policy: "queue",
        },
        endSessionsOnFinish: false,
      },
    },
  },
  {
    id: "docs-reconciler",
    name: "Docs reconciler",
    description: "Every Monday, a fresh session reads the code and opens a PR that fixes the docs.",
    definition: {
      engine: 1,
      trigger: { kind: "cron", schedule: "0 9 * * 1", timezone: "UTC" },
      blocks: [
        {
          id: "agent",
          type: "run_agent",
          config: {
            profileId: "",
            promptTemplate:
              "Read the code that changed in the last week and the docs that describe it. Where the docs drifted, fix them and open one pull request with the changes.",
            titleTemplate: "Docs reconciliation",
            keepOnFinish: false,
            reuse: "never",
          },
        },
      ],
      inputsSchema: [],
      settings: { endSessionsOnFinish: true },
    },
  },
  {
    id: "weekly-digest",
    name: "Weekly digest",
    description: "Every Friday, a fresh session writes up the week and posts it to a channel.",
    definition: {
      engine: 1,
      trigger: { kind: "cron", schedule: "0 16 * * 5", timezone: "UTC" },
      blocks: [
        {
          id: "agent",
          type: "run_agent",
          config: {
            profileId: "",
            promptTemplate:
              "Write a digest of what merged this week: what shipped, what is still open, and anything a reader should know. Post it to the team channel.",
            titleTemplate: "Weekly digest",
            keepOnFinish: false,
            reuse: "never",
          },
        },
      ],
      inputsSchema: [],
      settings: { endSessionsOnFinish: true },
    },
  },
];

export function automationTemplate(id: string | undefined): AutomationTemplate | undefined {
  return id ? AUTOMATION_TEMPLATES.find((template) => template.id === id) : undefined;
}
