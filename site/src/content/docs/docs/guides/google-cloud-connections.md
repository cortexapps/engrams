---
title: Google Cloud connections
description: Give sessions short-lived Google Cloud credentials through Workload Identity Federation, with no keys anywhere.
sidebar:
  order: 6
---

A Google Cloud connection lets an agent call Google APIs as a service account you choose,
with credentials that are minted per request and never enter the VM. engrams uses Workload
Identity Federation for this. It does not accept a service-account key, and it does not use
the identity of the machine it runs on.

## Requirements

- `ORCHESTRATOR_PUBLIC_URL` must be a public HTTPS origin. Google reads its OpenID discovery
  and JWKS endpoints from there.
- `ENGRAM_DEPLOYMENT_ID` names this deployment in the token's organization claim. It defaults
  to the public hostname. The federation setup pins it in an attribute condition, so treat it
  as immutable once a project has applied the setup.
- Sessions that use a connection need a Firecracker or Apple Virtualization host with the
  egress proxy. The process backend refuses them, because it cannot intercept the metadata
  endpoint.
- One Google service account per set of privileges. A connection always targets one service
  account.
- The Security Token Service API and the IAM Service Account Credentials API enabled in the
  project.
- Every Google API hostname a session may call, listed as an exact endpoint on the
  connection. Do not add the STS or OAuth endpoints; they are always denied from the guest.

Common endpoints are `compute.googleapis.com`, `logging.googleapis.com`,
`cloudtrace.googleapis.com`, and `container.googleapis.com`.

## Set up a connection

1. Open **Settings → Integrations → Google Cloud** and select **Add connection**.
2. Enter a name, the numeric project number, and the target service account's email, and
   select the Google APIs sessions may reach. engrams generates the connection alias and the
   federation provider resource; the pool and provider ids are under **Advanced settings**.
3. Create the connection. It starts disabled, and engrams opens its setup page.
4. Apply the generated Terraform or `gcloud` configuration from that page. It fixes the
   allowed audience, the claim mapping, the connection condition, and the
   `roles/iam.workloadIdentityUser` binding.
5. Select **Test**. engrams performs the token exchange and the service-account
   impersonation.
6. Enable the connection once the test passes, and add its operations to a profile.

Prefer the curated operations when one covers the call. Cloud Logging, Cloud Trace list and
detail, Cloud Monitoring metric descriptors and time series, Compute Engine instance reads,
and GKE cluster metadata each have one. `api.call` is for APIs without a curated operation;
it permits calls to every configured `*.googleapis.com` endpoint, subject to the service
account's IAM policy.

Disabling, editing, or deleting a connection affects new sessions. A running session keeps
the connection snapshot it launched with and can refresh from it until it ends; to cut it
off, revoke the Google-side binding or end the session. Editing a connection disables it, so
test again before you re-enable it.

## Resource constraints

Google IAM on the target service account is the primary boundary. A profile can add a second,
proxy-enforced boundary as full Google API path patterns, where `*` matches one path segment
and never spans a `/`:

```text
/compute/v1/projects/acme-prod/zones/us-central1-a/instances/my-instance/start
```

`logging.entries.list` puts resource names in the request body, where the proxy cannot see
them, so engrams rejects path constraints for that operation. Use IAM and a dedicated service
account there instead.

## Audit

The credential broker records the user, session, the immutable profile snapshot, the
connection, the service account, and whether the mint succeeded. The egress proxy records the
session, the credential source, the target host, the HTTP method, the path without its query
string, and the policy outcome. Correlate the two by session and connection. engrams never
records tokens, authorization headers, query strings, or request bodies.

In Google Cloud, enable Data Access audit logs for the Security Token Service, IAM Service
Account Credentials, and each data API the connection can call. The federation subject and the
connection attribute in a Google audit entry match the engrams session and connection fields.
Google's [federation guide for products](https://docs.cloud.google.com/iam/docs/use-workload-identity-federation-to-let-customers-access-their-cloud-resources)
and [security guidance](https://docs.cloud.google.com/iam/docs/best-practices-for-using-workload-identity-federation)
cover the Google side.

## Signing-key rotation

The orchestrator signs each subject token with an RSA key it keeps in its database and
publishes through JWKS. It creates the active key at startup, retires a key after 90 days,
and keeps the retiring key published for a further 7 days. That overlap is what makes
rotation safe: a token minted just before the change is still in flight when the new key
becomes active, and Google resolves it by key id. Nothing changes on the Google side; the pool
reads JWKS at validation time.

To rotate at once, after a suspected compromise, call
`POST /api/v1/admin/integrations/oidc/rotate`. The same overlap applies.

## What the guest sees

Google tools inside the VM use metadata-style Application Default Credentials. The host
serves a session-local metadata endpoint that returns only an opaque placeholder. The egress
proxy strips the guest's authorization, checks the session's policy, method, path, and
endpoint, adds the real short-lived token, binds the HTTP authority to the validated TLS
server name, and forwards the request. If an upstream response echoes the credential, the
proxy redacts it.

Do not run `gcloud auth login`, create a credentials file, or copy a key into a session. The
session bundle adds `gcloud` only; Kubernetes clients, SSH clients, and sync tools are
outside this integration.
