//! Identity-store behaviour tests.
//!
//! These need a real Postgres — set `AUTH_TEST_DATABASE_URL` and run with
//! `cargo test -p auth-service -- --ignored`. Same convention as
//! option-scheduler's `SCHEDULER_TEST_DATABASE_URL`.

use std::sync::Arc;

use chrono::{Duration, Utc};
use diesel::prelude::*;
use uuid::Uuid;

use super::models::{IdentityKind, Role};
use super::repo::Repo;
use super::{establish_pool, run_migrations, DbPool};

fn test_pool() -> DbPool {
    let url = std::env::var("AUTH_TEST_DATABASE_URL")
        .expect("set AUTH_TEST_DATABASE_URL to run DB tests");
    let pool = establish_pool(&url, 2).expect("pool");
    run_migrations(&pool).expect("migrations");
    let mut conn = pool.get().unwrap();
    // `users` cascades into `identities`; `invites` references users, so it
    // has to go in the same statement.
    diesel::sql_query("TRUNCATE users, identities, invites RESTART IDENTITY CASCADE")
        .execute(&mut conn)
        .expect("truncate");
    pool
}

fn repo() -> Repo {
    Repo::new(Arc::new(test_pool()))
}

/// Mint an open invite for `role`, scoped when the role demands it.
fn open_invite(repo: &Repo, role: Role) -> Uuid {
    let scope = role.requires_scope().then(|| "cus_probe".to_string());
    repo.create_invite(role, scope, None, Some("test".into()), 3600)
        .expect("create invite")
        .id
}

// ------------------------------------------------------------------ linking

#[test]
#[ignore] // requires AUTH_TEST_DATABASE_URL
fn wallet_account_can_add_a_password_and_both_resolve_to_one_user() {
    let repo = repo();
    let user = repo
        .create_user_with_identity(Role::Admin, None, IdentityKind::SuiWallet, "0xabc", None)
        .unwrap();

    repo.add_identity(
        user.id,
        IdentityKind::Password,
        "evan",
        Some(crate::password::hash("correct horse battery").unwrap()),
    )
    .unwrap();

    // Both doors open onto the same account — the whole point of the model.
    let via_wallet = repo
        .find_identity(IdentityKind::SuiWallet, "0xabc")
        .unwrap()
        .expect("wallet identity");
    let via_password = repo
        .find_identity(IdentityKind::Password, "evan")
        .unwrap()
        .expect("password identity");
    assert_eq!(via_wallet.user.id, user.id);
    assert_eq!(via_password.user.id, user.id);
    assert_eq!(repo.list_identities(user.id).unwrap().len(), 2);
}

#[test]
#[ignore]
fn password_account_can_add_a_wallet() {
    let repo = repo();
    let user = repo
        .create_user_with_identity(
            Role::Individual,
            Some("cus_1".into()),
            IdentityKind::Password,
            "jane",
            Some(crate::password::hash("correct horse battery").unwrap()),
        )
        .unwrap();

    repo.add_identity(user.id, IdentityKind::SuiWallet, "0xdef", None)
        .unwrap();

    let via_wallet = repo
        .find_identity(IdentityKind::SuiWallet, "0xdef")
        .unwrap()
        .expect("wallet identity");
    assert_eq!(via_wallet.user.id, user.id);
    // Scope rides on the account, so it applies however you signed in.
    assert_eq!(via_wallet.user.scope_id.as_deref(), Some("cus_1"));
}

#[test]
#[ignore]
fn an_identifier_cannot_be_claimed_twice() {
    let repo = repo();
    let a = repo
        .create_user_with_identity(Role::Admin, None, IdentityKind::SuiWallet, "0xabc", None)
        .unwrap();
    let b = repo
        .create_user_with_identity(Role::Admin, None, IdentityKind::SuiWallet, "0xbbb", None)
        .unwrap();
    assert_ne!(a.id, b.id);

    // b tries to graft a's wallet onto itself — the UNIQUE index is what stops
    // an account takeover here.
    assert!(repo
        .add_identity(b.id, IdentityKind::SuiWallet, "0xabc", None)
        .is_err());
}

#[test]
#[ignore]
fn the_last_identity_cannot_be_removed() {
    let repo = repo();
    let user = repo
        .create_user_with_identity(Role::Admin, None, IdentityKind::SuiWallet, "0xabc", None)
        .unwrap();
    let only = repo.list_identities(user.id).unwrap().remove(0);

    // With no email on file there is no recovery path, so this must not be
    // allowed to succeed.
    assert!(repo.remove_identity(user.id, only.id).is_err());

    let second = repo
        .add_identity(
            user.id,
            IdentityKind::Password,
            "evan",
            Some(crate::password::hash("correct horse battery").unwrap()),
        )
        .unwrap();
    repo.remove_identity(user.id, second.id).expect("second is removable");
    assert_eq!(repo.list_identities(user.id).unwrap().len(), 1);
}

#[test]
#[ignore]
fn cannot_remove_an_identity_belonging_to_someone_else() {
    let repo = repo();
    let victim = repo
        .create_user_with_identity(Role::Admin, None, IdentityKind::SuiWallet, "0xabc", None)
        .unwrap();
    repo.add_identity(victim.id, IdentityKind::Password, "victim", Some("x".into()))
        .unwrap();
    let attacker = repo
        .create_user_with_identity(Role::Admin, None, IdentityKind::SuiWallet, "0xbbb", None)
        .unwrap();
    repo.add_identity(attacker.id, IdentityKind::Password, "attacker", Some("x".into()))
        .unwrap();

    let victim_identity = repo.list_identities(victim.id).unwrap().remove(0);
    assert!(repo.remove_identity(attacker.id, victim_identity.id).is_err());
    assert_eq!(repo.list_identities(victim.id).unwrap().len(), 2);
}

// ------------------------------------------------------------------ invites

#[test]
#[ignore]
fn invite_carries_role_and_scope_onto_the_new_account() {
    let repo = repo();
    let invite = open_invite(&repo, Role::Business);
    let user = repo
        .register_with_invite(invite, IdentityKind::Password, "acme", Some("hash".into()))
        .unwrap();

    // Authority comes from the invite, never from the registration body.
    assert_eq!(user.role, "business");
    assert_eq!(user.scope_id.as_deref(), Some("cus_probe"));
}

#[test]
#[ignore]
fn an_invite_is_single_use() {
    let repo = repo();
    let invite = open_invite(&repo, Role::Individual);
    repo.register_with_invite(invite, IdentityKind::Password, "first", Some("hash".into()))
        .expect("first redemption wins");

    let second =
        repo.register_with_invite(invite, IdentityKind::Password, "second", Some("hash".into()));
    assert!(second.is_err(), "a spent invite must not mint a second account");
}

#[test]
#[ignore]
fn an_expired_invite_is_refused() {
    let repo = repo();
    let invite = repo
        .create_invite(Role::Individual, Some("cus_1".into()), None, None, -60)
        .unwrap();
    assert!(repo
        .register_with_invite(invite.id, IdentityKind::Password, "late", Some("hash".into()))
        .is_err());
}

#[test]
#[ignore]
fn a_failed_registration_leaves_the_invite_open() {
    let repo = repo();
    repo.create_user_with_identity(
        Role::Individual,
        Some("cus_1".into()),
        IdentityKind::Password,
        "taken",
        Some("hash".into()),
    )
    .unwrap();

    let invite = open_invite(&repo, Role::Individual);
    // Username collision aborts the transaction mid-way.
    assert!(repo
        .register_with_invite(invite, IdentityKind::Password, "taken", Some("hash".into()))
        .is_err());

    // The invite must roll back with it, or a typo would burn the link.
    assert!(repo.peek_invite(invite).unwrap().unwrap().consumed_at.is_none());
    repo.register_with_invite(invite, IdentityKind::Password, "free", Some("hash".into()))
        .expect("invite still redeemable");
}

#[test]
#[ignore]
fn a_scoped_role_requires_a_scope() {
    let repo = repo();
    // A business invite with nothing to be scoped to would mint an account
    // that can see everything or nothing, depending on the caller's care.
    assert!(repo.create_invite(Role::Business, None, None, None, 3600).is_err());
    assert!(repo.create_invite(Role::Individual, None, None, None, 3600).is_err());
    // Admins are unscoped by design.
    assert!(repo.create_invite(Role::Admin, None, None, None, 3600).is_ok());
}

#[test]
#[ignore]
fn peek_reports_expiry_without_consuming() {
    let repo = repo();
    let invite = repo
        .create_invite(Role::Individual, Some("cus_1".into()), None, None, 3600)
        .unwrap();
    let seen = repo.peek_invite(invite.id).unwrap().expect("invite");
    assert!(seen.consumed_at.is_none());
    assert!(seen.expires_at > Utc::now() + Duration::seconds(3000));
    // Still redeemable after peeking.
    assert!(repo
        .register_with_invite(invite.id, IdentityKind::Password, "peeked", Some("h".into()))
        .is_ok());
}

#[test]
#[ignore]
fn unknown_identifier_resolves_to_none() {
    let repo = repo();
    assert!(repo
        .find_identity(IdentityKind::Password, "nobody")
        .unwrap()
        .is_none());
    assert!(repo.peek_invite(Uuid::new_v4()).unwrap().is_none());
}
