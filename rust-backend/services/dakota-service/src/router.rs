//! The public router.
//!
//! Three tiers:
//!
//! - **open** — health, and the webhook receiver. The webhook cannot carry our
//!   JWT; it authenticates itself with an Ed25519 signature instead.
//! - **authenticated** — everything customer-facing. `require_auth` verifies
//!   the token with auth-service and inserts the claims; each handler then
//!   scopes off those claims.
//! - **admin** — control plane, gated by `require_admin` so a business or
//!   individual token cannot reach it even if it guesses the path.
//!
//! The admin routes also re-check `require_admin()` inside their handlers.
//! That is deliberate belt-and-braces: the layer is easy to drop when adding a
//! route, and these are the operations where being wrong is expensive.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use auth_client::AuthClient;
use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::handlers::{accounts, admin, catalog, customers, flows, wallets};
use crate::state::AppState;
use crate::webhook;

pub fn build(state: Arc<AppState>, auth: Arc<AuthClient>, allowed_origins: &[String]) -> Result<Router> {
    let cors = build_cors(allowed_origins)?;

    let open = Router::new()
        .route("/health", get(crate::handlers::ready))
        .route("/webhooks/dakota", post(webhook::receive));

    let authed = Router::new()
        .route("/catalog", get(catalog::get_catalog))
        .route("/rates", get(catalog::get_rates))
        .route("/customers", get(customers::list_customers).post(customers::create_customer))
        .route("/customers/:id", get(customers::get_customer))
        .route("/customers/:id/capabilities", get(customers::get_capabilities))
        .route("/customers/:id/invite", post(customers::create_invite))
        .route("/customers/:id/recipients", post(accounts::create_recipient))
        .route("/recipients/:id/destinations", post(accounts::create_destination))
        .route("/accounts", get(accounts::list_accounts).post(accounts::create_account))
        .route("/accounts/:id", get(accounts::get_account))
        .route("/flows", get(flows::get_flows))
        .route("/flows/feed", get(flows::feed))
        .route("/flows/:customer_id", get(flows::customer_feed))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&auth),
            auth_client::require_auth,
        ));

    let admin_routes = Router::new()
        .route("/admin/assets", put(catalog::upsert_asset))
        .route("/admin/assets/:id", delete(catalog::delete_asset))
        .route("/admin/rates", post(catalog::set_rates))
        .route("/admin/sub-clients", get(customers::list_sub_clients))
        .route("/admin/transactions", get(accounts::list_transactions))
        .route("/admin/sandbox/onboarding", post(admin::simulate_onboarding))
        .route("/admin/sandbox/inbound", post(admin::simulate_inbound))
        .route("/admin/webhooks", get(admin::list_webhooks))
        .route("/admin/webhooks/register", post(admin::register_webhook))
        .route("/admin/resync", post(admin::resync))
        .route("/admin/treasury", get(wallets::list))
        .route("/admin/treasury/setup", post(wallets::setup))
        .route("/admin/treasury/:id/balances", get(wallets::balances))
        .route("/admin/treasury/:id/send", post(wallets::send))
        .route_layer(axum::middleware::from_fn_with_state(
            auth,
            auth_client::require_admin,
        ));

    Ok(open
        .merge(authed)
        .merge(admin_routes)
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ))
        .layer(cors))
}

pub async fn serve(
    addr: SocketAddr,
    state: Arc<AppState>,
    auth: Arc<AuthClient>,
    allowed_origins: &[String],
) -> Result<()> {
    let app = build(state, auth, allowed_origins)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "dakota-service listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_cors(allowed_origins: &[String]) -> Result<CorsLayer> {
    if allowed_origins.iter().any(|o| o == "*") {
        return Ok(CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any));
    }
    let mut origins = Vec::with_capacity(allowed_origins.len());
    for o in allowed_origins {
        origins.push(o.parse()?);
    }
    Ok(CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(Any)
        .allow_headers(Any))
}
