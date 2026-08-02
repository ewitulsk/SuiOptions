//! Identity-store queries.
//!
//! Everything that mutates more than one row runs inside a transaction —
//! registration in particular must consume the invite and create the user in
//! one atomic step, or a crash mid-way would burn an invite with no account to
//! show for it.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use diesel::prelude::*;
use uuid::Uuid;

use super::models::{Identity, IdentityKind, Invite, NewIdentity, NewInvite, NewUser, Role, User};
use super::schema::{identities, invites, users};
use super::DbPool;

#[derive(Clone)]
pub struct Repo {
    pool: Arc<DbPool>,
}

/// An identity paired with the account it belongs to — what every login path
/// actually needs.
pub struct ResolvedIdentity {
    pub identity: Identity,
    pub user: User,
}

impl Repo {
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }

    fn conn(&self) -> Result<r2d2::PooledConnection<diesel::r2d2::ConnectionManager<PgConnection>>> {
        self.pool.get().context("checking out a db connection")
    }

    // ------------------------------------------------------------- lookups

    /// Resolve a login method to its account. Returns `None` when the
    /// identifier is unknown; callers must not distinguish that from a bad
    /// password in what they return to the client.
    pub fn find_identity(
        &self,
        kind: IdentityKind,
        identifier: &str,
    ) -> Result<Option<ResolvedIdentity>> {
        let mut conn = self.conn()?;
        let found = identities::table
            .inner_join(users::table)
            .filter(identities::kind.eq(kind.as_str()))
            .filter(identities::identifier.eq(identifier))
            .select((Identity::as_select(), User::as_select()))
            .first::<(Identity, User)>(&mut conn)
            .optional()
            .context("looking up identity")?;
        Ok(found.map(|(identity, user)| ResolvedIdentity { identity, user }))
    }

    pub fn get_user(&self, user_id: Uuid) -> Result<Option<User>> {
        let mut conn = self.conn()?;
        users::table
            .find(user_id)
            .select(User::as_select())
            .first(&mut conn)
            .optional()
            .context("loading user")
    }

    pub fn list_identities(&self, user_id: Uuid) -> Result<Vec<Identity>> {
        let mut conn = self.conn()?;
        identities::table
            .filter(identities::user_id.eq(user_id))
            .order(identities::created_at.asc())
            .select(Identity::as_select())
            .load(&mut conn)
            .context("listing identities")
    }

    pub fn touch_identity(&self, identity_id: Uuid) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(identities::table.find(identity_id))
            .set(identities::last_used_at.eq(Utc::now()))
            .execute(&mut conn)
            .context("stamping identity last_used_at")?;
        Ok(())
    }

    // -------------------------------------------------------- registration

    /// Redeem an invite into a brand-new account with its first identity.
    ///
    /// Atomic: the invite is claimed with a conditional UPDATE, so two
    /// simultaneous redemptions of the same link cannot both win.
    pub fn register_with_invite(
        &self,
        invite_id: Uuid,
        kind: IdentityKind,
        identifier: &str,
        secret_hash: Option<String>,
    ) -> Result<User> {
        let mut conn = self.conn()?;
        conn.transaction(|conn| {
            let invite: Invite = invites::table
                .find(invite_id)
                .select(Invite::as_select())
                .first(conn)
                .optional()
                .context("loading invite")?
                .ok_or_else(|| anyhow::anyhow!("unknown invite"))?;

            if invite.consumed_at.is_some() {
                bail!("invite already used");
            }
            if invite.expires_at <= Utc::now() {
                bail!("invite expired");
            }

            let user_id = Uuid::new_v4();
            diesel::insert_into(users::table)
                .values(NewUser {
                    id: user_id,
                    role: invite.role.clone(),
                    scope_id: invite.scope_id.clone(),
                })
                .execute(conn)
                .context("inserting user")?;

            diesel::insert_into(identities::table)
                .values(NewIdentity {
                    id: Uuid::new_v4(),
                    user_id,
                    kind: kind.as_str().to_string(),
                    identifier: identifier.to_string(),
                    secret_hash,
                })
                .execute(conn)
                .context("inserting first identity")?;

            // Conditional claim: `consumed_at IS NULL` in the predicate means a
            // concurrent redemption updates 0 rows and loses.
            let claimed = diesel::update(
                invites::table
                    .find(invite_id)
                    .filter(invites::consumed_at.is_null()),
            )
            .set((
                invites::consumed_at.eq(Utc::now()),
                invites::consumed_by.eq(user_id),
            ))
            .execute(conn)
            .context("claiming invite")?;
            if claimed != 1 {
                bail!("invite already used");
            }

            users::table
                .find(user_id)
                .select(User::as_select())
                .first(conn)
                .context("reloading new user")
        })
    }

    /// Create an account directly, bypassing invites. Used only to bootstrap an
    /// allowlisted admin wallet on first login — there is no other unsolicited
    /// account-creation path.
    pub fn create_user_with_identity(
        &self,
        role: Role,
        scope_id: Option<String>,
        kind: IdentityKind,
        identifier: &str,
        secret_hash: Option<String>,
    ) -> Result<User> {
        let mut conn = self.conn()?;
        conn.transaction(|conn| {
            let user_id = Uuid::new_v4();
            diesel::insert_into(users::table)
                .values(NewUser {
                    id: user_id,
                    role: role.as_str().to_string(),
                    scope_id,
                })
                .execute(conn)
                .context("inserting user")?;
            diesel::insert_into(identities::table)
                .values(NewIdentity {
                    id: Uuid::new_v4(),
                    user_id,
                    kind: kind.as_str().to_string(),
                    identifier: identifier.to_string(),
                    secret_hash,
                })
                .execute(conn)
                .context("inserting identity")?;
            users::table
                .find(user_id)
                .select(User::as_select())
                .first(conn)
                .context("reloading new user")
        })
    }

    // ---------------------------------------------------- identity linking

    /// Attach an additional login method to an existing account.
    pub fn add_identity(
        &self,
        user_id: Uuid,
        kind: IdentityKind,
        identifier: &str,
        secret_hash: Option<String>,
    ) -> Result<Identity> {
        let mut conn = self.conn()?;
        diesel::insert_into(identities::table)
            .values(NewIdentity {
                id: Uuid::new_v4(),
                user_id,
                kind: kind.as_str().to_string(),
                identifier: identifier.to_string(),
                secret_hash,
            })
            .returning(Identity::as_returning())
            .get_result(&mut conn)
            .context("adding identity")
    }

    /// Remove a login method. Refuses to remove the last one — an account with
    /// no identities is unreachable forever, with no recovery path since we
    /// store no email.
    pub fn remove_identity(&self, user_id: Uuid, identity_id: Uuid) -> Result<()> {
        let mut conn = self.conn()?;
        conn.transaction(|conn| {
            let remaining: i64 = identities::table
                .filter(identities::user_id.eq(user_id))
                .count()
                .get_result(conn)
                .context("counting identities")?;
            if remaining <= 1 {
                bail!("cannot remove the only login method on this account");
            }
            let deleted = diesel::delete(
                identities::table
                    .find(identity_id)
                    .filter(identities::user_id.eq(user_id)),
            )
            .execute(conn)
            .context("deleting identity")?;
            if deleted == 0 {
                bail!("identity not found on this account");
            }
            Ok(())
        })
    }

    // ------------------------------------------------------------- invites

    pub fn create_invite(
        &self,
        role: Role,
        scope_id: Option<String>,
        created_by: Option<Uuid>,
        label: Option<String>,
        ttl_secs: i64,
    ) -> Result<Invite> {
        if role.requires_scope() && scope_id.is_none() {
            bail!("role {} requires a scope_id", role.as_str());
        }
        let mut conn = self.conn()?;
        let expires_at: DateTime<Utc> = Utc::now() + Duration::seconds(ttl_secs);
        diesel::insert_into(invites::table)
            .values(NewInvite {
                id: Uuid::new_v4(),
                role: role.as_str().to_string(),
                scope_id,
                created_by,
                label,
                expires_at,
            })
            .returning(Invite::as_returning())
            .get_result(&mut conn)
            .context("creating invite")
    }

    /// Read an invite without consuming it, so the signup page can show what it
    /// is for before the user commits.
    pub fn peek_invite(&self, invite_id: Uuid) -> Result<Option<Invite>> {
        let mut conn = self.conn()?;
        invites::table
            .find(invite_id)
            .select(Invite::as_select())
            .first(&mut conn)
            .optional()
            .context("peeking invite")
    }
}
