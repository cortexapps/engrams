//! `SessionService` over gRPC (ADR 0039 §2.3). Each RPC is a thin
//! transport adapter: auth-check, decode the request, delegate to a
//! transport-agnostic core in `crate::api::*` (the SAME core the axum
//! handler calls), encode the response. The cores carry no principal and
//! no authz — the caller is the trusted orchestrator (ADR §6).

use std::sync::Arc;

use engram_protocol::app;
use futures::StreamExt as _;
use tonic::{Request, Response, Status};

use super::{auth, into_status, parse_session_id, BoxStream};
use crate::state::SharedState;

/// Chunk size for `GetArtifact` body frames — 64 KiB keeps messages well
/// under the default 4 MiB tonic limit while staying efficient.
const ARTIFACT_CHUNK_BYTES: usize = 64 * 1024;

pub struct AppSessionService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::session_service_server::SessionService for AppSessionService {
    async fn list_sessions(
        &self,
        req: Request<app::ListSessionsRequest>,
    ) -> Result<Response<app::ListSessionsResponse>, Status> {
        self.auth.check(&req)?;
        // Trusted caller: return ALL sessions, owner-annotated. The
        // orchestrator scopes by task ownership (ADR §6).
        let resp = crate::api::sessions::list_sessions_core(&self.state)
            .await
            .map_err(into_status)?;
        Ok(Response::new(super::convert::list_sessions_to_proto(resp)))
    }

    async fn create_session(
        &self,
        req: Request<app::CreateSessionRequest>,
    ) -> Result<Response<app::CreateSessionResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        // ADR 0051 Drip A: the orchestrator owns the user's harness auth token
        // and passes it as an opaque name→value map in `harness_env` (e.g.
        // CLAUDE_CODE_OAUTH_TOKEN). The coordinator is harness-agnostic: it does
        // not interpret these names. Extract them before moving r into the
        // converter (convert.rs binds `harness_env: _`). NEVER log these values.
        let identity_env: std::collections::HashMap<String, String> =
            r.harness_env.clone().into_iter().collect();
        let api_req = super::convert::create_request_from_proto(r).map_err(into_status)?;
        // ADR 0051: the gRPC create has no human principal — the orchestrator
        // owns attribution and passes the owner through (today `None`).
        // `identity_env` carries the orchestrator-resolved harness env; the core
        // injects it into the launch env AND persists it into session_secrets so
        // it is replayed on resume (closing the gap where an idle-evicted gRPC
        // session — user_id = NULL — would otherwise silently lose its token).
        let body =
            crate::api::sessions::create_session_core(&self.state, identity_env, api_req, None)
                .await
                .map_err(into_status)?;
        Ok(Response::new(app::CreateSessionResponse {
            session_id: body.session_id.to_string(),
            status: body.status.to_string(),
            image_version: body.image_version,
            kind: body.kind.to_string(),
        }))
    }

    async fn get_session(
        &self,
        req: Request<app::GetSessionRequest>,
    ) -> Result<Response<app::GetSessionResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        let session = crate::api::sessions::get_session_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::GetSessionResponse {
            session: Some(super::convert::session_to_proto(&session)),
        }))
    }

    async fn delete_session(
        &self,
        req: Request<app::DeleteSessionRequest>,
    ) -> Result<Response<app::DeleteSessionResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        crate::api::sessions::delete_session_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::DeleteSessionResponse {}))
    }

    async fn send_prompt(
        &self,
        req: Request<app::SendPromptRequest>,
    ) -> Result<Response<app::SendPromptResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;
        let note = crate::api::prompt::send_prompt_core(&self.state, id, r.prompt_id, r.text)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::SendPromptResponse {
            session_id: id.to_string(),
            note: note.to_string(),
        }))
    }

    async fn edit_queued_prompt(
        &self,
        req: Request<app::EditQueuedPromptRequest>,
    ) -> Result<Response<app::EditQueuedPromptResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;
        let note =
            crate::api::prompt::edit_queued_prompt_core(&self.state, id, r.prompt_id, r.text)
                .await
                .map_err(into_status)?;
        Ok(Response::new(app::EditQueuedPromptResponse {
            session_id: id.to_string(),
            note: note.to_string(),
        }))
    }

    async fn dequeue_queued_prompt(
        &self,
        req: Request<app::DequeueQueuedPromptRequest>,
    ) -> Result<Response<app::DequeueQueuedPromptResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;
        let note = crate::api::prompt::dequeue_queued_prompt_core(&self.state, id, r.prompt_id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::DequeueQueuedPromptResponse {
            session_id: id.to_string(),
            note: note.to_string(),
        }))
    }

    async fn answer_question(
        &self,
        req: Request<app::AnswerQuestionRequest>,
    ) -> Result<Response<app::AnswerQuestionResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;
        // ADR 0054: unwrap the proto StringList map back into the canonical
        // Answers (BTreeMap<String, Vec<String>>).
        let answers: engram_harness_proto::Answers = r
            .answers
            .into_iter()
            .map(|(question, list)| (question, list.values))
            .collect();
        let note =
            crate::api::prompt::answer_question_core(&self.state, id, r.tool_call_id, answers)
                .await
                .map_err(into_status)?;
        Ok(Response::new(app::AnswerQuestionResponse {
            session_id: id.to_string(),
            note: note.to_string(),
        }))
    }

    async fn interrupt(
        &self,
        req: Request<app::InterruptRequest>,
    ) -> Result<Response<app::InterruptResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        let note = crate::api::interrupt::interrupt_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::InterruptResponse {
            session_id: id.to_string(),
            note: note.to_string(),
        }))
    }

    type StreamEventsStream = BoxStream<app::SessionEvent>;

    // `clippy::result_large_err`: the `.map(Ok)` closure over the merged
    // stream must return `Result<_, tonic::Status>` — the unavoidable RPC
    // error type. Boxing here would just force an unbox at every poll site.
    #[allow(clippy::result_large_err)]
    async fn stream_events(
        &self,
        req: Request<app::StreamEventsRequest>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;

        // proto3: `since` unset = from the start. We pass None so
        // events_core treats it as the "all" sentinel (-1), resolved
        // internally by merged_event_stream.
        let since: Option<i64> = r.since;

        let (replayed, live_rx) = crate::api::events::events_core(&self.state, id, since)
            .await
            .map_err(into_status)?;

        // Thin map over the single shared merge stream. All
        // dedupe/high-water/lag semantics live in merged_event_stream +
        // merged_to_parts (api/events.rs). This handler only converts the
        // transport-agnostic parts into proto SessionEvent messages.
        let merged = crate::api::events::merged_event_stream(replayed, live_rx, since).map(|ev| {
            let (idx, kind, payload_json) = crate::api::events::merged_to_parts(ev);
            Ok(app::SessionEvent {
                idx,
                kind,
                payload_json,
            })
        });

        // RAII teardown guard: when the stream is dropped (client
        // disconnects or the sender closes), log at debug level. The
        // broadcast Receiver inside merged drops at the same time,
        // releasing the slot. The poll_fn arm IS reached on normal
        // teardown (BroadcastStream ends with None when the sender
        // closes); the log message reflects both causes.
        struct DisconnectGuard {
            session_id: engram_core::SessionId,
        }
        impl Drop for DisconnectGuard {
            fn drop(&mut self) {
                tracing::debug!(
                    session_id = %self.session_id,
                    "StreamEvents: stream ended (client disconnect or sender close)"
                );
            }
        }
        let guard = DisconnectGuard { session_id: id };

        let full_stream = merged.chain(futures::stream::poll_fn(move |_| {
            // Keep guard alive until the upstream stream exhausts.
            // Rust's drop-ordering requires the value to be moved into
            // the closure body.
            let _ = &guard;
            std::task::Poll::Ready(None)
        }));

        Ok(Response::new(Box::pin(full_stream)))
    }

    // ADR 0060: unary catch-up read of the persistent log for the reverse
    // channel. Thin adapter over `list_session_events_core`; maps each
    // persisted event through the SAME decoder the StreamEvents replay arm
    // uses (`merged_to_parts`), so the unary page is byte-identical to the
    // stream. Unfiltered — curation is the consumer's concern.
    async fn list_session_events(
        &self,
        req: Request<app::ListSessionEventsRequest>,
    ) -> Result<Response<app::ListSessionEventsResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;
        let (events, next_after_idx) =
            crate::api::events::list_session_events_core(&self.state, id, r.after_idx, r.limit)
                .await
                .map_err(into_status)?;
        let events = events
            .into_iter()
            .map(|ev| {
                let (idx, kind, payload_json) = crate::api::events::merged_to_parts(
                    crate::api::events::MergedEvent::Replay(ev),
                );
                app::SessionEvent {
                    idx,
                    kind,
                    payload_json,
                }
            })
            .collect();
        Ok(Response::new(app::ListSessionEventsResponse {
            events,
            next_after_idx,
        }))
    }

    type ExecStream = BoxStream<app::ExecOutput>;

    // ADR 0039 Task 12: streaming Exec over gRPC.
    //
    // Proto framing (session.proto ExecOutput oneof):
    //   first  → started { exec_id }
    //   middle → stdout bytes | stderr bytes
    //   last   → exit { exit_status, rusage }
    //
    // The two axum handlers (unary + SSE) remain unchanged; this shares
    // the new `exec_stream_core` that drives the same backend path.
    //
    // `clippy::result_large_err`: `tonic::Status` is the unavoidable RPC
    // error type here.
    #[allow(clippy::result_large_err)]
    async fn exec(
        &self,
        req: Request<app::ExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;

        // Map proto ExecRequest → api ExecRequest via the totality-enforced
        // converter in convert.rs (exhaustive destructure, same pattern as
        // all other converters in that module).
        let api_req = super::convert::exec_request_from_proto(r);

        let (exec_id, body_stream) = crate::api::exec::exec_stream_core(&self.state, id, api_req)
            .await
            .map_err(into_status)?;

        // Prepend the `started` frame, then map the body events, then the
        // `exit` frame is the last item emitted by exec_stream_core.
        let exec_id_clone = exec_id.clone();
        let started = futures::stream::once(async move {
            Ok::<app::ExecOutput, Status>(app::ExecOutput {
                event: Some(app::exec_output::Event::Started(app::ExecStarted {
                    exec_id: exec_id_clone,
                })),
            })
        });

        let body = body_stream.map(move |ev| {
            let ev = ev.map_err(into_status)?;
            use crate::api::exec::ExecStreamEvent;
            let proto_event = match ev {
                ExecStreamEvent::Stdout(b) => app::exec_output::Event::Stdout(b),
                ExecStreamEvent::Stderr(b) => app::exec_output::Event::Stderr(b),
                ExecStreamEvent::Exit {
                    exit_status,
                    rusage,
                } => app::exec_output::Event::Exit(app::ExecExit {
                    exit_status,
                    rusage: Some(super::convert::exec_rusage_to_proto(rusage)),
                }),
            };
            Ok(app::ExecOutput {
                event: Some(proto_event),
            })
        });

        Ok(Response::new(Box::pin(started.chain(body))))
    }

    async fn get_log(
        &self,
        req: Request<app::GetLogRequest>,
    ) -> Result<Response<app::GetLogResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;
        let entries = crate::api::sessions_inspect::get_log_core(&self.state, id, r.kind, r.limit)
            .await
            .map_err(into_status)?;
        let proto_entries = entries
            .into_iter()
            .map(super::convert::conversation_entry_to_proto)
            .collect();
        Ok(Response::new(app::GetLogResponse {
            session_id: id.to_string(),
            kind: "conversation".to_string(),
            events: proto_entries,
        }))
    }

    async fn snapshot(
        &self,
        req: Request<app::SnapshotRequest>,
    ) -> Result<Response<app::SnapshotResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        let resp = crate::api::snapshot::snapshot_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::SnapshotResponse {
            session_id: id.to_string(),
            snapshot_id: resp.snapshot_id,
            size_bytes: resp.size_bytes,
            note: resp.note.to_string(),
        }))
    }

    async fn resume(
        &self,
        req: Request<app::ResumeRequest>,
    ) -> Result<Response<app::ResumeResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        let resp = crate::api::snapshot::resume_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::ResumeResponse {
            session_id: id.to_string(),
            snapshot_id: resp.snapshot_id,
            size_bytes: resp.size_bytes,
            note: resp.note.to_string(),
        }))
    }

    async fn evict_local(
        &self,
        req: Request<app::EvictLocalRequest>,
    ) -> Result<Response<app::EvictLocalResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        crate::api::snapshot::evict_local_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::EvictLocalResponse {}))
    }

    async fn evict_idle(
        &self,
        req: Request<app::EvictIdleRequest>,
    ) -> Result<Response<app::EvictIdleResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        let result = crate::api::admin::evict_idle_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::EvictIdleResponse {
            session_id: result.session_id.to_string(),
            status: result.status.to_string(),
        }))
    }

    async fn get_cow_state(
        &self,
        req: Request<app::GetCowStateRequest>,
    ) -> Result<Response<app::GetCowStateResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        let maybe_view = crate::api::sessions_inspect::cow_state_core(&self.state, id)
            .await
            .map_err(into_status)?;
        let proto_state = maybe_view.map(|v| super::convert::cow_state_to_proto(&v));
        Ok(Response::new(app::GetCowStateResponse {
            session_id: id.to_string(),
            state: proto_state,
        }))
    }

    async fn list_checkpoints(
        &self,
        req: Request<app::ListCheckpointsRequest>,
    ) -> Result<Response<app::ListCheckpointsResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        let summaries = crate::api::sessions_inspect::checkpoints_core(&self.state, id)
            .await
            .map_err(into_status)?;
        let proto_checkpoints = summaries
            .into_iter()
            .map(super::convert::checkpoint_summary_to_proto)
            .collect();
        Ok(Response::new(app::ListCheckpointsResponse {
            session_id: id.to_string(),
            checkpoints: proto_checkpoints,
        }))
    }

    type GetArtifactStream = BoxStream<app::GetArtifactResponse>;

    // ADR 0039 Task 12: streaming GetArtifact.
    //
    // Proto framing (session.proto GetArtifactResponse oneof):
    //   first  → metadata { media_type, size_bytes, file_name }
    //   rest   → chunk bytes (64 KiB each)
    //
    // The axum serve_artifact handler is unchanged; this shares
    // get_artifact_core which returns the same ArtifactRow + ByteStream.
    //
    // Abort on client disconnect: the stream is a `BoxStream` pinned
    // inside tonic, which drops the future when the client disconnects.
    // The `ByteStream` from BlobStorage is a lazy stream; dropping it
    // aborts the read at the next poll without any explicit teardown.
    //
    // `clippy::result_large_err`: `tonic::Status` is the unavoidable RPC
    // error type here.
    #[allow(clippy::result_large_err)]
    async fn get_artifact(
        &self,
        req: Request<app::GetArtifactRequest>,
    ) -> Result<Response<Self::GetArtifactStream>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;

        let (meta, byte_stream) =
            crate::api::upload::get_artifact_core(&self.state, id, &r.artifact_id)
                .await
                .map_err(into_status)?;

        let metadata_frame = app::GetArtifactResponse {
            msg: Some(app::get_artifact_response::Msg::Metadata(
                super::convert::artifact_meta_to_proto(meta),
            )),
        };

        // Lazy incremental stream: emit the metadata frame first, then
        // re-chunk the ByteStream into ≤64 KiB `chunk` frames as it's
        // polled.  Dropping this stream aborts the read at the next poll
        // (the ByteStream's own future is dropped), so client disconnect
        // reliably cancels the read — no transient full-blob buffering.
        let chunk_stream = async_stream::try_stream! {
            let mut acc: Vec<u8> = Vec::new();
            let mut stream = byte_stream;
            while let Some(item) = stream.next().await {
                let chunk = item.map_err(|e| Status::internal(format!("artifact read: {e}")))?;
                acc.extend_from_slice(&chunk);
                while acc.len() >= ARTIFACT_CHUNK_BYTES {
                    let out: Vec<u8> = acc.drain(..ARTIFACT_CHUNK_BYTES).collect();
                    yield app::GetArtifactResponse {
                        msg: Some(app::get_artifact_response::Msg::Chunk(out)),
                    };
                }
            }
            // Drain any remaining bytes (last partial chunk).
            if !acc.is_empty() {
                yield app::GetArtifactResponse {
                    msg: Some(app::get_artifact_response::Msg::Chunk(acc)),
                };
            }
        };

        let full_stream =
            futures::stream::once(async move { Ok(metadata_frame) }).chain(chunk_stream);

        Ok(Response::new(Box::pin(full_stream)))
    }

    async fn create_artifact_from_path(
        &self,
        req: Request<app::CreateArtifactFromPathRequest>,
    ) -> Result<Response<app::CreateArtifactFromPathResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = parse_session_id(&r.session_id)?;
        let artifact =
            crate::api::upload::create_artifact_from_path_core(&self.state, id, &r.path, r.caption)
                .await
                .map_err(into_status)?;
        Ok(Response::new(app::CreateArtifactFromPathResponse {
            artifact_id: artifact.artifact_id,
            media_type: artifact.media_type,
            size_bytes: artifact.size_bytes,
        }))
    }
}

#[cfg(test)]
mod tests {
    /// `create_request_from_proto` must NOT map `harness_env` into the api
    /// request — it is intentionally dropped (bound as `_` in convert.rs).
    /// The RPC layer (create_session above) extracts it into `identity_env`
    /// before convert.rs is called; this test confirms the converter itself
    /// does not surface the field on the output struct (i.e., the api
    /// `CreateSessionRequest` has no such field, proving the totality drop is
    /// correct).
    #[test]
    fn create_request_from_proto_drops_harness_env() {
        use engram_protocol::app;
        let r = app::CreateSessionRequest {
            selected_skills: Vec::new(),
            capabilities: Vec::new(),
            integration_policy_json: String::new(),
            image_uri: "localhost:5001/demo:warm".into(),
            mode: "agent".into(),
            prompt: None,
            harness_env: std::collections::HashMap::from([(
                "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
                "tok-abc-123".to_string(),
            )]),
            secrets: std::collections::HashMap::new(),
            prompt_id: None,
            harness: None,
        };
        let api =
            super::super::convert::create_request_from_proto(r).expect("converter must succeed");
        // The api struct has image, mode, prompt, secrets — no harness_env. The
        // type system is the assertion: if harness_env were added to the api
        // request and mistakenly mapped, this file would not compile.
        assert_eq!(api.image, "localhost:5001/demo:warm");
        // ADR 0062: the (unset) per-session harness maps through verbatim.
        assert_eq!(api.selected_harness, None);
    }

    /// ADR 0062: a set `harness` threads into the api request's
    /// `selected_harness` (the catalog key the coordinator resolves).
    #[test]
    fn create_request_from_proto_threads_selected_harness() {
        use engram_protocol::app;
        let r = app::CreateSessionRequest {
            selected_skills: Vec::new(),
            capabilities: Vec::new(),
            integration_policy_json: String::new(),
            image_uri: "localhost:5001/demo:warm".into(),
            mode: "agent".into(),
            prompt: None,
            harness_env: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::new(),
            prompt_id: None,
            harness: Some("claude".into()),
        };
        let api =
            super::super::convert::create_request_from_proto(r).expect("converter must succeed");
        assert_eq!(api.selected_harness.as_deref(), Some("claude"));
    }

    /// The RPC-layer extraction folds `harness_env` verbatim into the map that
    /// becomes `identity_env` (passed to `create_session_core`). This mirrors
    /// the extraction in `create_session` above so a regression in the field
    /// name / collection is caught here rather than only in an integration run.
    #[test]
    fn harness_env_folds_into_identity_env_verbatim() {
        use engram_protocol::app;
        let r = app::CreateSessionRequest {
            selected_skills: Vec::new(),
            capabilities: Vec::new(),
            integration_policy_json: String::new(),
            image_uri: "localhost:5001/demo:warm".into(),
            mode: "agent".into(),
            prompt: None,
            harness_env: std::collections::HashMap::from([
                ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "tok-xyz".to_string()),
                ("OTHER".to_string(), "v".to_string()),
            ]),
            secrets: std::collections::HashMap::new(),
            prompt_id: None,
            harness: None,
        };
        // Same expression the RPC handler uses.
        let identity_env: std::collections::HashMap<String, String> =
            r.harness_env.clone().into_iter().collect();
        assert_eq!(
            identity_env
                .get("CLAUDE_CODE_OAUTH_TOKEN")
                .map(String::as_str),
            Some("tok-xyz")
        );
        assert_eq!(identity_env.get("OTHER").map(String::as_str), Some("v"));
        assert_eq!(identity_env.len(), 2);
    }
}
