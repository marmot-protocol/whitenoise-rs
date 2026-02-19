use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::Whitenoise;
use crate::WhitenoiseError;

mod tasks;

pub(crate) use self::tasks::CachedGraphUserCleanup;
pub(crate) use self::tasks::ConsumedKeyPackageCleanup;
pub(crate) use self::tasks::KeyPackageMaintenance;

/// Trait for implementing scheduled background tasks.
///
/// Tasks are executed on startup and periodically thereafter based on their configured interval.
/// Implementations should be idempotent and handle transient failures gracefully.
#[async_trait]
pub trait Task: Send + Sync {
    /// Returns the unique name of this task for logging and identification.
    fn name(&self) -> &'static str;

    /// Returns the interval between task executions.
    fn interval(&self) -> Duration;

    /// Executes the task.
    ///
    /// Implementations should:
    /// - Be idempotent (safe to run multiple times)
    /// - Handle transient failures gracefully (log and continue)
    /// - Avoid holding locks for extended periods
    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError>;
}

/// Configuration for the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Whether the scheduler is enabled.
    pub enabled: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Starts all scheduled tasks and returns their handles.
///
/// Each task runs in its own spawned tokio task. The first execution happens
/// immediately (tokio interval first-tick behavior), then repeats at the
/// configured interval.
pub(super) fn start_scheduled_tasks(
    whitenoise: &'static Whitenoise,
    shutdown_rx: watch::Receiver<bool>,
    config: Option<SchedulerConfig>,
    tasks: Vec<Arc<dyn Task>>,
) -> Vec<JoinHandle<()>> {
    let config = config.unwrap_or_default();

    if !config.enabled {
        tracing::info!(target: "whitenoise::scheduler", "Scheduler is disabled");
        return Vec::new();
    }

    if tasks.is_empty() {
        tracing::debug!(target: "whitenoise::scheduler", "No scheduled tasks registered");
        return Vec::new();
    }

    let mut handles = Vec::with_capacity(tasks.len());

    for task in tasks {
        let mut task_shutdown_rx = shutdown_rx.clone();
        let handle = tokio::spawn(async move {
            let task_name = task.name();

            // First tick fires immediately, then at interval
            let mut interval = tokio::time::interval(task.interval());
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        tracing::debug!(
                            target: "whitenoise::scheduler",
                            "Executing task: {}",
                            task_name
                        );
                        if let Err(e) = task.execute(whitenoise).await {
                            tracing::warn!(
                                target: "whitenoise::scheduler",
                                "Task {} failed: {}",
                                task_name,
                                e
                            );
                        }
                    }
                    _ = task_shutdown_rx.changed() => {
                        tracing::info!(
                            target: "whitenoise::scheduler",
                            "Task {} received shutdown signal",
                            task_name
                        );
                        break;
                    }
                }
            }
        });

        handles.push(handle);
    }

    tracing::info!(
        target: "whitenoise::scheduler",
        "Started {} scheduled task(s)",
        handles.len()
    );

    handles
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    /// Test task that tracks execution count
    struct CountingTask {
        name: &'static str,
        interval: Duration,
        execution_count: Arc<AtomicUsize>,
    }

    impl CountingTask {
        fn new(name: &'static str, interval: Duration) -> (Self, Arc<AtomicUsize>) {
            let count = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    name,
                    interval,
                    execution_count: count.clone(),
                },
                count,
            )
        }
    }

    #[async_trait]
    impl Task for CountingTask {
        fn name(&self) -> &'static str {
            self.name
        }

        fn interval(&self) -> Duration {
            self.interval
        }

        async fn execute(&self, _whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
            self.execution_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn test_scheduler_config_default() {
        let config = SchedulerConfig::default();
        assert!(config.enabled);
    }

    #[tokio::test]
    async fn test_empty_task_list_returns_empty_handles() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let handles = start_scheduled_tasks(whitenoise, shutdown_rx, None, vec![]);

        assert!(handles.is_empty());
    }

    #[tokio::test]
    async fn test_disabled_scheduler_returns_empty_handles() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let config = SchedulerConfig { enabled: false };
        let (task, _count) = CountingTask::new("test", Duration::from_millis(10));

        let handles =
            start_scheduled_tasks(whitenoise, shutdown_rx, Some(config), vec![Arc::new(task)]);

        assert!(handles.is_empty());
    }

    #[tokio::test]
    async fn test_task_executes_on_first_tick() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let (task, count) = CountingTask::new("test_task", Duration::from_secs(3600));
        let handles = start_scheduled_tasks(whitenoise, shutdown_rx, None, vec![Arc::new(task)]);

        assert_eq!(handles.len(), 1);

        // Give the task time to execute its first tick
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Should have executed once (first tick is immediate)
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Shutdown
        let _ = shutdown_tx.send(true);
        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_task_responds_to_shutdown() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let (task, _count) = CountingTask::new("shutdown_test", Duration::from_secs(3600));
        let handles = start_scheduled_tasks(whitenoise, shutdown_rx, None, vec![Arc::new(task)]);

        assert_eq!(handles.len(), 1);

        // Send shutdown signal
        let _ = shutdown_tx.send(true);

        // Task should complete promptly
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            handles.into_iter().next().unwrap(),
        )
        .await;

        assert!(result.is_ok(), "Task should shut down within timeout");
    }

    #[tokio::test]
    async fn test_multiple_tasks_spawn_independently() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let (task1, count1) = CountingTask::new("task1", Duration::from_secs(3600));
        let (task2, count2) = CountingTask::new("task2", Duration::from_secs(3600));

        let handles = start_scheduled_tasks(
            whitenoise,
            shutdown_rx,
            None,
            vec![Arc::new(task1), Arc::new(task2)],
        );

        assert_eq!(handles.len(), 2);

        // Give tasks time to execute first tick
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Both should have executed once
        assert_eq!(count1.load(Ordering::SeqCst), 1);
        assert_eq!(count2.load(Ordering::SeqCst), 1);

        // Shutdown
        let _ = shutdown_tx.send(true);
        for handle in handles {
            handle.await.unwrap();
        }
    }
}
