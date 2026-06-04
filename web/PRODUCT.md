# Product

## Register

product

## Users

Two audiences, weighted toward the first:

- **Developers (≈90% of sessions)** — engineers who launch bounded units of agent work instead of running it on their local machine. They start a session against a container image, hand an agent a task (fix a flaky test, chase a bug, build a feature), watch the transcript stream, and drop into a shell when they need to. They also kick off **background agents** that work unattended. Context: at their desk, mid-task, glancing over from their editor to check on work in flight. Fluent in dev tools; they expect keyboard reach, dense data, and no hand-holding.
- **Platform operators (admins)** — the same kind of person wearing a second hat: managing the fleet (Firecracker/VZ hosts, capacity, draining), the content-addressed storage substrate (chunks, snapshots, COW durability), enabled images, registry credentials, and org members. They need capacity and credential surfaces to read like instrument panels.

## Product Purpose

Engrams is a **self-hosted, open-source orchestrator for ephemeral AI agent sandboxes** — the open-source counterpart to Devin / Factory.ai / Ramp's Inspect. It gives teams a hosted "software factory": developers and background agents do real work (flaky-test fixes, bug fixes, features) inside disposable, snapshotable sandboxes instead of on local machines. Each **session** pairs an OCI image with an optional agent harness and moves through a lifecycle (pending → active → idle → snapshotted/resumed). Content-addressed chunked storage makes sandboxes cheap to fork, snapshot, and resume.

Success: a developer trusts engrams to run their work unattended and can tell at a glance what each agent did, what state every sandbox is in, and that their credentials and snapshots are safe.

## Brand Personality

**Creative, calm, precise instrument.** A lab notebook for software work: it records and reveals what agents do like entries in an engineering logbook. Confident and legible, never loud. The expressive register is "old-school engineering logbook / scientific journal"; the functional register is "precision instrument." Three words: **recorded, exact, composed.**

## Anti-references

- **Cluttered enterprise console** (AWS / GCP style): nested panels, tab soup, breadcrumb mazes, power buried under chrome. Engrams hides admin depth behind a calm surface.
- **Toy / playful consumer app**: bubbly rounded everything, emoji, mascots, fun-gradients. This product handles sealed credentials and live infrastructure; playfulness undermines trust.

(Not avoided: a degree of technical/terminal character is welcome, and earned familiarity with category-leading dev tools is a feature, not a failure.)

## Design Principles

- **The work is the experiment; the UI is the logbook.** Make session state and history legible at a glance — what ran, what changed, what state every sandbox is in. Recording and revealing work is the core job.
- **Legibility is the feature.** Dense technical data (IDs, digests, URIs, transcripts, capacity numbers) must be instantly scannable. Clarity beats decoration every time.
- **Earned familiarity over novelty.** Standard affordances, consistent component vocabulary screen to screen. Trust comes from precision, not surprise; the tool disappears into the task.
- **Calm under live state.** Sessions stream, hosts drain, snapshots fire. Motion and color convey state changes quietly and never alarm. Reserve attention for what actually needs it.
- **Show the receipts.** Durability, COW ledgers, run summaries, sealed-token confirmations — the product earns trust by exposing what it actually did. It's a dev tool built for people who read the receipts.

## Accessibility & Inclusion

- **WCAG 2.1 AA.** Body text ≥4.5:1, large text ≥3:1 — verified in both themes, with particular care on the cream/paper light mode where washed-out ink is the failure mode.
- **Color is never the sole status carrier.** Session lifecycle and host/storage state are always conveyed by glyph + text label alongside color (the existing status-glyph vocabulary).
- **Keyboard-first.** The developer audience expects full keyboard navigation and visible focus rings throughout.
- **Reduced motion honored.** Every transition has a `prefers-reduced-motion` alternative (crossfade or instant); nothing essential is gated on animation.
