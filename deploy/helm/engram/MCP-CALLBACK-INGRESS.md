# MCP callback ingress protection

Keep external MCP disabled until the deployment checks below pass. An OAuth
redirect sends a GET to `/integrations/mcp/callback` with transient `code`,
`state`, and `iss` query values. The browser removes the query before it loads
the application. That does not protect the initial request at a proxy.

The completion request is an authenticated POST to
`/rpc/engram.app.v1.IntegrationService/CompleteMcpAuthorization`. Same-origin
deployments send it through web nginx. Split-host deployments send it directly
to the API host. Both paths must exclude request bodies, queries, and referrers
from logs, traces, error reports, and request capture.

## Checked-in controls

The web nginx server logs method, original path without query, status, duration,
and response size as JSON. The server-wide format applies after SPA internal
redirects and to `/rpc/` and `/api/`. Health requests do not create access records.
There is no body or header field in this format. nginx logs in the location
where request processing ends, so a callback-location override alone is not
sufficient. See the [nginx access-log documentation](https://nginx.org/en/docs/http/ngx_http_log_module.html).

HTTP error diagnostics are sent to `/dev/null`. nginx error messages can include
the complete request and referrer; raising the severity threshold does not
redact them. This removes HTTP error detail for all web routes. Status and
timing remain in access logs, and main-process startup diagnostics remain.
Do not enable debug logging or add another request-bearing log destination.
See [nginx error-log contexts](https://nginx.org/en/docs/ngx_core_module.html#error_log).

The shipped public ingress profiles explicitly disable unsafe access logging:

| Profile | Configuration |
| --- | --- |
| GCP quickstart and manual GCP example | `engram-web` BackendConfig has `spec.logging.enable: false`. |
| GCP split-host IAP | Both `engram-web-iap` and `engram-api-no-iap` BackendConfigs have `spec.logging.enable: false`. The rendered web and webhook Services reference these names. |
| AWS ALB | `alb.ingress.kubernetes.io/load-balancer-attributes` includes `access_logs.s3.enabled=false` and retains the idle timeout. |

GKE supports the logging field from version `1.16.8-gke.10`. Omitting it leaves
version-dependent behavior. See [GKE HTTP access logging](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/ingress-configuration#http_access_logging).
The AWS controller updates only specified attributes; an explicit false is
required to disable previously enabled logs. See [AWS Load Balancer Controller
attributes](https://kubernetes-sigs.github.io/aws-load-balancer-controller/latest/guide/ingress/annotations/#custom-attributes).
These controls disable the backend or ALB access log as a whole. They do not
provide field redaction or control independent security, CDN, or application logs.

## Local canary and CI

Prerequisites are Python 3.9 or later, Ruby with YAML, Helm, and a working Docker
daemon. Pull the official nginx tag specified in `docker/web.Dockerfile`, then
run from the repository root:

```sh
python3 -m unittest discover -s deploy/helm/engram/ci/tests -p 'test_callback_profiles.py'
python3 deploy/helm/engram/ci/callback-ingress-canary.py --stage var/mcp-callback-canary
```

Use a new stage directory for each run. It must be a real directory under this
checkout's `var/mcp-*`. The script does not install or pull a runtime. Missing
prerequisites, no served requests, invalid configuration, unexpected responses,
and missing access records fail the check. The inspected image must be at most
100 MiB. Its exact ID is used for all runs and its digest is saved in `image.json`.

The script renders the chart, runs the official entrypoint and envsubst filter,
and saves `nginx -T`, container logs, and cleanup records. It starts three serial
containers: same-origin web, split-host web, and a deliberately unsafe negative
control. Each has no network or published ports, one CPU, 128 MiB memory,
32 processes, a read-only root, and bounded writable tmpfs mounts. Each run has
a 90-second deadline and removes its own container. Only synthetic values are used.

The requests cover callback serving, SPA internal fallback, POST proxying,
duplicate-Host rejection, a missing-index redirect cycle, and a closed upstream.
The safe cases require useful access records without the canary query,
referrer, or body. The negative control must expose canaries in both access and
error logs. Its upstream only supplies HTTP fixture responses; it does not
implement authentication or OAuth. The split-host case checks the web server
and runtime API origin, not the external API ingress or its application logs.

The existing `deploy-config` CI lane runs the profile tests and canary. Chart,
Terraform, canary, workflow, detector, and web-image changes select that lane.
It remains a dependency of `CI Gate`. No extra required status is needed.

## Deployment qualification

The local canary proves behavior of the rendered nginx configuration and image.
Rendered cloud objects prove configuration intent. Neither proves that a cloud
controller applied the settings, nor that a deployment's entire log path is safe.

Before enabling external MCP in a deployment:

1. Identify the first public ingress for the callback host and the API host.
   Include any CDN, WAF, identity proxy, load balancer, ingress controller,
   service mesh, request recorder, trace exporter, and log collector.
2. Check the effective cloud backend or ALB logging attributes, Service bindings,
   nginx configuration, and downstream capture policies. Disable capture of the
   query, referrer, and POST body at each source before those fields reach a
   collector. A collector filter alone does not protect upstream copies.
3. Send neutral, unique canary values through the real GET and POST paths.
   Include success, rejected requests, internal fallback, and upstream errors
   in an isolated qualification environment. Use metadata or counters to prove
   requests arrived. After the documented ingestion delay, inspect every
   configured log, trace, and error sink for those values. Keep the observed
   request counts, effective configuration, sink coverage, and result.
4. Establish the negative control only in an isolated validation environment.
   Never enable unsafe logging on a real credential flow. Requalify after an
   ingress, runtime, collector, or logging-policy change.

An unverified first ingress or collector keeps the feature disabled. An
administrator's MCP zone selection cannot override operator logging or network
policy. These checks are separate from real Firecracker acceptance, VZ parity,
enforcing CNI checks, OAuth/provider checks, and the complete application suites.
