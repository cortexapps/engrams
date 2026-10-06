---
title: Upgrade engrams
description: Move a deployment from one release to the next, and what happens to running sessions while you do.
sidebar:
  order: 4
---

A release of engrams is a git tag, `v0.10.0` for example, and a set of container images with
the same version as their tag. The two charts in a checkout of that tag install those images
by default: an image tag you do not set falls back to the chart's `appVersion`. To upgrade,
check out the newer tag and run `helm upgrade` on both releases.

engrams is a 0.x project, so any release can change a wire format, a database schema, or a
Terraform module. The [release notes](https://github.com/cortexapps/engrams/releases) say
which. Read them before you start.

## Before you upgrade

- **Back up both databases.** The coordinator and the orchestrator apply their schema
  migrations during the upgrade, and the migrations only go forward. A backup is the one way
  back to the older release.
- **Keep your values files.** You need the two files you passed to `helm install`, and the
  Terraform variables you applied with.

## 1. Check out the release

```sh
git fetch --tags
git checkout v0.10.0
```

Use the tag of the release you are moving to. `git tag --list 'v*' --sort=-v:refname` lists
the releases, newest first.

## 2. Apply Terraform and render the values again

A release can add a secret, an identity, or a value that the charts expect. Apply the
quickstart with the variables you used the first time, then write the two overlays again.
On GCP:

```sh
cd deploy/terraform/gcp/quickstart
terraform init
terraform apply   # with your -var flags or your .tfvars file
terraform output -raw engram_values     > /tmp/engram.tfvalues.yaml
terraform output -raw host_fleet_values > /tmp/host-fleet.tfvalues.yaml
cd ../../../..
```

On AWS the directory is `deploy/terraform/aws/quickstart`. Read the plan before you approve
it: a release should not replace the cluster, the database, or the bucket, and a plan that
says it will is a reason to stop.

Then compare your two values files with the examples in the new checkout,
`deploy/helm/engram/values-<cloud>.yaml.example` and
`deploy/helm/engram-host-fleet/values-<cloud>.yaml.example`, and copy over any key that is
new. If your files set an image tag, remove it, or the upgrade keeps the old images.

## 3. Upgrade the control plane

```sh
helm upgrade engram deploy/helm/engram \
  -n engrams -f /tmp/engram-values.yaml -f /tmp/engram.tfvalues.yaml
```

Helm first runs a job that migrates the orchestrator's database, and stops the upgrade if
the job fails. The coordinator migrates its own database when its new pods start. Then the
three Deployments roll. Sessions run on the hosts, so a control-plane roll does not touch
them; the dashboard reconnects on its own.

`kubectl get pods -n engrams` should show the coordinator, web, and orchestrator pods
`Running` on the new image.

## 4. Upgrade the host fleet

Helm installs a custom resource definition once and never updates it, so apply the new one by
hand before the chart:

```sh
kubectl apply -f deploy/helm/engram-host-fleet/crds/

helm upgrade hf deploy/helm/engram-host-fleet \
  -n engrams-hosts -f /tmp/fleet-values.yaml -f /tmp/host-fleet.tfvalues.yaml
```

This command returns at once and no host pod restarts yet. The operator sees the new images
and replaces the host pods one node at a time: it cordons the node, deletes the pod, waits
until the new pod is ready, and moves to the next. The VMs on the node keep running while
the pod is replaced, and the new pod reattaches to them. Watch the roll with `kubectl get pods -n engrams-hosts -w`, or on the dashboard's
Settings › Fleet page.

Do steps 3 and 4 together. The coordinator and the host agents share a wire version and
refuse to talk across a mismatch, so after a release that changes it, new sessions queue
until the first host is on the new version.

## Run a build that is not a release

Set the image tags to the full commit SHA of a commit on `main`; every build is published
under that tag. The control-plane chart takes `image.tag`, `web.image.tag`, and
`orchestrator.image.tag`, and the fleet chart takes `image.tag`, `operator.image.tag`, and
`nodeAssets.image.tag`. Use the charts from the same commit.

**Do not** deploy the `main` tag. It moves, and the charts pull with `IfNotPresent`, so a
node that already has `main` keeps the build it has while a new node pulls a newer one.
