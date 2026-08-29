//! Interval cranks: the spoke's permissionless `syncState()` (default
//! 5 min) and the hub's `build_config_sync` + `endpoint_relayer::send`
//! (default 15 min, plus an immediate push whenever the hub watcher
//! observes a pause/risk/identity event). Repeated submit failures fire
//! the tx-failed alert per docs/tx-alerting.md.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use crate::evm::SpokeChain;
use crate::hub::HubChain;
use crate::state::AppState;

const ALERT_AFTER_CONSECUTIVE: u32 = 3;

pub struct CrankParams {
    pub state: Arc<AppState>,
    pub hub: Arc<dyn HubChain>,
    pub spoke: Arc<dyn SpokeChain>,
    pub state_sync_interval: Duration,
    pub config_sync_interval: Duration,
}

pub fn spawn(p: CrankParams) {
    let state_sync = StateSyncCrank { spoke: Arc::clone(&p.spoke), interval: p.state_sync_interval };
    tokio::spawn(async move { state_sync.run().await });

    tokio::spawn(async move { config_sync_loop(p).await });
}

struct StateSyncCrank {
    spoke: Arc<dyn SpokeChain>,
    interval: Duration,
}

impl StateSyncCrank {
    async fn run(self) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut consecutive: u32 = 0;
        loop {
            ticker.tick().await;
            match self.spoke.sync_state().await {
                Ok(tx) => {
                    consecutive = 0;
                    info!(%tx, "spoke syncState() submitted");
                }
                Err(e) => {
                    consecutive += 1;
                    if consecutive >= ALERT_AFTER_CONSECUTIVE {
                        error!(
                            alert_id = "tx-failed-vault-messenger",
                            op = "sync_state",
                            consecutive,
                            error = %format!("{e:#}"),
                            "spoke syncState() failing repeatedly — the hub's spoke view is going stale"
                        );
                    } else {
                        warn!(error = %format!("{e:#}"), "spoke syncState() failed; next tick retries");
                    }
                }
            }
        }
    }
}

/// ConfigSync fires on its heartbeat cadence AND immediately when the hub
/// watcher raises `config_sync_due` (checked every few seconds).
async fn config_sync_loop(p: CrankParams) {
    let check = Duration::from_secs(5);
    let mut since_last = p.config_sync_interval; // fire once at boot
    let mut consecutive: u32 = 0;
    loop {
        let due_flag = p.state.config_sync_due.swap(false, Ordering::Relaxed);
        if due_flag || since_last >= p.config_sync_interval {
            match p.hub.send_config_sync().await {
                Ok(digest) => {
                    consecutive = 0;
                    since_last = Duration::ZERO;
                    info!(%digest, triggered = due_flag, "ConfigSync pushed");
                }
                Err(e) => {
                    consecutive += 1;
                    // Keep the trigger armed so the next pass retries now.
                    p.state.config_sync_due.store(true, Ordering::Relaxed);
                    if consecutive >= ALERT_AFTER_CONSECUTIVE {
                        error!(
                            alert_id = "tx-failed-vault-messenger",
                            op = "config_sync",
                            consecutive,
                            error = %format!("{e:#}"),
                            "ConfigSync push failing repeatedly"
                        );
                    } else {
                        warn!(error = %format!("{e:#}"), "ConfigSync push failed; retrying");
                    }
                }
            }
        }
        tokio::time::sleep(check).await;
        since_last += check;
    }
}
