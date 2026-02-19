//! Pipeline instrumentation for benchmarking and parameter tuning.
//!
//! Tracks item flow, fetch timing, retry effectiveness, and failure rates
//! across all pipeline tiers. Always allocated (atomic overhead is negligible),
//! but only logged when the `benchmark-tests` feature is enabled.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// Number of retry attempt histogram buckets (one per possible attempt: 0..=MAX_QUEUE_RETRIES).
const ATTEMPT_BUCKETS: usize = 4;

pub(super) struct PipelineMetrics {
    // -- Producer --
    pub producer_pubkeys: AtomicUsize,

    // -- Tier 1: User table --
    pub t1_received: AtomicUsize,
    pub t1_found: AtomicUsize,
    pub t1_forwarded: AtomicUsize,

    // -- Tier 2: Cache --
    pub t2_received: AtomicUsize,
    pub t2_found: AtomicUsize,
    pub t2_forwarded: AtomicUsize,

    // -- Tier 3: Network (default relays) --
    pub t3_received: AtomicUsize,
    pub t3_found: AtomicUsize,
    pub t3_forwarded_ok: AtomicUsize,
    pub t3_forwarded_failed: AtomicUsize,
    /// Chunks that succeeded on attempt N (index = attempt number).
    pub t3_ok_by_attempt: [AtomicUsize; ATTEMPT_BUCKETS],
    pub t3_exhausted: AtomicUsize,
    pub t3_errors: AtomicUsize,
    pub t3_fetches: AtomicUsize,
    pub t3_fetch_us: AtomicU64,
    pub t3_fetch_max_us: AtomicU64,

    // -- Tier 4: Relay list discovery --
    pub t4_received: AtomicUsize,
    pub t4_user_table_found: AtomicUsize,
    pub t4_has_relays: AtomicUsize,
    pub t4_no_relays_cached: AtomicUsize,
    pub t4_no_relays_dropped: AtomicUsize,
    pub t4_exhausted: AtomicUsize,
    pub t4_errors: AtomicUsize,
    pub t4_fetches: AtomicUsize,
    pub t4_fetch_us: AtomicU64,
    pub t4_fetch_max_us: AtomicU64,

    // -- Tier 5: User relay metadata --
    pub t5_received: AtomicUsize,
    pub t5_found: AtomicUsize,
    pub t5_eose: AtomicUsize,
    pub t5_error_retried: AtomicUsize,
    pub t5_error_exhausted: AtomicUsize,
    pub t5_fetches: AtomicUsize,
    pub t5_fetch_us: AtomicU64,
    pub t5_fetch_max_us: AtomicU64,
}

impl PipelineMetrics {
    pub fn new() -> Self {
        Self {
            producer_pubkeys: AtomicUsize::new(0),

            t1_received: AtomicUsize::new(0),
            t1_found: AtomicUsize::new(0),
            t1_forwarded: AtomicUsize::new(0),

            t2_received: AtomicUsize::new(0),
            t2_found: AtomicUsize::new(0),
            t2_forwarded: AtomicUsize::new(0),

            t3_received: AtomicUsize::new(0),
            t3_found: AtomicUsize::new(0),
            t3_forwarded_ok: AtomicUsize::new(0),
            t3_forwarded_failed: AtomicUsize::new(0),
            t3_ok_by_attempt: std::array::from_fn(|_| AtomicUsize::new(0)),
            t3_exhausted: AtomicUsize::new(0),
            t3_errors: AtomicUsize::new(0),
            t3_fetches: AtomicUsize::new(0),
            t3_fetch_us: AtomicU64::new(0),
            t3_fetch_max_us: AtomicU64::new(0),

            t4_received: AtomicUsize::new(0),
            t4_user_table_found: AtomicUsize::new(0),
            t4_has_relays: AtomicUsize::new(0),
            t4_no_relays_cached: AtomicUsize::new(0),
            t4_no_relays_dropped: AtomicUsize::new(0),
            t4_exhausted: AtomicUsize::new(0),
            t4_errors: AtomicUsize::new(0),
            t4_fetches: AtomicUsize::new(0),
            t4_fetch_us: AtomicU64::new(0),
            t4_fetch_max_us: AtomicU64::new(0),

            t5_received: AtomicUsize::new(0),
            t5_found: AtomicUsize::new(0),
            t5_eose: AtomicUsize::new(0),
            t5_error_retried: AtomicUsize::new(0),
            t5_error_exhausted: AtomicUsize::new(0),
            t5_fetches: AtomicUsize::new(0),
            t5_fetch_us: AtomicU64::new(0),
            t5_fetch_max_us: AtomicU64::new(0),
        }
    }

    /// Record a tier 3 chunk result (success or error, attempt tracking).
    pub fn record_t3_result(
        &self,
        found: usize,
        remaining: usize,
        all: usize,
        attempt: u8,
        errored: bool,
        d: Duration,
    ) {
        let r = Ordering::Relaxed;
        self.record_fetch(
            &self.t3_fetches,
            &self.t3_fetch_us,
            &self.t3_fetch_max_us,
            d,
        );
        if errored {
            self.t3_errors.fetch_add(1, r);
            if attempt >= super::MAX_QUEUE_RETRIES {
                self.t3_exhausted.fetch_add(1, r);
                self.t3_forwarded_failed.fetch_add(all, r);
            }
        } else {
            if (attempt as usize) < ATTEMPT_BUCKETS {
                self.t3_ok_by_attempt[attempt as usize].fetch_add(1, r);
            }
            self.t3_found.fetch_add(found, r);
            self.t3_forwarded_ok.fetch_add(remaining, r);
        }
    }

    /// Record a fetch duration for a given tier's counters.
    pub fn record_fetch(
        &self,
        count: &AtomicUsize,
        total_us: &AtomicU64,
        max_us: &AtomicU64,
        d: Duration,
    ) {
        let us = d.as_micros() as u64;
        count.fetch_add(1, Ordering::Relaxed);
        total_us.fetch_add(us, Ordering::Relaxed);
        max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Log a human-readable summary of all pipeline metrics.
    #[cfg(feature = "benchmark-tests")]
    pub fn log_summary(&self) {
        let r = Ordering::Relaxed;

        let producer = self.producer_pubkeys.load(r);

        let t1_in = self.t1_received.load(r);
        let t1_found = self.t1_found.load(r);
        let t1_fwd = self.t1_forwarded.load(r);

        let t2_in = self.t2_received.load(r);
        let t2_found = self.t2_found.load(r);
        let t2_fwd = self.t2_forwarded.load(r);

        let t3_in = self.t3_received.load(r);
        let t3_found = self.t3_found.load(r);
        let t3_fwd_ok = self.t3_forwarded_ok.load(r);
        let t3_fwd_fail = self.t3_forwarded_failed.load(r);
        let t3_attempts: Vec<usize> = self.t3_ok_by_attempt.iter().map(|a| a.load(r)).collect();
        let t3_exhausted = self.t3_exhausted.load(r);
        let t3_errors = self.t3_errors.load(r);

        let t4_in = self.t4_received.load(r);
        let t4_uf = self.t4_user_table_found.load(r);
        let t4_relays = self.t4_has_relays.load(r);
        let t4_cached = self.t4_no_relays_cached.load(r);
        let t4_dropped = self.t4_no_relays_dropped.load(r);
        let t4_errors = self.t4_errors.load(r);
        let t4_exhausted = self.t4_exhausted.load(r);

        let t5_in = self.t5_received.load(r);
        let t5_found = self.t5_found.load(r);
        let t5_eose = self.t5_eose.load(r);
        let t5_retried = self.t5_error_retried.load(r);
        let t5_exhausted = self.t5_error_exhausted.load(r);

        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "=== Pipeline Metrics ==="
        );
        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "  Producer:            {} pubkeys emitted",
            producer,
        );
        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "  Tier 1 (User table): {} in -> {} found, {} fwd",
            t1_in, t1_found, t1_fwd,
        );
        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "  Tier 2 (Cache):      {} in -> {} found, {} fwd",
            t2_in, t2_found, t2_fwd,
        );
        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "  Tier 3 (Network):    {} in -> {} found, {} fwd-ok, {} fwd-failed",
            t3_in, t3_found, t3_fwd_ok, t3_fwd_fail,
        );
        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "    Chunks by attempt: [{}@0, {}@1, {}@2, {}@3, {} exhausted] | {} errors",
            t3_attempts[0], t3_attempts[1], t3_attempts[2], t3_attempts[3], t3_exhausted, t3_errors,
        );
        self.log_fetch_stats(
            "    Fetches",
            &self.t3_fetches,
            &self.t3_fetch_us,
            &self.t3_fetch_max_us,
        );

        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "  Tier 4 (Relay lists): {} in -> {} user-found, {} has-relays, {} cached-empty, {} dropped",
            t4_in, t4_uf, t4_relays, t4_cached, t4_dropped,
        );
        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "    {} errors, {} exhausted",
            t4_errors, t4_exhausted,
        );
        self.log_fetch_stats(
            "    Fetches",
            &self.t4_fetches,
            &self.t4_fetch_us,
            &self.t4_fetch_max_us,
        );

        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "  Tier 5 (User relays): {} in -> {} found, {} eose, {} retried, {} exhausted",
            t5_in, t5_found, t5_eose, t5_retried, t5_exhausted,
        );
        self.log_fetch_stats(
            "    Fetches",
            &self.t5_fetches,
            &self.t5_fetch_us,
            &self.t5_fetch_max_us,
        );
    }

    #[cfg(feature = "benchmark-tests")]
    fn log_fetch_stats(
        &self,
        label: &str,
        count: &AtomicUsize,
        total_us: &AtomicU64,
        max_us: &AtomicU64,
    ) {
        let r = Ordering::Relaxed;
        let n = count.load(r);
        let total = total_us.load(r);
        let max = max_us.load(r);

        if n == 0 {
            tracing::info!(
                target: "whitenoise::user_search::metrics",
                "{}: 0",
                label,
            );
            return;
        }

        let avg = total / n as u64;
        tracing::info!(
            target: "whitenoise::user_search::metrics",
            "{}: {} total, avg {}, max {}, sum {}",
            label,
            n,
            format_us(avg),
            format_us(max),
            format_us(total),
        );
    }
}

#[cfg(feature = "benchmark-tests")]
fn format_us(us: u64) -> String {
    if us < 1_000 {
        format!("{}us", us)
    } else if us < 1_000_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2}s", us as f64 / 1_000_000.0)
    }
}
