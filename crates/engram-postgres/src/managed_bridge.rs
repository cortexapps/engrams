//! Held-input and intent CAS. Gate then session locks; no external I/O in a transaction.
use crate::{db_err, managed_create, PostgresStore};
use engram_core::{
    types::{managed_boot::ManagedBootRecord, managed_bridge::*, session::SessionState},
    MetaError, SessionId,
};
use sqlx::{PgConnection, Row};
fn decode<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, MetaError> {
    serde_json::from_value(v).map_err(|_| conflict())
}
async fn gate(c: &mut PgConnection) -> Result<(), MetaError> {
    let row=sqlx::query("SELECT admission_open,published_generation,published_identity FROM managed_operator_generation_gate WHERE singleton FOR SHARE").fetch_one(c).await.map_err(db_err)?;
    if !row.try_get::<bool, _>("admission_open").map_err(db_err)?
        || row
            .try_get::<Option<i64>, _>("published_generation")
            .map_err(db_err)?
            .is_none()
        || row
            .try_get::<Option<String>, _>("published_identity")
            .map_err(db_err)?
            .is_none()
    {
        return Err(conflict());
    }
    Ok(())
}
async fn lock(c: &mut PgConnection, sid: SessionId) -> Result<(), MetaError> {
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE")
        .bind(sid.as_uuid())
        .fetch_optional(c)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
    Ok(())
}
async fn slot_need(c: &mut PgConnection, sid: SessionId) -> Result<u32, MetaError> {
    let need: i64 = sqlx::query_scalar("SELECT nbd_slot_need FROM sessions WHERE id=$1")
        .bind(sid.as_uuid())
        .fetch_optional(c)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
    u32::try_from(need).map_err(|_| conflict())
}
async fn selection(
    c: &mut PgConnection,
    q: &ManagedCreationQuery,
) -> Result<ManagedBootSelection, MetaError> {
    q.matches(&managed_create::read_with_terminal(c, &q.identity, false, true).await?)?;
    let v = sqlx::query_scalar(
        "SELECT selection FROM session_managed_boot_selections WHERE session_id=$1",
    )
    .bind(q.identity.session_id.as_uuid())
    .fetch_optional(c)
    .await
    .map_err(db_err)?
    .ok_or_else(conflict)?;
    let s: ManagedBootSelection = decode(v)?;
    s.validate()?;
    Ok(s)
}
async fn intent(
    c: &mut PgConnection,
    q: &ManagedCreationQuery,
    op: &str,
) -> Result<Option<ManagedBootIntent>, MetaError> {
    q.matches(&managed_create::read_with_terminal(c, &q.identity, false, true).await?)?;
    let v: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT record FROM session_managed_boot_intents WHERE session_id=$1 AND operation_key=$2",
    )
    .bind(q.identity.session_id.as_uuid())
    .bind(op)
    .fetch_optional(c)
    .await
    .map_err(db_err)?;
    v.map(decode).transpose()
}
impl PostgresStore {
    pub(crate) async fn bridge_state(
        &self,
        q: &ManagedCreationQuery,
    ) -> Result<ManagedCreationState, MetaError> {
        q.validate()?;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM sessions WHERE id=$1 FOR SHARE")
                .bind(q.identity.session_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM session_managed_creations WHERE session_id=$1)",
        )
        .bind(q.identity.session_id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let creation = if exists {
            Some(managed_create::read_with_terminal(&mut tx, &q.identity, false, true).await?)
        } else {
            None
        };
        let b: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE session_id=$1")
                .bind(q.identity.session_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        let i:Option<serde_json::Value>=sqlx::query_scalar("SELECT record FROM session_managed_boot_intents WHERE session_id=$1 ORDER BY binding_epoch DESC LIMIT 1").bind(q.identity.session_id.as_uuid()).fetch_optional(&mut *tx).await.map_err(db_err)?;
        let i: Option<ManagedBootIntent> = i.map(decode).transpose()?;
        let status = status
            .map(|s| decode::<SessionState>(s.into()))
            .transpose()?;
        let b: Option<ManagedBootRecord> = b.map(decode).transpose()?;
        if status.is_some_and(|s| !s.is_terminal()) {
            if let Some(b) = &b {
                crate::managed_boot::read(&mut tx, &b.request.key).await?;
            }
        }
        let state =
            ManagedCreationState::from_parts(q.clone(), status, creation.as_ref(), b, i.as_ref())?;
        tx.commit().await.map_err(db_err)?;
        Ok(state)
    }
    pub(crate) async fn bridge_selection(
        &self,
        q: &ManagedCreationQuery,
    ) -> Result<ManagedBootSelection, MetaError> {
        selection(&mut *self.pool.acquire().await.map_err(db_err)?, q).await
    }
    pub(crate) async fn bridge_slot_need(
        &self,
        q: &ManagedCreationQuery,
    ) -> Result<u32, MetaError> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        q.matches(&managed_create::read(&mut conn, &q.identity, false).await?)?;
        slot_need(&mut conn, q.identity.session_id).await
    }
    pub(crate) async fn bridge_intent(
        &self,
        q: &ManagedCreationQuery,
        op: &str,
    ) -> Result<Option<ManagedBootIntent>, MetaError> {
        intent(&mut *self.pool.acquire().await.map_err(db_err)?, q, op).await
    }
    pub(crate) async fn bridge_begin(
        &self,
        i: ManagedBootIntent,
    ) -> Result<ManagedBootIntent, MetaError> {
        i.validate()?;
        if i.revision != 1 || i.host_id.is_some() || i.assets.is_some() || i.completed {
            return Err(conflict());
        }
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        gate(&mut tx).await?;
        lock(&mut tx, i.query.identity.session_id).await?;
        let original = selection(&mut tx, &i.query).await?;
        if let Some(old) = intent(&mut tx, &i.query, &i.operation_key()).await? {
            if old.recovery.as_ref().map(|r| (&r.recovery_id, &r.source))
                != i.recovery.as_ref().map(|r| (&r.recovery_id, &r.source))
            {
                return Err(conflict());
            }
            tx.commit().await.map_err(db_err)?;
            return Ok(old);
        }
        managed_create::read(&mut tx, &i.query.identity, false).await?;
        let collision: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM session_managed_boot_intents WHERE intent_id=$1 OR (session_id=$2 AND (binding_epoch=$3 OR NOT completed)))")
            .bind(&i.intent_id).bind(i.query.identity.session_id.as_uuid()).bind(i.binding_epoch as i64)
            .fetch_one(&mut *tx).await.map_err(db_err)?;
        if collision {
            return Err(conflict());
        }
        let b: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE session_id=$1")
                .bind(i.query.identity.session_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        let b: Option<ManagedBootRecord> = b.map(decode).transpose()?;
        if let Some(r) = &i.recovery {
            let old = b.as_ref().ok_or_else(conflict)?;
            if old.request.key != r.source
                || old.prepared.is_none()
                || i.selection.image != old.request.cold.image
                || i.selection.aux_ro_drives != old.request.cold.aux_ro_drives
                || i.selection.launch != original.launch
                || i.selection.image_digest != original.image_digest
                || i.selection.base_manifest != original.base_manifest
                || i.selection.vcpus != old.request.spec()?.cpu.vcpus
                || i.selection.memory_mib != old.request.spec()?.memory.max_mib
                || i.selection.disk_gib != old.request.spec()?.disk.max_gib
            {
                return Err(conflict());
            }
        } else {
            managed_create::read(&mut tx, &i.query.identity, true).await?;
            if b.is_some() || i.selection != original {
                return Err(conflict());
            }
        }
        sqlx::query("INSERT INTO session_managed_boot_intents(intent_id,session_id,operation_key,binding_epoch,revision,record,created_at,updated_at) VALUES($1,$2,$3,$4,1,$5,$6,$6)").bind(&i.intent_id).bind(i.query.identity.session_id.as_uuid()).bind(i.operation_key()).bind(i.binding_epoch as i64).bind(serde_json::to_value(&i).map_err(|_|conflict())?).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation=generation+1 WHERE id=TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(i)
    }
    pub(crate) async fn bridge_advance(
        &self,
        id: &str,
        revision: u64,
        c: ManagedIntentAdvance,
    ) -> Result<ManagedBootIntent, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        gate(&mut tx).await?;
        let sid: uuid::Uuid = sqlx::query_scalar(
            "SELECT session_id FROM session_managed_boot_intents WHERE intent_id=$1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(conflict)?;
        lock(&mut tx, sid.into()).await?;
        let v = sqlx::query_scalar(
            "SELECT record FROM session_managed_boot_intents WHERE intent_id=$1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let old: ManagedBootIntent = decode(v)?;
        old.query
            .matches(&managed_create::read(&mut tx, &old.query.identity, false).await?)?;
        if old.revision != revision {
            return Err(conflict());
        }
        if matches!(c, ManagedIntentAdvance::Complete) {
            let v =
                sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE session_id=$1")
                    .bind(sid)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?
                    .ok_or_else(conflict)?;
            let b: ManagedBootRecord = decode(v)?;
            old.validate_claim(&b.request)?;
        }
        if !matches!(c, ManagedIntentAdvance::Complete) {
            let v: Option<serde_json::Value> =
                sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE session_id=$1")
                    .bind(sid)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?;
            let boot: Option<ManagedBootRecord> = v.map(decode).transpose()?;
            if boot.as_ref().map(|b| &b.request.key) != old.recovery.as_ref().map(|r| &r.source) {
                return Err(conflict());
            }
        }
        if let ManagedIntentAdvance::SelectHost(host) = &c {
            let need = slot_need(&mut tx, old.query.identity.session_id).await?;
            if crate::pick_host_2d(
                &mut tx,
                &[host.as_uuid()],
                0,
                i64::from(old.selection.memory_mib),
                i64::from(old.selection.vcpus),
                need,
            )
            .await?
                != Some(host.as_uuid())
            {
                return Err(conflict());
            }
        }
        let next = old.advance(c)?;
        sqlx::query("UPDATE session_managed_boot_intents SET revision=$2,completed=$3,record=$4,updated_at=$5 WHERE intent_id=$1").bind(id).bind(next.revision as i64).bind(next.completed).bind(serde_json::to_value(&next).map_err(|_|conflict())?).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(next)
    }
    pub(crate) async fn bridge_list(
        &self,
        after: Option<SessionId>,
        limit: u32,
    ) -> Result<Vec<ManagedBootIntent>, MetaError> {
        let rows:Vec<serde_json::Value>=sqlx::query_scalar("SELECT i.record FROM session_managed_boot_intents i JOIN sessions s ON s.id=i.session_id WHERE NOT i.completed AND s.status NOT IN ('dead','failed','completed') AND ($1::UUID IS NULL OR i.session_id>$1) ORDER BY i.session_id LIMIT $2").bind(after.map(|x|x.as_uuid())).bind(i64::from(limit.min(128))).fetch_all(&self.pool).await.map_err(db_err)?;
        rows.into_iter().map(decode).collect()
    }
}
