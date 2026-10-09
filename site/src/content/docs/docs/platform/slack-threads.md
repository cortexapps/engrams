---
title: Slack threads
description: Mention the bot in a channel and a session opens for that thread, as the person who asked, with their credentials.
sidebar:
  order: 4
---

Slack threads is the built-in automation that turns a mention into a session. Mention the bot
in a channel it is a member of, and engrams starts a session as you, relays the agent's work
back into the thread, and keeps the same session for every reply until the thread goes quiet.

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
| `default_profile` | The profile every thread runs on, unless a channel override or smart routing picks another. With no default, only channels with an override are answered (or every channel, with smart routing on). |
| `channels` | Channel overrides: a map from channel to the profile its threads run on. An override always wins. |
| `routing` | `default` or `smart`. See [Smart routing](#smart-routing). `default` unless you change it. |
| `smart_profiles` | The profiles smart routing chooses between. Empty means every active profile. |
| `smart_min_confidence` | Smart routing picks a profile on its own at or above this confidence, from 0 to 1. Below it, it asks in the thread. 0.8 by default. |
| `ask_timeout` | How long the profile question waits for a click before smart routing takes its best pick. Ten minutes by default. |
| `idle_timeout` | How long a thread may go quiet before its run ends. One minute to a day; an hour by default. |
| `max_turns` | The most replies one thread's run will take before it ends. Fifty by default. |

Enable the automation. The bot must be a member of a channel to see messages in it: it can
join a public channel on its own, and a private channel needs a person to invite it.

## Smart routing

With an [OpenRouter key](../../guides/model-routers/), the Slack page offers smart routing.
For a new thread in a channel with no override, a decision model reads the thread and picks
the profile. It reads each profile's description, its repositories, and its integrations, so
keep descriptions specific: "the web app and its API" routes better than "development".

- When the model is confident, the session starts on its pick. The thread's first message
  names the profile and the confidence.
- When the model is not confident, or the person asks to choose, the thread gets a question
  with one button per likely profile. The click decides. With no click before the timeout,
  the model's best pick runs, and the question shows what was chosen.
- A thread that resumes its kept session is never routed again.

Without an OpenRouter key, smart routing is not offered. If it was on and the key is
removed, threads use the default profile, exactly as with `default` routing.

## How it works

Slack threads is an ordinary automation with its structure locked. Its trigger is the Slack
app mention event. A mention in a thread that already has a run joins that run; it never
starts a second one. Its concurrency key is the thread, with the join policy, which is what
keeps one run per thread. Smart routing is three blocks in the graph: List profiles, Decide,
and Slack choice. The Activity tab under the automation lists each thread's run with its step
trace, and Duplicate gives you an editable copy if you want a different flow. See
[Automations](../automations/) for the model.
