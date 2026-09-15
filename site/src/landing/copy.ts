// Every word on the landing page, in one place. Links are docs-relative; the
// page prefixes them with the deployment's base path.
//
// This copy came out of the design pass and is the placeholder the owner will
// finalise; edit it here and nothing else has to change.

export const hero = {
  eyebrow: "Software factory · Automation running",
  title: "Automate your",
  titleAccent: "SDLC.",
  lede: "engrams is a self-hosted software factory. It runs Claude Code and Codex in Firecracker microVMs on servers you own. A schedule, a Slack thread, a pull request, or a webhook starts a run. Every run keeps its transcript, its diff, and its snapshot, and a person is one message away.",
  primary: {
    label: "Run it locally →",
    href: "getting-started/local-quickstart/",
  },
  secondary: { label: "Deploy to your cloud", href: "guides/deploy-overview/" },
  chips: ["AGPL-3.0", "Firecracker · KVM", "GKE · EKS", "Claude Code · Codex"],
  graphTitle: "Automation · nightly-flaky-tests",
};

export const modules = {
  badge: "Sec. 0",
  label: "Mission parameters",
  cards: [
    {
      kicker: "Self-hosted",
      num: "01",
      title: "Your cloud.",
      body: "One Terraform apply and two Helm releases bring it up on GKE or EKS. No transcript, workspace, or key leaves your account.",
      foot: "GKE · EKS · GCS · S3",
    },
    {
      kicker: "Any harness",
      num: "02",
      title: "Any agent.",
      body: "A harness is a bundle the host mounts at boot, so your image never contains the agent, and you can register your own.",
      foot: "Claude Code · Codex · BYO",
    },
    {
      kicker: "Any model",
      num: "03",
      title: "Your keys.",
      body: "You bring an Anthropic or OpenAI key, or point a harness at OpenRouter. Build and Plan modes, effort levels.",
      foot: "Anthropic · OpenAI · OpenRouter",
    },
    {
      kicker: "Idle = 0",
      num: "04",
      title: "Idle is free.",
      body: "A paused session is chunks in the blob store and a row in Postgres. It holds no host resources until the next prompt.",
      foot: "Deduplicated · Content-addressed",
    },
  ],
};

export const automations = {
  badge: "Sec. 1",
  label: "Automations",
  meta: "Fig. 1.1 – 1.2",
  title: "The routine work is the point.",
  lede: "An automation is one trigger, a tree of blocks, and typed inputs: start a session, send it a prompt, wait for it, run a command inside it, post to Slack or GitHub, branch, loop. Runs are durable, so a run survives a restart and can wait hours for a reply without holding anything open.",
  screenshotAlt:
    "The New automation page asks what the automation should do; a drafting agent assembles it on the canvas.",
  channel: "CH 01 · Fig. 1.1",
  caption:
    "Describe it in a sentence · a drafting agent assembles the automation on the canvas · nothing runs until you enable it",
  signal: "Signal ● Locked",
  board: {
    fig: "Fig. 1.2 · runs board",
    title: "Every run is a step timeline.",
    body: "Each step opens to its inputs, its outputs, its output text, and the session it used. Trace spans for every model call and tool call can go to Langfuse or any OpenTelemetry collector.",
    items: [
      {
        title: "One run per thread",
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
  badge: "Sec. 2",
  label: "How it works",
  meta: "Fig. 2.1 · chunk store",
  title: "Enable once.",
  titleAccent: "Restore forever.",
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
  title: "Built to be extended.",
  lede: "Claude Code, Codex and the 23 connectors are what ships in the box. The box is open: register your own harness, add your own connector, and the factory treats them exactly like the built-ins.",
  cards: [
    {
      kicker: "Harnesses",
      title: "Bring your own agent.",
      body: "A harness is a bundle the host mounts at boot, so your image never contains the agent. Claude Code and Codex ship as harnesses; register yours and it gets the same models, modes and effort levels.",
      foot: "claude code · codex · yours",
      href: "guides/custom-harness/",
    },
    {
      kicker: "Connectors",
      title: "Bring your own connector.",
      body: "Credentials are held by engrams and brokered at the egress proxy, so an agent can call an API without ever holding the key. Define a connector for any service and it is brokered the same way.",
      foot: "23 built in · unlimited custom",
      href: "concepts/egress-and-brokering/",
    },
  ],
};

export const belt = { label: "23 connectors built in · plus yours" };

export const start = {
  eyebrow: "A few minutes on one machine",
  title: "Start",
  lede: "The local quickstart runs the whole stack on one machine in a few minutes. The deployment guides take a fresh GCP project or AWS account to a running fleet.",
  ctas: [
    {
      label: "Run it locally",
      href: "getting-started/local-quickstart/",
      primary: true,
    },
    { label: "Deploy on GCP", href: "guides/deploy-gcp/", primary: false },
    { label: "Deploy on AWS", href: "guides/deploy-aws/", primary: false },
  ],
};
