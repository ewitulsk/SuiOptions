//! `sui-test-validator` localnet wrapper.
//!
//! Spawns `sui start --force-regenesis --with-faucet=<port>
//! --fullnode-rpc-port=<port> --data-ingestion-dir=<dir>` in a tempdir on
//! randomized ports. Polls the RPC until it returns a chain identifier;
//! then funds a freshly-generated deployer keypair via the faucet.
//!
//! ## sui binary version
//!
//! The harness defaults to whatever `sui` is on PATH. **It must match the
//! Move framework version that the contracts' `Move.lock` pins** (see
//! `contracts/Move.lock` → `[pinned.testnet.Sui].rev`). The rust
//! workspace tracks `framework/mainnet` for its sui-sdk dependencies and
//! `sui_move_build::BuildConfig::new_for_testing` compiles against the
//! same framework — so the local sui binary needs to know about exactly
//! that framework version too. A mismatch surfaces as
//! `VMVerificationOrDeserializationError` on publish.
//!
//! Override with `SUI_BIN=/path/to/sui` if your system sui is older than
//! the framework `Move.lock` pins. To build a matching binary from source:
//!
//! ```sh
//! cargo install --git https://github.com/MystenLabs/sui \
//!   --branch framework/mainnet --locked sui
//! ```
//!
//! `Drop` kills the child and removes the tempdir.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::base_types::SuiAddress;
use sui_types::crypto::{get_account_key_pair, SuiKeyPair};
use tempfile::TempDir;
use tracing::{debug, info, warn};

use shared::sui_client::Signer;

/// Owns a running `sui start` child process plus the tempdir it writes
/// into. Drop tears both down.
pub struct LocalNet {
    pub rpc_url: String,
    pub faucet_url: String,
    pub data_ingestion_dir: std::path::PathBuf,
    /// Funded deployer / admin keypair. The localnet faucet has dropped
    /// gas into this address before `start()` returns.
    pub deployer: Signer,
    /// Connected RPC client to the localnet.
    pub client: SuiClient,
    /// Tempdir holding the sui network config + checkpoints. Survives as
    /// long as `self` does.
    _tempdir: TempDir,
    /// Child handle. Dropped explicitly in `Drop` so the process tree
    /// goes down with the harness.
    child: Option<Child>,
}

impl LocalNet {
    /// Bring up a fresh localnet. Caps total wait at ~30s before erroring.
    /// Caller must hold the returned [`LocalNet`] for the lifetime of the
    /// test — dropping it kills the network.
    pub async fn spawn() -> Result<Self> {
        let _ = tracing_subscriber::fmt::try_init();
        let tempdir = TempDir::new().context("creating sui localnet tempdir")?;
        let ingestion = tempdir.path().join("checkpoints");
        std::fs::create_dir_all(&ingestion).context("creating data-ingestion-dir")?;

        let rpc_port = free_port()?;
        let faucet_port = free_port()?;

        info!(
            rpc_port,
            faucet_port,
            dir = %tempdir.path().display(),
            "starting sui localnet"
        );

        // `--epoch-duration-ms 60000` keeps epoch noise out of tests.
        // Stdout/err go to the tempdir for debuggability.
        let log_path = tempdir.path().join("sui.log");
        let log_file = std::fs::File::create(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        let log_file_err = log_file
            .try_clone()
            .context("dup'ing sui log file for stderr")?;

        let sui_bin = std::env::var("SUI_BIN").unwrap_or_else(|_| "sui".to_string());
        let child = Command::new(&sui_bin)
            .arg("start")
            .arg("--force-regenesis")
            .arg(format!("--with-faucet=127.0.0.1:{faucet_port}"))
            .arg("--fullnode-rpc-port")
            .arg(rpc_port.to_string())
            .arg("--data-ingestion-dir")
            .arg(&ingestion)
            .arg("--epoch-duration-ms")
            .arg("60000")
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_err))
            .spawn()
            .with_context(|| format!("spawning `{sui_bin} start` — is sui on PATH? Set SUI_BIN to override."))?;

        let rpc_url = format!("http://127.0.0.1:{rpc_port}");
        let faucet_url = format!("http://127.0.0.1:{faucet_port}");

        // Wait for the RPC to answer `sui_getChainIdentifier`. Generous
        // timeout — first boot compiles genesis on a cold machine.
        let client = wait_for_rpc(&rpc_url, Duration::from_secs(30))
            .await
            .with_context(|| {
                format!(
                    "sui localnet RPC never came up — see logs at {}",
                    log_path.display()
                )
            })?;

        // Fresh deployer keypair, funded via the faucet.
        let deployer = generate_keypair();
        fund_via_faucet(&faucet_url, deployer.address)
            .await
            .context("funding deployer via faucet")?;
        // The faucet returns before the gas object is queryable on the
        // fullnode — wait until at least one coin shows up.
        wait_for_funds(&client, deployer.address, Duration::from_secs(15))
            .await
            .context("waiting for deployer gas to land")?;

        info!(
            rpc_url = %rpc_url,
            faucet_url = %faucet_url,
            deployer = %deployer.address,
            "localnet ready"
        );

        Ok(Self {
            rpc_url,
            faucet_url,
            data_ingestion_dir: ingestion,
            deployer,
            client,
            _tempdir: tempdir,
            child: Some(child),
        })
    }
}

impl Drop for LocalNet {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            if let Err(e) = c.kill() {
                warn!(error = %e, "failed to kill sui localnet child");
            }
            // Best-effort wait so the OS reclaims the PID before tempdir
            // cleanup. Don't block forever — if sui wedged, leak.
            let _ = c.wait();
        }
    }
}

/// Bind a TCP listener on port 0, take the assigned port, then drop. There's
/// a small race window before the test sub-process reclaims the port, but
/// in practice it's tight enough not to matter at our test concurrency.
fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").context("binding ephemeral port")?;
    let port = l.local_addr()?.port();
    drop(l);
    Ok(port)
}

fn generate_keypair() -> Signer {
    // Sui's `get_account_key_pair()` returns an Ed25519 keypair seeded
    // from OsRng — no collisions across concurrent test runs.
    let (address, kp) = get_account_key_pair();
    Signer {
        keypair: SuiKeyPair::Ed25519(kp),
        address,
    }
}

async fn wait_for_rpc(rpc_url: &str, timeout: Duration) -> Result<SuiClient> {
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<anyhow::Error> = None;
    while Instant::now() < deadline {
        match SuiClientBuilder::default().build(rpc_url).await {
            Ok(client) => match client.read_api().get_chain_identifier().await {
                Ok(id) => {
                    debug!(chain_id = %id, "localnet rpc up");
                    return Ok(client);
                }
                Err(e) => last_err = Some(anyhow!("get_chain_identifier: {e}")),
            },
            Err(e) => last_err = Some(anyhow!("build SuiClient: {e}")),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(last_err.unwrap_or_else(|| anyhow!("rpc never came up within {:?}", timeout)))
}

async fn fund_via_faucet(faucet_url: &str, recipient: SuiAddress) -> Result<()> {
    // Sui v2 faucet endpoint: `POST /v2/gas` with a FixedAmountRequest.
    let url = format!("{faucet_url}/v2/gas");
    let body = serde_json::json!({
        "FixedAmountRequest": {
            "recipient": recipient.to_string(),
        }
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building reqwest client")?;
    // Localnet faucet sometimes 503s in the first few seconds after RPC
    // is up; retry briefly.
    let mut last: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let resp = client.post(&url).json(&body).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                debug!(?recipient, "faucet funded");
                return Ok(());
            }
            Ok(r) => {
                last = Some(format!("status {}: {}", r.status(), r.text().await.unwrap_or_default()));
            }
            Err(e) => last = Some(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("faucet refused to fund {recipient}: {}", last.unwrap_or_default())
}

async fn wait_for_funds(
    client: &SuiClient,
    address: SuiAddress,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let coins = client
            .coin_read_api()
            .get_coins(address, None, None, Some(1))
            .await;
        if let Ok(page) = coins {
            if !page.data.is_empty() {
                debug!(%address, "deployer gas observed");
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("no SUI coins observed for {address} within {:?}", timeout)
}
