---
title: Slack threads
description: Mention the bot in a channel and a session opens for that thread, as the person who asked, with their credentials.
sidebar:
  order: 4
---

Slack threads is the built-in automation that turns a mention into a session. Mention the bot
in a channel you have mapped to a profile, and engrams starts a session on that profile as
you, relays the agent's work back into the thread, and keeps the same session for every reply
until the thread goes quiet.

## What a person sees

Mention the bot in a channel or in a thread. Within a moment a reaction marks the message as
in progress and the agent's first message lands in the thread. From there:

- The agent's messages arrive in the thread as it produces them.
- When the agent asks a question, it arrives as a Slack form; answering it continues the
  session.
- Each turn gets a reaction while it runs and another when it ends.
- Replies in the thread go to the same session. Nobody has to mention the bot again.
- When the thread has been quiet for the idle timeout, the run ends and posts a recap. The
  session is kept, so someone can pick it up in the dashboard later with the full history.

The session runs as the person who wrote the mention: their harness credential, their git
identity, and their personal integration connections where the profile asks for them. A
mention from someone whose Slack email does not match an engrams user gets a reply asking
them to log in first, and nothing starts.

## Turning it on

The Slack app has to be connected first; [Connect Slack](../../guides/connect-slack/) has the
manifest and the three URLs. Then open Settings → Automations → Slack threads → Inputs.

| Input | Meaning |
|---|---|
| `channels` | A map from channel to the profile its sessions run on. A channel that is not in the map is never answered. |
| `default_profile` | The profile for a mapped channel that names none. |
| `idle_timeout` | How long a thread may go quiet before its run ends. One minute to a day; an hour by default. |
| `max_turns` | The most replies one thread's run will take before it ends. Fifty by default. |

Enable the automation. The bot must be a member of a channel to see messages in it: it can
join a public channel on its own, and a private channel needs a person to invite it.

## How it works

Slack threads is an ordinary automation with its structure locked. Its trigger is the Slack
app mention event, scoped to the keys of the `channels` map, with thread messages marked as
continue-only so a reply can join the run that owns the thread but never start a second one.
Its concurrency key is the thread, with the join policy, which is what keeps one run per
thread. The Activity tab under the automation lists each thread's run with its step trace, and
Duplicate gives you an editable copy if you want a different flow. See
[Automations](../automations/) for the model.
