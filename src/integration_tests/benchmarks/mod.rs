pub mod core;
pub mod perf_layer;
pub mod registry;
pub mod scenarios;
pub mod stats;
pub mod test_cases;

use std::sync::OnceLock;

// Re-export commonly used items for convenience
pub use core::{BenchmarkConfig, BenchmarkResult, BenchmarkScenario, BenchmarkTestCase};
pub use perf_layer::{PerfBreakdown, PerfSample, PerfTracingLayer};

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
