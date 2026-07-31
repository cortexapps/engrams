---
name: engrams-prod-ops
description: Inspect the Engrams production deployment from an authorized session with brokered Google ADC and separate product, Kubernetes, and database grants.
---

# Engrams production operations

Google Cloud authentication is automatic through metadata-style Application
Default Credentials. Do not run a Google login command. Do not create or import
a credential file or a service-account key. The profile fixes the connection,
service account, operations, resources, and endpoints. This skill cannot change
those choices.

Treat these authorities as separate:

- Brokered Google ADC authorizes allowed Google API calls.
- Kubernetes RBAC authorizes `kubectl` after GKE credential discovery.
- The Engrams product API key authorizes product RPCs.
- Database access is a separate grant and is not implied by cloud access.
- Other provider secrets remain separate integration grants.

Do not fetch Kubernetes secrets to obtain Google credentials. Do not print
tokens, authorization headers, credential exchange bodies, pod environments, or
database connection strings. Do not enable `gcloud --log-http`, `curl -v`, or
shell tracing around authenticated calls.

Use `gcloud container clusters get-credentials` only when the profile grants
cluster discovery and the GKE API endpoint. Use the bundled GKE auth plugin and
`kubectl`; Kubernetes can still deny a request after Google authentication
succeeds. Confirm with the user before any production write.
