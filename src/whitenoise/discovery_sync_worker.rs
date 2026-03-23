use std::future::Future;
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio::time::timeout;

use crate::whitenoise::error::Result;

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

pub(crate) struct DiscoverySyncWorker {
    notify: Notify,
    generation: watch::Sender<u64>,
}

impl DiscoverySyncWorker {
    pub(crate) fn new() -> Self {
        let (generation, _) = watch::channel(0u64);
        Self {
            notify: Notify::new(),
            generation,
        }
    }

    pub(crate) fn request_rebuild(&self) {
        tracing::debug!(
            target: "whitenoise::discovery_sync",
            "Discovery sync rebuild requested"
        );
        self.notify.notify_one();
    }

    pub(crate) async fn request_rebuild_and_wait(&self) -> Result<()> {
        let mut rx = self.generation.subscribe();
        let current = *rx.borrow_and_update();
        self.notify.notify_one();
        rx.wait_for(|g| *g > current).await.map_err(|_| {
            crate::whitenoise::error::WhitenoiseError::Other(anyhow::anyhow!(
                "Discovery sync worker stopped"
            ))
        })?;
        Ok(())
    }

    /// Runs the worker loop using `sync_discovery_subscriptions` from the
    /// global Whitenoise instance. This is the production entry point.
    pub(crate) async fn run(&self, shutdown_rx: watch::Receiver<bool>) {
        self.run_with(shutdown_rx, || async {
            crate::whitenoise::Whitenoise::get_instance()?
                .sync_discovery_subscriptions()
                .await
        })
        .await;
    }

    /// Runs the worker loop with a custom work function. Used by tests to
    /// inject a fake rebuild without requiring database or relay connections.
    async fn run_with<F, Fut>(&self, mut shutdown_rx: watch::Receiver<bool>, work: F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                _ = self.notify.notified() => {
                    loop {
                        self.do_rebuild(&work).await;
                        match timeout(DEBOUNCE_WINDOW, self.notify.notified()).await {
                            Ok(()) => {
                                tracing::debug!(
                                    target: "whitenoise::discovery_sync",
                                    "Signal received during debounce window, rebuilding again"
                                );
                                continue;
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        }
    }

    async fn do_rebuild<F, Fut>(&self, work: &F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        tracing::info!(
            target: "whitenoise::discovery_sync",
            "Discovery sync rebuild started"
        );
        if let Err(e) = work().await {
            tracing::warn!(
                target: "whitenoise::discovery_sync",
                "Discovery sync rebuild failed: {e}"
            );
        }
        self.generation.send_modify(|g| *g += 1);
        tracing::info!(
            target: "whitenoise::discovery_sync",
            generation = *self.generation.borrow(),
            "Discovery sync rebuild completed"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn signal_triggers_one_rebuild() {
        let worker = Arc::new(DiscoverySyncWorker::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let count = Arc::new(AtomicUsize::new(0));

        let count_clone = count.clone();
        let worker_clone = worker.clone();
        let handle = tokio::spawn(async move {
            worker_clone
                .run_with(shutdown_rx, || {
                    let c = count_clone.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        // Signal the worker
        worker.request_rebuild();

        // Advance time past debounce window so the worker settles
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Shut down
        let _ = shutdown_tx.send(true);
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn rapid_signals_coalesce() {
        let worker = Arc::new(DiscoverySyncWorker::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let count = Arc::new(AtomicUsize::new(0));

        let count_clone = count.clone();
        let worker_clone = worker.clone();
        let handle = tokio::spawn(async move {
            worker_clone
                .run_with(shutdown_rx, || {
                    let c = count_clone.clone();
                    async move {
                        // Simulate some work
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        // Fire 5 signals rapidly before any rebuild can finish
        for _ in 0..5 {
            worker.request_rebuild();
        }

        // Advance time enough for rebuilds + debounce to settle
        tokio::time::sleep(Duration::from_secs(2)).await;

        let total = count.load(Ordering::SeqCst);
        // Notify stores at most one permit, so 5 rapid signals should coalesce
        // to at most 2 rebuilds (one for initial wakeup, one for coalesced follow-up)
        assert!(
            total <= 2,
            "Expected at most 2 rebuilds from 5 rapid signals, got {total}"
        );
        assert!(total >= 1, "Expected at least 1 rebuild, got {total}");

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn request_rebuild_and_wait_blocks_until_done() {
        let worker = Arc::new(DiscoverySyncWorker::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let count = Arc::new(AtomicUsize::new(0));

        let count_clone = count.clone();
        let worker_clone = worker.clone();
        let handle = tokio::spawn(async move {
            worker_clone
                .run_with(shutdown_rx, || {
                    let c = count_clone.clone();
                    async move {
                        // Slow work — 200ms
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        // request_rebuild_and_wait should not return until the rebuild finishes
        worker.request_rebuild_and_wait().await.unwrap();

        // At this point the work function must have completed
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn generation_advances_with_each_rebuild() {
        let worker = Arc::new(DiscoverySyncWorker::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut gen_rx = worker.generation.subscribe();

        let worker_clone = worker.clone();
        let handle = tokio::spawn(async move {
            worker_clone
                .run_with(shutdown_rx, || async { Ok(()) })
                .await;
        });

        assert_eq!(*gen_rx.borrow(), 0);

        // Trigger 3 separate rebuilds, each separated by enough time for debounce
        for expected in 1..=3u64 {
            worker.request_rebuild_and_wait().await.unwrap();
            // Let the debounce window pass so the worker goes idle
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(*gen_rx.borrow_and_update(), expected);
        }

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_worker() {
        let worker = Arc::new(DiscoverySyncWorker::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let worker_clone = worker.clone();
        let handle = tokio::spawn(async move {
            worker_clone
                .run_with(shutdown_rx, || async { Ok(()) })
                .await;
        });

        // Send shutdown without any rebuild signal
        let _ = shutdown_tx.send(true);

        // Worker should exit promptly
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "Worker should shut down within the timeout");
        result.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn errors_do_not_stop_worker() {
        let worker = Arc::new(DiscoverySyncWorker::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let count = Arc::new(AtomicUsize::new(0));

        let count_clone = count.clone();
        let worker_clone = worker.clone();
        let handle = tokio::spawn(async move {
            worker_clone
                .run_with(shutdown_rx, || {
                    let c = count_clone.clone();
                    async move {
                        let call = c.fetch_add(1, Ordering::SeqCst);
                        if call == 0 {
                            // First call fails
                            Err(crate::whitenoise::error::WhitenoiseError::Other(
                                anyhow::anyhow!("simulated failure"),
                            ))
                        } else {
                            Ok(())
                        }
                    }
                })
                .await;
        });

        // First rebuild (will error)
        worker.request_rebuild_and_wait().await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Second rebuild (will succeed) — worker must still be alive
        worker.request_rebuild_and_wait().await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(count.load(Ordering::SeqCst), 2);
        // Generation should be 2 — errors still advance the counter
        assert_eq!(*worker.generation.subscribe().borrow(), 2);

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();
    }
}
