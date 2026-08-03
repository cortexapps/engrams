---
name: integrations
description: Third-party integration CLIs (e.g. gh for GitHub, glab for GitLab, stripe for Stripe, pup for Datadog, linear for Linear, slack for Slack, gcloud for Google Cloud) are available in this session, authenticated automatically by the engrams platform. Use when you need to interact with a connected service from the command line. Run `engrams-integrations` to see which are enabled and how to use each.
---

# Integration CLIs

This session has command-line tools for the third-party services your profile
connected — for example `gh` (GitHub), `glab` (GitLab), `stripe` (Stripe),
`pup` (Datadog), `linear` (Linear), `slack` (Slack), and `gcloud` (Google
Cloud). **You never
handle real credentials**: each tool carries a harmless placeholder token, and the
engrams egress proxy injects the real, capability-scoped credential on the wire.
Do not paste, export, or `login` with real API tokens — it's already wired.

## See what's enabled (and how to use it)

```bash
engrams-integrations
```

This prints the integrations authenticated **in this session** and the
per-tool usage notes. Only the services your profile enabled will authenticate;
calls to anything else are denied at the network boundary.

## How auth works (so you don't fight it)

- Each CLI already has a placeholder credential in its environment (e.g.
  `GH_TOKEN=x-engrams-managed`). **Leave it as-is** — the platform replaces it
  host-side on every request. Re-`login` or overwriting it will not help and may
  break the brokering.
- Requests only reach the hosts your enabled integrations allow; everything else
  is blocked. A tool whose integration you didn't enable will fail at the network
  boundary, not because it's missing.
- There is nothing to log in to, no token to paste, no `*_API_KEY` to set.
- `gcloud` reads its credential from a session-local metadata endpoint, the same
  way it would on a Compute Engine instance. Do not run `gcloud auth login`, do
  not create an application-default-credentials file, and do not copy a service
  account key in — all three are refused, and none is needed. The metadata
  endpoint holds no project, so pass `--project` (or set
  `CLOUDSDK_CORE_PROJECT`) when a command needs one.
