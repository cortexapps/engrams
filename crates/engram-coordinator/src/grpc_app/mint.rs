//! ADR 0057 C3: `MintService` over app-gRPC — the read-only mint-kind registry,
//! plus `RunConnectorTest` (redesign): the actual unseal/mint + benign GET that
//! backs the Connect/Replace "Test connection" step. The caller is the trusted
//! orchestrator (bearer-authed); per-user authz (admin-only) lives there. The
//! coordinator is the only tier that can unseal org secrets + run the mint
//! engine, so the test executes here from a resolved spec the orchestrator built.

use std::sync::Arc;
use std::time::Duration;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use engram_core::traits::{CredentialHint, ScopedCredential, SecretContext};
use engram_core::types::SecretSchema;

use super::auth;
use crate::state::SharedState;

pub struct AppMintService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

fn field_kind_to_proto(k: engram_core::traits::MintFieldKind) -> i32 {
    let v = match k {
        engram_core::traits::MintFieldKind::Config => app::MintFieldKind::Config,
        engram_core::traits::MintFieldKind::SealedSecret => app::MintFieldKind::SealedSecret,
    };
    v as i32
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::mint_service_server::MintService for AppMintService {
    async fn list_mint_kinds(
        &self,
        req: Request<app::ListMintKindsRequest>,
    ) -> Result<Response<app::ListMintKindsResponse>, Status> {
        self.auth.check(&req)?;
        let mint_kinds = crate::integrations::mint_kind_registry()
            .into_iter()
            .map(|d| app::MintKind {
                kind: d.kind.to_string(),
                provider: d.provider.to_string(),
                display_name: d.display_name.to_string(),
                fields: d
                    .fields
                    .iter()
                    .map(|f| app::MintFieldDescriptor {
                        name: f.name.to_string(),
                        label: f.label.to_string(),
                        field_kind: field_kind_to_proto(f.field_kind),
                        required: f.required,
                    })
                    .collect(),
            })
            .collect();
        Ok(Response::new(app::ListMintKindsResponse { mint_kinds }))
    }

    async fn run_connector_test(
        &self,
        req: Request<app::RunConnectorTestRequest>,
    ) -> Result<Response<app::RunConnectorTestResponse>, Status> {
        self.auth.check(&req)?;
        let spec = req.into_inner();
        let state = &self.state;
        // Resolve the credential (stored or draft) and make one benign GET. The
        // inner block yields Ok(msg)=accepted / Err(msg)=rejected; a failed test
        // is a normal {ok:false} result, not a transport-level Status. (Inlined
        // rather than a free `async fn` so the grpc_app auth-convention test —
        // which counts one `self.auth.check` per `async fn` — stays satisfied.)
        let outcome: Result<String, String> = async {
            let http = reqwest::Client::builder()
                .user_agent("engram-connector-test")
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| format!("http client: {e}"))?;
            let url = format!("https://{}/", spec.host);

            let request = if spec.source == "mint" {
                // Build the engine from the draft fields, or resolve it from the
                // stored org secrets; minting itself validates the credentials.
                let engine = if !spec.draft_fields.is_empty() {
                    let desc = crate::integrations::mint_kind_registry()
                        .into_iter()
                        .find(|d| d.kind == spec.kind)
                        .ok_or_else(|| format!("unknown mint kind \"{}\"", spec.kind))?;
                    (desc.build)(&spec.draft_fields)
                        .map_err(|e| format!("invalid credentials: {e}"))?
                } else {
                    state
                        .integrations
                        .resolve(&spec.provider, &state.services.secrets)
                        .await
                        .ok_or_else(|| "mint credentials are not configured".to_string())?
                };
                // served_host is the integration's OWN host identity (e.g.
                // github.com, the git host a multi-host provider validates
                // against) — a different namespace from the connector's egress/API
                // host we GET below (api.github.com). The real mint paths pass the
                // git host (forge.rs) or None (egress broker), never the API host,
                // so do NOT derive it from spec.host or the provider rejects the
                // request ("does not serve host `api.github.com`"). Mint the
                // default credential; the probe still targets spec.host via `url`.
                let hint = CredentialHint {
                    served_host: None,
                    owner: None,
                };
                let cred = engine
                    .mint_credential(&[], &hint)
                    .await
                    .map_err(|e| format!("could not mint a token: {e}"))?;
                match cred {
                    ScopedCredential::Bearer { token, .. } => http.get(&url).bearer_auth(token),
                    ScopedCredential::Basic {
                        username, password, ..
                    } => http.get(&url).basic_auth(username, Some(password)),
                    // STS isn't simple header auth; minting succeeded, so that's the test.
                    ScopedCredential::AwsSts { .. } => {
                        return Ok(format!(
                            "Minted scoped AWS credentials for {}",
                            spec.provider
                        ));
                    }
                }
            } else {
                // inject: add EVERY declared header, each using its draft value or
                // the stored org secret (ADR 0058: a connector may inject several,
                // e.g. Datadog's DD-API-KEY + DD-APPLICATION-KEY — the probe only
                // passes if they ALL authenticate).
                if spec.injects.is_empty() {
                    return Err("connector has no inject credential".to_string());
                }
                let mut builder = http.get(&url);
                for inj in &spec.injects {
                    let value = if !inj.draft_secret.is_empty() {
                        inj.draft_secret.clone()
                    } else {
                        let ctx = SecretContext {
                            repo: "",
                            image_tag: "",
                        };
                        let schema = SecretSchema {
                            required: true,
                            ..Default::default()
                        };
                        state
                            .services
                            .secrets
                            .get(&ctx, &inj.secret_ref, &schema)
                            .await
                            .map_err(|e| format!("could not resolve org secret: {e}"))?
                            .ok_or_else(|| {
                                format!("org secret \"{}\" is not set", inj.secret_ref)
                            })?
                    };
                    let header_name = if inj.header.is_empty() {
                        "Authorization"
                    } else {
                        &inj.header
                    };
                    let template = if inj.template.is_empty() {
                        "{}"
                    } else {
                        &inj.template
                    };
                    builder = builder.header(header_name, template.replace("{}", &value));
                }
                builder
            };

            let resp = request
                .send()
                .await
                .map_err(|e| format!("request to {} failed: {e}", spec.host))?;
            let code = resp.status().as_u16();
            if code == 401 || code == 403 {
                Err(format!(
                    "{} rejected the credential (HTTP {code})",
                    spec.host
                ))
            } else {
                Ok(format!(
                    "Reached {} · HTTP {code} · credential accepted",
                    spec.host
                ))
            }
        }
        .await;
        let (ok, message) = match outcome {
            Ok(m) => (true, m),
            Err(m) => (false, m),
        };
        Ok(Response::new(app::RunConnectorTestResponse { ok, message }))
    }
}
