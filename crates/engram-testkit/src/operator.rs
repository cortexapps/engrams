//! Reusable operator publication fixture. It uses the real store transitions.
use engram_core::{
    traits::MetadataStore,
    types::{managed_create::digest, managed_operator::*},
};
pub fn candidate() -> OperatorCandidate {
    let d = digest(b"configured operator fixture");
    OperatorCandidate { version:1,zones:[("zone-a".into(),OperatorZone {
        revision:"1".into(),worker_digest:d.clone(),authorities:vec!["tools.example".into()],aliases:Vec::new(),
        configuration:serde_json::json!({"id":"zone-a","revision":"1","mode":"public","authorities":["tools.example"],"aliases":[],"cidrs":[],"denyCidrs":[]}),
    })].into(),deployment_identity:d.clone(),network_identity:d.clone(),trust_identity:d.clone(),ca_content_digests:[("fixture-root".into(),d)].into() }
}
pub async fn publish_empty(meta: &dyn MetadataStore) {
    if meta
        .read_operator_publication()
        .await
        .unwrap()
        .published
        .is_some()
    {
        return;
    }
    let staged = meta
        .stage_operator_publication(None, candidate())
        .await
        .unwrap();
    meta.publish_operator_generation(&staged.pending.unwrap())
        .await
        .unwrap();
}
