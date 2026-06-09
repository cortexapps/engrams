//! User identity, roles, and the per-request `Principal` (ADR 0031).
//!
//! engrams had no user identity before 0031 — the coordinator only knew
//! "is this request bearer-authorized". These types model the new identity
//! layer:
//!
//! - [`User`] / [`Role`] / [`RoleSource`] — the `users` table row.
//! - [`UserToken`] — a per-user KEK-sealed credential (today: the Claude
//!   Code OAuth token), same envelope shape as `session_secrets` /
//!   `registry_credentials`.
//! - [`WebSession`] — one opaque human session cookie, hashed at rest.
//! - [`Principal`] — the normalized identity every authentication
//!   mechanism (OIDC, forward-auth, service bearer, synthetic) reduces a
//!   request to. Handlers and the `AdminOnly` extractor consume this.
//!
//! The types live in `engram-core` (not `engram-auth`) so the `UserStore`
//! trait and `Session` can reference them without depending on the
//! verifier crate — mirroring how `RegistryCredential` lives here while
//! the resolver lives in `engram-oci-auth`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::ids::UserId;

/// Coarse authorization role. Two levels only (ADR 0031): `Admin` sees the
/// operator surfaces (fleet/storage/registries/enabled-images/user-admin),
/// `Member` sees their own sessions and profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Member,
}

impl Role {
    /// Stable wire string — matches `serde(rename_all)` and the SQL CHECK
    /// constraint in migration 0046.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    /// Parse the DB/wire string back into a `Role`. Returns `None` for an
    /// unknown value so the row mapper can fail loudly rather than silently
    /// defaulting to `member`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// Where a user's role came from. Seeded now for the SCIM-later design:
/// `Manual` (an admin set it via `PATCH /admin/users`) must survive SCIM
/// group sync, so SCIM only overwrites `Scim`/`Claim`-sourced roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// Set by an operator via the admin API. Authoritative; SCIM won't clobber.
    Manual,
    /// Driven by a SCIM group → role mapping (not implemented yet).
    Scim,
    /// Derived at JIT login (bootstrap-admin allowlist or an OIDC group claim).
    Claim,
}

impl RoleSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Scim => "scim",
            Self::Claim => "claim",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(Self::Manual),
            "scim" => Some(Self::Scim),
            "claim" => Some(Self::Claim),
            _ => None,
        }
    }
}

/// One row in `users`. `email` is the join key across authentication
/// (OIDC/IAP) and provisioning (JIT now, SCIM later). `groups` is seeded
/// empty until SCIM populates it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub display_name: Option<String>,
    pub role: Role,
    pub role_source: RoleSource,
    /// Deprovisioning gate. An inactive user is rejected even if the edge
    /// proxy still authenticates them (SCIM-later sets this false on offboard).
    pub active: bool,
    /// SCIM-later group membership; empty for JIT-provisioned users.
    pub groups: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl User {
    /// Project the row to a request `Principal`.
    pub fn to_principal(&self) -> Principal {
        Principal {
            user_id: self.id,
            email: self.email.clone(),
            display_name: self.display_name.clone(),
            role: self.role,
            active: self.active,
        }
    }
}

/// One row in `user_tokens`: a per-user KEK-sealed credential. Today the
/// only `kind` is `claude_oauth` (the Claude Code OAuth token auto-injected
/// into built-in-Claude sessions). Structurally identical to
/// `session_secrets` — the DEK wraps the plaintext token, the KEK wraps the
/// DEK. Never echoed by any API.
#[derive(Clone, Debug)]
pub struct UserToken {
    pub user_id: UserId,
    /// Discriminator for future per-user credentials. `claude_oauth` for now.
    pub kind: String,
    pub wrapped_dek: Vec<u8>,
    /// 12-byte AES-GCM nonce stored as `bytea` (see `SessionSecrets::nonce`).
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl UserToken {
    /// Well-known `kind` for the Claude Code OAuth token.
    pub const KIND_CLAUDE_OAUTH: &'static str = "claude_oauth";
}

/// One row in `web_sessions`: a single human login. The cookie carries an
/// opaque random token; only its SHA-256 hash is stored, so a DB read can't
/// mint a valid cookie and revocation is a row delete.
#[derive(Clone, Debug)]
pub struct WebSession {
    pub token_hash: Vec<u8>,
    pub user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// Normalized per-request identity. Every `IdentityVerifier` produces one of
/// these (OIDC/forward-auth via JIT upsert; service-bearer/synthetic via a
/// constructed sentinel principal). Handlers read it; `AdminOnly` gates on
/// `role`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Principal {
    pub user_id: UserId,
    pub email: String,
    pub display_name: Option<String>,
    pub role: Role,
    pub active: bool,
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        self.role.is_admin()
    }

    /// Best-effort display name for git attribution: the stored display
    /// name, else the email local-part.
    pub fn git_name(&self) -> String {
        self.display_name.clone().unwrap_or_else(|| {
            self.email
                .split('@')
                .next()
                .unwrap_or(&self.email)
                .to_string()
        })
    }

    /// True when this is the machine/service principal, identified by the
    /// configured `service_email`. Used to skip git `[user]` attribution
    /// (ADR 0031): the service email is not a real commit author — injecting it
    /// makes GitHub reject a squash-merge — and only human-initiated sessions
    /// should be attributed.
    pub fn is_service(&self, service_email: &str) -> bool {
        self.email == service_email
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_wire_round_trip() {
        for r in [Role::Admin, Role::Member] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("nope"), None);
        assert!(Role::Admin.is_admin());
        assert!(!Role::Member.is_admin());
    }

    #[test]
    fn role_source_wire_round_trip() {
        for s in [RoleSource::Manual, RoleSource::Scim, RoleSource::Claim] {
            assert_eq!(RoleSource::parse(s.as_str()), Some(s));
        }
        assert_eq!(RoleSource::parse("nope"), None);
    }

    #[test]
    fn git_name_falls_back_to_email_local_part() {
        let p = Principal {
            user_id: UserId::new(),
            email: "ada@example.com".into(),
            display_name: None,
            role: Role::Member,
            active: true,
        };
        assert_eq!(p.git_name(), "ada");
        let named = Principal {
            display_name: Some("Ada Lovelace".into()),
            ..p
        };
        assert_eq!(named.git_name(), "Ada Lovelace");
    }

    #[test]
    fn is_service_matches_only_the_configured_service_email() {
        let svc = Principal {
            user_id: UserId::new(),
            email: "service@engram.local".into(),
            display_name: Some("Engram Service".into()),
            role: Role::Admin,
            active: true,
        };
        assert!(svc.is_service("service@engram.local"));
        assert!(!svc.is_service("ada@example.com"));
        // A human (different email) is never the service principal, even as admin.
        let human = Principal {
            email: "ada@example.com".into(),
            ..svc
        };
        assert!(!human.is_service("service@engram.local"));
    }
}
