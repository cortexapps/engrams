use super::SimDb;
use engram_core::{types::managed_operator::*, MetaError, SessionId};
use std::collections::BTreeSet;

pub(super) fn ids(db: &SimDb) -> BTreeSet<SessionId> {
    db.managed_creations
        .keys()
        .chain(db.managed_boots.keys())
        .copied()
        .chain(
            db.session_integration_policy
                .iter()
                .filter_map(|(id, raw)| {
                    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
                    value
                        .get("managed_deny")
                        .filter(|v| !v.is_null())
                        .map(|_| *id)
                }),
        )
        .collect()
}
pub(super) fn snapshot(
    db: &SimDb,
    sid: SessionId,
    hosts: Vec<String>,
    first: bool,
) -> Result<(String, OperatorParticipant), MetaError> {
    let s = db.sessions.get(&sid).ok_or(MetaError::NotFound)?;
    let (raw, policy) = retain_generation(
        db.session_integration_policy.get(&sid).map(String::as_str),
        hosts,
        first,
    )?;
    Ok((
        raw,
        OperatorParticipant {
            session_id: sid,
            status: s.session.status.as_str().into(),
            host_id: s.session.host_id,
            sandbox_id: s.session.sandbox_id,
            binding_epoch: u64::try_from(s.binding_epoch).map_err(|_| conflict())?,
            held_creation: db.managed_creations.contains_key(&sid),
            boot: db
                .managed_boots
                .get(&sid)
                .map(|b| (b.request.clone(), b.revision, b.prepared.clone())),
            policy,
        },
    ))
}
