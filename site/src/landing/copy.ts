// Every word on the landing page, in one place. Links are docs-relative; the
// page prefixes them with the deployment's base path.
//
// Each section connects an outcome to the product behavior that makes it possible.

export const hero = {
  eyebrow: "Coding agents, running in your cloud",
  title: "Automate your",
  titleAccent: "SDLC.",
  lede: "engrams runs Claude Code, Codex, or your own agent in an isolated microVM on your infrastructure. Start one from a prompt, a schedule, a pull request, or a Slack thread. Walk away mid-task, and pick it up days later with its files and history intact.",
  primary: { label: "Explore automations", href: "platform/automations/" },
  secondary: { label: "Run it locally", href: "getting-started/local-quickstart/" },
  chips: ["AGPL-3.0", "Firecracker · KVM", "GKE · EKS", "Claude Code · Codex"],
  graphTitle: "Example workflow · fix failing tests",
};

export const modules = {
  badge: "Sec. 0",
  label: "Work on your terms",
  cards: [
    {
      kicker: "Real environments",
      num: "01",
      title: "Run your whole stack.",
      body: "Each agent gets its own microVM, so it can clone the repo, install dependencies, run the build, and start a dev server.",
      foot: "Firecracker · full workspace",
    },
    {
      kicker: "Human control",
      num: "02",
      title: "Stay involved.",
      body: "Delegate a task, inspect its changes, and send a follow-up. The dashboard and Slack keep you in the conversation while agents work in your cloud.",
      foot: "Inspect · reply · steer",
    },
    {
      kicker: "Controlled access",
      num: "03",
      title: "Keep keys out.",
      body: "Give an agent the API operations it needs. For brokered integrations, the proxy inserts the credential outside the VM and blocks requests the policy does not allow.",
      foot: "Brokered keys · egress policy",
    },
    {
      kicker: "Optimized for cost",
      num: "04",
      title: "Pay for work, not idle VMs.",
      body: "When a session goes idle, engrams snapshots it to object storage and frees the host for other work. The next prompt restores it on any host in seconds. Give every service its own agent without a VM running for each.",
      foot: "Snapshot · release · resume",
    },
  ],
};

export const session = {
  badge: "Sec. 1",
  label: "A session",
  meta: "Fig. 1.1 · session view",
  title: "Run any harness in a rich web interface.",
  body: "A session is Claude Code, Codex, or your own harness, working the way it does on your laptop, but in a microVM your team can reach. The transcript streams on the left. On the right, open the same machine the agent is using: a terminal, the browser it drives, VS Code, the files it changed. People start sessions from the dashboard or Slack; an automation starts the same kind of session from a trigger.",
  screenshotAlt:
    "A session in the engrams dashboard: the agent has drawn a pelican riding a bicycle and shared the PNG in the transcript; the right pane holds a shell open on the guest.",
  caption: "Fig. 1.1 · session se_9f3ea71c · 212 events",
  live: "● live",
  panes: [
    {
      name: "Shell",
      body: "A terminal on the guest, holding the same filesystem and the same processes the agent is working in.",
    },
    {
      name: "Browser",
      body: "The browser the agent drives, streamed over VNC. Watch it work through a page, or take the mouse.",
    },
    {
      name: "IDE",
      body: "VS Code on the workspace (code-server), with its own integrated terminal.",
    },
    {
      name: "Files",
      body: "Images and files the agent shares render in the transcript. Attach your own in a reply and they land in the guest.",
    },
  ],
};

export const automations = {
  badge: "Sec. 2",
  label: "Automations",
  meta: "Fig. 2.1",
  title: "Hand off the work that repeats.",
  lede: "A schedule, a pull request, a Slack mention, or a webhook can start a session, with no one typing a prompt. When one prompt is not enough, chain steps into a workflow: prompt the agent, run the tests, branch on the result, and post to Slack or GitHub.",
  cases: {
    label: "In production at Cortex",
    note: "Four automations that run in our engineering org today.",
    items: [
      {
        trigger: "schedule",
        title: "Dependency upgrades and CVE fixes",
        body: "Scans for outdated dependencies and open CVEs, opens each upgrade, runs the tests, and fixes what breaks. One pull request per change, reviewed by the pull request bot.",
      },
      {
        trigger: "schedule · Datadog",
        title: "Memory hot spots",
        body: "Queries Datadog profiles for the largest allocation frames, opens a task on the service that owns the code, and rewrites the hot path. The pull request carries the before and after numbers.",
      },
      {
        trigger: "Datadog webhook",
        title: "Bug triage",
        body: "A new production error opens a run. The agent reproduces and diagnoses it, then pushes a fix or files an issue with the root cause. Duplicate errors join the run already in flight.",
      },
      {
        trigger: "Linear · Slack",
        title: "A project with an owner",
        body: "One agent watches a Linear project and its Slack channel. It picks up issues, opens pull requests, answers questions in the thread, and posts a daily status.",
      },
    ],
  },
  board: {
    fig: "Fig. 2.1 · runs board",
    title: "Every run is a session you can open.",
    body: "Open a run to see each step, its inputs and outputs, and the session that did the work. Read the transcript, reply to the agent, or take over in its shell. Runs survive a server restart and can wait hours for a reply without holding a VM.",
    itemsLabel: "Ships in the box",
    items: [
      {
        title: "Slack threads",
        body: "Mention the bot in Slack and a session opens for that thread, as the person who asked, with their credentials. Replies join the same run.",
      },
      {
        title: "Pull request review",
        body: "A finder and a verifier review each pull request on the repositories you list, and post the confirmed findings as one GitHub review.",
      },
    ],
    footRight: "durable · survives restart",
  },
};

export const how = {
  badge: "Sec. 3",
  label: "How it works",
  meta: "Fig. 3.1 · chunk store",
  title: "Start in a second.",
  titleAccent: "Pick up anywhere.",
  steps: [
    {
      s: "01",
      title: "Bring your image",
      body: "Push any Linux image with `/bin/sh` and enable it once. engrams boots it, runs your warm-up command, and saves the result, so installs and caches are done before the first session.",
    },
    {
      s: "02",
      title: "Start warm",
      body: "A prompt or a trigger starts a session from that saved state in under a second, with dependencies installed and caches already warm.",
    },
    {
      s: "03",
      title: "Sleep, wake, fork",
      body: "An idle session is saved to storage and its VM is released. The next prompt wakes it on any host in one to two seconds. Fork a session to try two approaches from the same point.",
    },
  ],
  legend: [
    { cls: "base", label: "base image · stored once" },
    { cls: "delta", label: "session delta" },
    { cls: "writing", label: "writing now" },
  ],
  leds: [
    { k: "cold start", v: "<1s" },
    { k: "resume · same host", v: "<100ms" },
    { k: "resume · any host", v: "1–2s" },
    { k: "1000 × 4 GiB", v: "≈100GiB" },
  ],
  footnote:
    "† Measured on the reference deployment: GKE, C3 nodes, Firecracker with lazy memory paging. Measure your own fleet before you promise them to anyone.",
};

export const extensible = {
  badge: "Sec. 4",
  label: "Extensible",
  title: "Make it work with your stack.",
  lede: "Connect an internal service or run another agent without building a separate control plane. Custom harnesses use the same session UI; custom connectors use the same credential broker and access policies as the built-ins.",
  cards: [
    {
      kicker: "Harnesses",
      title: "Change agents. Keep your setup.",
      body: "Register an agent through the harness SDK and descriptor. It appears in the picker with its models, modes, and effort levels. The host mounts it at boot, separately from your image.",
      foot: "claude code · codex · yours",
      href: "guides/custom-harness/",
    },
    {
      kicker: "Connectors",
      title: "Connect the tools your work needs.",
      body: "Define a service’s hosts, credential headers, and allowed operations. The proxy enforces those rules and inserts the key outside the VM. Profiles choose which connections each session can use.",
      foot: "24 built in · unlimited custom",
      href: "concepts/egress-and-brokering/",
    },
  ],
};

export const belt = { label: "24 connectors built in · plus yours" };

export const start = {
  eyebrow: "Start with one task",
  title: "Start",
  lede: "Run the stack locally and give an agent its first task. When you are ready for a shared deployment, follow the GCP or AWS guide to run engrams in your own cloud.",
  ctas: [
    { label: "Run it locally", href: "getting-started/local-quickstart/", primary: true },
    { label: "Deploy on GCP", href: "guides/deploy-gcp/", primary: false },
    { label: "Deploy on AWS", href: "guides/deploy-aws/", primary: false },
  ],
};
