//! ADR 0106 OAuthCredentialService. Per-user authorization remains in the
//! orchestrator; this trusted surface receives opaque subjects and owns the
//! provider lifecycle. Session fetch/update add the session broker-token gate.

use std::sync::Arc;

use engram_core::types::oauth::{
    OAuthCredentialKey, OAuthFlow, OAuthSubjectKind, SealedOAuthCredential,
};
use engram_protocol::app;
use tonic::{Code, Request, Response, Status};

use super::auth;
use crate::oauth::{OAuthServiceError, MAX_OAUTH_BUNDLE_BYTES};
use crate::oauth_redirect::{RedirectMetadataProbe, RedirectMetadataSpec, RedirectOauthSpec};
use crate::state::SharedState;

pub struct AppOAuthCredentialService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

fn subject_from_proto(
    subject: Option<app::OAuthSubject>,
) -> Result<(OAuthSubjectKind, String), &'static str> {
    let subject = subject.ok_or("subject is required")?;
    if subject.id.trim().is_empty() {
        return Err("subject id must not be empty");
    }
    let kind = match app::OauthSubjectKind::try_from(subject.kind) {
        Ok(app::OauthSubjectKind::User) => OAuthSubjectKind::User,
        Ok(app::OauthSubjectKind::Connector) => OAuthSubjectKind::Connector,
        Ok(app::OauthSubjectKind::Mcp) => OAuthSubjectKind::Mcp,
        _ => return Err("subject kind is required"),
    };
    Ok((kind, subject.id))
}

fn key(
    subject: Option<app::OAuthSubject>,
    provider: String,
) -> Result<OAuthCredentialKey, &'static str> {
    let (subject_kind, subject_id) = subject_from_proto(subject)?;
    if provider.trim().is_empty() {
        return Err("provider must not be empty");
    }
    Ok(OAuthCredentialKey {
        subject_kind,
        subject_id,
        provider,
    })
}

fn flow_to_proto(flow: OAuthFlow) -> app::OAuthFlow {
    app::OAuthFlow {
        id: flow.id.to_string(),
        provider: flow.key.provider,
        status: flow.status.as_str().into(),
        error_code: flow.error_code,
        expires_at: flow.expires_at.to_rfc3339(),
        updated_at: flow.updated_at.to_rfc3339(),
    }
}

fn credential_to_proto(
    row: SealedOAuthCredential,
    now: chrono::DateTime<chrono::Utc>,
) -> app::OAuthCredentialMeta {
    let status = row.status(now).as_str().to_string();
    app::OAuthCredentialMeta {
        provider: row.key.provider,
        version: row.version,
        connected: row.revoked_at.is_none(),
        status,
        expires_at: row.expires_at.map(|at| at.to_rfc3339()).unwrap_or_default(),
        subject_id: row.key.subject_id,
        account: Some(app::OAuthAccountMetadata {
            display_name: row.metadata.display_name,
            plan_type: row.metadata.plan_type,
            workspace_id: row.metadata.workspace_id,
            workspace_name: row.metadata.workspace_name,
        }),
        created_at: row.created_at.to_rfc3339(),
        updated_at: row.updated_at.to_rfc3339(),
    }
}

fn redirect_spec_from_proto(
    spec: Option<app::RedirectOauthSpec>,
) -> Result<RedirectOauthSpec, &'static str> {
    let spec = spec.ok_or("spec is required")?;
    let metadata = spec.metadata.unwrap_or_default();
    Ok(RedirectOauthSpec {
        authorize_url: spec.authorize_url,
        token_url: spec.token_url,
        scopes: spec.scopes,
        scope_delimiter: spec.scope_delimiter,
        extra_authorize_params: spec.extra_authorize_params.into_iter().collect(),
        client_id_ref: spec.client_id_ref,
        client_secret_ref: spec.client_secret_ref,
        pkce: spec.pkce,
        metadata: RedirectMetadataSpec {
            from_token_response: metadata.from_token_response.into_iter().collect(),
            probe: metadata.probe.map(|probe| RedirectMetadataProbe {
                method: probe.method,
                host: probe.host,
                path: probe.path,
                body: probe.body,
                map: probe.map.into_iter().collect(),
            }),
        },
    })
}

fn oauth_status(error: OAuthServiceError) -> Status {
    let code = match &error {
        OAuthServiceError::BadRequest(_) | OAuthServiceError::InvalidBundle => {
            Code::InvalidArgument
        }
        OAuthServiceError::NotFound => Code::NotFound,
        OAuthServiceError::Busy => Code::ResourceExhausted,
        OAuthServiceError::OwnerLost | OAuthServiceError::Disconnected => Code::Unavailable,
        OAuthServiceError::Conflict | OAuthServiceError::AccountChanged => Code::FailedPrecondition,
        OAuthServiceError::Driver(_) => Code::Unavailable,
        OAuthServiceError::Crypto | OAuthServiceError::Meta(_) => Code::Internal,
    };
    let detail = match &error {
        OAuthServiceError::BadRequest(message) => message.as_str(),
        OAuthServiceError::NotFound => "OAuth resource not found",
        OAuthServiceError::Busy => "too many OAuth flows are active",
        OAuthServiceError::OwnerLost => "OAuth flow owner was lost; retry connection",
        OAuthServiceError::Disconnected => "OAuth credential is disconnected",
        OAuthServiceError::Conflict => "OAuth credential version changed",
        OAuthServiceError::AccountChanged => "OAuth account identity changed",
        OAuthServiceError::InvalidBundle => "OAuth credential bundle was rejected",
        OAuthServiceError::Crypto => "OAuth credential encryption failed",
        OAuthServiceError::Driver(driver) => driver.detail.as_str(),
        OAuthServiceError::Meta(_) => "OAuth metadata operation failed",
    };
    let slug = error.code();
    let mut status = Status::new(code, detail);
    if let Ok(value) = slug.parse() {
        status.metadata_mut().insert("engram-error-slug", value);
    }
    status
}

#[tonic::async_trait]
impl app::o_auth_credential_service_server::OAuthCredentialService for AppOAuthCredentialService {
    async fn begin_flow(
        &self,
        req: Request<app::BeginFlowRequest>,
    ) -> Result<Response<app::BeginFlowResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let begun = self
            .state
            .oauth
            .begin(key(req.subject, req.provider).map_err(Status::invalid_argument)?)
            .await
            .map_err(oauth_status)?;
        Ok(Response::new(app::BeginFlowResponse {
            flow: Some(flow_to_proto(begun.flow)),
            verification_url: begun.verification_url,
            user_code: begun.user_code,
        }))
    }

    async fn get_flow(
        &self,
        req: Request<app::GetFlowRequest>,
    ) -> Result<Response<app::GetFlowResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let (kind, id) = subject_from_proto(req.subject).map_err(Status::invalid_argument)?;
        let flow_id = req
            .flow_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed flow id"))?;
        let flow = self
            .state
            .oauth
            .get_flow(
                &OAuthCredentialKey {
                    subject_kind: kind,
                    subject_id: id,
                    provider: String::new(),
                },
                flow_id,
            )
            .await
            .map_err(oauth_status)?;
        Ok(Response::new(app::GetFlowResponse {
            flow: Some(flow_to_proto(flow)),
        }))
    }

    async fn cancel_flow(
        &self,
        req: Request<app::CancelFlowRequest>,
    ) -> Result<Response<app::CancelFlowResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let (kind, id) = subject_from_proto(req.subject).map_err(Status::invalid_argument)?;
        let flow_id = req
            .flow_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed flow id"))?;
        let flow = self
            .state
            .oauth
            .cancel(
                &OAuthCredentialKey {
                    subject_kind: kind,
                    subject_id: id,
                    provider: String::new(),
                },
                flow_id,
            )
            .await
            .map_err(oauth_status)?;
        Ok(Response::new(app::CancelFlowResponse {
            flow: Some(flow_to_proto(flow)),
        }))
    }

    async fn begin_redirect_flow(
        &self,
        req: Request<app::BeginRedirectFlowRequest>,
    ) -> Result<Response<app::BeginRedirectFlowResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let spec = redirect_spec_from_proto(req.spec).map_err(Status::invalid_argument)?;
        let begun = self
            .state
            .oauth
            .begin_redirect(
                key(req.subject, req.provider).map_err(Status::invalid_argument)?,
                &spec,
                &req.redirect_uri,
            )
            .await
            .map_err(oauth_status)?;
        Ok(Response::new(app::BeginRedirectFlowResponse {
            flow: Some(flow_to_proto(begun.flow)),
            authorize_url: begun.authorize_url,
        }))
    }

    async fn complete_redirect_flow(
        &self,
        req: Request<app::CompleteRedirectFlowRequest>,
    ) -> Result<Response<app::CompleteRedirectFlowResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let spec = redirect_spec_from_proto(req.spec).map_err(Status::invalid_argument)?;
        let flow_id = req
            .flow_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed flow id"))?;
        let row = self
            .state
            .oauth
            .complete_redirect(
                key(req.subject, req.provider).map_err(Status::invalid_argument)?,
                flow_id,
                &req.code,
                &req.redirect_uri,
                &spec,
            )
            .await
            .map_err(oauth_status)?;
        Ok(Response::new(app::CompleteRedirectFlowResponse {
            credential: Some(credential_to_proto(
                row,
                self.state.services.clock.now_utc(),
            )),
        }))
    }

    async fn list_credentials(
        &self,
        req: Request<app::ListCredentialsRequest>,
    ) -> Result<Response<app::ListCredentialsResponse>, Status> {
        self.auth.check(&req)?;
        // An EMPTY subject id lists every credential of the kind (the
        // connector status surface); a set id scopes to one subject.
        let subject = req
            .into_inner()
            .subject
            .ok_or_else(|| Status::invalid_argument("subject is required"))?;
        let kind = match app::OauthSubjectKind::try_from(subject.kind) {
            Ok(app::OauthSubjectKind::User) => OAuthSubjectKind::User,
            Ok(app::OauthSubjectKind::Connector) => OAuthSubjectKind::Connector,
            Ok(app::OauthSubjectKind::Mcp) => OAuthSubjectKind::Mcp,
            _ => return Err(Status::invalid_argument("subject kind is required")),
        };
        let id = subject.id.trim();
        let credentials = self
            .state
            .oauth
            .list(kind, (!id.is_empty()).then_some(id))
            .await
            .map_err(oauth_status)?;
        let now = self.state.services.clock.now_utc();
        Ok(Response::new(app::ListCredentialsResponse {
            credentials: credentials
                .into_iter()
                .map(|row| credential_to_proto(row, now))
                .collect(),
        }))
    }

    async fn disconnect(
        &self,
        req: Request<app::DisconnectRequest>,
    ) -> Result<Response<app::DisconnectResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let row = self
            .state
            .oauth
            .disconnect(
                &key(req.subject, req.provider).map_err(Status::invalid_argument)?,
                req.expected_version,
            )
            .await
            .map_err(oauth_status)?;
        Ok(Response::new(app::DisconnectResponse {
            credential: Some(credential_to_proto(
                row,
                self.state.services.clock.now_utc(),
            )),
        }))
    }

    async fn fetch_session_credential(
        &self,
        req: Request<app::FetchSessionCredentialRequest>,
    ) -> Result<Response<app::FetchSessionCredentialResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let session_id = req
            .session_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed session id"))?;
        if !crate::api::session_auth::authorize_broker_token(
            &self.state,
            session_id,
            &req.broker_token,
        )
        .await
        {
            return Err(Status::permission_denied("invalid session credential"));
        }
        let (provider, version, opaque_bundle) = self
            .state
            .oauth
            .fetch_session(session_id)
            .await
            .map_err(oauth_status)?;
        Ok(Response::new(app::FetchSessionCredentialResponse {
            provider,
            version,
            opaque_bundle,
        }))
    }

    async fn update_session_credential(
        &self,
        req: Request<app::UpdateSessionCredentialRequest>,
    ) -> Result<Response<app::UpdateSessionCredentialResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        if req.opaque_bundle.len() > MAX_OAUTH_BUNDLE_BYTES {
            return Err(Status::resource_exhausted(
                "OAuth credential bundle exceeds limit",
            ));
        }
        let session_id = req
            .session_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed session id"))?;
        if !crate::api::session_auth::authorize_broker_token(
            &self.state,
            session_id,
            &req.broker_token,
        )
        .await
        {
            return Err(Status::permission_denied("invalid session credential"));
        }
        let (provider, version, opaque_bundle) = self
            .state
            .oauth
            .update_session(session_id, req.expected_version, &req.opaque_bundle)
            .await
            .map_err(oauth_status)?;
        Ok(Response::new(app::UpdateSessionCredentialResponse {
            provider,
            version,
            opaque_bundle,
        }))
    }
}
