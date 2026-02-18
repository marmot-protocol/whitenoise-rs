use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkResult, BenchmarkScenario};
use crate::integration_tests::core::ScenarioContext;
use crate::whitenoise::user_search::UserSearchParams;
use crate::{SearchUpdateTrigger, UserSearchResult, Whitenoise, WhitenoiseError};

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

pub struct UserSearchBenchmark;

#[async_trait]
impl BenchmarkScenario for UserSearchBenchmark {
    fn name(&self) -> &str {
        "User Search - Cold Run"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            iterations: 1,
            warmup_iterations: 0,
            cooldown_between_iterations: Duration::ZERO,
        }
    }

    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        // Create an identity to use as the searcher
        let account = context.whitenoise.create_identity().await?;
        context.add_account("searcher", account);
        Ok(())
    }

    async fn single_iteration(
        &self,
        _context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        // Not used — we override run_benchmark for custom orchestration
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

        tracing::info!("=== Benchmark: Layer Building ===");

        // Measure radius 0-1 search (layer building + metadata resolution)
        let (r1_timings, _) = run_search_and_collect(whitenoise, "", searcher_pubkey, 0, 1).await?;

        for (label, duration) in &r1_timings {
            tracing::info!("  {}: {:?}", label, duration);
        }

        // Measure radius 2 search (friends-of-friends)
        let (r2_timings, _) = run_search_and_collect(whitenoise, "", searcher_pubkey, 0, 2).await?;

        for (label, duration) in &r2_timings {
            tracing::info!("  {}: {:?}", label, duration);
        }

        tracing::info!("=== Benchmark: Name Search ===");

        let mut search_timings: Vec<Duration> = Vec::new();

        for &(query, expected_npub) in SEARCH_TARGETS {
            let expected_pk = PublicKey::parse(expected_npub).map_err(|e| {
                WhitenoiseError::InvalidInput(format!("Invalid target npub: {}", e))
            })?;

            let start = Instant::now();
            let (timings, results) =
                run_search_and_collect(whitenoise, query, searcher_pubkey, 0, 2).await?;
            let total = start.elapsed();

            let found = results.iter().any(|r| r.pubkey == expected_pk);

            tracing::info!(
                "  Search '{}': {:?} (found={}, results={})",
                query,
                total,
                found,
                results.len()
            );
            for (label, duration) in &timings {
                tracing::info!("    {}: {:?}", label, duration);
            }

            search_timings.push(total);
        }

        // Use the total of all search timings as the benchmark duration
        let total_duration: Duration = search_timings.iter().sum();

        Ok(BenchmarkResult::from_timings(
            self.name().to_string(),
            &self.config(),
            search_timings,
            total_duration,
        ))
    }
}

/// Run a search and collect timing breakdowns from streaming updates.
///
/// Returns per-radius timing labels and all search results found.
async fn run_search_and_collect(
    whitenoise: &Whitenoise,
    query: &str,
    searcher_pubkey: PublicKey,
    radius_start: u8,
    radius_end: u8,
) -> Result<(Vec<(String, Duration)>, Vec<UserSearchResult>), WhitenoiseError> {
    let sub = whitenoise
        .search_users(UserSearchParams {
            query: query.to_string(),
            searcher_pubkey,
            radius_start,
            radius_end,
        })
        .await?;

    let mut rx = sub.updates;
    let mut timings: Vec<(String, Duration)> = Vec::new();
    let mut results = Vec::new();
    let mut radius_starts: HashMap<u8, Instant> = HashMap::new();
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
                        timings.push((
                            format!("Radius {} ({} pubkeys)", radius, total_pubkeys_searched),
                            start.elapsed(),
                        ));
                    }
                }
                SearchUpdateTrigger::RadiusTimeout { radius } => {
                    if let Some(start) = radius_starts.get(radius) {
                        timings.push((format!("Radius {} (TIMEOUT)", radius), start.elapsed()));
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
                    results.extend(update.new_results);
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

    timings.push(("Total".to_string(), overall_start.elapsed()));

    Ok((timings, results))
}
