//! Row structs for the identity store.

use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde::Serialize;
use uuid::Uuid;

use super::schema::{identities, invites, users};

// ---------------------------------------------------------------------- role

/// Authorization role. Kept as a plain string in the database — this enum is
/// only the parse boundary, so an unknown value from a future version is a
/// clean error rather than a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Unscoped. Full control plane.
    Admin,
    /// Scoped to a Dakota sub-client; may act for customers beneath it.
    Business,
    /// Scoped to a single Dakota customer.
    Individual,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Business => "business",
            Role::Individual => "individual",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "admin" => Role::Admin,
            "business" => Role::Business,
            "individual" => Role::Individual,
            other => anyhow::bail!("unknown role {other:?}"),
        })
    }

    /// Whether this role requires a `scope_id`. Admins are unscoped; everyone
    /// else is meaningless without something to be scoped to.
    pub fn requires_scope(self) -> bool {
        !matches!(self, Role::Admin)
    }
}

// ------------------------------------------------------------------ identity

/// Login method discriminator. New methods are added here and in
/// [`IdentityKind::parse`]; nothing else in the service needs to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityKind {
    /// Username + Argon2id password.
    Password,
    /// Sui address proved by a personal-message signature.
    SuiWallet,
}

impl IdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityKind::Password => "password",
            IdentityKind::SuiWallet => "sui_wallet",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "password" => IdentityKind::Password,
            "sui_wallet" => IdentityKind::SuiWallet,
            other => anyhow::bail!("unknown identity kind {other:?}"),
        })
    }
}

// ------------------------------------------------------------------- queries

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = users)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct User {
    pub id: Uuid,
    pub role: String,
    pub scope_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub disabled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = users)]
pub struct NewUser {
    pub id: Uuid,
    pub role: String,
    pub scope_id: Option<String>,
}

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = identities)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Identity {
    pub id: Uuid,
    pub user_id: Uuid,
    pub kind: String,
    pub identifier: String,
    pub secret_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = identities)]
pub struct NewIdentity {
    pub id: Uuid,
    pub user_id: Uuid,
    pub kind: String,
    pub identifier: String,
    pub secret_hash: Option<String>,
}

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = invites)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Invite {
    pub id: Uuid,
    pub role: String,
    pub scope_id: Option<String>,
    pub created_by: Option<Uuid>,
    pub label: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
    pub consumed_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = invites)]
pub struct NewInvite {
    pub id: Uuid,
    pub role: String,
    pub scope_id: Option<String>,
    pub created_by: Option<Uuid>,
    pub label: Option<String>,
    pub expires_at: DateTime<Utc>,
}
