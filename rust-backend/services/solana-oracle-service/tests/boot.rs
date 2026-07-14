//! Boot smoke test: the shared engine wired from the SOLANA catalog. Feed
//! discovery runs against a stub solana-token-info server (exactly what
//! `main` does, minus clap), then `oracle_service::run` is spawned and the
//! REST surface is asserted up. The stub also stands in for Hermes /
//! Benchmarks so the test never touches the network — the SSE subscriber
//! just retries against it and `upstream_healthy` stays false.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use axum::routing::get;
use axum::{Json, Router};
use solana_token_info_client::{ProgramInfo, SupportedToken, TokenInfoClient};

const FEED: &str = "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";

fn token(ticker: &str, mint: &str, pyth_feed_id: Option<&str>) -> SupportedToken {
    SupportedToken {
        mint: mint.into(),
        ticker: ticker.into(),
        name: ticker.into(),
        logo_uri: None,
        decimals: 8,
        pyth_feed_id: pyth_feed_id.map(Into::into),
        enabled: true,
    }
}

/// Serve `router` on an ephemeral port; returns the bound address.
async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

#[tokio::test]
async fn run_boots_from_solana_token_info_catalog() {
    // Stub solana-token-info: /program-info + /tokens (one token with a Pyth
    // feed, one without — only the former becomes a subscription).
    let program_info = ProgramInfo {
        options_core_program_id: "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t".into(),
        auction_venue_program_id: "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk".into(),
        options_vault_program_id: "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe".into(),
        config_pda: "cfg".into(),
        treasury_pda: "treas".into(),
        admin: "adm".into(),
        network: "devnet".into(),
        deployed_at: String::new(),
        initialize_signature: None,
        test_tokens: None,
    };
    let tokens = vec![
        token("TBTC", "So11111111111111111111111111111111111111112", Some(FEED)),
        token("TUSDC", "11111111111111111111111111111111", None),
    ];
    let stub = Router::new()
        .route(
            "/program-info",
            get(move || async move { Json(program_info) }),
        )
        .route("/tokens", get(move || async move { Json(tokens) }));
    let stub_addr = serve(stub).await;
    let stub_url = format!("http://{stub_addr}");

    // Discovery, exactly as solana-oracle-service's main does it.
    let snapshot = TokenInfoClient::new(&stub_url)
        .fetch_blocking_until_ready(3, Duration::from_millis(100))
        .await
        .unwrap();
    let feeds =
        oracle_service::resolve_feeds(snapshot.tokens.iter().filter_map(|t| t.pyth_feed().ok()))
            .unwrap();
    assert_eq!(feeds.len(), 1);

    // Missing secrets file → anonymous tier, must not block boot.
    let secrets = oracle_service::load_secrets(Path::new("/nonexistent/secrets.toml")).unwrap();

    // Reserve an ephemeral port for the service itself.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind_addr = probe.local_addr().unwrap();
    drop(probe);

    let cfg = oracle_service::Config {
        environment: "test".into(),
        bind_addr,
        token_info_url: stub_url.clone(),
        hermes_url: stub_url.clone(),
        benchmarks_url: stub_url,
    };
    tokio::spawn(oracle_service::run(cfg, secrets, feeds.clone()));

    // /health comes up.
    let http = reqwest::Client::new();
    let mut healthy = false;
    for _ in 0..50 {
        if let Ok(resp) = http.get(format!("http://{bind_addr}/health")).send().await {
            if resp.status().is_success() {
                healthy = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(healthy, "service did not serve /health");

    // /prices reflects the discovered feed set: no live prices yet and the
    // (stubbed) upstream is unhealthy.
    let prices: serde_json::Value = http
        .get(format!("http://{bind_addr}/prices"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(prices["upstream_healthy"], serde_json::json!(false));
    assert_eq!(prices["prices"].as_array().unwrap().len(), 0);

    // An unknown-but-valid feed id 404s (nothing cached).
    let resp = http
        .get(format!("http://{bind_addr}/prices/{FEED}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}
