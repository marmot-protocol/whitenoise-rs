/// Performance tracking infrastructure for Whitenoise.
///
/// # Usage
///
/// Place `perf_span!` at the start of any function or block you want to time. The span
/// automatically ends when the returned guard is dropped (RAII).
///
/// ```ignore
/// // Entire function
/// let _span = perf_span!("messages::send_message");
///
/// // Sub-operation within a function
/// let _enc = perf_span!("mls::encrypt");
/// let ciphertext = mdk.encrypt(...)?;
/// drop(_enc); // explicit end; or let scope handle it
/// ```
///
/// # Production cost
///
/// `perf_span!` expands to a lightweight RAII guard that records an `Instant` on
/// creation and emits a `tracing::info!` event on drop, targeting
/// `"whitenoise::perf"`. In production the `whitenoise::perf` filter is not
/// enabled by default, so the level-filter check short-circuits the whole call.
/// The only runtime cost in production is a single `LevelFilter` comparison —
/// effectively zero.
///
/// # Benchmark capture
///
/// In benchmark builds (`benchmark-tests` feature) a `PerfTracingLayer`
/// subscriber listens exclusively to this target and accumulates timing samples
/// by parsing the `duration_ns` field from the event. Call
/// `PerfTracingLayer::drain()` after a benchmark loop to retrieve them.
///
/// # Design note: why not `tracing::EnteredSpan`?
///
/// `EnteredSpan` is `!Send`, so holding one across an `.await` point causes a
/// compile error in `tokio::spawn` futures. This custom guard emits a regular
/// `info!` event (which is `Send`) on drop instead, sidestepping the problem
/// entirely.
use std::time::Instant;

/// RAII performance guard. Emits a `tracing::info!` event with target
/// `"whitenoise::perf"` and field `duration_ns` when dropped.
///
/// This type is `Send`, so it is safe to hold across `.await` points.
pub struct PerfGuard {
    name: &'static str,
    start: Instant,
}

impl PerfGuard {
    #[inline]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }
}

impl Drop for PerfGuard {
    #[inline]
    fn drop(&mut self) {
        let ns = self.start.elapsed().as_nanos() as u64;
        tracing::info!(
            target: "whitenoise::perf",
            name = self.name,
            duration_ns = ns,
            "perf"
        );
    }
}

/// Emits a performance timing event with target `"whitenoise::perf"` when
/// the returned guard is dropped.
///
/// The returned `PerfGuard` is `Send` and safe to hold across `.await` points.
/// Name the guard with a leading underscore (`_span`) to keep the RAII lifetime
/// intact without triggering the unused-variable warning.
///
/// # Examples
///
/// ```ignore
/// let _span = perf_span!("messages::send_message_to_group");
/// // ... async work including awaits ...
/// // guard dropped here → timing event emitted
/// ```
#[macro_export]
macro_rules! perf_span {
    ($name:literal) => {
        $crate::perf::PerfGuard::new($name)
    };
}
