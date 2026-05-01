use std::path::PathBuf;

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
    /// Terminal: the session's FC snapshot is gone (host crashed
    /// mid-run, disk full, eviction past hard cap, etc.). Engram
    /// is a one-shot task runner — sessions live ↔ FC-snapshot
    /// life. The only affordance from Dead is `engram session fork
    /// <id>` to start a new session with the workspace at the
    /// last checkpoint SHA. (Renamed from `PendingReassign` when
    /// the cross-host-resume code path was retired.)
    Dead,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }
}

/// Identifies a baked image. Decoupled from "where the workspace
/// comes from" — the same image can host git, local-mount, or
/// empty workspaces. Only the registry-backed variant exists today;
/// the enum leaves room for a future remote-OCI pull path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageRef {
    /// `<images_dir>/<repo>/<tag>/` against the local image registry.
    /// `repo` here is a registry label, not a workspace identity —
    /// nothing requires it to match `WorkspaceSpec::Git.url`.
    Registry { repo: String, tag: String },
}

impl ImageRef {
    pub fn repo(&self) -> &str {
        match self {
            Self::Registry { repo, .. } => repo,
        }
    }

    pub fn tag(&self) -> &str {
        match self {
            Self::Registry { tag, .. } => tag,
        }
    }
}

/// Where the user's code lives inside the VM at session time.
/// Decoupled from `ImageRef` so a generic image (e.g. `react-toolchain`)
/// can host any git URL, a host bind-mount, or nothing at all.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceSpec {
    /// Boot with no workspace overlay. Whatever the image baked is
    /// what's there. Pairs with `HarnessSpec::None` for the
    /// "just give me a VM and a shell" mode.
    Empty,

    /// Clone a remote git repo into the VM at create time. `read_only`
    /// suppresses the per-session checkpoint branch on the remote.
    Git {
        url: String,
        branch: String,
        #[serde(default)]
        read_only: bool,
    },

    /// Bind a host directory into the VM via virtio-fs (VZ) or a
    /// symlink (Process). Rejected on the Firecracker backend until
    /// `virtiofsd` parity lands. `read_only` here is *mount-policy
    /// only* — there is no remote, so checkpointing is never engaged.
    LocalMount {
        host_path: PathBuf,
        guest_path: PathBuf,
        #[serde(default)]
        read_only: bool,
    },
}

impl WorkspaceSpec {
    /// True if this workspace participates in the per-session
    /// checkpoint branch (writable git only).
    pub fn is_writable_git(&self) -> bool {
        matches!(
            self,
            Self::Git {
                read_only: false,
                ..
            }
        )
    }

    /// The git URL of a Git workspace (writable or read-only). `None`
    /// for non-git workspaces. Used by `engram session log/diff/fork`
    /// and the checkpoint runner — neither makes sense without a remote.
    pub fn git_url(&self) -> Option<&str> {
        match self {
            Self::Git { url, .. } => Some(url),
            Self::Empty | Self::LocalMount { .. } => None,
        }
    }

    /// Branch name of a Git workspace. `None` for non-git workspaces.
    pub fn git_branch(&self) -> Option<&str> {
        match self {
            Self::Git { branch, .. } => Some(branch),
            Self::Empty | Self::LocalMount { .. } => None,
        }
    }
}

/// What long-running agent process (if any) attaches to the session.
/// `None` is the "VM with a shell" mode — engram-bootstrap runs but
/// never receives a `BootstrapLaunch` frame.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessSpec {
    #[default]
    None,
    /// One of the harnesses declared in the image manifest's
    /// `harnesses` array. The coordinator validates `name` against
    /// the manifest at create time and returns 400 on mismatch.
    Builtin { name: String },
}

impl HarnessSpec {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// User-facing request to create a session. Three orthogonal axes:
/// image (immutable rootfs), workspace (where code comes from),
/// harness (what agent attaches).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSpec {
    pub image: ImageRef,
    pub workspace: WorkspaceSpec,
    #[serde(default)]
    pub harness: HarnessSpec,
    pub user_id: Option<String>,
}

/// What kind of session this is, derived from `workspace` at create
/// time. Persisted as a string so future variants extend without a
/// schema migration. Drives checkpoint behavior:
/// - `Git` allocates a checkpoint branch and pushes on each checkpoint.
/// - `Readonly` clones at create but never pushes back.
/// - `Ephemeral` has no remote — `LocalMount` and `Empty` workspaces
///   both fall here regardless of their `read_only` flag (the flag is
///   mount-policy on `LocalMount`, not checkpoint-policy).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Git,
    Readonly,
    Ephemeral,
}

impl SessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Readonly => "readonly",
            Self::Ephemeral => "ephemeral",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "git" => Ok(Self::Git),
            "readonly" => Ok(Self::Readonly),
            "ephemeral" => Ok(Self::Ephemeral),
            other => Err(format!("unknown session_kind: {other}")),
        }
    }

    /// Derive the kind from a parsed workspace spec.
    pub fn derive(workspace: &WorkspaceSpec) -> Self {
        match workspace {
            WorkspaceSpec::Empty => Self::Ephemeral,
            WorkspaceSpec::Git { read_only: true, .. } => Self::Readonly,
            WorkspaceSpec::Git { .. } => Self::Git,
            WorkspaceSpec::LocalMount { .. } => Self::Ephemeral,
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
    pub user_id: Option<String>,
    pub status: SessionStatus,
    pub host_id: Option<HostId>,
    /// In-memory `SandboxId` of the live sandbox serving this
    /// session. `None` in `Pending` (sandbox not created yet) /
    /// `Idle` (sandbox evicted) / `Dead` (host died) /
    /// `Completed` / `Failed`.
    #[serde(default)]
    pub sandbox_id: Option<SandboxId>,
    pub image: ImageRef,
    pub workspace: WorkspaceSpec,
    #[serde(default)]
    pub harness: HarnessSpec,
    /// Derived from `workspace` at create time. Persisted as a
    /// string so the constraint can be widened in future migrations.
    pub session_kind: SessionKind,
    /// `engram/sessions/<id>` for `SessionKind::Git`; `None` for
    /// `Readonly` / `Ephemeral`. Set by the coord at create time;
    /// stable across resumes.
    #[serde(default)]
    pub checkpoint_branch: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
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
            let trimmed = via_serde.trim_matches('"');
            assert_eq!(s.as_str(), trimmed, "as_str must match wire format");
        }
    }

    #[test]
    fn image_ref_accessors() {
        let img = ImageRef::Registry {
            repo: "cortex/api".into(),
            tag: "warm-2026".into(),
        };
        assert_eq!(img.repo(), "cortex/api");
        assert_eq!(img.tag(), "warm-2026");
    }

    #[test]
    fn workspace_helpers_match_variants() {
        let empty = WorkspaceSpec::Empty;
        assert_eq!(empty.git_url(), None);
        assert_eq!(empty.git_branch(), None);
        assert!(!empty.is_writable_git());

        let git_rw = WorkspaceSpec::Git {
            url: "https://x/y.git".into(),
            branch: "main".into(),
            read_only: false,
        };
        assert_eq!(git_rw.git_url(), Some("https://x/y.git"));
        assert_eq!(git_rw.git_branch(), Some("main"));
        assert!(git_rw.is_writable_git());

        let git_ro = WorkspaceSpec::Git {
            url: "https://x/y.git".into(),
            branch: "main".into(),
            read_only: true,
        };
        assert!(!git_ro.is_writable_git());

        let mount = WorkspaceSpec::LocalMount {
            host_path: "/Users/me/code".into(),
            guest_path: "/workspace".into(),
            read_only: false,
        };
        assert_eq!(mount.git_url(), None);
        assert!(!mount.is_writable_git());
    }

    #[test]
    fn session_kind_derive_covers_every_workspace_variant() {
        assert_eq!(
            SessionKind::derive(&WorkspaceSpec::Empty),
            SessionKind::Ephemeral
        );
        assert_eq!(
            SessionKind::derive(&WorkspaceSpec::Git {
                url: "u".into(),
                branch: "b".into(),
                read_only: false
            }),
            SessionKind::Git
        );
        assert_eq!(
            SessionKind::derive(&WorkspaceSpec::Git {
                url: "u".into(),
                branch: "b".into(),
                read_only: true
            }),
            SessionKind::Readonly
        );
        // LocalMount is always Ephemeral — the read_only flag here
        // is mount-policy only, not checkpoint-policy.
        for ro in [false, true] {
            assert_eq!(
                SessionKind::derive(&WorkspaceSpec::LocalMount {
                    host_path: "/h".into(),
                    guest_path: "/g".into(),
                    read_only: ro,
                }),
                SessionKind::Ephemeral
            );
        }
    }

    #[test]
    fn session_kind_round_trips_via_as_str_parse() {
        for k in [SessionKind::Git, SessionKind::Readonly, SessionKind::Ephemeral] {
            assert_eq!(SessionKind::parse(k.as_str()).unwrap(), k);
        }
        assert!(SessionKind::parse("local").is_err());
        assert!(SessionKind::parse("bogus").is_err());
    }

    #[test]
    fn harness_spec_default_is_none() {
        assert!(HarnessSpec::default().is_none());
        assert!(!HarnessSpec::Builtin { name: "claude".into() }.is_none());
    }

    #[test]
    fn workspace_spec_round_trips_through_json() {
        let cases = vec![
            WorkspaceSpec::Empty,
            WorkspaceSpec::Git {
                url: "https://github.com/x/y.git".into(),
                branch: "main".into(),
                read_only: false,
            },
            WorkspaceSpec::LocalMount {
                host_path: "/Users/me/code".into(),
                guest_path: "/workspace".into(),
                read_only: true,
            },
        ];
        for ws in cases {
            let blob = serde_json::to_string(&ws).unwrap();
            let back: WorkspaceSpec = serde_json::from_str(&blob).unwrap();
            assert_eq!(back, ws);
        }
    }

    #[test]
    fn harness_spec_round_trips_through_json() {
        let cases = vec![
            HarnessSpec::None,
            HarnessSpec::Builtin {
                name: "claude".into(),
            },
        ];
        for h in cases {
            let blob = serde_json::to_string(&h).unwrap();
            let back: HarnessSpec = serde_json::from_str(&blob).unwrap();
            assert_eq!(back, h);
        }
    }

    #[test]
    fn image_ref_round_trips_through_json() {
        let img = ImageRef::Registry {
            repo: "cortex/api".into(),
            tag: "warm-2026".into(),
        };
        let blob = serde_json::to_string(&img).unwrap();
        let back: ImageRef = serde_json::from_str(&blob).unwrap();
        assert_eq!(back, img);
    }

    #[test]
    fn session_round_trips_through_json() {
        let original = Session {
            id: SessionId::new(),
            user_id: Some("u1".into()),
            status: SessionStatus::Active,
            host_id: Some(HostId::new()),
            sandbox_id: Some(SandboxId::new()),
            image: ImageRef::Registry {
                repo: "cortex/api".into(),
                tag: "warm-20260101T000000Z".into(),
            },
            workspace: WorkspaceSpec::Git {
                url: "https://github.com/cortex/api.git".into(),
                branch: "main".into(),
                read_only: false,
            },
            harness: HarnessSpec::Builtin {
                name: "claude".into(),
            },
            session_kind: SessionKind::Git,
            checkpoint_branch: Some(checkpoint_branch_for(SessionId::new())),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
        };
        let blob = serde_json::to_string(&original).unwrap();
        let back: Session = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.id, original.id);
        assert_eq!(back.image, original.image);
        assert_eq!(back.workspace, original.workspace);
        assert_eq!(back.harness, original.harness);
        assert_eq!(back.session_kind, original.session_kind);
        assert_eq!(back.checkpoint_branch, original.checkpoint_branch);
    }

    #[test]
    fn checkpoint_branch_uses_engram_sessions_namespace() {
        let id = SessionId::new();
        let branch = checkpoint_branch_for(id);
        assert!(branch.starts_with("engram/sessions/"));
        assert!(branch.ends_with(&id.to_string()));
    }
}
