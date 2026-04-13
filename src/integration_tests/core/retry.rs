use crate::WhitenoiseError;
use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 10,
            delay: Duration::from_millis(100),
        }
    }
}

impl RetryConfig {
    /// Create custom retry configuration
    pub fn new(max_retries: usize, delay: Duration) -> Self {
        Self { max_retries, delay }
    }
}

/// Retry an async operation until it succeeds or max retries is reached
pub async fn retry_until<F, Fut, T>(
    config: RetryConfig,
    operation: F,
    description: &str,
) -> Result<T, WhitenoiseError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, WhitenoiseError>>,
{
    let mut retry_count = 0;

    loop {
        match operation().await {
            Ok(result) => {
                if retry_count > 0 {
                    tracing::info!("✓ {} succeeded after {} retries", description, retry_count);
                }
                return Ok(result);
            }
            Err(e) => {
                retry_count += 1;

                if retry_count >= config.max_retries {
                    tracing::error!(
                        "✗ {} failed after {} retries: {}",
                        description,
                        retry_count,
                        e
                    );
                    return Err(WhitenoiseError::Internal(format!(
                        "Operation '{}' failed after {} retries: {}",
                        description, retry_count, e
                    )));
                }

                let delay = config.delay;
                tracing::debug!(
                    "Retry {}/{}: {} failed, waiting {:?} before retry",
                    retry_count,
                    config.max_retries,
                    description,
                    delay
                );

                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Retry with default configuration (10 retries, 100ms delay)
pub async fn retry_default<F, Fut, T>(operation: F, description: &str) -> Result<T, WhitenoiseError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, WhitenoiseError>>,
{
    retry_until(RetryConfig::default(), operation, description).await
}

/// Retry with custom configuration
pub async fn retry<F, Fut, T>(
    max_retries: usize,
    delay: Duration,
    operation: F,
    description: &str,
) -> Result<T, WhitenoiseError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, WhitenoiseError>>,
{
    let config = RetryConfig::new(max_retries, delay);
    retry_until(config, operation, description).await
}
