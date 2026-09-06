//! Global speed limiter (cf. XDM `SpeedLimiter`).
//!
//! XDM throttles by comparing actual vs. expected transfer time and
//! sleeping the difference. Same idea here, in testable pieces:
//! [`SpeedLimiter::throttle_delay_ms`] is a pure calculation (unit
//! tested), [`SpeedLimiter::throttle`] adds the async sleep.

/// Current time in milliseconds since the Unix epoch.
pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Token-free throttler enforcing a global KiB/s cap.
#[derive(Debug)]
pub struct SpeedLimiter {
    /// 0 = unlimited.
    limit_kbps: u32,
    enabled: bool,
    last_tick_ms: u128,
    last_bytes: u64,
    /// Set on the very first observation.
    initialized: bool,
}

impl SpeedLimiter {
    /// Create a limiter; `limit_kbps == 0` disables throttling.
    pub fn new(limit_kbps: u32, enabled: bool) -> Self {
        Self {
            limit_kbps,
            enabled,
            last_tick_ms: 0,
            last_bytes: 0,
            initialized: false,
        }
    }

    /// Change the cap at runtime (0 = unlimited).
    pub fn set_limit(&mut self, limit_kbps: u32, enabled: bool) {
        self.limit_kbps = limit_kbps;
        self.enabled = enabled;
    }

    /// Is throttling currently active?
    pub fn active(&self) -> bool {
        self.enabled && self.limit_kbps > 0
    }

    /// Pure part of XDM's `ThrottleIfNeeded`: given the total bytes
    /// downloaded so far and `now`, return how many milliseconds the
    /// caller should sleep. Updates the internal baseline. The first
    /// call only records the baseline and returns 0.
    pub fn throttle_delay_ms(&mut self, total_downloaded: u64, now_ms: u128) -> u128 {
        if !self.active() {
            return 0;
        }
        if !self.initialized {
            self.initialized = true;
            self.last_bytes = total_downloaded;
            self.last_tick_ms = now_ms;
            return 0;
        }
        // Max bytes per millisecond, exactly like XDM:
        // (limit KiB/s * 1024) / 1000.
        let max_bytes_per_ms = self.limit_kbps as f64 * 1024.0 / 1000.0;
        let actual = now_ms.saturating_sub(self.last_tick_ms);
        if actual < 1 {
            return 0;
        }
        let diff = total_downloaded.saturating_sub(self.last_bytes) as f64;
        self.last_bytes = total_downloaded;
        self.last_tick_ms = now_ms;
        let expected = diff / max_bytes_per_ms;
        if expected > actual as f64 {
            (expected - actual as f64).ceil() as u128
        } else {
            0
        }
    }

    /// Async wrapper: compute the delay and sleep it.
    pub async fn throttle(&mut self, total_downloaded: u64) {
        let delay = self.throttle_delay_ms(total_downloaded, now_ms());
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay.min(5_000) as u64)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_never_sleeps() {
        let mut l = SpeedLimiter::new(0, true);
        assert_eq!(l.throttle_delay_ms(1_000_000, 1000), 0);
        let mut l = SpeedLimiter::new(100, false);
        assert_eq!(l.throttle_delay_ms(1_000_000, 1000), 0);
    }

    #[test]
    fn first_call_only_baselines() {
        let mut l = SpeedLimiter::new(100, true);
        assert_eq!(l.throttle_delay_ms(500, 1000), 0);
    }

    #[test]
    fn too_fast_gets_delay() {
        // 100 KiB/s => 102.4 bytes/ms. 10240 bytes in 10ms should take 100ms.
        let mut l = SpeedLimiter::new(100, true);
        l.throttle_delay_ms(0, 1000);
        assert_eq!(l.throttle_delay_ms(10_240, 1010), 90);
    }

    #[test]
    fn slow_enough_gets_no_delay() {
        let mut l = SpeedLimiter::new(100, true);
        l.throttle_delay_ms(0, 1000);
        // 100 bytes in 1000ms is way below the cap.
        assert_eq!(l.throttle_delay_ms(100, 2000), 0);
    }
}
