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

/// Register the global perf layer. Call once from `benchmark_test.rs` before
/// initializing Whitenoise. Returns a clone of the layer suitable for adding
/// to the tracing subscriber stack.
pub fn init_perf_layer() -> PerfTracingLayer {
    let layer = PerfTracingLayer::new();
    // If already initialised (e.g., called twice in tests) just return a new
    // layer — PERF_LAYER will already be set.
    let _ = PERF_LAYER.set(layer.clone());
    layer
}
