use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkResult, BenchmarkScenario};
use crate::integration_tests::core::ScenarioContext;
use crate::whitenoise::user_search::UserSearchParams;
use crate::{SearchUpdateTrigger, Whitenoise, WhitenoiseError};

/// Target npub for the benchmark searcher identity.
/// This user's social graph is traversed during the benchmark.
const SEARCHER_NPUB: &str = "npub1jgm0ntzjr03wuzj5788llhed7l6fst05um4ej2r86ueaa08etv6sgd669p";

/// Search targets: (query, expected npub)
const SEARCH_TARGETS: &[(&str, &str)] = &[
    (
        "beaver",
        "npub10ydhlqtaxg3l9qevy35n48qw3wvjycc6kd4tng7qk3alewcaxtgs5gp6sv",
    ),
    (
        "jeff",
        "npub1zuuajd7u3sx8xu92yav9jwxpr839cs0kc3q6t56vd5u9q033xmhsk6c2uc",
    ),
];

/// Timing data for a single search run.
struct SearchTimings {
    /// When the first ResultsFound event arrived (any result).
    time_to_first_result: Option<Duration>,
    /// When the expected target pubkey first appeared in results.
    time_to_target: Option<Duration>,
    /// Per-radius timing (RadiusStarted → RadiusCompleted).
    radius_timings: Vec<(String, Duration)>,
    /// Total time from search start to SearchCompleted.
    time_to_complete: Duration,
    /// Total results found.
    total_results: usize,
}

pub struct UserSearchBenchmark;

#[async_trait]
impl BenchmarkScenario for UserSearchBenchmark {
    fn name(&self) -> &str {
        "User Search - Time to Result"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            iterations: 1,
            warmup_iterations: 0,
            cooldown_between_iterations: Duration::ZERO,
        }
    }

    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.whitenoise.create_identity().await?;
        context.add_account("searcher", account);
        Ok(())
    }

    async fn single_iteration(
        &self,
        _context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        unreachable!()
    }

    async fn run_benchmark(
        &mut self,
        whitenoise: &'static Whitenoise,
    ) -> Result<BenchmarkResult, WhitenoiseError> {
        let mut context = ScenarioContext::new(whitenoise);

        tracing::info!("Setting up benchmark: {}", self.name());
        self.setup(&mut context).await?;

        let searcher = context.get_account("searcher")?;
        let searcher_pubkey = searcher.pubkey;

        let target_pubkey = PublicKey::parse(SEARCHER_NPUB)
            .map_err(|e| WhitenoiseError::InvalidInput(format!("Invalid searcher npub: {}", e)))?;

        // Follow the target so their social graph becomes our radius 1
        whitenoise
            .follow_user(searcher, &target_pubkey)
            .await
            .map_err(|e| {
                WhitenoiseError::Other(anyhow::anyhow!("Failed to follow target: {}", e))
            })?;

        let mut all_timings: Vec<Duration> = Vec::new();

        for &(query, expected_npub) in SEARCH_TARGETS {
            let expected_pk = PublicKey::parse(expected_npub).map_err(|e| {
                WhitenoiseError::InvalidInput(format!("Invalid target npub: {}", e))
            })?;

            // === Cold search (no cache) ===
            tracing::info!("=== Cold search: '{}' ===", query);
            let cold = run_search_timed(whitenoise, query, searcher_pubkey, 0, 2, &expected_pk)
                .await?;

            log_timings("Cold", query, &cold);
            all_timings.push(cold.time_to_target.unwrap_or(cold.time_to_complete));

            // === Warm search (cache populated from cold run) ===
            tracing::info!("=== Warm search: '{}' ===", query);
            let warm = run_search_timed(whitenoise, query, searcher_pubkey, 0, 2, &expected_pk)
                .await?;

            log_timings("Warm", query, &warm);
            all_timings.push(warm.time_to_target.unwrap_or(warm.time_to_complete));
        }

        let total_duration: Duration = all_timings.iter().sum();

        Ok(BenchmarkResult::from_timings(
            self.name().to_string(),
            &self.config(),
            all_timings,
            total_duration,
        ))
    }
}

fn log_timings(phase: &str, query: &str, t: &SearchTimings) {
    tracing::info!(
        "  [{phase}] '{query}': target={}, first_result={}, complete={:?} ({} results)",
        t.time_to_target
            .map(|d| format!("{d:?}"))
            .unwrap_or_else(|| "NOT FOUND".to_string()),
        t.time_to_first_result
            .map(|d| format!("{d:?}"))
            .unwrap_or_else(|| "none".to_string()),
        t.time_to_complete,
        t.total_results,
    );
    for (label, duration) in &t.radius_timings {
        tracing::info!("    {}: {:?}", label, duration);
    }
}

/// Run a search, tracking when the target pubkey first appears in the result stream.
async fn run_search_timed(
    whitenoise: &Whitenoise,
    query: &str,
    searcher_pubkey: PublicKey,
    radius_start: u8,
    radius_end: u8,
    target_pubkey: &PublicKey,
) -> Result<SearchTimings, WhitenoiseError> {
    let sub = whitenoise
        .search_users(UserSearchParams {
            query: query.to_string(),
            searcher_pubkey,
            radius_start,
            radius_end,
        })
        .await?;

    let mut rx = sub.updates;
    let mut radius_timings: Vec<(String, Duration)> = Vec::new();
    let mut radius_starts: HashMap<u8, Instant> = HashMap::new();
    let mut time_to_first_result: Option<Duration> = None;
    let mut time_to_target: Option<Duration> = None;
    let mut total_results: usize = 0;
    let overall_start = Instant::now();

    loop {
        match rx.recv().await {
            Ok(update) => match &update.trigger {
                SearchUpdateTrigger::RadiusStarted { radius } => {
                    radius_starts.insert(*radius, Instant::now());
                }
                SearchUpdateTrigger::RadiusCompleted {
                    radius,
                    total_pubkeys_searched,
                } => {
                    if let Some(start) = radius_starts.get(radius) {
                        radius_timings.push((
                            format!("Radius {} ({} pubkeys)", radius, total_pubkeys_searched),
                            start.elapsed(),
                        ));
                    }
                }
                SearchUpdateTrigger::RadiusTimeout { radius } => {
                    if let Some(start) = radius_starts.get(radius) {
                        radius_timings
                            .push((format!("Radius {} (TIMEOUT)", radius), start.elapsed()));
                    }
                }
                SearchUpdateTrigger::RadiusCapped {
                    radius,
                    cap,
                    actual,
                } => {
                    tracing::info!(
                        "    Radius {} capped: {} -> {} pubkeys",
                        radius,
                        actual,
                        cap
                    );
                }
                SearchUpdateTrigger::ResultsFound => {
                    let batch_size = update.new_results.len();
                    total_results += batch_size;

                    if time_to_first_result.is_none() && batch_size > 0 {
                        time_to_first_result = Some(overall_start.elapsed());
                    }

                    if time_to_target.is_none() {
                        if update.new_results.iter().any(|r| r.pubkey == *target_pubkey) {
                            time_to_target = Some(overall_start.elapsed());
                            tracing::info!(
                                "    >> Target found at {:?} (after {} total results)",
                                time_to_target.unwrap(),
                                total_results
                            );
                        }
                    }
                }
                SearchUpdateTrigger::SearchCompleted { .. } => break,
                SearchUpdateTrigger::Error { message } => {
                    tracing::warn!("    Search error: {}", message);
                }
            },
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("    Receiver lagged by {} messages", n);
            }
        }
    }

    Ok(SearchTimings {
        time_to_first_result,
        time_to_target,
        radius_timings,
        time_to_complete: overall_start.elapsed(),
        total_results,
    })
}
