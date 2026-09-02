//! Limits and the kill switch: the policy is `desk_core::limits`; this
//! module adds the persisted kill-switch wrapper (NAV history file) so a
//! −10%-in-7d drawdown survives restarts.

use std::path::PathBuf;

pub use desk_core::limits::*;

/// Persisted rolling-high-water kill switch: latched while NAV sits more
/// than `kill_drawdown` below the window's high water.
pub struct KillSwitch {
    path: PathBuf,
    state: KillSwitchState,
}

impl KillSwitch {
    pub fn load(path: PathBuf) -> Self {
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, state }
    }

    /// The loaded history (the kernel seeds its kill-switch state from it).
    pub fn state(&self) -> &KillSwitchState {
        &self.state
    }

    /// Persist `state` as the current history.
    pub fn persist(&mut self, state: &KillSwitchState) {
        self.state = state.clone();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Ok(json) = serde_json::to_string(&self.state) {
            if let Err(e) = std::fs::write(&self.path, json) {
                tracing::warn!(error = %e, path = %self.path.display(), "kill-switch persist failed");
            }
        }
    }

    /// Record the NAV sample, persist the history and return whether the
    /// switch is tripped.
    pub fn check(&mut self, cfg: &LimitsConfig, nav: u64, now_ms: u64) -> bool {
        let tripped = self.state.check(cfg, nav, now_ms);
        let state = self.state.clone();
        self.persist(&state);
        tripped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_switch_trips_on_seven_day_drawdown_and_persists() {
        let path = std::env::temp_dir().join(format!(
            "mm-desk-kill-test-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let day = 86_400_000u64;
        let c = LimitsConfig::default();
        {
            let mut k = KillSwitch::load(path.clone());
            assert!(!k.check(&c, 1_000, day));
            assert!(!k.check(&c, 950, 2 * day)); // −5%: fine
            assert!(k.check(&c, 890, 3 * day)); // −11% from high water: trip
        }
        // Reload from disk: the high water survives the restart.
        {
            let mut k = KillSwitch::load(path.clone());
            assert!(k.check(&c, 890, 4 * day));
            // Once the 1_000 sample ages out of the window, 890 vs recent
            // high water 890 → no drawdown.
            assert!(!k.check(&c, 890, 9 * day));
        }
        let _ = std::fs::remove_file(&path);
    }
}
