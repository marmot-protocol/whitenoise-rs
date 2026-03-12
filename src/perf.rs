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
/// # Trace format
///
/// Each guard emits a single log line at drop time with four fields:
///
/// ```text
/// whitenoise::perf: name="..." trace_id=NNN ts_begin_us=NNN duration_ns=NNN
/// ```
///
/// - `trace_id` — stable ID for the current logical operation (e.g. one event
///   being processed, one user-initiated action).  Set by calling
///   [`with_trace_id`] or [`next_trace_id`] at the start of each logical unit of
///   work and read by every `perf_span!` within that call tree.  Falls back to
///   `0` when not set.  Use as `tid` in Chrome Trace Format: all spans
///   belonging to the same operation share a lane; independent concurrent
///   operations get separate lanes.
/// - `ts_begin_us` — microseconds since Unix epoch at the moment the guard was
///   created.  Use as the `B` timestamp; `ts_begin_us + duration_ns/1000` gives
///   the `E` timestamp.
///
/// # Trace context setup
///
/// Wrap each logical unit of work with [`with_trace_id`] — typically at the top
/// of each event handler iteration or each user-facing API call:
///
/// ```ignore
/// // In the event processing loop, before dispatching each event:
/// crate::perf::with_trace_id(crate::perf::next_trace_id(), async {
///     handle_event(event).await;
/// }).await;
/// ```
///
/// All `perf_span!` calls within the wrapped future inherit the same `trace_id`,
/// even across `.await` points and Tokio worker-thread rescheduling.  For work
/// spawned onto a new Tokio task, capture the current ID and wrap the spawn body:
///
/// ```ignore
/// let tid = crate::perf::current_trace_id();
/// tokio::spawn(crate::perf::with_trace_id(tid, async move {
///     let _span = perf_span!("my::background_work");
///     do_work().await;
/// }));
/// ```
///
/// # Design note: why not `tracing::EnteredSpan`?
///
/// `EnteredSpan` is `!Send`, so holding one across an `.await` point causes a
/// compile error in `tokio::spawn` futures. This custom guard emits a regular
/// `info!` event (which is `Send`) on drop instead, sidestepping the problem
/// entirely.
///
/// # Design note: why `tokio::task_local!` instead of `thread_local!`?
///
/// Tokio's multi-thread runtime uses a work-stealing scheduler: a task may
/// resume on a different worker thread after any `.await` point.
/// `std::thread_local!` is OS-thread-scoped, so a rescheduled task would see
/// a different (or unset) value.  `tokio::task_local!` is scoped to the
/// *task* (future) rather than the OS thread, so it propagates correctly
/// across rescheduling.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::task_local;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

task_local! {
    static TRACE_ID: u64;
}

/// Returns a new monotonically increasing trace ID.
#[inline]
pub fn next_trace_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Wraps `fut` in a task-local scope where [`current_trace_id`] returns `id`.
///
/// Use this at the outermost entry point of each logical unit of work so that
/// all `perf_span!` calls within the future — including those that resume after
/// `.await` on a different Tokio worker thread — share the same `trace_id`.
///
/// For spawned tasks, pass the future directly to `tokio::spawn`:
///
/// ```ignore
/// let tid = crate::perf::current_trace_id();
/// tokio::spawn(crate::perf::with_trace_id(tid, async move { ... }));
/// ```
pub fn with_trace_id<F: std::future::Future>(
    id: u64,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    TRACE_ID.scope(id, fut)
}

/// Returns the trace ID for the current task, or `0` if none has been set.
#[inline]
pub fn current_trace_id() -> u64 {
    TRACE_ID.try_with(|id| *id).unwrap_or(0)
}

/// RAII performance guard. Emits a `tracing::info!` event with target
/// `"whitenoise::perf"` and fields `trace_id`, `ts_begin_us`, and `duration_ns`
/// when dropped.
///
/// This type is `Send`, so it is safe to hold across `.await` points.
pub struct PerfGuard {
    name: &'static str,
    trace_id: u64,
    ts_begin_us: u64,
    start: Instant,
}

impl PerfGuard {
    #[inline]
    pub fn new(name: &'static str) -> Self {
        let ts_begin_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        Self {
            name,
            trace_id: current_trace_id(),
            ts_begin_us,
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
            trace_id = self.trace_id,
            ts_begin_us = self.ts_begin_us,
            duration_ns = ns,
            "perf"
        );
    }
}

/// Emits a performance timing event with target `"whitenoise::perf"` when
/// the returned guard is dropped.
///
/// Returns `Option<PerfGuard>`: `Some` when the `whitenoise::perf` target is
/// enabled by the current tracing subscriber, `None` otherwise.  When `None`,
/// no `Instant::now()` or `SystemTime::now()` call is made — truly zero cost.
///
/// The returned value is `Send` and safe to hold across `.await` points.
/// Name the guard with a leading underscore (`_span`) to keep the RAII lifetime
/// intact without triggering the unused-variable warning.
///
/// # Examples
///
/// ```ignore
/// let _span = perf_span!("messages::send_message_to_group");
/// // ... async work including awaits ...
/// // guard dropped here → timing event emitted (or no-op if tracing disabled)
/// ```
#[macro_export]
macro_rules! perf_span {
    ($name:literal) => {
        if tracing::enabled!(target: "whitenoise::perf", tracing::Level::INFO) {
            Some($crate::perf::PerfGuard::new($name))
        } else {
            None
        }
    };
}
