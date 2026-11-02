//! Metadata shared with the trusted control caller. No credential values.
use super::worker::{WorkerIdentity, WorkerLease, WorkerScope};
use engram_core::types::org_secret::McpSecretReservation;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Destination {
    pub zone_id: String,
    pub endpoint: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeContext {
    pub connection_id: String,
    pub config_revision: i64,
    pub previous_authorization_generation: i64,
    pub resource: Destination,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorityContext {
    pub policy_digest: String,
    pub policy_revision: String,
    pub connection_id: String,
    pub config_revision: i64,
    pub authorization_generation: i64,
    pub credential_identity: String,
    pub resource: Destination,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialReference {
    ApiKey { reservation: McpSecretReservation },
    ClientSecret { reservation: McpSecretReservation },
    OAuth { registration_id: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialStatus {
    pub context: AuthorityContext,
    pub reference: CredentialReference,
    pub version: i64,
    pub status: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceLease {
    pub context: AuthorityContext,
    pub reference: CredentialReference,
    pub worker: WorkerIdentity,
    pub intent_id: String,
    pub request_digest: String,
    pub operation: String,
    pub scope: WorkerScope,
    /// Credential delivery deadline, at most ten seconds.
    pub expires_at: i64,
    /// Frozen worker operation deadline; admission must cover the full operation.
    pub operation_expires_at: i64,
    pub admission: WorkerLease,
}
// Raw values live only in an authenticated request or one operation's memory.
#[derive(Serialize, Deserialize)]
pub struct ProviderCredential {
    pub header: String,
    pub value: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Zone {
    pub policy_digest: String,
    pub policy_revision: String,
    pub endpoints: BTreeSet<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientRegistration {
    pub id: String,
    pub client_id: String,
    pub confidential: bool,
    pub redirect_uri: String,
    pub issuer: String,
    pub resource: Destination,
    pub resource_metadata: Destination,
    pub authorization_metadata: Destination,
    pub authorization: Destination,
    pub token: Destination,
    pub scopes: BTreeSet<String>,
    pub issuer_parameter_required: bool,
    pub account_field: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registry {
    #[serde(default)]
    pub enabled: bool,
    pub deployment_id: String,
    pub zones: BTreeMap<String, Zone>,
    pub registrations: BTreeMap<String, ClientRegistration>,
}
