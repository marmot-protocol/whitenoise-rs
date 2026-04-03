# Integration Tests

This directory contains the modular integration test framework for Whitenoise, designed for maintainability, reusability, and comprehensive testing coverage.

## Architecture Principles

### 🎯 **Scenarios** - High-Level Test Workflows

Scenarios orchestrate multiple TestCases to test complete user workflows or system behaviors.
Each scenario targets best-effort isolation with fresh context and cleanup between runs.
Note: Because Whitenoise is a singleton, some state leakage may occur across scenarios.

**Responsibilities:**

- Define the high-level test flow (e.g., "test complete messaging workflow")
- Coordinate multiple TestCases in the correct order
- Provide scenario-specific context and setup
- Handle scenario-level error handling and cleanup

**Design Rules:**

- **Independence**: Each scenario is completely self-contained
- **Fresh Context**: Each scenario gets a new `ScenarioContext`
- **No Cross-Contamination**: No data sharing between scenarios

### 🔧 **TestCases** - Atomic Test Operations

TestCases perform single, focused operations that can be reused across scenarios.

**Responsibilities:**

- Test one specific functionality (e.g., "send a message", "create an account")
- Mutate the shared context to pass data to subsequent TestCases
- Provide meaningful assertions and error messages
- Handle operation-specific timing and setup

**Design Rules:**

- **Single Responsibility**: One TestCase = One logical operation
- **Builder Pattern**: Use fluent builders for configuration
- **Context Mutation**: Store results in context for other TestCases to use
- **Reusability**: Design for use in multiple scenarios

### 📦 **ScenarioContext** - Data Passing Between TestCases

ScenarioContext enables TestCases within the same scenario to share data.

**Allowed Usage:**

- Store accounts, groups, and message IDs created during the scenario
  - We can add more items, but be mindful to avoid unnecessary bloat
- Pass data between TestCases in the same scenario
- Track test execution statistics

**Forbidden Usage:**

- Cross-scenario data sharing
- Long-lived caches or state mirrors
- Complex business logic

## Running Integration Tests

**Prerequisites:**

- Docker services must be running: `just docker-up`

**Commands:**

```bash
# Run all integration test scenarios
just int-test

# Run a specific integration test scenario
just int-test account-management
just int-test basic-messaging
just int-test advanced-messaging
just int-test group-membership
just int-test chat-media-upload
just int-test user-discovery
```

## Directory Structure

```text
src/integration_tests/
├── mod.rs                    # Main module exports
├── registry.rs               # ScenarioRegistry - runs all scenarios
├── core/                     # Core framework components (shared by tests & benchmarks)
│   ├── mod.rs
│   ├── context.rs           # ScenarioContext for data passing
│   ├── scenario_result.rs   # Test result tracking
│   ├── test_clients.rs      # Test client utilities
│   └── traits.rs            # Scenario and TestCase traits
├── scenarios/                # High-level test workflows
│   ├── mod.rs
│   ├── account_management.rs # Account creation, login, logout
│   ├── basic_messaging.rs   # Simple messaging workflows
│   └── ...                  # Additional scenario files
├── test_cases/              # Reusable atomic test operations
│   ├── mod.rs
│   ├── account_management/   # Account-specific operations
│   │   ├── mod.rs
│   │   ├── login.rs
│   │   └── logout_account.rs
│   ├── messaging/            # Message-specific operations
│   │   ├── mod.rs
│   │   └── send_message.rs
│   ├── shared/              # Cross-scenario reusable operations
│   │   ├── mod.rs
│   │   ├── create_accounts.rs
│   │   └── create_group.rs
│   └── ...                  # Additional test case directories
└── benchmarks/              # Performance benchmarks (feature-gated)
    ├── mod.rs               # Module exports and re-exports
    ├── core/                # Core benchmark infrastructure
    │   ├── mod.rs
    │   ├── benchmark_config.rs      # BenchmarkConfig type
    │   ├── benchmark_result.rs      # BenchmarkResult type
    │   ├── benchmark_scenario.rs    # BenchmarkScenario trait
    │   └── benchmark_test_case.rs   # BenchmarkTestCase trait
    ├── registry.rs          # BenchmarkRegistry - runs all benchmarks
    ├── stats.rs             # Statistical utilities
    ├── scenarios/           # Benchmark scenarios
    │   ├── mod.rs
    │   └── messaging_performance.rs
    └── test_cases/          # Benchmark test cases
        ├── mod.rs
        └── messaging/
            ├── mod.rs
            └── send_message_benchmark.rs
```

## Adding New Tests

### Adding a New TestCase

1. **Choose the Right Directory:**

   - `test_cases/shared/` - Reusable across multiple scenarios
   - `test_cases/{domain}/` - Specific to one domain (messaging, account_management, etc.)

2. **Follow the TestCase Pattern:**

   ```rust
   use crate::integration_tests::core::*;
   use crate::WhitenoiseError;
   use async_trait::async_trait;

   // 1. Define struct with configuration
   pub struct YourTestCase {
       // Configuration fields
   }

   // 2. Implement builder methods (only if needed)
   impl YourTestCase {
       pub fn new(/* params */) -> Self { /* */ }
       pub fn with_option(mut self, option: SomeType) -> Self { /* */ }
   }

   // 3. Implement TestCase trait
   #[async_trait]
   impl TestCase for YourTestCase {
       async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
           // 1. Get data from context
           // 2. Perform operation
           // 3. Store results in context
           // 4. Add assertions
           Ok(())
       }
   }
   ```

3. **Update Module Exports:**

   ```rust
   // In the appropriate mod.rs file
   pub mod your_test_case;
   pub use your_test_case::*;
   ```

### Adding a New Scenario

1. **Create the Scenario File:**

   ```bash
   touch src/integration_tests/scenarios/your_scenario.rs
   ```

2. **Follow the Scenario Pattern:**

   ```rust
   use crate::integration_tests::core::*;
   use crate::{Whitenoise, WhitenoiseError};
   use async_trait::async_trait;

   pub struct YourScenario {
       context: ScenarioContext,
   }

   impl YourScenario {
       pub fn new(whitenoise: &'static Whitenoise) -> Self {
           Self {
               context: ScenarioContext::new(whitenoise),
           }
       }
   }

   #[async_trait]
   impl Scenario for YourScenario {
       fn context(&self) -> &ScenarioContext {
           &self.context
       }

       async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
           // Compose TestCases to create your workflow
           // Note: Use .execute instead of .run inside a scenario.

           YourTestCase::new(/* params */)
               .execute(&mut self.context)
               .await?;

           Ok(())
       }
   }
   ```

3. **Update Module Exports:**

   ```rust
   // In scenarios/mod.rs
   pub mod your_scenario;
   pub use your_scenario::*;
   ```

4. **Add to ScenarioRegistry:**

   ```rust
   // In registry.rs
   impl ScenarioRegistry {
       pub async fn run_all_scenarios(whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
           // ... existing scenarios ...
           run_scenario!(YourScenario);

           Self::print_summary(&results, overall_start.elapsed()).await;
           // ...
       }
   }
   ```

## Performance Benchmarks

Performance benchmarks are a separate category of tests designed to measure and track performance characteristics over time. They reuse the integration test infrastructure but are gated behind the `benchmark-tests` feature flag.

### Key Differences from Integration Tests

| Aspect             | Integration Tests        | Performance Benchmarks |
| ------------------ | ------------------------ | ---------------------- |
| **Purpose**        | Verify correctness       | Measure performance    |
| **Feature Flag**   | `integration-tests`      | `benchmark-tests`      |
| **Binary**         | `integration_test`       | `benchmark_test`       |
| **CI Execution**   | ✅ Always runs           | ❌ Never runs          |
| **Build Mode**     | Debug                    | Release (for accuracy) |
| **Output**         | Pass/fail results        | Timing statistics      |
| **Infrastructure** | Own scenarios/test cases | Reuses + extends       |

### Running Benchmarks

**Prerequisites:**

- Docker services must be running: `just docker-up`

**Commands:**

```bash
# Run all performance benchmarks once
just benchmark

# Run a specific benchmark scenario
just benchmark messaging-performance
just benchmark message-aggregation
just benchmark user-discovery-blocking
just benchmark user-discovery-background

# Run benchmarks and save results with timestamp
just benchmark-save                      # All benchmarks
just benchmark-save messaging-performance  # Specific benchmark
```

Benchmark results are saved to `./benchmark_results/` (git-ignored).

### Benchmark Architecture

Benchmarks use the same infrastructure as integration tests but with performance-focused traits using the **template method pattern**:

```rust
// BenchmarkScenario trait - uses template method pattern
#[async_trait]
pub trait BenchmarkScenario {
    fn name(&self) -> &str;
    fn config(&self) -> BenchmarkConfig { /* default */ }

    // Implementers define these two methods:
    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError>;
    async fn single_iteration(&self, context: &mut ScenarioContext) -> Result<Duration, WhitenoiseError>;

    // Default implementation handles all orchestration:
    async fn run_benchmark(&mut self, whitenoise: &'static Whitenoise) -> Result<BenchmarkResult, WhitenoiseError> {
        // Warmup + benchmark loops + statistics calculation
        // No need to override unless you need custom orchestration
    }
}

// BenchmarkTestCase trait - atomic timed operation
#[async_trait]
pub trait BenchmarkTestCase {
    async fn run_iteration(&self, context: &mut ScenarioContext)
        -> Result<Duration, WhitenoiseError>;
}
```

The template method pattern eliminates boilerplate - scenarios only implement `setup()` and `single_iteration()`, while the trait's default `run_benchmark(whitenoise)` implementation handles all orchestration automatically.

### Benchmark Lifecycle

The default `run_benchmark()` implementation provides this lifecycle:

1. **Setup** (not timed): Calls `setup()` to create accounts, groups, and necessary data
2. **Warmup**: Run operations to warm caches and connections
3. **Benchmark**: Execute timed iterations with cooldown between each
4. **Statistics**: Calculate mean, median, p95, p99, throughput, etc.
5. **Output**: Display comprehensive performance metrics

You can override `run_benchmark()` for custom orchestration if needed (rare).

### Adding a New Benchmark

Benchmarks use the **template method pattern** - implement `setup()` and `single_iteration()`, and the trait handles all orchestration automatically.

1. **Create a Benchmark Test Case** in `benchmarks/test_cases/`:

   ```rust
   use crate::integration_tests::benchmarks::BenchmarkTestCase;
   use crate::integration_tests::core::ScenarioContext;
   use crate::WhitenoiseError;
   use async_trait::async_trait;
   use std::time::{Duration, Instant};

   pub struct YourOperationBenchmark {
       // Configuration fields
   }

   impl YourOperationBenchmark {
       pub fn new(/* params */) -> Self {
           Self { /* ... */ }
       }
   }

   #[async_trait]
   impl BenchmarkTestCase for YourOperationBenchmark {
       async fn run_iteration(&self, context: &mut ScenarioContext)
           -> Result<Duration, WhitenoiseError> {
           let start = Instant::now();

           // Perform the operation to benchmark

           Ok(start.elapsed())
       }
   }
   ```

2. **Create a Benchmark Scenario** in `benchmarks/scenarios/`:

   ```rust
   use std::time::Duration;
   use async_trait::async_trait;
   use crate::integration_tests::benchmarks::{
       BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase,
   };
   use crate::integration_tests::core::{ScenarioContext, TestCase};
   use crate::integration_tests::test_cases::shared::CreateAccountsTestCase;
   use crate::{Whitenoise, WhitenoiseError};

   pub struct YourBenchmarkScenario {
       test_case: YourOperationBenchmark,
   }

   impl YourBenchmarkScenario {
       pub fn new(test_case: YourOperationBenchmark) -> Self {
           Self { test_case }
       }
   }

   impl Default for YourBenchmarkScenario {
       fn default() -> Self {
           Self::new(YourOperationBenchmark::new(/* params */))
       }
   }

   #[async_trait]
   impl BenchmarkScenario for YourBenchmarkScenario {
       fn name(&self) -> &str {
           "Your Benchmark Name"
       }

       fn config(&self) -> BenchmarkConfig {
           BenchmarkConfig {
               iterations: 100,
               warmup_iterations: 10,
               cooldown_between_iterations: Duration::from_millis(50),
           }
       }

       async fn setup(&mut self, context: &mut ScenarioContext)
           -> Result<(), WhitenoiseError> {
           // Create accounts, groups, and test data
           // This is NOT timed
           CreateAccountsTestCase::with_names(vec!["alice", "bob"])
               .run(context)
               .await?;

           Ok(())
       }

       async fn single_iteration(&self, context: &mut ScenarioContext)
           -> Result<Duration, WhitenoiseError> {
           self.test_case.run_iteration(context).await
       }

       // run_benchmark(whitenoise) uses default implementation automatically!
       // Only override if you need custom orchestration (rare)
   }
   ```

3. **Register in BenchmarkRegistry** (`benchmarks/registry.rs`):

   ```rust
   match YourBenchmarkScenario::default().run_benchmark(whitenoise).await {
       Ok(result) => results.push(result),
       Err(e) => {
           tracing::error!("Benchmark failed: {}", e);
           if first_error.is_none() {
               first_error = Some(e);
           }
       }
   }
   ```

**Key Points:**

- Implement only 2 methods: `setup()` and `single_iteration()`
- The `setup()` method is NOT timed - do all preparation there
- The `single_iteration()` method is timed - this is your benchmark operation
  - Be mindful of what you call here, as it will be repeated and timed for each iteration
- No need to implement `run_benchmark()` - the default handles warmup, timing, statistics
- Override `run_benchmark()` only for custom orchestration needs

### Example Output

```text
=== Running Performance Benchmarks ===

Benchmark: Message Sending Performance
  Iterations:  100
  Total Time:  4.52s

  Statistics:
    Mean:      45.2ms
    Median:    44.8ms
    Std Dev:   3.1ms
    Min:       38.5ms
    Max:       58.3ms
    P95:       51.2ms
    P99:       56.1ms

  Throughput:  22.12 ops/sec

---

Total Benchmarks: 1
Overall Duration: 4.52s
```
