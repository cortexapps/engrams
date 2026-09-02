# engrams on GCP — Terraform

The GCP deployment surface (ADR 0122): **composable modules + a
quickstart root that wires them all**. Use the quickstart for a fresh
project; compose the modules yourself when you bring your own cluster
or database.

The step-by-step bring-up (including the manual steps Terraform
deliberately does not do) is [`docs/deploy-gcp.md`](../../../docs/deploy-gcp.md).
The cloud-agnostic contract both clouds implement is
[`../README.md`](../README.md).

## Layout

```
modules/
  network/            VPC + subnet + Cloud NAT + firewall
  storage/            GCS chunks bucket (the ADR 0007 blob tier)
  fc-host-gsa/        The FC hosts' GSA (identity only)
  gke-cluster/        Regional cluster: Workload Identity, Managed
                      Prometheus, VPC-native + the small control-plane
                      pool — the chart prerequisites, made explicit
  gke-kvm-pool/       THE nested-virt host pool (Intel-only, operator-
                      owned size, the CPUID one-way door — read its
                      header before touching it)
  cloudsql/           Postgres 16, private IP, the controlplane +
                      orchestrator databases, DSNs in Secret Manager
                      (with the load-bearing sslmode split)
  secret-shells/      Empty Secret Manager shells for the operator-
                      populated secrets (KEK, auth tokens, better-auth,
                      egress CA) — material never enters tfstate
  host-operator-iam/  The autoscaling operator's least-privilege
                      custom role + GSA (ADR 0048)
quickstart/           ONE `terraform apply` from a fresh project to an
                      engram-ready cluster: all of the above plus
                      namespaces (fleet PSA-privileged), the External
                      Secrets relay, identity bindings, the web static
                      IP + managed certificate — and rendered Helm
                      values as outputs
examples/
  minimal/            The BYO-cluster shape: VPC + bucket + KMS +
                      identities only; bring your own GKE + Cloud SQL
```

## Quickstart

```sh
cd deploy/terraform/gcp/quickstart
terraform init
terraform apply \
  -var project_id=my-project \
  -var region=us-west2 \
  -var domain=engrams.example.com \
  -var admin_email=you@example.com
```

Then follow `docs/deploy-gcp.md`: populate the secret shells (the
`secret_shell_ids` output lists them; the shells module header carries
the exact `openssl` + `gcloud` commands), create the DNS record if you
didn't pass `dns_zone_name` (the `web_static_ip` output), render the
values overlays —

```sh
terraform output -raw engram_values      > engram.tfvalues.yaml
terraform output -raw host_fleet_values  > host-fleet.tfvalues.yaml
```

— and `helm install` the two charts with your copied
`values-gcp.yaml.example` files plus these overlays.

Mind the ordering: ExternalSecrets sync only after the shells are
populated; Helm releases installed before that CrashLoop on missing
Secrets until the first successful sync.

## BYO cluster / database

Compose the modules directly — each stands alone. `examples/minimal/`
shows the smallest useful set (network + bucket + KMS + identities);
add `gke-kvm-pool` against your existing cluster and `cloudsql` (or
any Postgres ≥14) as needed. The published Google modules
(`terraform-google-modules/kubernetes-engine`, `.../sql-db`) also
compose fine with these — our modules carry the engram-specific
invariants, not a re-rolled cloud.

## Module stability

`modules/{network,storage,fc-host-gsa}` are consumed by git ref from
external deployments — their paths and variable contracts are frozen.
New modules are additive.
