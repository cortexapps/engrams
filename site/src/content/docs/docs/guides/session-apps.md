---
title: Session apps
description: Give a service running inside a session a stable URL that people and sibling apps can reach, behind the login wall.
sidebar:
  order: 9
---

A session app is an HTTP service inside a session, published at its own hostname. An agent
that starts a web server on port 3000 can hand you a link; a frontend and an API in the same
session can find each other by name; a person can open the running thing instead of reading
about it. Every request passes through the orchestrator, which authenticates the visitor and
relays the bytes into the VM.

## Declare apps on a profile

In Settings → Profiles, a profile's Apps section takes a name and a port per app. Names are
1 to 24 lowercase characters with interior hyphens; ports are 1 to 65535; a profile may not
repeat a name or a port. A profile with no apps publishes nothing.

Each session draws one random slug, three words like `tidy-swift-otters`, and every app in
the session is `<name>-<slug>` under the deployment's preview domain:

```
web-tidy-swift-otters.preview.example.com
api-tidy-swift-otters.preview.example.com
```

The hostname does not encode the port and is not a secret. Authorization is the wall; the
name is only an address.

## What the session sees

Every process in the session gets two variables per app, for every app in the session, not
only its own:

```
WEB_INGRESS_HOST = web-tidy-swift-otters.preview.example.com
WEB_INGRESS_URL  = https://web-tidy-swift-otters.preview.example.com
API_INGRESS_HOST = api-tidy-swift-otters.preview.example.com
API_INGRESS_URL  = https://api-tidy-swift-otters.preview.example.com
```

The stem is the app name uppercased, with runs of punctuation collapsed to one underscore.
Half of what a service needs is a peer's address, which is why every app gets every name: a
CORS allow-list and a post-login redirect live on the API and want the frontend's URL. Map
them into the names your code expects with `${…}` in the profile's environment variables:

```
CORS_ALLOWED_ORIGINS = ${WEB_INGRESS_URL}
API_BASE_URL         = ${API_INGRESS_URL}
```

An unknown reference is left as written, since env values legitimately contain `$`. A
reference shaped like an ingress variable that resolves to nothing is reported as a typo or
a renamed app. The profile editor previews the resolved values as you type.

An app published ad hoc on a running session, from the session's Diagnostics drawer, gets a
hostname but no environment variable, because the VM's environment was fixed when the
harness started. Declare on the profile anything a sibling must reach.

## Who can open one

| Visibility | Who may open the app |
|---|---|
| `org` (the default) | Any signed-in member of the deployment. |
| `private` | The session's owner and admins. |

An unauthenticated navigation redirects to the login page and back. An unauthenticated
request that is not a navigation, such as a `fetch`, gets a 401 rather than a login page,
so a client sees a clean error instead of an HTML parse failure. A credentialed cross-origin
request is allowed only from a sibling app of the same session; two sessions are two trust
domains even under one owner. A CORS preflight is allowed through without a cookie, because
browsers never send one on a preflight and nothing the app could do would fix a refusal.

The proxy strips any cookie a guest app tries to set under the orchestrator's own cookie
name, so a guest cannot overwrite a visitor's session. A guest app that itself uses the same
auth library under its default cookie name will find its login not sticking; rename its
cookie.

The Diagnostics drawer on a session lists its apps with a liveness dot: serving, no response
on the port, or unknown while the session is not running. Revoking an app removes its
hostname at once.

## What the deployment needs

Session apps are off until the orchestrator has a preview domain. In the control-plane
chart, set `orchestrator.preview.baseDomain` to something like `preview.example.com`, and:

- add a `*.preview.example.com` host to the web Ingress whose root path targets the
  orchestrator;
- provision a wildcard TLS certificate for `*.preview.example.com` and a wildcard DNS record
  at the web Ingress's address;
- set `orchestrator.sessionCookieDomain` to the domain the app host and the preview domain
  share, `.example.com` for `app.example.com` and `*.preview.example.com`, so one login
  covers the dashboard and every app;
- put no identity-aware proxy in front of the preview host. The orchestrator is the wall, and
  an external proxy breaks cross-app calls by rejecting cookieless preflights.

For development the default preview domain is `lvh.me` with the orchestrator's port, which
resolves to localhost, so apps work over plain HTTP with no DNS or certificate.
