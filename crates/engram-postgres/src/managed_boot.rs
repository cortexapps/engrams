//! One session-row lock per transition. Host calls never occur in a transaction.
use crate::{db_err, managed_create, pick_host_2d, PostgresStore};
use engram_core::{
    types::{managed_boot::*, managed_cold::ManagedColdSource},
    MetaError, SessionId,
};
use sqlx::{PgConnection, Row};

pub(crate) async fn read(
    conn: &mut PgConnection,
    key: &ManagedBootKey,
) -> Result<ManagedBootRecord, MetaError> {
    key.validate()?;
    managed_create::read(conn, &key.creation, false).await?;
    let r = sqlx::query("SELECT b.record,s.sandbox_id AS current_sandbox FROM session_managed_boots b JOIN sessions s ON s.id=b.session_id WHERE b.session_id=$1 AND s.host_id=b.host_id AND (s.sandbox_id=b.sandbox_id OR s.sandbox_id IS NULL) AND s.binding_epoch=b.binding_epoch AND s.status NOT IN ('dead','failed','completed')")
        .bind(key.creation.session_id.as_uuid()).fetch_optional(&mut *conn).await.map_err(db_err)?.ok_or_else(conflict)?;
    let record: ManagedBootRecord =
        serde_json::from_value(r.try_get("record").map_err(db_err)?).map_err(|_| conflict())?;
    if &record.request.key != key {
        return Err(conflict());
    }
    if r.try_get::<Option<uuid::Uuid>, _>("current_sandbox")
        .map_err(db_err)?
        .is_none()
    {
        if record.phase != ManagedBootPhase::Interrupted {
            return Err(conflict());
        }
        record.retired.as_ref().ok_or_else(conflict)?.validate_for(
            record.prepared.as_ref().ok_or_else(conflict)?,
            &record.request.bootstrap_policy,
        )?;
    }
    Ok(record)
}
impl PostgresStore {
    pub(crate) async fn boot_claim(
        &self,
        request: ManagedBootPrepare,
    ) -> Result<ManagedBootRecord, MetaError> {
        let spec = request.spec()?;
        let k = &request.key;
        let t = &k.context.target;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        crate::managed_operator_gate::require(&mut tx, k).await?;
        let s = sqlx::query("SELECT status,host_id,sandbox_id,binding_epoch,image_uri,mem_budget_mib,cpu_budget_vcpus,nbd_slot_need FROM sessions WHERE id=$1 FOR UPDATE")
            .bind(t.session_id.as_uuid()).fetch_optional(&mut *tx).await.map_err(db_err)?.ok_or(MetaError::NotFound)?;
        if spec.image != s.try_get::<String, _>("image_uri").map_err(db_err)?
            || spec.cpu.vcpus == 0
            || i64::from(spec.cpu.vcpus)
                > i64::from(s.try_get::<i32, _>("cpu_budget_vcpus").map_err(db_err)?)
            || spec.memory.max_mib == 0
            || i64::from(spec.memory.max_mib)
                > s.try_get::<i64, _>("mem_budget_mib").map_err(db_err)?
        {
            return Err(conflict());
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM session_managed_boots WHERE session_id=$1)",
        )
        .bind(t.session_id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let intent: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT record FROM session_managed_boot_intents WHERE session_id=$1 AND intent_id=$2",
        )
        .bind(t.session_id.as_uuid())
        .bind(&k.boot_attempt)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if let Some(v) = intent {
            let i: engram_core::types::managed_bridge::ManagedBootIntent =
                serde_json::from_value(v).map_err(|_| conflict())?;
            i.validate_claim(&request)?;
        } else {
            let selected: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM session_managed_boot_selections WHERE session_id=$1)",
            )
            .bind(t.session_id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            if selected {
                return Err(conflict());
            }
        }
        let old = if exists {
            let value: serde_json::Value =
                sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE session_id=$1")
                    .bind(t.session_id.as_uuid())
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db_err)?;
            let old: ManagedBootRecord = serde_json::from_value(value).map_err(|_| conflict())?;
            let old = read(&mut tx, &old.request.key).await?;
            if old.request == request && old.phase != ManagedBootPhase::Interrupted {
                tx.commit().await.map_err(db_err)?;
                return Ok(old);
            }
            if old.phase != ManagedBootPhase::Interrupted {
                return Err(conflict());
            }
            let ManagedColdSource::Capture { capture_id, .. } = request.cold.disk.source else {
                return Err(conflict());
            };
            let value: Option<serde_json::Value>=sqlx::query_scalar("SELECT authority FROM session_managed_captures WHERE capture_id=$1 AND session_id=$2 AND binding_epoch=$3").bind(capture_id.as_uuid()).bind(t.session_id.as_uuid()).bind(old.request.key.context.target.binding_epoch as i64).fetch_optional(&mut *tx).await.map_err(db_err)?.flatten();
            let capture: ManagedCaptureAuthority =
                serde_json::from_value(value.ok_or_else(conflict)?).map_err(|_| conflict())?;
            capture.validate_recovery(&request)?;
            if request.capture_authority.as_ref() != Some(&capture) {
                return Err(MetaError::Conflict("capture authority mismatch".into()));
            }
            Some(old)
        } else {
            None
        };
        let source = managed_create::read(&mut tx, &k.creation, old.is_none()).await?;
        if source.native.digest != k.native_digest
            || (old.is_none()
                && (t.binding_epoch != 1
                    || !matches!(
                        request.cold.disk.source,
                        ManagedColdSource::FreshImage { .. }
                    )))
        {
            return Err(conflict());
        }
        let prior: Option<String> = sqlx::query_scalar(
            "SELECT policy_json FROM session_integration_policy WHERE session_id=$1",
        )
        .bind(t.session_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let retained = engram_core::types::managed_deny::retain_in_policy_json(
            prior.as_deref(),
            &request.bootstrap_policy,
        )
        .map_err(MetaError::Conflict)?;
        let picked = pick_host_2d(
            &mut tx,
            &[t.host_id.as_uuid()],
            0,
            s.try_get("mem_budget_mib").map_err(db_err)?,
            i64::from(s.try_get::<i32, _>("cpu_budget_vcpus").map_err(db_err)?),
            u32::try_from(s.try_get::<i64, _>("nbd_slot_need").map_err(db_err)?)
                .map_err(|_| conflict())?,
        )
        .await?;
        if picked != Some(t.host_id.as_uuid()) {
            return Err(conflict());
        }
        sqlx::query("UPDATE sessions SET status='created',host_id=$2,sandbox_id=$3,binding_epoch=$4,queued_at=NULL,queue_origin=NULL,updated_at=$5,last_active_at=$5 WHERE id=$1")
            .bind(t.session_id.as_uuid()).bind(t.host_id.as_uuid()).bind(t.sandbox_id.as_uuid()).bind(t.binding_epoch as i64).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation=generation+1 WHERE id=TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let record = ManagedBootRecord::claimed(request.clone());
        sqlx::query("INSERT INTO session_managed_boots(session_id,host_id,sandbox_id,binding_epoch,boot_attempt,revision,phase,record,updated_at) VALUES($1,$2,$3,$4,$5,1,'claimed',$6,$7) ON CONFLICT(session_id) DO UPDATE SET host_id=EXCLUDED.host_id,sandbox_id=EXCLUDED.sandbox_id,binding_epoch=EXCLUDED.binding_epoch,boot_attempt=EXCLUDED.boot_attempt,revision=1,phase='claimed',record=EXCLUDED.record,updated_at=EXCLUDED.updated_at")
            .bind(t.session_id.as_uuid()).bind(t.host_id.as_uuid()).bind(t.sandbox_id.as_uuid()).bind(t.binding_epoch as i64).bind(&k.boot_attempt).bind(serde_json::to_value(&record).map_err(|_| conflict())?).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
        sqlx::query("INSERT INTO session_integration_policy(session_id,policy_json) VALUES($1,$2) ON CONFLICT(session_id) DO UPDATE SET policy_json=EXCLUDED.policy_json").bind(t.session_id.as_uuid()).bind(retained).execute(&mut *tx).await.map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(record)
    }
    pub(crate) async fn boot_read(
        &self,
        key: &ManagedBootKey,
    ) -> Result<ManagedBootRecord, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE")
            .bind(key.creation.session_id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or(MetaError::NotFound)?;
        let record = read(&mut tx, key).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(record)
    }
    pub(crate) async fn boot_advance(
        &self,
        key: &ManagedBootKey,
        revision: u64,
        event: ManagedBootAdvance,
    ) -> Result<ManagedBootRecord, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        if matches!(
            &event,
            ManagedBootAdvance::Finalize { .. }
                | ManagedBootAdvance::IssueStart(_)
                | ManagedBootAdvance::Started(_)
        ) {
            crate::managed_operator_gate::require(&mut tx, key).await?;
        }
        let sid = key.creation.session_id;
        let policy = engram_core::types::mcp_policy::compose_policy(
            &crate::mcp_policy::nodes(&mut tx, sid).await?,
        )?;
        let old = read(&mut tx, key).await?;
        if old.revision != revision {
            return Err(conflict());
        }
        if let ManagedBootAdvance::Prepared(receipt) = &event {
            let v:Option<serde_json::Value>=sqlx::query_scalar("SELECT record FROM session_managed_boot_intents WHERE intent_id=$1 AND session_id=$2").bind(&key.boot_attempt).bind(sid.as_uuid()).fetch_optional(&mut *tx).await.map_err(db_err)?;
            if let Some(v) = v {
                let i: engram_core::types::managed_bridge::ManagedBootIntent =
                    serde_json::from_value(v).map_err(|_| conflict())?;
                i.validate_claim(&receipt.request)?;
                if i.assets.as_ref().map(|a| &a.host_incarnation) != Some(&receipt.host_incarnation)
                {
                    return Err(conflict());
                }
            }
        }
        let source = managed_create::read(&mut tx, &key.creation, false).await?;
        match &event {
            ManagedBootAdvance::Finalize { write_policy, .. } if write_policy != &policy => {
                return Err(conflict())
            }
            ManagedBootAdvance::IssueStart(_) | ManagedBootAdvance::Started(_)
                if old.write_policy.as_ref() != Some(&policy) =>
            {
                return Err(conflict())
            }
            _ => {}
        }
        let next = old.advance(event, &source.native)?;
        if let Some(retired) = &next.retired {
            let prior: Option<String> = sqlx::query_scalar(
                "SELECT policy_json FROM session_integration_policy WHERE session_id=$1",
            )
            .bind(sid.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            let retained = engram_core::types::managed_deny::retain_retired_in_policy_json(
                prior.as_deref(),
                &retired.retained_policy,
            )
            .map_err(MetaError::Conflict)?;
            sqlx::query("INSERT INTO session_integration_policy(session_id,policy_json) VALUES($1,$2) ON CONFLICT(session_id) DO UPDATE SET policy_json=EXCLUDED.policy_json").bind(sid.as_uuid()).bind(retained).execute(&mut *tx).await.map_err(db_err)?;
        }
        if next.phase == ManagedBootPhase::Interrupted && old.phase != ManagedBootPhase::Interrupted
        {
            sqlx::query("INSERT INTO session_managed_interrupted_targets(session_id,binding_epoch,boot_attempt,interrupted_at,event_floor) SELECT $1,$2,$3,$4,next_event_idx FROM sessions WHERE id=$1 ON CONFLICT DO NOTHING").bind(sid.as_uuid()).bind(key.context.target.binding_epoch as i64).bind(&key.boot_attempt).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
            sqlx::query("UPDATE session_outbox SET acked_at=$2 WHERE session_id=$1 AND kind='tool_result' AND acked_at IS NULL").bind(sid.as_uuid()).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
            let node = &crate::mcp_policy::nodes(&mut tx, sid).await?[0];
            let floor: i64 = sqlx::query_scalar("SELECT next_event_idx FROM sessions WHERE id=$1")
                .bind(sid.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
            let policy = engram_core::types::mcp_policy::StoredWritePolicy::restrict(
                node.policy.as_ref(),
                &format!("cold:{}", key.boot_attempt),
                floor,
            )?;
            sqlx::query("UPDATE sessions SET mcp_write_policy=$2 WHERE id=$1")
                .bind(sid.as_uuid())
                .bind(serde_json::to_value(policy).map_err(|_| conflict())?)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            let latest=sqlx::query("SELECT kind,payload FROM session_events WHERE session_id=$1 AND rewound_at IS NULL AND kind IN ('run_started','run_completed','run_interrupted') ORDER BY idx DESC LIMIT 1").bind(sid.as_uuid()).fetch_optional(&mut *tx).await.map_err(db_err)?;
            if let Some(event) = latest {
                let payload: serde_json::Value = event.try_get("payload").map_err(db_err)?;
                if event.try_get::<String, _>("kind").map_err(db_err)? == "run_started" {
                    if let Some(run) = payload.get("run_id").and_then(|v| v.as_str()) {
                        crate::append_event_idempotent_tx(
                            &mut tx, sid, &format!("managed-interrupt:{}:{run}", key.boot_attempt),
                            "run_interrupted",
                            serde_json::json!({"type":"harness_run_interrupted","run_id":run,"at":self.clock.now_utc()}),
                            self.clock.now_utc(),
                        ).await?;
                    }
                }
            }
        }
        if next.phase == ManagedBootPhase::Started {
            sqlx::query(
                "UPDATE sessions SET status='active',last_active_at=$2,updated_at=$2 WHERE id=$1",
            )
            .bind(sid.as_uuid())
            .bind(self.clock.now_utc())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        sqlx::query("UPDATE session_managed_boots SET revision=$2,phase=$3,record=$4,updated_at=$5 WHERE session_id=$1").bind(sid.as_uuid()).bind(i64::try_from(next.revision).map_err(|_|conflict())?).bind(next.phase.as_str()).bind(serde_json::to_value(&next).map_err(|_|conflict())?).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(next)
    }
    pub(crate) async fn boot_list(
        &self,
        after: Option<SessionId>,
        limit: u32,
    ) -> Result<Vec<ManagedBootKey>, MetaError> {
        let rows: Vec<serde_json::Value> = sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE phase IN ('claimed','prepared','finalizing','finalized','start_issued','started') AND ($2::UUID IS NULL OR session_id > $2) ORDER BY session_id LIMIT $1").bind(i64::from(limit.min(128))).bind(after.map(|id|id.as_uuid())).fetch_all(&self.pool).await.map_err(db_err)?;
        rows.into_iter()
            .map(|v| {
                serde_json::from_value::<ManagedBootRecord>(v)
                    .map(|r| r.request.key)
                    .map_err(|_| conflict())
            })
            .collect()
    }
    pub(crate) async fn boot_open(&self, sid: SessionId) -> Result<Option<bool>, MetaError> {
        sqlx::query_scalar("SELECT COALESCE(b.phase='started' AND EXISTS(SELECT 1 FROM managed_operator_generation_gate g WHERE g.singleton AND g.admission_open AND g.published_generation=(b.record->'request'->'key'->'context'->>'operator_generation')::BIGINT AND g.published_identity=b.record->'request'->'key'->'context'->>'operator_identity') AND s.status='active' AND s.host_id=b.host_id AND s.sandbox_id=b.sandbox_id AND s.binding_epoch=b.binding_epoch AND s.mcp_policy_parent_id IS NOT DISTINCT FROM c.parent_session_id,false) FROM session_managed_creations c JOIN sessions s ON s.id=c.session_id LEFT JOIN session_managed_boots b ON b.session_id=c.session_id WHERE c.session_id=$1").bind(sid.as_uuid()).fetch_optional(&self.pool).await.map_err(db_err)
    }
    pub(crate) async fn boot_target_gone(
        &self,
        sid: SessionId,
        epoch: u64,
    ) -> Result<bool, MetaError> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM session_managed_interrupted_targets WHERE session_id=$1 AND binding_epoch=$2)").bind(sid.as_uuid()).bind(i64::try_from(epoch).map_err(|_|conflict())?).fetch_one(&self.pool).await.map_err(db_err)
    }
}

impl PostgresStore {
    pub(crate) async fn boot_read_target(
        &self,
        target: &engram_core::types::managed_deny::ManagedPolicyTarget,
    ) -> Result<ManagedBootRecord, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let sandbox: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT sandbox_id FROM sessions WHERE id=$1 FOR SHARE")
                .bind(target.session_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?
                .ok_or(MetaError::NotFound)?;
        if sandbox != Some(target.sandbox_id.as_uuid()) {
            return Err(conflict());
        }
        let value: serde_json::Value =
            sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE session_id=$1")
                .bind(target.session_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?
                .ok_or_else(conflict)?;
        let row: ManagedBootRecord = serde_json::from_value(value).map_err(|_| conflict())?;
        if row.request.key.context.target != *target {
            return Err(conflict());
        }
        let result = read(&mut tx, &row.request.key).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(result)
    }
    pub(crate) async fn boot_get(
        &self,
        identity: &engram_core::types::managed_create::ManagedCreateIdentity,
    ) -> Result<Option<ManagedBootRecord>, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE")
            .bind(identity.session_id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or(MetaError::NotFound)?;
        managed_create::read(&mut tx, identity, false).await?;
        let value: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE session_id=$1")
                .bind(identity.session_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        let Some(value) = value else {
            return Ok(None);
        };
        let row: ManagedBootRecord = serde_json::from_value(value).map_err(|_| conflict())?;
        let result = read(&mut tx, &row.request.key).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(Some(result))
    }
    pub(crate) async fn capture_reserve(
        &self,
        request: &ManagedCaptureRequest,
    ) -> Result<Option<ManagedCaptureAuthority>, MetaError> {
        request.key.validate()?;
        if request.capture_id.as_uuid().is_nil() {
            return Err(conflict());
        }
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let sid = request.key.creation.session_id;
        sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE")
            .bind(sid.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or(MetaError::NotFound)?;
        if let Some(prior) = sqlx::query("SELECT request,authority FROM session_managed_captures WHERE capture_id=$1 AND authority IS NOT NULL")
            .bind(request.capture_id.as_uuid()).fetch_optional(&mut *tx).await.map_err(db_err)? {
            let original:ManagedCaptureRequest=serde_json::from_value(prior.try_get("request").map_err(db_err)?).map_err(|_|conflict())?;
            if &original != request { return Err(conflict()); }
            managed_create::read(&mut tx,&request.key.creation,false).await?;
            let authority=serde_json::from_value(prior.try_get("authority").map_err(db_err)?).map_err(|_|conflict())?;
            tx.commit().await.map_err(db_err)?; return Ok(Some(authority));
        }
        let boot = read(&mut tx, &request.key).await?;
        if boot.phase != ManagedBootPhase::Interrupted || boot.prepared.is_none() {
            return Err(conflict());
        }
        sqlx::query("INSERT INTO session_managed_captures(capture_id,session_id,binding_epoch,source_sandbox_id,request,created_at) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING").bind(request.capture_id.as_uuid()).bind(sid.as_uuid()).bind(request.key.context.target.binding_epoch as i64).bind(request.key.context.target.sandbox_id.as_uuid()).bind(serde_json::to_value(request).map_err(|_|conflict())?).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation=generation+1 WHERE id=TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let row = sqlx::query(
            "SELECT request,authority FROM session_managed_captures WHERE capture_id=$1",
        )
        .bind(request.capture_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(conflict)?;
        let original: ManagedCaptureRequest =
            serde_json::from_value(row.try_get("request").map_err(db_err)?)
                .map_err(|_| conflict())?;
        if original != *request {
            return Err(conflict());
        }
        let authority: Option<serde_json::Value> = row.try_get("authority").map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        authority
            .map(|v| serde_json::from_value(v).map_err(|_| conflict()))
            .transpose()
    }
    pub(crate) async fn capture_record(
        &self,
        authority: &ManagedCaptureAuthority,
    ) -> Result<(), MetaError> {
        authority.validate()?;
        let request = &authority.request;
        let sid = request.key.creation.session_id;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE")
            .bind(sid.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or(MetaError::NotFound)?;
        let boot = read(&mut tx, &request.key).await?;
        if boot.phase != ManagedBootPhase::Interrupted
            || boot.prepared.as_ref().map(|r| &r.host_incarnation)
                != Some(&authority.host_incarnation)
            || boot.request.cold.image != authority.image
            || boot.request.cold.kernel_sha256 != authority.kernel_sha256
            || boot.request.cold.aux_ro_drives != authority.aux_ro_drives
        {
            return Err(conflict());
        }
        let row = sqlx::query(
            "SELECT request,authority FROM session_managed_captures WHERE capture_id=$1 FOR UPDATE",
        )
        .bind(request.capture_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(conflict)?;
        let original: ManagedCaptureRequest =
            serde_json::from_value(row.try_get("request").map_err(db_err)?)
                .map_err(|_| conflict())?;
        let prior: Option<serde_json::Value> = row.try_get("authority").map_err(db_err)?;
        let value = serde_json::to_value(authority).map_err(|_| conflict())?;
        if original != *request || prior.as_ref().is_some_and(|p| p != &value) {
            return Err(conflict());
        }
        sqlx::query(
            "UPDATE session_managed_captures SET authority=$2,record_digest=$3 WHERE capture_id=$1",
        )
        .bind(request.capture_id.as_uuid())
        .bind(value)
        .bind(&authority.record_digest)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "UPDATE sessions SET status='idle',last_active_at=$2,updated_at=$2 WHERE id=$1",
        )
        .bind(sid.as_uuid())
        .bind(self.clock.now_utc())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }
    pub(crate) async fn boot_sandbox_open(
        &self,
        sandbox: engram_core::SandboxId,
    ) -> Result<Option<bool>, MetaError> {
        let sid: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT session_id FROM session_managed_boots WHERE sandbox_id=$1")
                .bind(sandbox.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(db_err)?;
        if let Some(sid) = sid {
            return self.boot_open(sid.into()).await;
        }
        let old: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM session_managed_captures WHERE source_sandbox_id=$1)",
        )
        .bind(sandbox.as_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(old.then_some(false))
    }
}

impl PostgresStore {
    pub(crate) async fn boot_tool_target_gone(
        &self,
        sid: SessionId,
        call: &str,
    ) -> Result<bool, MetaError> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM session_managed_creations WHERE session_id=$1) AND COALESCE((SELECT min(e.idx) FROM session_events e WHERE e.session_id=$1 AND e.kind='tool_call_requested' AND e.payload->>'tool_call_id'=$2),-1) < COALESCE((SELECT max(event_floor) FROM session_managed_interrupted_targets WHERE session_id=$1),0)").bind(sid.as_uuid()).bind(call).fetch_one(&self.pool).await.map_err(db_err)
    }
}

/// Called while holding the session lock, before any durable result write.
pub(crate) async fn require_tool_result_target(
    conn: &mut PgConnection,
    row: &engram_core::types::outbox::OutboxRow,
) -> Result<(), MetaError> {
    if row.kind != engram_core::types::outbox::OutboxKind::ToolResult {
        return Ok(());
    }
    let call = row
        .payload
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let gone:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM session_managed_creations WHERE session_id=$1) AND COALESCE((SELECT min(e.idx) FROM session_events e WHERE e.session_id=$1 AND e.kind='tool_call_requested' AND e.payload->>'tool_call_id'=$2),-1) < COALESCE((SELECT max(event_floor) FROM session_managed_interrupted_targets WHERE session_id=$1),0)").bind(row.session_id.as_uuid()).bind(call).fetch_one(conn).await.map_err(db_err)?;
    if gone {
        return Err(MetaError::Conflict("delivery target gone".into()));
    }
    Ok(())
}

impl PostgresStore {
    pub(crate) async fn managed_event_append(
        &self,
        event: &engram_core::types::event::ManagedHarnessEvent,
    ) -> Result<engram_core::types::event::ManagedHarnessEventAccepted, MetaError> {
        event.validate()?;
        let sid = event.target.session_id;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Read only to select the generation lock. Authority is checked again
        // after the session lock, which also serializes interruption and append.
        let raw: serde_json::Value =
            sqlx::query_scalar("SELECT record FROM session_managed_boots WHERE session_id=$1")
                .bind(sid.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?
                .ok_or_else(conflict)?;
        let candidate: ManagedBootRecord = serde_json::from_value(raw).map_err(|_| conflict())?;
        crate::managed_operator_gate::require(&mut tx, &candidate.request.key).await?;
        let active: bool =
            sqlx::query_scalar("SELECT status='active' FROM sessions WHERE id=$1 FOR UPDATE")
                .bind(sid.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?
                .ok_or_else(conflict)?;
        let current = read(&mut tx, &candidate.request.key).await?;
        let interrupted: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM session_managed_interrupted_targets WHERE session_id=$1 AND binding_epoch=$2)")
            .bind(sid.as_uuid()).bind(event.target.binding_epoch as i64).fetch_one(&mut *tx).await.map_err(db_err)?;
        if !active
            || interrupted
            || current.phase != ManagedBootPhase::Started
            || current.request.key.context.target != event.target
        {
            return Err(conflict());
        }
        let key = event.key();
        if let Some(previous) = sqlx::query("SELECT kind,payload FROM session_events WHERE session_id=$1 AND payload->>'idempotency_key'=$2")
            .bind(sid.as_uuid()).bind(&key).fetch_optional(&mut *tx).await.map_err(db_err)? {
            if !event.matches_replay(&previous.try_get::<String,_>("kind").map_err(db_err)?, &previous.try_get("payload").map_err(db_err)?) {
                return Err(conflict());
            }
        }
        let now = self.clock.now_utc();
        let idx = crate::append_event_idempotent_tx(
            &mut tx,
            sid,
            &key,
            &event.kind,
            event.payload.clone(),
            now,
        )
        .await?;
        let outbox_acked = if let Some(ack) = &event.ack_id {
            sqlx::query("UPDATE session_outbox SET acked_at=$3 WHERE session_id=$1 AND prompt_id=$2 AND acked_at IS NULL")
                .bind(sid.as_uuid()).bind(ack).bind(now).execute(&mut *tx).await.map_err(db_err)?.rows_affected() > 0
        } else {
            false
        };
        tx.commit().await.map_err(db_err)?;
        Ok(engram_core::types::event::ManagedHarnessEventAccepted { idx, outbox_acked })
    }
}
