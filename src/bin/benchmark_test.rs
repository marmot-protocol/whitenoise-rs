use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::Parser;

use ::whitenoise::init_tracing_with_perf_layer;
use ::whitenoise::integration_tests::benchmarks::chrome_trace::write_perfetto_trace;
use ::whitenoise::integration_tests::benchmarks::core::json_output::{
    BenchmarkOutput, ScenarioResult, thresholds_for,
};
use ::whitenoise::integration_tests::benchmarks::registry::BenchmarkRegistry;
use ::whitenoise::integration_tests::benchmarks::{DETAILED_MODE, init_perf_layer};
use ::whitenoise::*;

/// Filename written into the data directory by `--login` and read by `--seed-nsec`.
///
/// The file contains one line per keyring entry that must survive the process
/// boundary between the seeding run and the warm-init measurement runs:
///
///   `<keyring_key_id>` `<hex-encoded secret>`
///
/// Only MDK DB encryption keys (`mdk.db.key.*`) are written. The Nostr private
/// key is not needed across the boundary because the warm-init run does not
/// call `login()`.
const KEYRING_SIDECAR: &str = "benchmark_keyring.txt";

/// Service name used for all keyring entries in benchmark builds.
const KEYRING_SERVICE: &str = "com.whitenoise.benchmark";

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(long, value_name = "PATH", required = true)]
    data_dir: PathBuf,

    #[clap(long, value_name = "PATH", required = true)]
    logs_dir: PathBuf,

    /// Only measure initialization timing, then exit without running benchmarks.
    #[clap(long)]
    init_only: bool,

    /// Log in with the given nsec/hex private key, wait for the contact list
    /// to sync from relays, then shut down. Use this to seed a data directory
    /// with a real account before running `--init-only` measurements.
    ///
    /// After shutdown, the MDK DB encryption key is written to a sidecar file
    /// inside `--data-dir` so that subsequent `--init-only --seed-nsec` runs
    /// can restore it to the in-memory keyring before opening the encrypted DB.
    #[clap(long, value_name = "NSEC")]
    login: Option<String>,

    /// Restore keyring entries saved by a previous `--login` run, then proceed
    /// (usually combined with `--init-only`).
    ///
    /// The mock keyring used in benchmark builds is in-memory and does not
    /// survive across process boundaries.  The seeding `--login` run generates
    /// a random 32-byte MDK DB encryption key, stores it in the mock keyring,
    /// writes the DB, and exits — the key is gone.  The next `--init-only`
    /// process starts with an empty mock keyring, finds the encrypted SQLite
    /// file on disk, and fails with `KeyringEntryMissingForExistingDatabase`.
    ///
    /// `--login` writes the MDK key to `<data-dir>/benchmark_keyring.txt`.
    /// Passing `--seed-nsec <NSEC>` reads that sidecar and injects the key
    /// back into the mock keyring before `initialize_whitenoise` is called.
    /// The nsec is required so the Nostr private key is also re-seeded (some
    /// startup paths read the account keys from the keyring).
    #[clap(long, value_name = "NSEC")]
    seed_nsec: Option<String>,

    /// Write machine-readable JSON results to this path.
    #[clap(long, value_name = "PATH")]
    output_json: Option<PathBuf>,

    /// Capture per-iteration span detail. Required for `--chrome-trace`.
    /// Increases memory usage proportionally to iterations × spans per iteration.
    #[clap(long)]
    detailed: bool,

    /// Write a Perfetto-compatible JSON trace to this path. Requires `--detailed`.
    /// Open the output at <https://ui.perfetto.dev> or `chrome://tracing`.
    #[clap(long, value_name = "PATH")]
    chrome_trace: Option<PathBuf>,

    /// Optional scenario name to run a specific benchmark.
    /// If not provided, runs all benchmarks.
    #[clap(value_name = "SCENARIO")]
    scenario: Option<String>,
}

fn git_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn write_json_output(
    path: &std::path::Path,
    results: &[::whitenoise::integration_tests::benchmarks::BenchmarkResult],
) -> Result<(), WhitenoiseError> {
    let scenarios: Vec<ScenarioResult> = results
        .iter()
        .map(|r| {
            let cli_name = BenchmarkRegistry::cli_name_for(r).unwrap_or("unknown");
            ScenarioResult {
                result: r.clone(),
                thresholds: thresholds_for(cli_name),
            }
        })
        .collect();

    let output = BenchmarkOutput {
        generated_at: chrono::Utc::now().to_rfc3339(),
        git_sha: git_sha(),
        scenarios,
    };

    let json = serde_json::to_string_pretty(&output)?;
    std::fs::write(path, json)?;

    tracing::info!("JSON results written to {}", path.display());
    Ok(())
}

/// Saves keyring entries that must survive across process boundaries to a
/// plain-text sidecar file inside the benchmark data directory.
///
/// Only the MDK DB encryption key for the given account pubkey is saved — it
/// is a random 32-byte blob that cannot be re-derived from any other material.
/// The Nostr private key is re-seeded separately in `restore_keyring_sidecar`.
fn save_keyring_sidecar(
    data_dir: &std::path::Path,
    pubkey_hex: &str,
) -> Result<(), WhitenoiseError> {
    use keyring_core::Entry;

    let db_key_id = format!("mdk.db.key.{pubkey_hex}");
    let entry = Entry::new(KEYRING_SERVICE, &db_key_id)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!("keyring entry error: {e}")))?;

    let secret = entry.get_secret().map_err(|e| {
        WhitenoiseError::Other(anyhow::anyhow!(
            "failed to read MDK DB key from keyring: {e}"
        ))
    })?;

    let hex_secret = hex::encode(&secret);
    let line = format!("{db_key_id} {hex_secret}\n");

    let path = data_dir.join(KEYRING_SIDECAR);
    std::fs::write(&path, &line).map_err(|e| {
        WhitenoiseError::Other(anyhow::anyhow!("failed to write keyring sidecar: {e}"))
    })?;
    // Restrict to owner-read/write only — the file contains a raw encryption key.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!(
                "failed to set keyring sidecar permissions: {e}"
            ))
        })?;
    }

    tracing::info!(
        "Saved MDK DB key to sidecar {} (key_id={})",
        path.display(),
        db_key_id
    );
    Ok(())
}

/// Restores keyring entries from the sidecar file written by `save_keyring_sidecar`,
/// and also re-seeds the Nostr private key.
///
/// Must be called BEFORE `Whitenoise::initialize_whitenoise` so that the MDK
/// DB encryption key is present when `MdkSqliteStorage::new` tries to open the
/// existing database.
fn restore_keyring_sidecar(data_dir: &std::path::Path, nsec: &str) -> Result<(), WhitenoiseError> {
    use ::whitenoise::whitenoise::secrets_store::SecretsStore;
    use keyring_core::Entry;
    use nostr_sdk::Keys;

    // Must initialise the mock store before any keyring_core::Entry calls.
    Whitenoise::initialize_mock_keyring_store();

    // Re-seed the Nostr private key.
    let keys = Keys::parse(nsec)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!("Invalid --seed-nsec value: {e}")))?;
    SecretsStore::new(KEYRING_SERVICE)
        .store_private_key(&keys)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!("Failed to re-seed Nostr key: {e}")))?;
    tracing::info!(
        "Re-seeded Nostr key for pubkey {}",
        keys.public_key().to_hex()
    );

    // Restore MDK DB encryption keys from the sidecar.
    let path = data_dir.join(KEYRING_SIDECAR);
    let content = std::fs::read_to_string(&path).map_err(|e| {
        WhitenoiseError::Other(anyhow::anyhow!(
            "Failed to read keyring sidecar {}: {} — run --login first",
            path.display(),
            e
        ))
    })?;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key_id, hex_secret) = line.split_once(' ').ok_or_else(|| {
            WhitenoiseError::Other(anyhow::anyhow!("Malformed keyring sidecar line: {line:?}"))
        })?;
        let secret = hex::decode(hex_secret).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Hex decode error in sidecar: {e}"))
        })?;

        let entry = Entry::new(KEYRING_SERVICE, key_id)
            .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!("keyring entry error: {e}")))?;
        entry.set_secret(&secret).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!(
                "Failed to restore keyring entry {key_id}: {e}"
            ))
        })?;

        tracing::info!("Restored keyring entry: {key_id}");
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), WhitenoiseError> {
    let args = Args::parse();

    // Initialise the perf layer BEFORE Whitenoise initialises tracing so that
    // the layer is part of the subscriber stack from the very first span.
    let perf_layer = init_perf_layer();
    init_tracing_with_perf_layer(&args.logs_dir, perf_layer);

    if args.detailed {
        DETAILED_MODE.store(true, Ordering::Relaxed);
    }

    tracing::info!("=== Starting Whitenoise Performance Benchmark Suite ===");

    // Warm-init runs: restore MDK DB encryption key from the sidecar written by
    // the preceding --login run, and re-seed the Nostr private key. Both must be
    // in the mock keyring BEFORE initialize_whitenoise opens the encrypted MLS DB.
    if let Some(ref nsec) = args.seed_nsec {
        restore_keyring_sidecar(&args.data_dir, nsec)?;
    }

    let config = WhitenoiseConfig::new(&args.data_dir, &args.logs_dir, KEYRING_SERVICE);
    if let Err(err) = Whitenoise::initialize_whitenoise(config).await {
        tracing::error!("Failed to initialize Whitenoise: {}", err);
        std::process::exit(1);
    }

    if let Some(ref nsec) = args.login {
        let whitenoise = Whitenoise::get_instance()?;
        let account = whitenoise.login(nsec.clone()).await?;
        tracing::info!("Logged in as {}", account.pubkey.to_hex());

        // Wait for the contact list to arrive from relays
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let follows = whitenoise.follows(&account).await?;
            if !follows.is_empty() {
                tracing::info!("Contact list synced: {} follows", follows.len());
                break;
            }
            if Instant::now() > deadline {
                tracing::warn!("Timed out waiting for contact list (30s)");
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Save the MDK DB encryption key to a sidecar file so that subsequent
        // --init-only --seed-nsec runs can restore it to their empty mock keyrings.
        save_keyring_sidecar(&args.data_dir, &account.pubkey.to_hex())?;

        whitenoise.shutdown().await?;
        return Ok(());
    }

    if args.init_only {
        return Ok(());
    }

    let whitenoise = Whitenoise::get_instance()?;

    let results = match args.scenario {
        Some(ref scenario_name) => {
            BenchmarkRegistry::run_scenario(scenario_name, whitenoise).await?
        }
        None => {
            let results = BenchmarkRegistry::run_all_benchmarks(whitenoise).await?;
            tracing::info!("=== All Performance Benchmarks Completed Successfully ===");
            results
        }
    };

    if let Some(ref json_path) = args.output_json {
        write_json_output(json_path, &results)?;
    }

    if let Some(ref trace_path) = args.chrome_trace {
        if !args.detailed {
            tracing::error!("--chrome-trace requires --detailed; skipping trace output");
        } else {
            write_perfetto_trace(trace_path, &results)?;
        }
    }

    Ok(())
}
