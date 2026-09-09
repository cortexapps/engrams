// Every word on the landing page, in one place. Links are docs-relative; the
// page prefixes them with the deployment's base path.
//
// Each section connects an outcome to the product behavior that makes it possible.

export const hero = {
  eyebrow: "Your agents, working in your cloud",
  title: "Describe it once.",
  titleAccent: "Run it again.",
  lede: "Turn repeat work into an automation. engrams runs Claude Code, Codex, or your own agent when a schedule fires, a pull request changes, or someone asks in Slack. Inspect the changes and continue the conversation, on infrastructure you control.",
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
      kicker: "Human control",
      num: "01",
      title: "Stay involved.",
      body: "Delegate a task, inspect its changes, and send a follow-up. The dashboard and Slack keep you in the conversation while agents work in your cloud.",
      foot: "Inspect · reply · steer",
    },
    {
      kicker: "Agent choice",
      num: "02",
      title: "Use your agents.",
      body: "Run Claude Code, Codex, or a custom harness with the same session controls. Upgrade the agent without rebuilding your development image.",
      foot: "Claude Code · Codex · BYO",
    },
    {
      kicker: "Controlled access",
      num: "03",
      title: "Keep keys out.",
      body: "Give an agent the API operations it needs. For brokered integrations, the proxy inserts the credential outside the VM and blocks requests the policy does not allow.",
      foot: "Brokered credentials · egress policy",
    },
    {
      kicker: "Work that lasts",
      num: "04",
      title: "Pick it back up.",
      body: "Step away without losing the environment. An idle VM is snapshotted and removed; your next prompt restores it. Stored snapshots remain, without a running VM per paused session.",
      foot: "Snapshot · resume · continue",
    },
  ],
};

export const automations = {
  badge: "Sec. 1",
  label: "Automations",
  meta: "Fig. 1.1 – 1.2",
  title: "Build the workflow. Stop repeating the setup.",
  lede: "Describe what should happen. A drafting agent builds an editable workflow: start sessions, run commands, check results, and decide what comes next. Enable it when it is ready. A schedule or event starts each run, and saved progress lets it continue after a server restart.",
  screenshotAlt:
    "The New automation page asks what the automation should do; a drafting agent assembles it on the canvas.",
  channel: "Automation composer",
  caption: "Start with a request. The agent drafts a workflow you can edit before you enable it.",
  signal: "Product screenshot",
  board: {
    fig: "Fig. 1.2 · runs board",
    title: "See what happened. Decide what comes next.",
    body: "Open a run to see each step, its inputs and outputs, and the session that did the work. Inspect an error or a changed file without reconstructing the run from separate logs.",
    items: [
      {
        title: "Keep the conversation together",
        body: "Mention the bot in Slack and a session opens for that thread, as the person who asked, with their credentials. Replies join the same run.",
      },
      {
        title: "Review with evidence",
        body: "A finder and a verifier review each pull request on the repositories you list, and post the confirmed findings as one GitHub review.",
      },
    ],
    footRight: "durable · survives restart",
  },
};

export const how = {
  badge: "Sec. 2",
  label: "How it works",
  meta: "Fig. 2.1 · chunk store",
  title: "Keep the environment.",
  titleAccent: "Continue the work.",
  steps: [
    {
      s: "S₀",
      title: "Enable an image",
      body: "Push any Linux image with `/bin/sh` to a registry and enable it once. engrams writes it as content-addressed chunks, boots it on the fleet, runs your warm-up command, and freezes a base snapshot.",
    },
    {
      s: "S₁",
      title: "Start a run",
      body: "A person types a prompt, or a trigger fires. The host restores the snapshot into a fresh microVM, mounts the harness, and the agent starts with its caches already warm.",
    },
    {
      s: "Sₙ",
      title: "Snapshot, resume, fork",
      body: "When the agent goes idle the VM is snapshotted to chunks and destroyed. The next prompt restores it, on any host. Forking a session is a manifest copy of a few kilobytes.",
    },
  ],
  legend: [
    { cls: "base", label: "base image · stored once" },
    { cls: "delta", label: "session delta" },
    { cls: "writing", label: "writing now" },
  ],
  leds: [
    { k: "cold start", ghost: "888", v: "<1s" },
    { k: "resume · same host", ghost: "88888", v: "<100ms" },
    { k: "resume · any host", ghost: "8888", v: "1–2s" },
    { k: "1000 × 4 GiB", ghost: "888888", v: "≈100GiB" },
  ],
  footnote:
    "† Measured on the reference deployment: GKE, C3 nodes, Firecracker with lazy memory paging. Measure your own fleet before you promise them to anyone.",
};

export const session = {
  badge: "Sec. 3",
  label: "A session",
  meta: "Fig. 3.1 · session view",
  title: "Run any harness in a rich web interface.",
  body: "Claude Code, Codex, and any harness you register all run the same way: in a microVM, with the dashboard wrapped around it. The transcript streams on the left. On the right, a pane opens onto the same guest the agent is working in: a terminal, the browser it drives, VS Code, the files it changed. Open one in the middle of a run and the agent keeps going.",
  screenshotAlt:
    "A session in the engrams dashboard: the agent has drawn a pelican riding a bicycle and shared the PNG in the transcript; the right pane holds a shell open on the guest.",
  caption: "Fig. 3.1 · session se_9f3ea71c · 212 events",
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
      foot: "23 built in · unlimited custom",
      href: "concepts/egress-and-brokering/",
    },
  ],
};

export const belt = { label: "23 connectors built in · plus yours" };

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
