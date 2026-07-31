---
name: integrations
description: Third-party integration CLIs and Google Cloud tools are available in this session and authenticate through the engrams host broker. Use them only with the profile's existing connection grants.
---

# Integration CLIs

This session has command-line tools for the third-party services your profile
connected — for example `gh` (GitHub), `glab` (GitLab), `stripe` (Stripe),
`pup` (Datadog), `linear` (Linear), and `slack` (Slack). **You never
handle real credentials**: each tool carries a harmless placeholder token, and the
engrams egress proxy injects the real, capability-scoped credential on the wire.
Do not paste, export, or `login` with real API tokens — it's already wired.

Google Cloud sessions include `gcloud`, `gke-gcloud-auth-plugin`, `kubectl`,
`ssh`, and `mutagen`. Use metadata-style Application Default Credentials. Do
not run `gcloud auth login`, create a credential file, or request a service
account key. Use IAP tunnelling for every Compute Engine SSH connection. The skill cannot select
a connection or expand its profile grants or endpoints.

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
