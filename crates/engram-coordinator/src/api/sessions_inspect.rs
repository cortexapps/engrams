//! `GET /sessions/:id/log`, `GET /sessions/:id/diff`, `POST
//! /sessions/:id/fork` — Track F's git-native inspection +
//! forking surface. Backed by `state.git_workdir` (bare clones of
//! the writable repo) for the git queries; conversation log comes
//! straight from `session_events`.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use engram_core::types::session::{checkpoint_branch_for, WorkspaceSpec};
use engram_core::types::{SessionSpec, SessionStatus};
use engram_core::SessionId;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiError;
use crate::git_workdir::GitWorkdirError;
use crate::state::{SessionEvent, SharedState};

#[derive(Deserialize, Default)]
pub struct LogQuery {
    /// `conversation` (default) or `workspace`. Conversation reads
    /// `session_events` from Postgres; workspace reads `git log`
    /// from the bare clone.
    #[serde(default)]
    pub kind: Option<String>,
    /// Cap on number of rows / commits returned. Defaults to 200,
    /// hard-capped at 1000 to keep responses bounded.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct ConversationEntry {
    pub idx: i64,
    pub kind: String,
    pub at: chrono::DateTime<Utc>,
    pub payload: serde_json::Value,
}

#[derive(Serialize)]
pub struct WorkspaceCommit {
    pub sha: String,
    pub date: String,
    pub author: String,
    pub message: String,
}

pub async fn log(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Query(params): Query<LogQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let session = state.services.meta.get_session(id).await?;
    let limit = params.limit.unwrap_or(200).clamp(1, 1000);

    match params.kind.as_deref().unwrap_or("conversation") {
        "conversation" => {
            let rows = state
                .services
                .meta
                .list_session_events_since(id, -1, limit)
                .await?;
            let entries: Vec<ConversationEntry> = rows
                .into_iter()
                .map(|e| ConversationEntry {
                    idx: e.idx,
                    kind: e.kind,
                    at: e.created_at,
                    payload: e.payload,
                })
                .collect();
            Ok(Json(json!({
                "session_id": id,
                "kind": "conversation",
                "events": entries,
            })))
        }
        "workspace" => {
            let (url, branch) = git_target_for(&session)?;
            let base_branch = session.workspace.git_branch().ok_or_else(|| {
                ApiError::Internal("git session is missing workspace.git.branch".into())
            })?;
            let range = format!("{base_branch}..{branch}");
            let pretty = "--pretty=format:%H%x09%aI%x09%an%x09%s";
            let limit_str = format!("-n{limit}");
            let out = state
                .git_workdir
                .run_git(&url, &["log", &limit_str, pretty, &range])
                .await
                .map_err(map_git_err)?;
            if !out.exit_success() {
                // Empty range — no commits past the base branch yet —
                // is reported by git as exit 128 with a "fatal:
                // ambiguous argument" message. Treat that as an
                // empty list.
                let stderr = out.stderr_lossy();
                if stderr.contains("unknown revision") || stderr.contains("ambiguous argument") {
                    return Ok(Json(json!({
                        "session_id": id,
                        "kind": "workspace",
                        "branch": branch,
                        "commits": Vec::<WorkspaceCommit>::new(),
                    })));
                }
                return Err(ApiError::Internal(format!(
                    "git log failed: {}",
                    stderr.trim()
                )));
            }
            let commits = parse_log_lines(&out.stdout_lossy());
            Ok(Json(json!({
                "session_id": id,
                "kind": "workspace",
                "branch": branch,
                "commits": commits,
            })))
        }
        other => Err(ApiError::BadRequest(format!(
            "unknown kind `{other}` — expected `conversation` or `workspace`"
        ))),
    }
}

#[derive(Deserialize, Default)]
pub struct DiffQuery {
    /// What to diff against. Defaults to the session's base branch
    /// (e.g. `main`). Pass any ref the repo knows.
    #[serde(default)]
    pub vs: Option<String>,
}

pub async fn diff(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Query(params): Query<DiffQuery>,
) -> Result<Response, ApiError> {
    let session = state.services.meta.get_session(id).await?;
    let (url, branch) = git_target_for(&session)?;
    let base_branch = session
        .workspace
        .git_branch()
        .ok_or_else(|| ApiError::Internal("git session is missing workspace.git.branch".into()))?
        .to_string();
    let vs = params.vs.unwrap_or(base_branch);
    // `<vs>...<branch>` is symmetric-difference: shows what's on
    // <branch> that isn't on <vs>. Right semantics for "what did
    // the agent change relative to base."
    let range = format!("{vs}...{branch}");
    let out = state
        .git_workdir
        .run_git(&url, &["diff", &range])
        .await
        .map_err(map_git_err)?;
    if !out.exit_success() {
        return Err(ApiError::Internal(format!(
            "git diff failed: {}",
            out.stderr_lossy().trim()
        )));
    }
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        out.stdout,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct ForkRequest {
    /// Fork from the workspace_commit_sha of the
    /// `CheckpointPushed` event at-or-before this idx. If unset,
    /// forks from the current HEAD of the source's checkpoint
    /// branch.
    #[serde(default)]
    pub from_event_idx: Option<i64>,
    /// Optional human-readable label, surfaced in events / tooling
    /// — not load-bearing.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Serialize)]
pub struct ForkResponse {
    pub session_id: SessionId,
    pub branch: String,
    pub from_sha: String,
}

pub async fn fork(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Json(req): Json<ForkRequest>,
) -> Result<Json<ForkResponse>, ApiError> {
    let src = state.services.meta.get_session(id).await?;
    let (url, src_branch) = git_target_for(&src)?;

    // Decide which SHA to fork from. Either an explicit
    // workspace_commit_sha derived from a `CheckpointPushed`
    // event at-or-before from_event_idx, or the current HEAD of
    // the source's checkpoint branch.
    let from_sha = match req.from_event_idx {
        Some(idx) => {
            let sha = checkpoint_sha_at_or_before(&state, id, idx)
                .await?
                .ok_or_else(|| {
                    ApiError::Conflict(format!(
                        "no `checkpoint_pushed` event found at or before idx {idx}"
                    ))
                })?;
            sha
        }
        None => {
            // Make sure the bare clone is current, then resolve the
            // checkpoint branch's HEAD.
            let head = state
                .git_workdir
                .run_git(&url, &["rev-parse", &src_branch])
                .await
                .map_err(map_git_err)?;
            if !head.exit_success() {
                return Err(ApiError::Conflict(format!(
                    "source session's checkpoint branch `{src_branch}` not found on remote"
                )));
            }
            head.stdout_lossy().trim().to_string()
        }
    };

    // Persist the new session row. Same `image`/`workspace`/
    // `harness` as the source so the new sandbox lands on the same
    // warm-pool image at the same bake SHA — the smart-bootstrap
    // then resets to the new checkpoint branch we're about to publish.
    // Forks are always writable (they exist precisely to continue
    // working on a checkpoint), so override `read_only=false` if the
    // source was a Readonly Git workspace.
    let workspace = match &src.workspace {
        WorkspaceSpec::Git { url, branch, .. } => WorkspaceSpec::Git {
            url: url.clone(),
            branch: branch.clone(),
            read_only: false,
        },
        // git_target_for already rejected non-git sessions above, so
        // this branch is unreachable; fall back defensively.
        other => other.clone(),
    };
    let spec = SessionSpec {
        image: src.image.clone(),
        workspace,
        harness: src.harness.clone(),
        user_id: src.user_id.clone(),
    };
    let new_id = state.services.meta.create_session(spec).await?;
    let new_branch = checkpoint_branch_for(new_id);

    // Publish from_sha as the new session's checkpoint branch on
    // the writable repo's remote. The new session's first
    // resume / exec will smart-bootstrap a fresh sandbox to this
    // SHA.
    state
        .git_workdir
        .push_sha_to_branch(&url, &from_sha, &new_branch)
        .await
        .map_err(map_git_err)?;

    if let Some(title) = req.title.as_deref() {
        // Emit a status-changed-equivalent event so the new
        // session's SSE stream has a useful first entry. We use
        // the existing CheckpointPushed kind since the new branch
        // semantically *is* a fresh checkpoint with the source's
        // SHA.
        let _ = state
            .emit(
                new_id,
                SessionEvent::CheckpointPushed {
                    commit_sha: from_sha.clone(),
                    harness_acked: false,
                    at: Utc::now(),
                },
            )
            .await;
        tracing::info!(
            new_session_id = %new_id,
            source = %id,
            title,
            "session forked",
        );
    }

    Ok(Json(ForkResponse {
        session_id: new_id,
        branch: new_branch,
        from_sha,
    }))
}

fn git_target_for(session: &engram_core::types::Session) -> Result<(String, String), ApiError> {
    let url = session.workspace.git_url().ok_or_else(|| {
        ApiError::Conflict("session has no git workspace — nothing to inspect / fork".into())
    })?;
    let branch = session.checkpoint_branch.clone().ok_or_else(|| {
        ApiError::Conflict(
            "session has no checkpoint branch — readonly Git sessions are inspect-only via `vs=`"
                .into(),
        )
    })?;
    Ok((url.to_string(), branch))
}

fn parse_log_lines(out: &str) -> Vec<WorkspaceCommit> {
    out.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.len() != 4 {
                return None;
            }
            Some(WorkspaceCommit {
                sha: parts[0].to_string(),
                date: parts[1].to_string(),
                author: parts[2].to_string(),
                message: parts[3].to_string(),
            })
        })
        .collect()
}

/// Walk the session_events log backwards from `at_or_before_idx`
/// looking for a `checkpoint_pushed` event; return its
/// `commit_sha` payload field. None if no checkpoint exists at or
/// before that idx (e.g., session forked before any tool call
/// completed).
pub(crate) async fn checkpoint_sha_at_or_before(
    state: &SharedState,
    session_id: SessionId,
    at_or_before_idx: i64,
) -> Result<Option<String>, ApiError> {
    // The MetadataStore exposes `list_session_events_since(since,
    // limit)`; we ask for everything from -1 up to a generous cap
    // and filter client-side. Sessions with thousands of events
    // are unusual at this stage; if it becomes an issue we'd add
    // a kind-filtered query. For now, simple wins.
    let rows = state
        .services
        .meta
        .list_session_events_since(session_id, -1, 5000)
        .await?;
    for row in rows.into_iter().rev() {
        if row.idx > at_or_before_idx {
            continue;
        }
        if row.kind != "checkpoint_pushed" {
            continue;
        }
        if let Some(sha) = row
            .payload
            .get("commit_sha")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            return Ok(Some(sha));
        }
    }
    Ok(None)
}

fn map_git_err(e: GitWorkdirError) -> ApiError {
    match e {
        GitWorkdirError::Timeout(_) => ApiError::Internal(e.to_string()),
        GitWorkdirError::Git { .. } => ApiError::Internal(e.to_string()),
        GitWorkdirError::Io(_) => ApiError::Internal(e.to_string()),
    }
}

// Silence unused-warning since SessionStatus is part of the wider
// session lifecycle but isn't directly read here yet (will be when
// fork respects the source's status invariants).
#[allow(dead_code)]
fn _status_unused_check(_: SessionStatus) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::image_registry::ImageRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::SandboxBackend;
    use engram_core::types::session::{HarnessSpec, ImageRef, SessionKind};
    use engram_core::types::Session;
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn run_host(args: &[&str], cwd: &Path) {
        let out = StdCommand::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "host {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Build a bare remote populated with one commit on `main`,
    /// plus a `engram/sessions/<id>` branch that descends from main
    /// and adds `marker_path` containing `marker`. Returns the bare
    /// remote dir.
    fn seeded_remote_with_session_branch(
        session_branch: &str,
        marker_path: &str,
        marker: &str,
    ) -> TempDir {
        let remote = TempDir::new().unwrap();
        run_host(
            &["git", "init", "--bare", "--initial-branch=main"],
            remote.path(),
        );
        let work = TempDir::new().unwrap();
        run_host(&["git", "init", "--initial-branch=main"], work.path());
        run_host(
            &[
                "git",
                "remote",
                "add",
                "origin",
                &format!("{}", remote.path().display()),
            ],
            work.path(),
        );
        std::fs::write(work.path().join("README.md"), "base").unwrap();
        run_host(&["git", "add", "-A"], work.path());
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
            work.path(),
        );
        run_host(&["git", "push", "origin", "main"], work.path());

        // Now branch off main and add the marker file, push as the
        // session branch.
        std::fs::write(work.path().join(marker_path), marker).unwrap();
        run_host(&["git", "add", "-A"], work.path());
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "agent work",
            ],
            work.path(),
        );
        run_host(
            &["git", "push", "origin", &format!("HEAD:{session_branch}")],
            work.path(),
        );
        remote
    }

    fn build_state_for_session(session: Session) -> (SharedState, TempDir) {
        let local = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(local.path().join("sandboxes")));
        let host_registry = Arc::new(HostRegistry::new());
        host_registry.register(engram_core::HostId::new(), backend.clone());
        let services = Services {
            meta: Arc::new(MiniMeta::new(session)),
            cloud: Arc::new(MockCloud::new()),
            sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
            secrets: Arc::new(InMemorySecretStore::new()),
            images: ImageRegistry::new(local.path().join("images")),
            harnesses: Arc::new(crate::harness_registry::HarnessRegistry::empty()),
            harness_substrate: None,
        };
        let cfg = CoordinatorConfig {
            local_path: local.path().to_path_buf(),
            ..CoordinatorConfig::default()
        };
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        (state, local)
    }

    fn git_session_with_id(session_id: engram_core::SessionId, remote: &Path) -> Session {
        let branch = checkpoint_branch_for(session_id);
        let url = format!("file://{}", remote.display());
        Session {
            id: session_id,
            user_id: None,
            status: SessionStatus::Active,
            host_id: None,
            sandbox_id: None,
            image: ImageRef::Registry {
                repo: "test/repo".into(),
                tag: "test".into(),
            },
            workspace: WorkspaceSpec::Git {
                url,
                branch: "main".into(),
                read_only: false,
            },
            harness: HarnessSpec::None,
            session_kind: SessionKind::Git,
            checkpoint_branch: Some(branch),
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn log_workspace_returns_commits_on_session_branch() {
        // Allocate the session ID up front so the seeded remote
        // and the test session reference the same branch name.
        let session_id = engram_core::SessionId::new();
        let session_branch = checkpoint_branch_for(session_id);
        let remote = seeded_remote_with_session_branch(&session_branch, "agent-output.txt", "v2");

        let session = git_session_with_id(session_id, remote.path());
        let (state, _local) = build_state_for_session(session);

        let resp = log(
            State(state.clone()),
            Path(session_id),
            Query(LogQuery {
                kind: Some("workspace".into()),
                limit: None,
            }),
        )
        .await
        .expect("log workspace");

        let v = resp.0;
        let commits = v["commits"].as_array().expect("commits array");
        assert_eq!(
            commits.len(),
            1,
            "expected one commit; full response was {v:#}"
        );
        assert_eq!(commits[0]["message"], "agent work");
    }

    #[tokio::test]
    async fn log_conversation_returns_session_events_from_postgres() {
        let remote = TempDir::new().unwrap();
        run_host(
            &["git", "init", "--bare", "--initial-branch=main"],
            remote.path(),
        );
        let session_id = engram_core::SessionId::new();
        let session = git_session_with_id(session_id, remote.path());
        let (state, _local) = build_state_for_session(session);

        // Append a couple of synthetic events directly via meta so
        // we don't have to set up a full agent run.
        state
            .services
            .meta
            .append_session_event(
                session_id,
                "checkpoint_pushed",
                json!({"commit_sha": "deadbeef"}),
            )
            .await
            .unwrap();
        state
            .services
            .meta
            .append_session_event(session_id, "harness_idle", json!({}))
            .await
            .unwrap();

        let resp = log(
            State(state),
            Path(session_id),
            Query(LogQuery {
                kind: None, // defaults to conversation
                limit: None,
            }),
        )
        .await
        .expect("log conversation");

        let v = resp.0;
        assert_eq!(v["kind"], "conversation");
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["kind"], "checkpoint_pushed");
        assert_eq!(events[1]["kind"], "harness_idle");
    }

    #[tokio::test]
    async fn diff_returns_session_changes_vs_base_branch() {
        let session_id = engram_core::SessionId::new();
        let session_branch = checkpoint_branch_for(session_id);
        let remote =
            seeded_remote_with_session_branch(&session_branch, "added.txt", "agent-content");

        let session = git_session_with_id(session_id, remote.path());
        let (state, _local) = build_state_for_session(session);

        let resp = diff(
            State(state),
            Path(session_id),
            Query(DiffQuery { vs: None }),
        )
        .await
        .expect("diff");

        // Convert response back to bytes for assertion.
        let body = axum::body::to_bytes(resp.into_body(), 8 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("+++ b/added.txt"),
            "diff should mention the new file. got:\n{text}"
        );
        assert!(text.contains("agent-content"));
    }

    #[tokio::test]
    async fn log_workspace_on_ephemeral_session_returns_409() {
        let session = Session {
            id: engram_core::SessionId::new(),
            user_id: None,
            status: SessionStatus::Active,
            host_id: None,
            sandbox_id: None,
            image: ImageRef::Registry {
                repo: "test/repo".into(),
                tag: "test".into(),
            },
            workspace: WorkspaceSpec::Empty,
            harness: HarnessSpec::None,
            session_kind: SessionKind::Ephemeral,
            checkpoint_branch: None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };
        let session_id = session.id;
        let (state, _local) = build_state_for_session(session);

        let err = log(
            State(state),
            Path(session_id),
            Query(LogQuery {
                kind: Some("workspace".into()),
                limit: None,
            }),
        )
        .await
        .expect_err("ephemeral sessions have no git history");
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }
}
