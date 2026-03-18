pub mod bench_utils;
pub mod chrome_trace;
pub mod core;
pub mod perf_layer;
pub mod registry;
pub mod scenarios;
pub mod serde_duration;
pub mod stats;
pub mod test_cases;

use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

// Re-export commonly used items for convenience
pub use core::{BenchmarkConfig, BenchmarkResult, BenchmarkScenario, BenchmarkTestCase};
pub use perf_layer::{IterationDetail, PerfBreakdown, PerfSample, PerfTracingLayer};

/// Set to `true` before running benchmarks when `--detailed` is requested.
/// The benchmark scenario loop reads this to decide whether to call
/// `checkpoint()` and `drain_per_iteration()`.
pub static DETAILED_MODE: AtomicBool = AtomicBool::new(false);

/// Global handle to the `PerfTracingLayer` registered at benchmark binary startup.
///
/// Set once via [`init_perf_layer`]; readable everywhere inside the benchmark suite.
pub static PERF_LAYER: OnceLock<PerfTracingLayer> = OnceLock::new();

/// Return the global perf layer, creating it on the first call.
///
/// Every invocation returns a clone of the *same* `PerfTracingLayer` stored in
/// [`PERF_LAYER`], so the layer added to the subscriber stack and the one read
/// via `PERF_LAYER.get()` share the same backing sample buffer.
pub fn init_perf_layer() -> PerfTracingLayer {
    PERF_LAYER.get_or_init(PerfTracingLayer::new).clone()
}
