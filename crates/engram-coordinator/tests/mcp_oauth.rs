use engram_coordinator::{
    mcp::{model::*, oauth::*, worker::*, *},
    oauth::OAuthManager,
};
use engram_core::{
    traits::{Clock, StaticSecretStore},
    types::oauth::*,
};
use engram_crypto::EnvVarKeyProvider;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
};
struct Security(AtomicU8);
impl SecurityEntropy for Security {
    fn bytes(&self) -> Result<[u8; 32]> {
        Ok([self.0.fetch_add(1, Ordering::SeqCst); 32])
    }
}
struct Provider {
    identity: WorkerIdentity,
    registration: ClientRegistration,
    grants: Mutex<Vec<Value>>,
    calls: Mutex<Vec<Value>>,
}
#[async_trait::async_trait]
impl WorkerTransport for Provider {
    async fn identity(&self) -> Result<WorkerIdentity> {
        Ok(self.identity.clone())
    }
    async fn rpc(
        &self,
        _: &WorkerIdentity,
        intent: &WorkerIntent,
        op: &WorkerOperation,
    ) -> Result<Value> {
        let value = serde_json::to_value(op).unwrap();
        assert_eq!(intent.operation_digest, digest(&value).unwrap());
        self.calls.lock().unwrap().push(value);
        match op {
            WorkerOperation::Metadata { endpoint }
                if endpoint == &self.registration.resource_metadata.endpoint =>
            {
                Ok(
                    json!({"resource":self.registration.resource.endpoint,"authorization_servers":[self.registration.issuer]}),
                )
            }
            WorkerOperation::Metadata { .. } => Ok(
                json!({"issuer":self.registration.issuer,"authorization_endpoint":self.registration.authorization.endpoint,"token_endpoint":self.registration.token.endpoint,"code_challenge_methods_supported":["S256"],"authorization_response_iss_parameter_supported":true}),
            ),
            WorkerOperation::ValidateDestination { .. } => Ok(json!({"valid":true})),
            WorkerOperation::Token { .. } => {
                self.grants.lock().unwrap().pop().ok_or(McpError::Transport)
            }
        }
    }
}
struct Fixture {
    authority: McpAuthority,
    replica: McpAuthority,
    clock: Arc<ManualClock>,
    meta: Arc<SimMetadataStore>,
    provider: Arc<Provider>,
    change: ChangeContext,
}
fn fixture(grants: Vec<Value>) -> Fixture {
    fixture_with_client(grants, false)
}
fn fixture_with_client(grants: Vec<Value>, confidential: bool) -> Fixture {
    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(9));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let manager = OAuthManager::new(
        meta.clone(),
        Arc::new(EnvVarKeyProvider::from_bytes([8; 32], "test")),
        clock.clone(),
        entropy,
        Arc::new(StaticSecretStore::new(HashMap::new())),
    );
    let d = |path: &str| Destination {
        zone_id: "public".into(),
        endpoint: format!("https://auth.example/{path}"),
    };
    let registration = ClientRegistration {
        id: "registered-a".into(),
        client_id: "public-client".into(),
        confidential,
        redirect_uri: "https://app.example/callback".into(),
        issuer: "https://auth.example/".into(),
        resource: d("mcp"),
        resource_metadata: d("resource-metadata"),
        authorization_metadata: d("metadata"),
        authorization: d("authorize"),
        token: d("token"),
        scopes: BTreeSet::from(["tools.read".into()]),
        issuer_parameter_required: false,
        account_field: Some("account".into()),
    };
    let identity = WorkerIdentity {
        protocol: 1,
        boot_id: "boot-a".into(),
        zone_id: "public".into(),
        policy_digest: "a".repeat(64),
        policy_revision: "1".into(),
    };
    let endpoints = [
        &registration.resource,
        &registration.resource_metadata,
        &registration.authorization_metadata,
        &registration.authorization,
        &registration.token,
    ]
    .into_iter()
    .map(|d| d.endpoint.clone())
    .collect();
    let registry = Registry {
        enabled: true,
        deployment_id: "deployment-a".into(),
        zones: BTreeMap::from([(
            "public".into(),
            Zone {
                policy_digest: identity.policy_digest.clone(),
                policy_revision: "1".into(),
                endpoints,
            },
        )]),
        registrations: BTreeMap::from([(registration.id.clone(), registration.clone())]),
    };
    let provider = Arc::new(Provider {
        identity,
        registration: registration.clone(),
        grants: Mutex::new(grants),
        calls: Mutex::new(vec![]),
    });
    let security = Arc::new(Security(AtomicU8::new(1)));
    let workers = BTreeMap::from([(
        "public".into(),
        provider.clone() as Arc<dyn WorkerTransport>,
    )]);
    let authority = McpAuthority::new(
        manager.clone(),
        security.clone(),
        registry.clone(),
        workers.clone(),
    )
    .unwrap();
    let replica = McpAuthority::new(manager, security, registry, workers).unwrap();
    Fixture {
        authority,
        replica,
        clock,
        meta,
        provider,
        change: ChangeContext {
            connection_id: "connection-a".into(),
            config_revision: 1,
            previous_authorization_generation: 0,
            resource: registration.resource,
        },
    }
}
fn grant(token: &str) -> Value {
    json!({"access_token":token,"token_type":"Bearer","expires_in":60,"scope":"tools.read","refresh_token":"rotating-refresh"})
}
async fn begin(f: &Fixture) -> BegunAuthorization {
    f.authority
        .begin(&f.change, "registered-a", None, None, "actor-a", true)
        .await
        .unwrap()
}
fn callback(b: &BegunAuthorization) -> Callback {
    let u = url::Url::parse(&b.authorization_url).unwrap();
    Callback {
        context: b.context.clone(),
        registration_id: "registered-a".into(),
        flow_id: b.flow_id,
        state: u
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .into_owned(),
        redirect_uri: "https://app.example/callback".into(),
        issuer: Some("https://auth.example/".into()),
        code: "authorization-code".into(),
        actor_id: "actor-a".into(),
    }
}
fn lease(f: &Fixture, status: &CredentialStatus) -> ResourceLease {
    let now = f.clock.now_utc().timestamp_millis();
    ResourceLease {
        context: status.context.clone(),
        reference: status.reference.clone(),
        worker: f.provider.identity.clone(),
        intent_id: "intent-a".into(),
        request_digest: "a".repeat(64),
        operation: "discover".into(),
        scope: WorkerScope::Administration {
            request_id: "request-a".into(),
            actor_id: "actor-a".into(),
        },
        expires_at: now + 10000,
        admission: WorkerLease {
            id: "lease-a".into(),
            deployment_id: "deployment-a".into(),
            expires_at: now + 10000,
        },
    }
}
#[tokio::test]
async fn opaque_authorization_consumes_before_exchange_and_refresh_preserves_identity() {
    let f = fixture(vec![grant("new-token"), grant("opaque-token")]);
    let begun = begin(&f).await;
    let cb = callback(&begun);
    let (a, b) = tokio::join!(f.authority.callback(&cb), f.replica.callback(&cb));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let status = a.or(b).unwrap();
    let first = lease(&f, &status);
    assert_eq!(
        f.authority.lease(&first).await.unwrap().value,
        "Bearer opaque-token"
    );
    f.clock.advance(std::time::Duration::from_secs(61));
    let next = lease(&f, &status);
    assert_eq!(
        f.replica.lease(&next).await.unwrap().value,
        "Bearer new-token"
    );
    assert_eq!(status.context, begun.context);
    let calls = f.provider.calls.lock().unwrap();
    let tokens: Vec<_> = calls.iter().filter(|v| v["kind"] == "token").collect();
    assert_eq!(tokens.len(), 2);
    assert!(tokens
        .iter()
        .all(|v| v["grant"].get("client_secret").is_none()));
    assert_eq!(tokens[0]["grant"]["resource"], "https://auth.example/mcp");
    assert_eq!(calls.len(), 5, "no user-info or JWT probe");
}
#[tokio::test]
async fn callback_rejects_state_issuer_redirect_expiry_and_scope() {
    for defect in ["state", "issuer", "redirect", "expiry", "scope", "type"] {
        let mut response = grant("opaque");
        if defect == "scope" {
            response["scope"] = json!("tools.write");
        }
        if defect == "type" {
            response.as_object_mut().unwrap().remove("token_type");
        }
        let f = fixture(vec![response]);
        let b = begin(&f).await;
        let mut cb = callback(&b);
        match defect {
            "state" => cb.state = "wrong".into(),
            "issuer" => cb.issuer = None,
            "redirect" => cb.redirect_uri = "https://app.example/other".into(),
            "expiry" => f.clock.advance(std::time::Duration::from_secs(600)),
            _ => {}
        }
        assert!(f.authority.callback(&cb).await.is_err(), "{defect}");
        use engram_core::traits::MetadataStore;
        assert!(f
            .meta
            .get_oauth_credential(&OAuthCredentialKey {
                subject_kind: OAuthSubjectKind::Mcp,
                subject_id: "connection-a".into(),
                provider: "registered-a".into()
            })
            .await
            .unwrap()
            .is_none());
    }
}
#[tokio::test]
async fn uncertain_refresh_cannot_reuse_rotated_token_after_claim_expiry() {
    let f = fixture(vec![grant("opaque")]);
    let b = begin(&f).await;
    let status = f.authority.callback(&callback(&b)).await.unwrap();
    f.clock.advance(std::time::Duration::from_secs(61));
    assert!(f.authority.lease(&lease(&f, &status)).await.is_err());
    f.clock.advance(std::time::Duration::from_secs(31));
    assert!(f.authority.lease(&lease(&f, &status)).await.is_err());
    assert_eq!(
        f.provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["kind"] == "token")
            .count(),
        2
    );
}

#[tokio::test]
async fn reconnect_fences_old_binding_before_publication_and_revoke_is_cas() {
    let f = fixture(vec![grant("replacement"), grant("original")]);
    let first = f
        .authority
        .callback(&callback(&begin(&f).await))
        .await
        .unwrap();
    let old_lease = lease(&f, &first);
    let mut change = f.change.clone();
    change.previous_authorization_generation = first.context.authorization_generation;
    let next = f
        .authority
        .begin(
            &change,
            "registered-a",
            None,
            Some(first.version),
            "actor-a",
            true,
        )
        .await
        .unwrap();
    let second = f.replica.callback(&callback(&next)).await.unwrap();
    assert_ne!(
        first.context.credential_identity,
        second.context.credential_identity
    );
    assert_eq!(
        second.context.authorization_generation,
        first.context.authorization_generation + 1
    );
    assert!(f.authority.lease(&old_lease).await.is_err());
    assert_eq!(
        f.authority.lease(&lease(&f, &second)).await.unwrap().value,
        "Bearer replacement"
    );
    assert!(f.authority.revoke(&first).await.is_err());
    assert_eq!(f.authority.revoke(&second).await.unwrap().status, "revoked");
    assert!(f.authority.lease(&lease(&f, &second)).await.is_err());
}
#[tokio::test]
async fn provider_account_change_and_scope_expansion_fail_refresh() {
    for field in ["account", "scope", "resource", "issuer"] {
        let mut first = grant("original");
        first["account"] = json!("account-a");
        let mut refresh = grant("replacement");
        refresh[field] = json!("changed");
        let f = fixture(vec![refresh, first]);
        let status = f
            .authority
            .callback(&callback(&begin(&f).await))
            .await
            .unwrap();
        f.clock.advance(std::time::Duration::from_secs(61));
        assert!(
            f.authority.lease(&lease(&f, &status)).await.is_err(),
            "{field}"
        );
    }
}
#[tokio::test]
async fn confidential_secret_rotation_fences_oauth_even_without_orchestrator_update() {
    use engram_core::types::org_secret::McpSecretRole;
    let f = fixture_with_client(vec![grant("token")], true);
    let secret = f
        .authority
        .put_secret(
            &f.change,
            McpSecretRole::ClientSecret,
            None,
            "authorization",
            "client-private",
        )
        .await
        .unwrap();
    let b = f
        .authority
        .begin(
            &f.change,
            "registered-a",
            Some(&secret.reference),
            None,
            "actor-a",
            true,
        )
        .await
        .unwrap();
    let status = f.authority.callback(&callback(&b)).await.unwrap();
    {
        let calls = f.provider.calls.lock().unwrap();
        let token = calls.iter().find(|v| v["kind"] == "token").unwrap();
        assert_eq!(token["endpoint"], "https://auth.example/token");
        assert_eq!(token["grant"]["client_secret"], "client-private");
    }
    let mut change = f.change.clone();
    change.previous_authorization_generation = secret.context.authorization_generation;
    f.authority
        .put_secret(
            &change,
            McpSecretRole::ClientSecret,
            Some(&secret.reference),
            "authorization",
            "rotated-private",
        )
        .await
        .unwrap();
    assert!(f.authority.lease(&lease(&f, &status)).await.is_err());
}
#[tokio::test]
async fn encrypted_flow_swap_cannot_exchange_or_publish() {
    let f = fixture(vec![grant("token")]);
    let first = begin(&f).await;
    let mut other = f.change.clone();
    other.connection_id = "connection-b".into();
    let second = f
        .authority
        .begin(&other, "registered-a", None, None, "actor-a", true)
        .await
        .unwrap();
    f.meta.with_db_mut(|db| {
        let a = db.mcp_flow_states.remove(&first.flow_id).unwrap();
        let b = db.mcp_flow_states.remove(&second.flow_id).unwrap();
        db.mcp_flow_states.insert(first.flow_id, b);
        db.mcp_flow_states.insert(second.flow_id, a);
    });
    assert!(f.replica.callback(&callback(&first)).await.is_err());
    assert!(f.replica.callback(&callback(&second)).await.is_err());
    assert!(f
        .provider
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|v| v["kind"] != "token"));
}

#[tokio::test]
async fn saturated_oauth_budget_does_not_claim_or_break_an_undispatched_refresh() {
    use engram_core::{traits::MetadataStore, types::mcp::McpOAuthAdmission};
    let f = fixture(vec![grant("new"), grant("old")]);
    let status = f
        .authority
        .callback(&callback(&begin(&f).await))
        .await
        .unwrap();
    f.clock.advance(std::time::Duration::from_secs(61));
    let admissions: Vec<_> = (100..104)
        .map(|n| McpOAuthAdmission {
            id: uuid::Uuid::from_u128(n),
            deployment_id: "deployment-a".into(),
            connection_id: format!("busy-{n}"),
            expires_at: f.clock.now_utc() + chrono::Duration::seconds(20),
        })
        .collect();
    for a in &admissions {
        assert!(f.meta.acquire_mcp_oauth_admission(a).await.unwrap());
    }
    assert!(f.authority.lease(&lease(&f, &status)).await.is_err());
    let row = f
        .meta
        .get_oauth_credential(&OAuthCredentialKey {
            subject_kind: OAuthSubjectKind::Mcp,
            subject_id: "connection-a".into(),
            provider: "registered-a".into(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(
        row.broken_at.is_none(),
        "admission denial must not consume the refresh claim"
    );
    f.meta
        .release_mcp_oauth_admission(&admissions[0])
        .await
        .unwrap();
    assert_eq!(
        f.authority.lease(&lease(&f, &status)).await.unwrap().value,
        "Bearer new"
    );
}

#[tokio::test]
async fn revoked_client_secret_cannot_start_a_new_authorization() {
    use engram_core::types::org_secret::McpSecretRole;
    let f = fixture_with_client(vec![grant("token")], true);
    let secret = f
        .authority
        .put_secret(
            &f.change,
            McpSecretRole::ClientSecret,
            None,
            "authorization",
            "client-private",
        )
        .await
        .unwrap();
    let revoked = f.authority.revoke(&secret).await.unwrap();
    let mut change = f.change.clone();
    change.previous_authorization_generation = revoked.context.authorization_generation;
    assert!(f
        .authority
        .begin(
            &change,
            "registered-a",
            Some(&revoked.reference),
            None,
            "actor-a",
            true
        )
        .await
        .is_err());
    assert!(f.provider.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn revoke_before_authority_switch_fences_old_leases_and_late_callbacks() {
    use engram_core::types::org_secret::McpSecretRole;
    let f = fixture(vec![
        grant("replacement"),
        grant("late"),
        grant("other"),
        grant("original"),
    ]);
    let original = f
        .authority
        .callback(&callback(&begin(&f).await))
        .await
        .unwrap();
    let original_lease = lease(&f, &original);
    let mut other_change = f.change.clone();
    other_change.connection_id = "connection-b".into();
    let other_flow = f
        .authority
        .begin(&other_change, "registered-a", None, None, "actor-a", true)
        .await
        .unwrap();
    let other = f.authority.callback(&callback(&other_flow)).await.unwrap();
    let mut change = f.change.clone();
    change.previous_authorization_generation = original.context.authorization_generation;
    let late = f
        .authority
        .begin(
            &change,
            "registered-a",
            None,
            Some(original.version),
            "actor-a",
            true,
        )
        .await
        .unwrap();
    let revoked = f.authority.revoke(&original).await.unwrap();
    assert_eq!(revoked.status, "revoked");
    assert_eq!(revoked.version, original.version + 1);
    let replacement = f
        .authority
        .put_secret(
            &change,
            McpSecretRole::ApiKey,
            None,
            "x-api-key",
            "replacement-key",
        )
        .await
        .unwrap();
    // No orchestrator publication occurs: the old frozen lease still fails.
    assert!(f.authority.lease(&original_lease).await.is_err());
    assert!(f.replica.callback(&callback(&late)).await.is_err());
    assert_eq!(
        f.authority
            .status("connection-a", &original.reference)
            .await
            .unwrap()
            .status,
        "revoked"
    );
    assert_eq!(
        f.authority.lease(&lease(&f, &other)).await.unwrap().value,
        "Bearer other"
    );
    let old_key_lease = lease(&f, &replacement);
    let key_receipt = f.authority.revoke(&replacement).await.unwrap();
    change.previous_authorization_generation = key_receipt.context.authorization_generation;
    let flow = f
        .authority
        .begin(
            &change,
            "registered-a",
            None,
            Some(revoked.version),
            "actor-a",
            true,
        )
        .await
        .unwrap();
    f.authority.callback(&callback(&flow)).await.unwrap();
    assert!(f.authority.lease(&old_key_lease).await.is_err());
}
