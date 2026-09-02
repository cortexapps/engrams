# Writing style for engrams docs, README, and site copy

The rules below were distilled from a survey of open-source projects whose READMEs and docs
are widely held up as models: ripgrep, esbuild, Redis, SQLite, Vite, Astro, Tailscale, Coder,
Modal, E2B, Firecracker, htmx, Django and Diátaxis, plus the Google and Microsoft developer
style guides and PostHog's handbook. The full report with verbatim excerpts per project
is kept outside the repo at `~/Projects/docs-references/REPORT.md`, next to shallow clones of
the sources. Read this file before writing any public prose for engrams. The site's build
lint (`scripts/check-leaks.mjs`) enforces the greppable parts of the never-list.

Two house rules that sit above the survey: the product name is `engrams`, lowercase, even at
the start of a sentence (crate and binary names keep their real spelling), and the license is
the GNU Affero General Public License v3.0.

## Distilled style guide for engrams docs and landing copy

Rules are checkable. Each carries one example from the survey.

**Opening and framing**

1. **First sentence is a definition with the product as the subject and a category noun.** No adjective in front of the noun. — "ripgrep is a line-oriented search tool that recursively searches the current directory for a regex pattern." / "Coder is a self-hosted platform for running AI coding agents and cloud development environments on infrastructure you control."
2. **The second sentence names the mechanism, not a benefit.** — "Firecracker runs workloads in lightweight virtual machines, called microVMs, which combine the security and isolation properties provided by hardware virtualization technology with the speed and flexibility of containers."
3. **Name the comparables in the first screen, including the one you are not.** — "SQLite does not compete with client/server databases. SQLite competes with fopen()." / "ripgrep is similar to other popular search tools like The Silver Searcher, ack and grep."
4. **A landing page gets a runnable command above any explanation.** — Astro's landing: heading, one line, then `npm create astro@latest`. Modal: full example, then "That's it!".
5. **Every "why" page has a "why not" or "what it is not" section, and it names a better tool for the excluded case.** — ripgrep: "The best tool for this job is good old grep." / Coder: five lines beginning "Coder is not ...".
6. **Any speed or scale claim carries a number and a method caption.** — esbuild: "Above: the time to do a production bundle of 10 copies of the three.js library from scratch using default settings, including minification and source maps." / SQLite: "no lock lasts for more than a few dozen milliseconds."
7. **Limits are numbers with units, stated on the first screen of the page they apply to.** — Modal: "Sandboxes have a default maximum lifetime of 5 minutes. You can change this by passing a `timeout` of up to 24 hours". / Firecracker: "We **only** support AWS EC2 8th Gen Intel instances using a 6.1 or 6.18 host kernel."

**How-to prose**

8. **Introduce a command with the sentence that says what it does; follow it with the sentence that says what you should now see.** No "Run the following command:" line. — esbuild: "First, download and install the esbuild command locally. [...] `npm install ...` This should have installed esbuild in your local `node_modules` folder."
9. **Put the condition before the instruction.** — ripgrep: "If you're a **Debian** user (or a user of a Debian derivative like **Ubuntu**), then ripgrep can be installed using ..." (Google: "Put conditions before instructions, not after.")
10. **Write the failure path inside the step, not in a troubleshooting appendix.** — Modal: "Run `modal setup` to authenticate (if this doesn't work, try `python -m modal setup`)". / Django: "If it didn't work, see Problems running django-admin."
11. **Prerequisite checks are one-liners that print a result, not descriptions.** — Firecracker: "`[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo "OK" || echo "FAIL"`".
12. **Say what does not matter.** — Django: "The directory name doesn't matter to Django; you can rename it to anything you like."
13. **State the audience and the non-production status of a guide in its first line.** — Firecracker: "All resources are used for demonstration purposes and are not intended for production." / Coder: "This install guide is meant for **individual developers, small teams, and/or open source community members**".
14. **Warnings are one sentence at the point of use, with one bold word and a reason in parentheses.** — Django: "**don't** use this server in anything resembling a production environment. It's intended only for use while developing. (We're in the business of making web frameworks, not web servers.)" / htmx: "NOTE: If you push a URL into the history, you **must** be able to navigate to that URL and get a full page back!"
15. **Tell the reader what the tool will never do to their data, as a guarantee.** — ripgrep: "ripgrep **will never modify your files**."

**Voice and grammar**

16. **"You" is the reader. "We" is only the maintainers doing something real (support, test, host, recommend). Never "we" meaning "you and I in this guide".** — Firecracker: "We test all combinations of:". Django: "we'll walk you through" is acceptable because the authors are real people; "In this guide we will explore" is not.
17. **Product name as sentence subject, present tense, active voice.** — SQLite: "SQLite reads and writes directly to ordinary disk files."
18. **Vary sentence length; end a paragraph on a short flat sentence when the point is made.** — SQLite: "Writers queue up." / Vite: "Ecosystem health is not an afterthought. It is part of the release process."
19. **Headings are nouns or verb-first tasks, sentence case, no colon, no summary.** — Tailscale: "Rename devices", "Invite users". esbuild: "Why?", "Not a sandbox". Microsoft: "Don't use a period or a colon at the end of titles, headings".
20. **Bullets are "Bold label: full sentence" pairs or exact fact lists; never a stack of one-line fragments standing in for a paragraph.** — Coder: "**Defined in Terraform**: Templates describe the infrastructure for each workspace, from EC2 VMs and Kubernetes Pods to Docker containers." SQLite's Executive Summary is the allowed exception: every bullet is a linked fact with a number.
21. **Give credit to prior art by name.** — Vite: "Snowpack pioneered unbundled development and inspired Vite's dependency pre-bundling."
22. **State your own weaknesses as facts.** — esbuild: "primarily built by me"; PostHog: "we don't offer any sort of guarantees around it working in certain ways on your infrastructure." SQLite: "Of course, even with all this testing, there are still bugs."
23. **One joke per page at most, after the substantive claim, never in place of it.** — Modal: "Take a breath of fresh air and feel how good it tastes with no YAML in it." (after the zero-config claim).
24. **Introduce a term once with its short form in parentheses, then use only the short form.** — Tailscale: "a Tailscale network (known as a tailnet)". For engrams: "a snapshot of the guest's memory and disk (a checkpoint)" and then "checkpoint".
25. **End pages with the next link or nothing. No summary paragraph.** — SQLite ends on a timestamp; esbuild's README ends on "Check out the getting started instructions if you want to give esbuild a try."; Coder ends on "Learn more" links.

**The never list (AI tells to ban).** Grep for these before publishing.

- Words: seamlessly, seamless, robust, leverage/leveraging, powerful, cutting-edge, state-of-the-art, blazing/blazingly, effortless, elevate, empower, unlock, streamline, harness (as a verb), delve, dive in, journey, ecosystem (as a vague noun), holistic, comprehensive, best-in-class, world-class, game-changing, supercharge, "battle-tested" (unless you cite where), "production-ready" (unless you say what that means).
- Frames: "In this guide we will", "Let's dive in", "Let's get started!", "This guide will show you how to", "by running the following command in your terminal", "Now that you have X, let's", "Whether you're X or Y", "It's important to note that", "In today's fast-paced", "At its core", "the power of", "designed to", "aims to" (prefer "is").
- Structure: a heading that summarizes its section with a colon; a closing "Summary"/"Conclusion"/"Wrapping up" section; a paragraph that is a list of one-line bullets; three-adjective triplets ("fast, secure, and scalable"); rhetorical questions in body copy (a question is allowed only as a heading, as in ripgrep and esbuild); emoji in headings or bullets; bold on every other phrase; more than one em dash per paragraph (prefer spaced en dashes, a comma, or a full stop); "Note:" boxes stacked three deep.
- Hedges: "may help", "can potentially", "generally", "in most cases" without saying which cases; PostHog: "Avoid hedging".
- Marketing nouns stacked as a definition: "Zero Trust identity-based connectivity platform that replaces your legacy VPN, SASE, and PAM" (Tailscale's current opener) or "the preferred, fastest, and most feature-rich" (Redis' current opener). Both are the counter-examples.
- Title Case Headings, trailing periods on headings, "click here" links (Google: "Use descriptive link text").

## Recommended IA for the engrams docs site

Split the landing site from the docs the way Stripe and SQLite do: the landing page carries the definition, the "why", the "why not", pricing/license and one command; the docs never re-pitch.

**Landing page (one page, modeled on ripgrep's README + Coder's About):**
1. One-sentence definition (ripgrep, Coder). One-sentence mechanism: Firecracker microVMs, snapshots, the harness (Firecracker README).
2. One command or a 10-line config, then "That's it." (Astro landing, Modal).
3. "Why engrams" — three or four paragraphs, each with a number (esbuild "Why?", SQLite).
4. "Why not engrams" / "What engrams is not" — name the alternatives (ripgrep, Coder). Candidates: a hosted SaaS sandbox API (name E2B/Modal), a CDE for humans at the IDE (name Coder), a container-only runner, a non-Linux/no-KVM host.
5. "How it works" — three sentences (Modal "How does it work?"), a diagram.
6. Comparison table with named rows (ripgrep "Feature comparison").
7. License, support, security-disclosure link (Coder "Pricing", Firecracker "Policy for Security Disclosures").

**Docs site (Diátaxis, in Astro's four-tab shape, ordered like Coder's manifest):**

| Section | Contents | Modeled on |
|---|---|---|
| **About** | What engrams is; how it works; what it is not; tested platforms; known limitations | Coder `docs/README.md`; Firecracker "Tested platforms" + "Known issues and Limitations" |
| **Get started** (tutorial) | One path: prerequisites as OK/FAIL checks → install → first session → watch it stream → snapshot/resume. Declares audience and non-production status on line one | Firecracker getting-started; Django tutorial01; Astro install |
| **Install** (how-to) | Per-target pages: Linux+KVM bare metal, GCP/AWS metal, macOS (VZ) for development, Helm. "Fastest way" first, alternatives second | Coder install; Tailscale quickstart headings (verb-first) |
| **Guides** (how-to) | Task-shaped titles: "Add a profile", "Connect a model key", "Expose a session app", "Rotate the KEK", "Roll a host" | Tailscale "Rename devices / Invite users"; Stripe "Start here" cards |
| **Concepts** (explanation) | Session, sandbox backend, snapshot/eviction, harness, egress proxy, session apps. Each opens with the term-plus-short-form sentence | Tailscale "What is a tailnet?"; Modal "What are Sandboxes and why should I use them?"; Vite "Why" (history + credited prior art) |
| **Operate** (how-to + explanation) | Sizing, capacity, idle eviction tuning, upgrades, backup/restore, observability, security model | PostHog self-host (blunt about what you take on); Modal "Reliability and robustness", "Security and privacy" |
| **Reference** | CLI, API (Connect/gRPC), config keys, env vars, metrics, wire versions. Facts only | Django "Reference guides"; esbuild API page; SQLite limits page |
| **FAQ** | Short questions as headings; includes "Production readiness" and "Not a sandbox for X" style answers | esbuild FAQ; ripgrep FAQ.md |
| **Contribute** | Build from source, tests, security disclosure | Redis "Build Redis from source"; Firecracker "Contributing" |

Two extras worth copying: a one-line "Read these docs as an agent" note with an `llms.txt` and/or MCP endpoint (E2B), and Markdown-addressable pages (`/docs/x.md`, as Stripe does) so agents and `curl` get the source.

**Sources.** ripgrep README and GUIDE; esbuild README, esbuild.github.io, /getting-started/, /faq/; redis README (unstable); sqlite.org/whentouse.html and /about.html; vite.dev/guide/ and /guide/why; docs.astro.build getting-started, concepts/why-astro, install-and-setup; tailscale.com/kb, kb/1151, kb/1136, kb/1017; coder.com/docs, /docs/install, coder/coder docs/README.md and docs/manifest.json; modal.com/docs, /docs/guide, /docs/guide/sandbox; docs.e2b.dev/, /docs/quickstart, /docs/sandbox/lifecycle; firecracker README and docs/getting-started.md; htmx.org/docs/; docs.djangoproject.com/en/stable/ and intro/tutorial01; diataxis.fr/ and /start-here/; developers.google.com/style/highlights; learn.microsoft.com/en-us/style-guide/top-10-tips-style-voice; docs.stripe.com; posthog.com/docs, /docs/self-host, /docs/getting-started/install, posthog.com/handbook/content/posthog-style-guide, posthog.com/handbook/engineering/writing-docs.
