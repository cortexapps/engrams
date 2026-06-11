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
        // Reject harness_secret_id until Task 13 wires SecretService.
        // Reading it here (rather than silently dropping in convert.rs) means
        // the orchestrator gets a loud, actionable error instead of a silent
        // no-op. Self-deletes when Task 13 lands.
        if r.harness_secret_id.is_some() {
            return Err(Status::unimplemented(
                "harness_secret_id lands with SecretService (ADR 0039 Task 13)",
            ));
        }
        // No calling user here (ADR §2.1): owner = None, no principal
        // identity env. `harness_secret_id` has been checked above and is
        // `None` at this point; convert.rs binds it as `_`.
        let api_req = super::convert::create_request_from_proto(r).map_err(into_status)?;
        let body = crate::api::sessions::create_session_core(
            &self.state,
            None,
            std::collections::HashMap::new(),
            api_req,
        )
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
        let note = crate::api::prompt::send_prompt_core(&self.state, id, r.text)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::SendPromptResponse {
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

        // Map proto ExecRequest → api ExecRequest. argv wins when
        // non-empty; command wins otherwise (mirroring build_exec logic
        // which errors on neither-or-both).
        let api_req = crate::api::exec::ExecRequest {
            command: r.command,
            argv: if r.argv.is_empty() {
                None
            } else {
                Some(r.argv)
            },
            env: r.env,
            workdir: r.workdir,
            timeout_secs: r.timeout_secs,
        };

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
                    rusage: Some(app::ExecRusage {
                        wall_ms: rusage.wall_ms,
                        peak_rss_kb: rusage.peak_rss_kb,
                        user_cpu_ms: rusage.user_cpu_ms,
                        sys_cpu_ms: rusage.sys_cpu_ms,
                    }),
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
            .map(|e| app::ConversationEntry {
                idx: e.idx,
                kind: e.kind,
                at: e.at.to_rfc3339(),
                // The proto field is named payload_json to emphasise the
                // envelope; the Rust type carries it as `payload` (a
                // serde_json::Value). Serialize to a JSON string here.
                payload_json: e.payload.to_string(),
            })
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
            .map(|s| app::CheckpointSummary {
                snapshot_id: s.snapshot_id,
                created_at: s.created_at.to_rfc3339(),
                size_bytes: s.size_bytes,
                events_cursor: s.events_cursor,
                recoverable: s.recoverable,
                is_latest: s.is_latest,
            })
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
                app::ArtifactMetadata {
                    media_type: meta.media_type,
                    size_bytes: meta.size_bytes as u64,
                    file_name: meta.file_name,
                },
            )),
        };

        // Accumulate bytes into ARTIFACT_CHUNK_BYTES-sized frames.
        let chunk_stream = byte_stream
            .map(|res| res.map_err(|e| Status::internal(format!("artifact read: {e}"))))
            // Buffer into fixed-size chunks. We use a stateful fold that
            // emits a chunk when the buffer fills and drains the remainder
            // after the stream ends, using `async_stream::stream!` for
            // clarity and to avoid a complex `unfold`.
            .collect::<Vec<_>>()
            .await;

        // Build the full chunked sequence from the buffered bytes.
        // (We buffer fully because the ByteStream item size is arbitrary.)
        let chunks: Vec<Result<app::GetArtifactResponse, Status>> = {
            let mut acc: Vec<u8> = Vec::new();
            let mut out = Vec::new();
            for item in chunk_stream {
                match item {
                    Err(e) => {
                        out.push(Err(e));
                        break;
                    }
                    Ok(b) => {
                        acc.extend_from_slice(&b);
                        while acc.len() >= ARTIFACT_CHUNK_BYTES {
                            let chunk: Vec<u8> = acc.drain(..ARTIFACT_CHUNK_BYTES).collect();
                            out.push(Ok(app::GetArtifactResponse {
                                msg: Some(app::get_artifact_response::Msg::Chunk(chunk)),
                            }));
                        }
                    }
                }
            }
            // Emit any remaining bytes (last partial chunk).
            if !acc.is_empty() {
                out.push(Ok(app::GetArtifactResponse {
                    msg: Some(app::get_artifact_response::Msg::Chunk(acc)),
                }));
            }
            out
        };

        let full_stream = futures::stream::once(async move { Ok(metadata_frame) })
            .chain(futures::stream::iter(chunks));

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
    use engram_protocol::app;

    /// `create_session` must loudly refuse `harness_secret_id: Some(_)` until
    /// Task 13 wires SecretService. Validates Change 2 of the review fixes.
    /// The RPC body checks `r.harness_secret_id.is_some()` and returns
    /// `Status::unimplemented(...)` before delegating to convert.rs.
    #[test]
    fn create_request_from_proto_rejects_harness_secret_id() {
        let r = app::CreateSessionRequest {
            image_uri: "localhost:5001/demo:warm".into(),
            mode: String::new(),
            prompt: None,
            harness_secret_id: Some("secret-abc-123".into()),
            secrets: std::collections::HashMap::new(),
        };
        // Verify the field is detectable — the RPC layer (create_session)
        // checks this and short-circuits with Status::unimplemented.
        assert!(
            r.harness_secret_id.is_some(),
            "harness_secret_id must be detected as Some(_) so the RPC rejects it"
        );
    }
}
