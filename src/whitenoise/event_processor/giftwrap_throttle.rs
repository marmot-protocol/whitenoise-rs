//! Per-account token bucket gating NIP-44 decryption of inbound kind:1059
//! gift-wrap events. Bounds the CPU and battery a single account can spend
//! on gift-wrap processing per unit time, regardless of attacker fan-out or
//! relay count.
//!
//! Kind:1059 events are addressed to a public pubkey with no sender
//! authentication, and the KeyPackage reference identifying the inviter sits
//! inside the encrypted rumor — so transport-layer pre-filtering is
//! impossible. Without this bucket, anyone who knows a recipient's pubkey
//! can force unbounded ECDH+decrypt work on that recipient's device.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Token bucket sized for gift-wrap decryption budget on a single account.
///
/// Refills continuously at `refill_per_sec`, up to `capacity`. `try_acquire`
/// is the only consumer; it returns `true` and deducts one token when
/// budget exists, `false` otherwise. It does NOT block — back-pressure is
/// the caller's choice (whitenoise currently drops on `false`; relay replay
/// will redeliver the event on the next subscription resume).
///
/// **Thread-safety:** the bucket is `Send + Sync` via the inner `Mutex`.
/// Lock duration is bounded by a handful of float ops, so contention from
/// the single account event-processor task is negligible.
///
/// **Fail-open:** if the inner mutex is poisoned (a panic somewhere else
/// holding the lock), `try_acquire` returns `true` rather than locking the
/// account out of receiving welcomes. The DoS we are defending against is a
/// flood, not a deadlock.
pub struct GiftwrapThrottle {
    capacity: f64,
    refill_per_sec: f64,
    inner: Mutex<Inner>,
}

struct Inner {
    tokens: f64,
    last_refill: Instant,
}

impl GiftwrapThrottle {
    /// Construct a throttle with the given burst capacity and refill rate.
    ///
    /// `capacity` is the maximum number of gift-wrap decrypts that may
    /// happen in a burst after an idle period. `refill_per_sec` is the
    /// sustained rate.
    ///
    /// # Panics
    ///
    /// Panics if `capacity == 0` or `refill_per_sec <= 0.0`. Misconfiguration
    /// here would silently disable all gift-wrap processing for the account,
    /// which is worse than the panic surfacing the bug.
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        assert!(capacity > 0, "GiftwrapThrottle capacity must be > 0");
        assert!(
            refill_per_sec > 0.0,
            "GiftwrapThrottle refill_per_sec must be > 0.0"
        );
        Self {
            capacity: capacity as f64,
            refill_per_sec,
            inner: Mutex::new(Inner {
                tokens: capacity as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Default parameters for inbound welcome processing on a real user.
    ///
    /// Capacity 60: a legitimate user joining many groups in one burst (e.g.
    /// being added to a workspace) sees no throttling.
    /// Refill 2/sec: sustained throughput ceiling. At 2/sec the victim's
    /// CPU budget for gift-wrap decryption is ~400 µs/sec (well below 0.1%
    /// of one core), making the attack invisible to UI or battery.
    pub fn default_for_account() -> Self {
        Self::new(60, 2.0)
    }

    /// Try to deduct one token. Returns `true` if a token was available.
    pub fn try_acquire(&self) -> bool {
        self.try_acquire_at(Instant::now())
    }

    /// Test-visible variant that takes an explicit `now`, allowing the unit
    /// test to drive virtual time without depending on `tokio::time::pause`.
    pub(crate) fn try_acquire_at(&self, now: Instant) -> bool {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return true, // fail-open on poison
        };
        let elapsed = now
            .checked_duration_since(inner.last_refill)
            .unwrap_or(Duration::ZERO)
            .as_secs_f64();
        inner.tokens = (inner.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        inner.last_refill = now;
        if inner.tokens >= 1.0 {
            inner.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Best-effort snapshot of current token count. For telemetry only.
    pub fn tokens(&self) -> f64 {
        self.inner.lock().map(|i| i.tokens).unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trickle_at_legitimate_rate_never_throttles() {
        // 1 welcome per second for 30 seconds — well under refill of 2/sec.
        let bucket = GiftwrapThrottle::new(60, 2.0);
        let start = Instant::now();
        let mut accepted = 0u32;
        for i in 0..30 {
            let t = start + Duration::from_secs(i);
            if bucket.try_acquire_at(t) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 30, "all legitimate welcomes must pass");
    }

    #[test]
    fn flood_is_capped_at_capacity_then_refills() {
        // 10_000 events at t=0 — only `capacity` should pass.
        let bucket = GiftwrapThrottle::new(60, 2.0);
        let t0 = Instant::now();
        let mut accepted_burst = 0u32;
        for _ in 0..10_000 {
            if bucket.try_acquire_at(t0) {
                accepted_burst += 1;
            }
        }
        assert_eq!(accepted_burst, 60, "burst must be bounded by capacity");

        // 10 seconds later, refill should have added 20 tokens (2/sec * 10s).
        let t1 = t0 + Duration::from_secs(10);
        let mut accepted_refill = 0u32;
        for _ in 0..10_000 {
            if bucket.try_acquire_at(t1) {
                accepted_refill += 1;
            }
        }
        assert_eq!(
            accepted_refill, 20,
            "post-burst refill must be exactly refill_per_sec * elapsed"
        );
    }

    #[test]
    fn refill_caps_at_capacity_not_unbounded() {
        // Long idle then a burst — capacity must cap accumulated tokens.
        let bucket = GiftwrapThrottle::new(60, 2.0);
        let t0 = Instant::now();
        // Idle for an hour. Naive refill would credit 7200 tokens.
        let t1 = t0 + Duration::from_secs(3600);
        let mut accepted = 0u32;
        for _ in 0..1_000 {
            if bucket.try_acquire_at(t1) {
                accepted += 1;
            }
        }
        assert_eq!(
            accepted, 60,
            "accumulated tokens must be capped at capacity"
        );
    }

    #[test]
    fn under_sustained_flood_throughput_equals_refill_rate() {
        // 1000 events/sec for 5 seconds. Acceptance should be ~ capacity + 5 * refill.
        let bucket = GiftwrapThrottle::new(60, 2.0);
        let t0 = Instant::now();
        let mut accepted = 0u32;
        for s in 0..5 {
            for i in 0..1_000 {
                // Spread the 1000 events over 1 second.
                let t = t0 + Duration::from_millis(s * 1000 + i);
                if bucket.try_acquire_at(t) {
                    accepted += 1;
                }
            }
        }
        // capacity (60) at t=0 + 2/sec * ~5s = 70. Allow ±1 for floating-point.
        assert!(
            (69..=71).contains(&accepted),
            "expected ~70 accepted under sustained flood, got {accepted}"
        );
    }

    #[test]
    fn poisoned_lock_fails_open() {
        // Defense-in-depth: a panic in some unrelated holder of the mutex
        // must not lock the account out of welcome processing.
        use std::sync::Arc;
        use std::thread;
        let bucket = Arc::new(GiftwrapThrottle::new(60, 2.0));
        let b2 = bucket.clone();
        let _ = thread::spawn(move || {
            let _guard = b2.inner.lock().unwrap();
            panic!("poisoning");
        })
        .join();
        // Bucket is now poisoned — must fail-open.
        assert!(bucket.try_acquire());
    }
}
