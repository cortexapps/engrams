//! User identity + web-session persistence seams (ADR 0031).
//!
//! Split from [`MetadataStore`](super::MetadataStore) into their own traits
//! so the Postgres impl can satisfy all three independently and the test
//! stores don't need to grow user methods. In production one
//! `Arc<PostgresStore>` implements all three.
//!
//! - [`UserStore`] — the `users` + `user_tokens` rows. JIT login upserts a
//!   user; the admin API mutates role/active; the profile API seals a
//!   per-user Claude token here.
//! - [`WebSessionStore`] — the `web_sessions` rows backing the HttpOnly
//!   cookie. DB-backed so logout/deprovision is an instant row delete.

use async_trait::async_trait;

use crate::error::MetaError;
use crate::types::ids::UserId;
use crate::types::user::{Role, RoleSource, User, UserToken};

#[async_trait]
pub trait UserStore: Send + Sync {
    /// JIT upsert keyed by email. On first sight inserts a row with
    /// `default_role` / `default_role_source`; on a returning login it only
    /// refreshes `display_name` + `updated_at` and **never** touches the
    /// role (so a manual promotion/demotion survives subsequent logins).
    async fn upsert_user_by_email(
        &self,
        email: &str,
        display_name: Option<&str>,
        default_role: Role,
        default_role_source: RoleSource,
    ) -> Result<User, MetaError>;

    async fn get_user(&self, id: UserId) -> Result<User, MetaError>;
    async fn get_user_by_email(&self, email: &str) -> Result<Option<User>, MetaError>;
    async fn list_users(&self) -> Result<Vec<User>, MetaError>;

    /// Admin-driven role change. Records `source` so SCIM-later can respect
    /// `Manual` as authoritative.
    async fn set_user_role(
        &self,
        id: UserId,
        role: Role,
        source: RoleSource,
    ) -> Result<User, MetaError>;

    /// Toggle the deprovisioning gate.
    async fn set_user_active(&self, id: UserId, active: bool) -> Result<User, MetaError>;

    // ---- per-user sealed tokens ----

    /// Insert-or-replace a sealed per-user token (keyed by `(user_id, kind)`).
    async fn upsert_user_token(&self, token: UserToken) -> Result<(), MetaError>;

    /// Fetch a sealed token, or `None` if the user has saved none of this kind.
    async fn get_user_token(
        &self,
        user_id: UserId,
        kind: &str,
    ) -> Result<Option<UserToken>, MetaError>;

    async fn delete_user_token(&self, user_id: UserId, kind: &str) -> Result<(), MetaError>;
}

#[async_trait]
pub trait WebSessionStore: Send + Sync {
    async fn create_web_session(
        &self,
        session: crate::types::user::WebSession,
    ) -> Result<(), MetaError>;

    /// Resolve a cookie's token hash to its (still-valid) session + owning
    /// user. Impls must filter out expired and inactive-user rows so a
    /// deprovisioned user can't ride a live cookie.
    async fn lookup_web_session(
        &self,
        token_hash: &[u8],
    ) -> Result<Option<(crate::types::user::WebSession, User)>, MetaError>;

    async fn revoke_web_session(&self, token_hash: &[u8]) -> Result<(), MetaError>;

    /// Kill every session for a user (logout-everywhere / deprovision).
    async fn revoke_all_for_user(&self, user_id: UserId) -> Result<(), MetaError>;

    /// Delete expired rows; returns the count swept.
    async fn sweep_expired(&self) -> Result<u64, MetaError>;
}
