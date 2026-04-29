use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{HostId, SandboxId, SessionId};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Pending,
    Active,
    Idle,
    Completed,
    Failed,
    /// Phase 3d: the host that owned this session went dark and
    /// the dead-host detector cleared `host_id`. Next access picks
    /// a new host (snapshot affinity if any other host has the
    /// snapshot, else cold-tier blob restore) and transitions to
    /// `Active`.
    PendingReassign,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::PendingReassign => "pending_reassign",
        }
    }
}

/// User-facing request to create a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSpec {
    /// `git+https://...` / `git+ssh://...` for git-backed sessions
    /// (workspace cloned at create time, durable via per-session
    /// checkpoint branch). `local://<name>` for ephemeral local-only
    /// sessions used by `just dev` and unit tests — no clone, no
    /// checkpoint, lost on host loss.
    pub repo: String,
    pub branch: String,
    pub user_id: Option<String>,
    /// Optional override for the warm image to use. If unset, the
    /// coordinator picks the latest `ready` version for the repo.
    pub image_version: Option<String>,
    /// If true, the session clones its repo but never pushes back —
    /// no checkpoint branch is allocated. Useful for code-review
    /// agents that only read. Default false.
    #[serde(default)]
    pub read_only: bool,
}

/// Parsed `repo` field. Created once at session-create time and
/// stored alongside the raw `repo` string so the original API-shape
/// stays available for diagnostics / display.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepoUrl {
    /// `git+https://...` / `git+ssh://...`. The contained string is
    /// the URL with the `git+` prefix stripped (the form `git` itself
    /// understands).
    Git { url: String },
    /// `local://<name>` — ephemeral local-only mode for dev. `name`
    /// is just a label; nothing is cloned.
    Local { name: String },
}

impl RepoUrl {
    /// Parse the `repo` field on `POST /sessions`. The grammar is:
    ///
    /// - `git+https://...` or `git+ssh://...` → `Git`
    /// - `local://<name>` → `Local { name }` (name is the path-and-
    ///   query suffix, intended as a free-form label)
    /// - Anything else → error. The legacy "any string is fine" shape
    ///   is intentionally retired here; callers that want the old
    ///   behaviour use `local://` explicitly.
    pub fn parse(s: &str) -> Result<Self, RepoUrlError> {
        if let Some(suffix) = s.strip_prefix("git+") {
            // Reject empty url right after the prefix.
            if suffix.is_empty() {
                return Err(RepoUrlError::Empty);
            }
            return Ok(Self::Git {
                url: suffix.to_string(),
            });
        }
        if let Some(name) = s.strip_prefix("local://") {
            // Allow empty name — useful for unit-test fixtures where
            // the session has no real repo identity.
            return Ok(Self::Local {
                name: name.to_string(),
            });
        }
        Err(RepoUrlError::UnknownScheme(s.to_string()))
    }

    /// Render back to the canonical input form. Round-trips through
    /// `parse`.
    pub fn as_string(&self) -> String {
        match self {
            Self::Git { url } => format!("git+{url}"),
            Self::Local { name } => format!("local://{name}"),
        }
    }
}

/// Error from [`RepoUrl::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoUrlError {
    Empty,
    UnknownScheme(String),
}

impl std::fmt::Display for RepoUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty repo URL after `git+` prefix"),
            Self::UnknownScheme(s) => write!(
                f,
                "unrecognised repo URL `{s}` — expected `git+https://...`, \
                 `git+ssh://...`, or `local://<name>`"
            ),
        }
    }
}

impl std::error::Error for RepoUrlError {}

/// What kind of session this is, derived from `repo` and `read_only`
/// at create time. Persisted as a string; checked across the resume
/// / checkpoint paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Real writable git session: clones at create, pushes to
    /// `engram/sessions/<id>` on checkpoint, durable across host loss.
    Git,
    /// `local://` ephemeral mode. Conversation events still land in
    /// Postgres; workspace is lost on host loss.
    Local,
    /// Git URL but `read_only = true`: clones at create, never pushes
    /// back, no checkpoint branch.
    Readonly,
}

impl SessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Local => "local",
            Self::Readonly => "readonly",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "git" => Ok(Self::Git),
            "local" => Ok(Self::Local),
            "readonly" => Ok(Self::Readonly),
            other => Err(format!("unknown session_kind: {other}")),
        }
    }

    /// Derive the kind from a parsed `repo` URL + the `read_only` flag.
    pub fn derive(repo: &RepoUrl, read_only: bool) -> Self {
        match repo {
            RepoUrl::Local { .. } => Self::Local,
            RepoUrl::Git { .. } if read_only => Self::Readonly,
            RepoUrl::Git { .. } => Self::Git,
        }
    }
}

/// Convention: a git session's checkpoint branch is named
/// `engram/sessions/<session_id>` on the writable repo's remote.
pub fn checkpoint_branch_for(session_id: SessionId) -> String {
    format!("engram/sessions/{session_id}")
}

/// A persisted session row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub repo: String,
    pub branch: String,
    pub user_id: Option<String>,
    pub status: SessionStatus,
    pub image_version: String,
    pub host_id: Option<HostId>,
    /// In-memory `SandboxId` of the live sandbox serving this
    /// session, persisted so a coordinator restart can rebuild its
    /// in-memory routing maps from `SELECT ... FROM sessions WHERE
    /// status NOT IN ('completed','failed')`. `None` for sessions in
    /// `Pending` (sandbox not created yet) / `Idle` (sandbox evicted)
    /// / `PendingReassign` (host died, awaiting reschedule) /
    /// `Completed` / `Failed`.
    #[serde(default)]
    pub sandbox_id: Option<SandboxId>,
    /// Phase 4: derived from `SessionSpec::repo` + `read_only` at
    /// create time. Persisted as a string ("git" | "local" | "readonly")
    /// so future coordinator versions can extend without a schema
    /// migration. Defaults to `Local` for backwards-compat with rows
    /// inserted before the column existed.
    #[serde(default = "default_session_kind")]
    pub session_kind: SessionKind,
    /// Phase 4: parsed `repo` (canonical form). `None` for legacy rows
    /// that pre-date the column. New rows always populate this.
    #[serde(default)]
    pub repo_url: Option<RepoUrl>,
    /// Phase 4: `engram/sessions/<id>` for `SessionKind::Git`; `None`
    /// for Local / Readonly. Set by the coord at session-create time;
    /// stable across resumes.
    #[serde(default)]
    pub checkpoint_branch: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

fn default_session_kind() -> SessionKind {
    SessionKind::Local
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_serializes_lowercase() {
        let payload = serde_json::to_value(SessionStatus::Active).unwrap();
        assert_eq!(payload, serde_json::json!("active"));
        let parsed: SessionStatus = serde_json::from_str(r#""idle""#).unwrap();
        assert_eq!(parsed, SessionStatus::Idle);
    }

    #[test]
    fn session_status_unknown_string_rejected() {
        let res: Result<SessionStatus, _> = serde_json::from_str(r#""running""#);
        assert!(res.is_err(), "unknown variants must fail to deserialize");
    }

    #[test]
    fn session_status_as_str_matches_serde_form() {
        for s in [
            SessionStatus::Pending,
            SessionStatus::Active,
            SessionStatus::Idle,
            SessionStatus::Completed,
            SessionStatus::Failed,
        ] {
            let via_serde = serde_json::to_string(&s).unwrap();
            // strip the surrounding quotes from the JSON string
            let trimmed = via_serde.trim_matches('"');
            assert_eq!(s.as_str(), trimmed, "as_str must match wire format");
        }
    }

    #[test]
    fn session_roundtrips_through_json() {
        let original = Session {
            id: SessionId::new(),
            repo: "git+https://github.com/cortex/api.git".into(),
            branch: "main".into(),
            user_id: Some("u1".into()),
            status: SessionStatus::Active,
            image_version: "warm-20260101T000000Z".into(),
            host_id: Some(HostId::new()),
            sandbox_id: Some(SandboxId::new()),
            session_kind: SessionKind::Git,
            repo_url: Some(RepoUrl::Git {
                url: "https://github.com/cortex/api.git".into(),
            }),
            checkpoint_branch: Some(checkpoint_branch_for(SessionId::new())),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
        };
        let blob = serde_json::to_string(&original).unwrap();
        let back: Session = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.id, original.id);
        assert_eq!(back.repo, original.repo);
        assert_eq!(back.branch, original.branch);
        assert_eq!(back.user_id, original.user_id);
        assert_eq!(back.status, original.status);
        assert_eq!(back.image_version, original.image_version);
        assert_eq!(back.host_id, original.host_id);
        assert_eq!(back.session_kind, SessionKind::Git);
        assert_eq!(back.repo_url, original.repo_url);
        assert_eq!(back.checkpoint_branch, original.checkpoint_branch);
    }

    #[test]
    fn repo_url_parses_git_and_local_schemes() {
        match RepoUrl::parse("git+https://github.com/cortex/api.git").unwrap() {
            RepoUrl::Git { url } => assert_eq!(url, "https://github.com/cortex/api.git"),
            other => panic!("expected Git, got {other:?}"),
        }
        match RepoUrl::parse("git+ssh://git@github.com:cortex/api.git").unwrap() {
            RepoUrl::Git { url } => assert_eq!(url, "ssh://git@github.com:cortex/api.git"),
            other => panic!("expected Git, got {other:?}"),
        }
        match RepoUrl::parse("local://hello").unwrap() {
            RepoUrl::Local { name } => assert_eq!(name, "hello"),
            other => panic!("expected Local, got {other:?}"),
        }
        // Empty `local://` is permitted (test fixture shape).
        match RepoUrl::parse("local://").unwrap() {
            RepoUrl::Local { name } => assert!(name.is_empty()),
            other => panic!("expected Local, got {other:?}"),
        }
    }

    #[test]
    fn repo_url_rejects_unknown_schemes_and_empty() {
        // The legacy "anything goes" shape (`repo: "hello"`) is now
        // rejected — callers must say `local://hello` or `git+...`.
        // Forces explicit intent at the API boundary.
        assert!(matches!(
            RepoUrl::parse("hello"),
            Err(RepoUrlError::UnknownScheme(_))
        ));
        assert!(matches!(
            RepoUrl::parse("https://github.com/foo"),
            Err(RepoUrlError::UnknownScheme(_))
        ));
        assert!(matches!(RepoUrl::parse("git+"), Err(RepoUrlError::Empty)));
    }

    #[test]
    fn repo_url_round_trips_via_as_string() {
        for input in [
            "git+https://github.com/x/y.git",
            "git+ssh://git@host/y.git",
            "local://hello",
            "local://",
        ] {
            let parsed = RepoUrl::parse(input).unwrap();
            assert_eq!(parsed.as_string(), input);
        }
    }

    #[test]
    fn session_kind_derive_combines_repo_and_read_only() {
        let git = RepoUrl::Git {
            url: "https://x/y.git".into(),
        };
        let local = RepoUrl::Local { name: "h".into() };
        assert_eq!(SessionKind::derive(&git, false), SessionKind::Git);
        assert_eq!(SessionKind::derive(&git, true), SessionKind::Readonly);
        assert_eq!(SessionKind::derive(&local, false), SessionKind::Local);
        // `read_only = true` on a Local repo is meaningless but not
        // an error — kind stays Local.
        assert_eq!(SessionKind::derive(&local, true), SessionKind::Local);
    }

    #[test]
    fn checkpoint_branch_uses_engram_sessions_namespace() {
        let id = SessionId::new();
        let branch = checkpoint_branch_for(id);
        assert!(branch.starts_with("engram/sessions/"));
        assert!(branch.ends_with(&id.to_string()));
    }

    #[test]
    fn session_kind_round_trips_via_as_str_parse() {
        for k in [SessionKind::Git, SessionKind::Local, SessionKind::Readonly] {
            assert_eq!(SessionKind::parse(k.as_str()).unwrap(), k);
        }
        assert!(SessionKind::parse("bogus").is_err());
    }
}
