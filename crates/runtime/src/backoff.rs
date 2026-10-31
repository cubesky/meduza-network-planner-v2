//! Shared recovery gate for all requests to one manager (AGENT.md §7.5).
//!
//! One [`Gate`] per manager target coordinates config pulls, heartbeats, ACKs, job results and push
//! reconnects. It implements:
//! - capped exponential windows with "equal jitter": `window = min(cap, base·2^k)`,
//!   `delay = window/2 + U(0, window/2)`, re-drawn every time (§7.5.1);
//! - an extra `U(0, spread)` on cold start / network recovery / channel loss, coalesced per outage;
//! - Retry-After honoured as a lower bound plus bounded jitter, even beyond the local cap (§7.5.2);
//! - one counted failure per scheduling round (concurrent failures do not multiply the level);
//! - a single probe request while recovering; a shared budget of 1 connect + N short requests;
//! - credential failures (401/403/TLS trust) parked until repaired, never fast-retried;
//! - gradual level decrease only after `stable_after` of health that included a calibration/ACK (§7.5.3).
//!
//! Time is passed in explicitly (`Instant`) so tests and simulations control the clock.

use std::time::{Duration, Instant, SystemTime};

use meduza_np_protocol::report::BackoffStatus;

use crate::jitter::Jitter;

#[derive(Debug, Clone)]
pub struct BackoffPolicy {
    pub base: Duration,
    pub cap: Duration,
    /// Extra stagger after cold start, network recovery or a full channel loss.
    pub startup_spread: Duration,
    /// Continuous health needed (with a calibration) before the failure level drops by one.
    pub stable_after: Duration,
    /// Concurrent short control requests (config, report, ACK, job result).
    pub max_short_requests: u32,
    /// Upper bound for honoured Retry-After values.
    pub max_retry_after: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(2),
            cap: Duration::from_secs(300),
            startup_spread: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
            max_short_requests: 2,
            max_retry_after: Duration::from_secs(6 * 3600),
        }
    }
}

/// How a failed request is classified (§7.5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// DNS/connect failure, reset, timeout, abnormal stream end, recoverable 5xx.
    Retryable,
    /// 429 / 503 with explicit overload: also opens the shared server cooldown.
    Overloaded,
    /// 401/403 or certificate validation failure: wait for credential repair.
    Credential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// Establishing the push channel (WebSocket / event stream). At most one at a time.
    Connect,
    /// Short HTTPS control request.
    Short,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `calibrated` = the request confirmed the desired revision or delivered an ACK.
    Success {
        calibrated: bool,
    },
    Failure {
        class: FailureClass,
        retry_after: Option<Duration>,
        reason: String,
    },
    /// Request did not reach a verdict about manager health (local cancel, business denial).
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    Backoff,
    ServerCooldown,
    Budget,
    Probing,
    Credential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    pub reason: WaitReason,
    /// When to try again; `None` = wait for another event (budget release, credential repair).
    pub until: Option<Instant>,
}

/// Proof of an admitted request; must be returned with [`Gate::release`].
#[derive(Debug)]
#[must_use = "return the permit with Gate::release"]
pub struct Permit {
    kind: RequestKind,
    probe: bool,
    epoch: u64,
}

impl Permit {
    pub fn kind(&self) -> RequestKind {
        self.kind
    }
    pub fn is_probe(&self) -> bool {
        self.probe
    }
}

pub struct Gate {
    policy: BackoffPolicy,
    jitter: Box<dyn Jitter>,
    level: u32,
    next_allowed: Option<Instant>,
    cooldown_until: Option<Instant>,
    healthy_since: Option<Instant>,
    calibrated_since_healthy: bool,
    recovering: bool,
    spread_applied: bool,
    epoch: u64,
    last_window: Duration,
    last_failure: Option<String>,
    credential_error: Option<String>,
    in_flight_short: u32,
    connecting: bool,
    probe_in_flight: bool,
    dirty: bool,
}

/// Exponent limit; `base·2^20` already exceeds any sensible cap, and avoids overflow.
const MAX_EXPONENT: u32 = 20;
/// Persisted level never exceeds this (keeps restored windows at the cap without growing forever).
const MAX_LEVEL: u32 = 32;

impl Gate {
    pub fn new(policy: BackoffPolicy, jitter: Box<dyn Jitter>) -> Self {
        Self {
            policy,
            jitter,
            level: 0,
            next_allowed: None,
            cooldown_until: None,
            healthy_since: None,
            calibrated_since_healthy: false,
            recovering: false,
            spread_applied: false,
            epoch: 0,
            last_window: Duration::ZERO,
            last_failure: None,
            credential_error: None,
            in_flight_short: 0,
            connecting: false,
            probe_in_flight: false,
            dirty: false,
        }
    }

    /// Restores a persisted failure level after a restart. A non-zero level schedules the first attempt one
    /// drawn window later instead of immediately (§7.5.3: restarts must not reset and hammer the manager).
    pub fn restore(&mut self, level: u32, now: Instant) {
        self.level = level.min(MAX_LEVEL);
        if self.level > 0 {
            self.recovering = true;
            let delay = self.draw_delay(self.level - 1);
            self.push_next_allowed(now + delay);
        }
    }

    /// Window for failure level `k` (first failure uses k = 0).
    pub fn window_for(&self, k: u32) -> Duration {
        let factor = 1u32 << k.min(MAX_EXPONENT);
        self.policy.base.saturating_mul(factor).min(self.policy.cap)
    }

    fn draw_delay(&mut self, k: u32) -> Duration {
        let window = self.window_for(k);
        self.last_window = window;
        window / 2 + self.jitter.uniform(window / 2)
    }

    fn push_next_allowed(&mut self, at: Instant) {
        self.next_allowed = Some(self.next_allowed.map_or(at, |cur| cur.max(at)));
    }

    /// Call on cold start, network-up notifications and loss of the push channel. Several notifications in
    /// one outage add the stagger once (§7.5.1: merged through one scheduler).
    pub fn note_reconnect_round(&mut self, now: Instant) {
        if self.spread_applied {
            return;
        }
        self.spread_applied = true;
        let extra = self.jitter.uniform(self.policy.startup_spread);
        let base = self.next_allowed.map_or(now, |t| t.max(now));
        self.next_allowed = Some(base + extra);
    }

    pub fn try_acquire(&mut self, kind: RequestKind, now: Instant) -> Result<Permit, Wait> {
        if self.credential_error.is_some() {
            return Err(Wait { reason: WaitReason::Credential, until: None });
        }
        if let Some(t) = self.cooldown_until.filter(|t| *t > now) {
            return Err(Wait { reason: WaitReason::ServerCooldown, until: Some(t) });
        }
        if let Some(t) = self.next_allowed.filter(|t| *t > now) {
            return Err(Wait { reason: WaitReason::Backoff, until: Some(t) });
        }
        let probe = self.recovering;
        if probe && self.probe_in_flight {
            return Err(Wait { reason: WaitReason::Probing, until: None });
        }
        match kind {
            RequestKind::Connect if self.connecting => return Err(Wait { reason: WaitReason::Budget, until: None }),
            RequestKind::Short if self.in_flight_short >= self.policy.max_short_requests => {
                return Err(Wait { reason: WaitReason::Budget, until: None })
            }
            RequestKind::Connect => self.connecting = true,
            RequestKind::Short => self.in_flight_short += 1,
        }
        if probe {
            self.probe_in_flight = true;
        }
        Ok(Permit { kind, probe, epoch: self.epoch })
    }

    pub fn release(&mut self, permit: Permit, outcome: Outcome, now: Instant) {
        match permit.kind {
            RequestKind::Connect => self.connecting = false,
            RequestKind::Short => self.in_flight_short = self.in_flight_short.saturating_sub(1),
        }
        if permit.probe {
            self.probe_in_flight = false;
        }
        match outcome {
            Outcome::Neutral => {}
            Outcome::Success { calibrated } => self.on_success(calibrated, now),
            Outcome::Failure { class, retry_after, reason } => self.on_failure(permit.epoch, class, retry_after, reason, now),
        }
    }

    /// A long-lived channel that was healthy reports keepalive progress (Pong received, event processed).
    pub fn note_channel_healthy(&mut self, calibrated: bool, now: Instant) {
        self.on_success(calibrated, now);
    }

    /// The push channel dropped after it had been established. Counts as one retryable failure.
    pub fn note_channel_lost(&mut self, reason: String, now: Instant) {
        let epoch = self.epoch;
        self.on_failure(epoch, FailureClass::Retryable, None, reason, now);
        self.note_reconnect_round(now);
    }

    fn on_success(&mut self, calibrated: bool, now: Instant) {
        self.recovering = false;
        self.spread_applied = false;
        let since = *self.healthy_since.get_or_insert(now);
        self.calibrated_since_healthy |= calibrated;
        if self.level > 0 && self.calibrated_since_healthy && now.duration_since(since) >= self.policy.stable_after {
            self.level -= 1;
            self.healthy_since = Some(now);
            self.calibrated_since_healthy = false;
            self.dirty = true;
        }
    }

    fn on_failure(&mut self, epoch: u64, class: FailureClass, retry_after: Option<Duration>, reason: String, now: Instant) {
        self.healthy_since = None;
        self.calibrated_since_healthy = false;
        self.last_failure = Some(reason.clone());
        if class == FailureClass::Credential {
            self.credential_error = Some(reason);
            return;
        }
        self.recovering = true;
        let counted = epoch == self.epoch;
        let mut delay = if counted {
            let d = self.draw_delay(self.level);
            self.level = (self.level + 1).min(MAX_LEVEL);
            self.epoch += 1;
            self.dirty = true;
            d
        } else {
            // Another request of the same round already counted; keep its schedule.
            Duration::ZERO
        };
        if let Some(ra) = retry_after {
            let ra = ra.min(self.policy.max_retry_after);
            let spread = (ra / 2).max(Duration::from_secs(1)).min(self.policy.startup_spread);
            delay = delay.max(ra + self.jitter.uniform(spread));
        }
        if delay > Duration::ZERO {
            self.push_next_allowed(now + delay);
        }
        if class == FailureClass::Overloaded {
            let until = self.next_allowed.unwrap_or(now + delay);
            self.cooldown_until = Some(self.cooldown_until.map_or(until, |c| c.max(until)));
        }
    }

    /// A failure observed outside a permit (e.g. Access auth traffic with its own capacity) counted in the
    /// current round, so it cannot multiply the level with concurrent control-plane failures.
    pub fn record_failure(&mut self, class: FailureClass, retry_after: Option<Duration>, reason: String, now: Instant) {
        let epoch = self.epoch;
        self.on_failure(epoch, class, retry_after, reason, now);
    }

    /// Operator re-enrolled or rotated credentials.
    pub fn clear_credential_error(&mut self) {
        self.credential_error = None;
    }

    pub fn credential_error(&self) -> Option<&str> {
        self.credential_error.as_deref()
    }

    pub fn level(&self) -> u32 {
        self.level
    }

    pub fn is_recovering(&self) -> bool {
        self.recovering
    }

    /// Earliest instant any request may start, if gated.
    pub fn next_allowed(&self, now: Instant) -> Option<Instant> {
        [self.next_allowed, self.cooldown_until].into_iter().flatten().filter(|t| *t > now).max()
    }

    /// True once since the last call if the level changed and should be persisted (low frequency, §7.5.3).
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn status(&self, now: Instant) -> BackoffStatus {
        BackoffStatus {
            failure_level: self.level,
            window_seconds: self.last_window.as_secs(),
            next_attempt_in_ms: self.next_allowed(now).map(|t| t.duration_since(now).as_millis() as u64),
            last_failure: self.credential_error.clone().or_else(|| self.last_failure.clone()),
            server_cooldown_ms: self.cooldown_until.filter(|t| *t > now).map(|t| t.duration_since(now).as_millis() as u64),
            probing: self.recovering,
        }
    }
}

/// Parses an HTTP `Retry-After` value: delta-seconds or an HTTP-date (relative to `now`).
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Maps an HTTP status to a failure class. `None` = not a manager-health failure (2xx/3xx, 400/404/409/422).
pub fn classify_status(status: u16) -> Option<FailureClass> {
    match status {
        401 | 403 => Some(FailureClass::Credential),
        429 | 503 => Some(FailureClass::Overloaded),
        408 | 500 | 502 | 504 => Some(FailureClass::Retryable),
        s if s >= 500 => Some(FailureClass::Retryable),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jitter::{FixedJitter, OsJitter};

    fn gate(fractions: Vec<f64>) -> Gate {
        Gate::new(BackoffPolicy::default(), Box::new(FixedJitter::new(fractions)))
    }

    fn fail(g: &mut Gate, now: Instant) {
        let p = g.try_acquire(RequestKind::Short, now).expect("admitted");
        g.release(p, Outcome::Failure { class: FailureClass::Retryable, retry_after: None, reason: "connect".into() }, now);
    }

    #[test]
    fn windows_double_until_cap() {
        let g = gate(vec![0.0]);
        let windows: Vec<u64> = (0..10).map(|k| g.window_for(k).as_secs()).collect();
        assert_eq!(windows, vec![2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(g.window_for(u32::MAX).as_secs(), 300, "no overflow");
    }

    #[test]
    fn delay_is_between_half_window_and_window() {
        let t0 = Instant::now();
        for (fraction, expect_ms) in [(0.0, 1000), (1.0, 2000), (0.5, 1500)] {
            let mut g = gate(vec![fraction]);
            fail(&mut g, t0);
            let wait = g.next_allowed(t0).unwrap().duration_since(t0);
            assert_eq!(wait.as_millis(), expect_ms);
        }
    }

    #[test]
    fn successive_failures_grow_and_stay_randomized_at_cap() {
        let mut g = Gate::new(BackoffPolicy::default(), Box::new(OsJitter::new()));
        let mut now = Instant::now();
        let mut waits = Vec::new();
        for _ in 0..14 {
            fail(&mut g, now);
            let next = g.next_allowed(now).unwrap();
            waits.push(next.duration_since(now));
            now = next;
        }
        for (k, w) in waits.iter().enumerate() {
            let window = g.window_for(k as u32);
            assert!(*w >= window / 2 && *w <= window, "k={k} wait={w:?} window={window:?}");
        }
        let capped: Vec<_> = waits[9..].iter().collect();
        assert!(capped.windows(2).any(|w| w[0] != w[1]), "still jittered at cap: {capped:?}");
    }

    #[test]
    fn concurrent_failures_count_once() {
        let mut g = gate(vec![0.0]);
        let now = Instant::now();
        let a = g.try_acquire(RequestKind::Short, now).unwrap();
        let b = g.try_acquire(RequestKind::Short, now).unwrap();
        assert!(g.try_acquire(RequestKind::Short, now).is_err(), "budget of 2");
        let failure = || Outcome::Failure { class: FailureClass::Retryable, retry_after: None, reason: "reset".into() };
        g.release(a, failure(), now);
        g.release(b, failure(), now);
        assert_eq!(g.level(), 1);
    }

    #[test]
    fn retry_after_is_a_lower_bound_even_beyond_cap() {
        let mut g = gate(vec![0.0]);
        let now = Instant::now();
        let p = g.try_acquire(RequestKind::Short, now).unwrap();
        g.release(
            p,
            Outcome::Failure { class: FailureClass::Overloaded, retry_after: Some(Duration::from_secs(900)), reason: "429".into() },
            now,
        );
        let wait = g.next_allowed(now).unwrap().duration_since(now);
        assert!(wait >= Duration::from_secs(900));
        assert!(matches!(
            g.try_acquire(RequestKind::Connect, now + Duration::from_secs(600)),
            Err(Wait { reason: WaitReason::ServerCooldown, .. })
        ));
    }

    #[test]
    fn only_one_probe_while_recovering() {
        let mut g = gate(vec![0.0]);
        let now = Instant::now();
        fail(&mut g, now);
        let later = now + Duration::from_secs(10);
        let probe = g.try_acquire(RequestKind::Short, later).unwrap();
        assert!(probe.is_probe());
        assert!(matches!(g.try_acquire(RequestKind::Connect, later), Err(Wait { reason: WaitReason::Probing, .. })));
        g.release(probe, Outcome::Success { calibrated: false }, later);
        let a = g.try_acquire(RequestKind::Short, later).unwrap();
        let b = g.try_acquire(RequestKind::Connect, later).unwrap();
        assert!(!a.is_probe() && !b.is_probe());
        g.release(a, Outcome::Neutral, later);
        g.release(b, Outcome::Neutral, later);
    }

    #[test]
    fn level_drops_only_after_stable_calibrated_period() {
        let mut g = gate(vec![0.0]);
        let mut now = Instant::now();
        for _ in 0..3 {
            fail(&mut g, now);
            now = g.next_allowed(now).unwrap();
        }
        assert_eq!(g.level(), 3);
        g.note_channel_healthy(false, now);
        g.note_channel_healthy(false, now + Duration::from_secs(120));
        assert_eq!(g.level(), 3, "no calibration yet");
        g.note_channel_healthy(true, now + Duration::from_secs(121));
        assert_eq!(g.level(), 2);
        g.note_channel_healthy(true, now + Duration::from_secs(150));
        assert_eq!(g.level(), 2, "next step needs another stable period");
        g.note_channel_healthy(true, now + Duration::from_secs(182));
        assert_eq!(g.level(), 1);
    }

    #[test]
    fn flapping_keeps_level() {
        let mut g = gate(vec![0.0]);
        let mut now = Instant::now();
        for _ in 0..4 {
            fail(&mut g, now);
            now = g.next_allowed(now).unwrap();
            g.note_channel_healthy(true, now);
            now += Duration::from_secs(5);
        }
        assert_eq!(g.level(), 4);
    }

    #[test]
    fn credential_errors_park_the_gate() {
        let mut g = gate(vec![0.0]);
        let now = Instant::now();
        let p = g.try_acquire(RequestKind::Short, now).unwrap();
        g.release(p, Outcome::Failure { class: FailureClass::Credential, retry_after: None, reason: "401".into() }, now);
        assert!(matches!(
            g.try_acquire(RequestKind::Short, now + Duration::from_secs(3600)),
            Err(Wait { reason: WaitReason::Credential, until: None })
        ));
        g.clear_credential_error();
        assert!(g.try_acquire(RequestKind::Short, now).is_ok());
    }

    #[test]
    fn reconnect_spread_is_coalesced() {
        let mut g = gate(vec![1.0]);
        let now = Instant::now();
        g.note_reconnect_round(now);
        g.note_reconnect_round(now);
        g.note_reconnect_round(now);
        assert_eq!(g.next_allowed(now).unwrap().duration_since(now), Duration::from_secs(30));
    }

    #[test]
    fn restore_does_not_hammer() {
        let mut g = gate(vec![0.5]);
        let now = Instant::now();
        g.restore(5, now);
        g.note_reconnect_round(now);
        let wait = g.next_allowed(now).unwrap().duration_since(now);
        // level 5 → window for k=4 is 32 s → delay 24 s, plus 15 s spread.
        assert_eq!(wait, Duration::from_secs(39));
        assert!(g.try_acquire(RequestKind::Short, now).is_err());
    }

    #[test]
    fn retry_after_parsing_and_classification() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480); // Wed, 21 Oct 2015 07:28:00 GMT
        assert_eq!(parse_retry_after("120", now), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:30:00 GMT", now), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(classify_status(401), Some(FailureClass::Credential));
        assert_eq!(classify_status(429), Some(FailureClass::Overloaded));
        assert_eq!(classify_status(502), Some(FailureClass::Retryable));
        assert_eq!(classify_status(404), None);
    }

    /// Simulates 2000 nodes losing the manager at the same instant; reconnect attempts must spread out
    /// rather than spike (§17.1 "重连风暴保护").
    #[test]
    fn fleet_reconnects_are_spread() {
        let nodes = 2000;
        let t0 = Instant::now();
        let mut first_attempt_secs = vec![0u32; 64];
        for _ in 0..nodes {
            let mut g = Gate::new(BackoffPolicy::default(), Box::new(OsJitter::new()));
            g.note_channel_lost("manager restart".into(), t0);
            let at = g.next_allowed(t0).unwrap().duration_since(t0).as_secs() as usize;
            first_attempt_secs[at.min(63)] += 1;
        }
        let peak = *first_attempt_secs.iter().max().unwrap();
        // 2000 attempts over ~31 s of spread: an even spread is ~65/s; allow 2.5x for randomness.
        assert!(peak < 165, "peak {peak} per second: {first_attempt_secs:?}");
        assert!(first_attempt_secs[..2].iter().sum::<u32>() < 200, "no immediate stampede");
    }
}
