//! Initialization phase timing for benchmark diagnostics.
//!
//! Records wall-clock duration of each phase in [`initialize_whitenoise()`].
//! In production builds all functions are no-ops that compile to nothing.
//!
//! [`initialize_whitenoise()`]: super::Whitenoise::initialize_whitenoise

#[cfg(feature = "benchmark-tests")]
mod inner {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct State {
        overall_start: Instant,
        phase_start: Instant,
        phases: Vec<(&'static str, Duration)>,
    }

    static STATE: Mutex<Option<State>> = Mutex::new(None);

    /// Begin a new timing session, resetting any previous state.
    pub fn start() {
        let now = Instant::now();
        *STATE.lock().unwrap() = Some(State {
            overall_start: now,
            phase_start: now,
            phases: Vec::new(),
        });
    }

    /// Record the elapsed time since the last [`record`] (or [`start`]) call.
    pub fn record(phase: &'static str) {
        let mut guard = STATE.lock().unwrap();
        let state = guard.as_mut().expect("init_timing::start() not called");
        let elapsed = state.phase_start.elapsed();
        state.phases.push((phase, elapsed));
        state.phase_start = Instant::now();
    }

    /// Log a summary of all recorded phases via `tracing::info!`.
    pub fn report() {
        use std::fmt::Write;

        let guard = STATE.lock().unwrap();
        let state = guard.as_ref().expect("init_timing::start() not called");
        let total = state.overall_start.elapsed();

        let mut report = String::from("\n=== Initialization Timing ===\n");
        for (phase, duration) in &state.phases {
            let pct = if total.as_nanos() > 0 {
                duration.as_secs_f64() / total.as_secs_f64() * 100.0
            } else {
                0.0
            };
            let _ = writeln!(report, "  {phase:<40} {duration:>10.1?}  ({pct:.1}%)");
        }
        let _ = write!(report, "  {:<40} {:>10.1?}", "TOTAL", total);

        tracing::info!(target: "whitenoise::init_timing", "{}", report);
    }
}

#[cfg(not(feature = "benchmark-tests"))]
mod inner {
    #[inline(always)]
    pub fn start() {}

    #[inline(always)]
    pub fn record(_phase: &'static str) {}

    #[inline(always)]
    pub fn report() {}
}

pub use inner::*;
