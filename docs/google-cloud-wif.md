# Google Cloud connections

Engrams uses Google Workload Identity Federation (WIF). It does not accept a
service-account key and it does not use the deployment VM identity.

## Requirements

- The Engrams `BASE_URL` must be a public HTTPS origin. Google must be able to
  read its OIDC discovery and JWKS endpoints.
- Google-enabled sessions require a Firecracker or VZ host with the host egress
  proxy. The Process backend rejects them because it cannot intercept metadata.
- Create one Google service account for each privilege set. A connection always
  targets one service account.
- Enable the Security Token Service API and IAM Service Account Credentials API
  in the customer project.
- Add every required Google API hostname as an exact connection endpoint. Do not
  add STS or OAuth endpoints.

Common endpoints are `compute.googleapis.com`, `logging.googleapis.com`,
`cloudtrace.googleapis.com`, and `container.googleapis.com`.

## Configure a connection

1. Open **Settings → Integrations → Google Cloud**.
2. Select **Add connection**.
3. Enter the full workload identity provider resource, the target
   service-account email, and exact endpoints.
4. Create the disabled connection.
5. Apply either the generated Terraform or `gcloud` configuration. The output
   fixes the allowed audience, claim mapping, connection condition, and
   `roles/iam.workloadIdentityUser` principal set.
6. Select **Test**. Engrams performs both the STS exchange and service-account
   impersonation.
7. Enable the connection only after the test passes.
8. Add the connection operations to a profile.

Disabling, editing, or deleting a connection affects new sessions. An active
session keeps its immutable launch-time connection snapshot and can refresh from
that snapshot until it ends. To stop active sessions, revoke the Google-side WIF
or IAM binding, or end those sessions. Editing a connection disables it; test it
again before re-enabling it for new sessions.

## Resource constraints

Google IAM on the target service account is the primary resource boundary.
Profile resource constraints add a proxy boundary. They are full Google API path
patterns. `*` matches one path segment; it never spans `/`. Curated path-based
operations validate their expected path shape. For example:

```text
/compute/v1/projects/acme-prod/zones/us-central1-a/instances/engram-dev/start
```

`logging.entries.list` puts resource names in the request body. Engrams rejects
profile resource constraints for that operation because the proxy cannot enforce
them at the path boundary. Use IAM and a dedicated service account instead.

## Audit correlation

The credential broker records the user, session, immutable profile snapshot,
connection, service account, and mint outcome. The egress proxy records the
session, connection credential source, target host, HTTP method, path without
its query string, and policy outcome. Correlate the records by session and
connection. Engrams does not record tokens, authorization headers, query
strings, or request bodies.

In Google Cloud, enable Data Access audit logs for:

- Security Token Service (`sts.googleapis.com`);
- IAM Service Account Credentials (`iamcredentials.googleapis.com`);
- each data API that the connection can call.

Use the WIF subject and connection attribute from the Google audit entry to
correlate it with the Engrams session and connection audit fields. See Google's
[product federation guide](https://docs.cloud.google.com/iam/docs/use-workload-identity-federation-to-let-customers-access-their-cloud-resources)
and [WIF security guidance](https://docs.cloud.google.com/iam/docs/best-practices-for-using-workload-identity-federation).

## Guest behavior

Google tools use metadata-style Application Default Credentials. The host egress
process serves a session-local metadata endpoint that returns only an opaque
placeholder. The egress proxy removes guest authorization and adds the real
short-lived token after it checks the immutable session policy, method, path, and
endpoint. It binds the HTTP authority to the validated TLS server name before it
forwards the request. It also redacts a brokered credential if an upstream
response echoes it.

Do not run `gcloud auth login`, create an ADC file, or copy a key into a session.
The Google STS and OAuth endpoints and credential-producing IAM methods are
always denied from the guest.

The session bundle adds `gcloud` only. Kubernetes clients, SSH clients, and
development synchronization tools are outside this integration.
