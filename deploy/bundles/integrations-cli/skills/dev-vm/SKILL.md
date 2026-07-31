---
name: dev-vm
description: Operate the engram-dev Compute Engine VM from an authorized Engrams session through brokered Google ADC and IAP.
---

# Dev VM

Use only the Google Cloud connection that the session profile selected. The
skill cannot select a connection or add operations, resources, endpoints, or
profile launch access.

Authentication is automatic through metadata-style Application Default
Credentials. Do not run a Google login command. Do not create or import a
credential file or a service-account key.

Set the project and zone explicitly on each command. Use the resource values
that the task or administrator supplied. Do not infer another project, zone, or
VM name when the proxy denies a request.

Start or stop the VM with `gcloud compute instances start` or `stop`. Add
`--tunnel-through-iap` to every `gcloud compute ssh` command. Do not use a
public IP, direct SSH, or `gcloud compute config-ssh`.

For repeated SSH or Mutagen use, create a session-local SSH configuration with
a `ProxyCommand` that runs `gcloud compute start-iap-tunnel ... --listen-on-stdin`.
Store it only in the session workspace. Do not copy credentials into it.

The applicable bundle supplies `gcloud`, OpenSSH, and Mutagen. It does not
install Homebrew packages and does not depend on a laptop Google login.
