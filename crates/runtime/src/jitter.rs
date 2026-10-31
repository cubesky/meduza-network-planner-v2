//! Randomness for jitter. Every process seeds independently from the OS (§7.5.1: no fixed or time-only seeds).

use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// Source of uniform random durations; tests substitute a deterministic implementation.
pub trait Jitter: Send {
    /// Uniform in `[0, max]`.
    fn uniform(&mut self, max: Duration) -> Duration;
}

/// OS-seeded generator.
pub struct OsJitter(StdRng);

impl OsJitter {
    pub fn new() -> Self {
        let mut seed = [0u8; 32];
        // A failing OS RNG is unrecoverable for a security-relevant process; fall back to address/time mixing
        // only so that the process still staggers rather than aborting.
        if getrandom::getrandom(&mut seed).is_err() {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let addr = &seed as *const _ as usize as u128;
            seed[..16].copy_from_slice(&(t ^ addr.rotate_left(64) ^ u128::from(std::process::id())).to_le_bytes());
        }
        Self(StdRng::from_seed(seed))
    }
}

impl Default for OsJitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Jitter for OsJitter {
    fn uniform(&mut self, max: Duration) -> Duration {
        let max_ms = max.as_millis().min(u128::from(u64::MAX)) as u64;
        if max_ms == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(self.0.gen_range(0..=max_ms))
    }
}

/// Deterministic jitter for tests: returns `fraction * max` (fraction in 0..=1, cycled).
pub struct FixedJitter {
    fractions: Vec<f64>,
    index: usize,
}

impl FixedJitter {
    pub fn new(fractions: Vec<f64>) -> Self {
        assert!(!fractions.is_empty());
        Self { fractions, index: 0 }
    }
}

impl Jitter for FixedJitter {
    fn uniform(&mut self, max: Duration) -> Duration {
        let f = self.fractions[self.index % self.fractions.len()].clamp(0.0, 1.0);
        self.index += 1;
        max.mul_f64(f)
    }
}

/// Random initial phase for a periodic task so restarted nodes do not align (§7.5.1).
pub fn initial_phase(jitter: &mut dyn Jitter, period: Duration) -> Duration {
    jitter.uniform(period)
}

/// `period ± fraction*period`, never below half the period.
pub fn jittered_period(jitter: &mut dyn Jitter, period: Duration, fraction: f64) -> Duration {
    let spread = period.mul_f64(fraction.clamp(0.0, 0.5));
    let low = period.saturating_sub(spread);
    low + jitter.uniform(spread * 2)
}
