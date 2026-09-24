# Security policy

## Report a vulnerability

Use GitHub's private vulnerability reporting for this repository:
[Report a vulnerability](https://github.com/cortexapps/engrams/security/advisories/new).
Do not open a public issue, and do not include exploit details in a pull request.

Include the version or commit, the component (coordinator, host-agent, sandbox backend,
orchestrator, web, CLI), reproduction steps, and the impact you observed. We acknowledge a
report within three business days and keep you informed until the fix ships.

## Scope

engrams runs untrusted agent workloads in microVMs. Reports about isolation (guest to host,
guest to guest, egress policy bypass), authentication, and credential handling get the
highest priority.

## Supported versions

Only `main` receives security fixes. Deployments run a recent `main` image.
