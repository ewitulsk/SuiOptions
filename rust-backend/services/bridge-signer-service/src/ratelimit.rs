//! Minimal fixed-window per-IP rate limiter for the public `POST /sign_requests`
//! surface (bridge-spec.md §5.3 DoS guardrails). The verify-before-admit check
//! is the primary guard; this caps request churn from any single source.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Window {
    start: Instant,
    count: u32,
}

pub struct RateLimiter {
    inner: Mutex<HashMap<IpAddr, Window>>,
    window: Duration,
    max: u32,
}

impl RateLimiter {
    pub fn new(window_secs: u64, max: u32) -> Self {
        Self { inner: Mutex::new(HashMap::new()), window: Duration::from_secs(window_secs), max }
    }

    /// Record a request from `ip`; returns false if it exceeds `max` per window.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut map = self.inner.lock().unwrap();
        let now = Instant::now();
        let w = map.entry(ip).or_insert(Window { start: now, count: 0 });
        if now.duration_since(w.start) >= self.window {
            w.start = now;
            w.count = 0;
        }
        w.count += 1;
        w.count <= self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_then_blocks() {
        let rl = RateLimiter::new(60, 2);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert!(rl.check(ip)); // 1
        assert!(rl.check(ip)); // 2
        assert!(!rl.check(ip)); // 3 → blocked
        // a different IP is independent
        assert!(rl.check("5.6.7.8".parse().unwrap()));
    }

    #[test]
    fn window_resets() {
        let rl = RateLimiter::new(0, 1); // window 0 → every call is a fresh window
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert!(rl.check(ip));
        assert!(rl.check(ip)); // window elapsed (0s) → reset, allowed again
    }
}
