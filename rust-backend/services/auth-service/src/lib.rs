//! auth-service.
//!
//! Issues short-lived JWTs and is the single holder of the JWT signing secret:
//! other services gate endpoints by calling the internal `/verify` route (via
//! `auth-client`) rather than verifying tokens themselves.
//!
//! ## Identity model
//!
//! An **account** (`users`) holds a role and an opaque scope. It is reached
//! through one or more **identities** (`identities`), each a different way of
//! proving you are that account — today `password` (username + Argon2id) and
//! `sui_wallet` (address + signed challenge). Because they hang off a shared
//! account, a wallet user can add a password and a password user can add a
//! wallet, and either then signs them in. A new method is a new `kind` value
//! plus a branch in the two `match`es in [`handlers::account`]; nothing else
//! moves.
//!
//! Accounts are created by redeeming an **invite**, minted over the internal
//! port by whichever service knows the caller ought to exist. The lone
//! exception is a wallet listed in `admin_addresses`, auto-provisioned as an
//! admin on first login so the first operator can get in at all.
//!
//! ## No PII
//!
//! The store holds usernames, Sui addresses, roles and opaque scope ids —
//! deliberately no email, no legal name, nothing sourced from KYC. Password
//! recovery is consequently an admin re-invite, not a reset link.
//!
//! ## Two routers on two ports
//!
//! - public (proxied by nginx): `/challenge`, `/login`, `/login/password`,
//!   `/register`, `/refresh`, `/me`, `/identities`, `/invites/preview`.
//! - internal (network-isolated): `/verify`, `/invites`.

pub mod allowlist;
pub mod challenge;
pub mod config;
pub mod db;
pub mod handlers;
pub mod jwt;
pub mod password;
pub mod router;
pub mod state;
pub mod sui_sig;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "auth-service",
    about = "Multi-method identity service. Password or Sui-wallet login, linkable per account; \
             issues + verifies short-lived JWTs carrying a role and scope."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/auth-service/config/config.toml")]
    pub config: PathBuf,

    /// Secrets TOML holding `auth.jwt_secret`. No env-var fallback.
    #[arg(short = 's', long, default_value = "services/auth-service/config/secrets.toml")]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "auth-service",
    cargo_pkg   = "auth-service",
    working_dir = ".",
    description = "Identity service. Username+password or Sui-wallet login (linkable to one \
                   account), invite-gated registration, issues HS256 JWTs carrying role and \
                   scope, and exposes an internal verify route other services delegate to.",
    cli         = crate::Cli,
}
