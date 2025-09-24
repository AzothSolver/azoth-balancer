//! This module provides a structured graceful shutdown coordinator for Tokio tasks.
//!
//! It manages the lifecycle of all spawned background tasks, ensuring they are
//! cleanly terminated when the application receives a shutdown signal. Panics in
//! background tasks are propagated, and a shutdown timeout is enforced to avoid
//! the application hanging indefinitely. Tasks must be `Send` and `'static`
//! because they are spawned onto the Tokio runtime.

use std::time::Duration;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinSet};
use tracing::{error, info};

#[derive(Debug, Error)]
pub enum ShutdownError {
    #[error("A background task panicked during shutdown")]
    Panic(#[from] JoinError),
    #[error("Graceful shutdown timed out after {0:?}")]
    Timeout(Duration),
}

/// A manager for coordinating the graceful shutdown of background tasks.
pub struct ShutdownManager {
    tasks: JoinSet<()>,
    shutdown_tx: watch::Sender<()>,
}

impl ShutdownManager {
    /// Creates a new `ShutdownManager`.
    pub fn new() -> Self {
        let (shutdown_tx, _) = watch::channel(());
        Self { tasks: JoinSet::new(), shutdown_tx }
    }

    /// Spawns a new task that will be managed by the shutdown coordinator.
    ///
    /// The provided future will be spawned onto the Tokio runtime. The task must
    /// be `Send` and `'static` because it may outlive the current scope.
    ///
    /// If the `ShutdownManager` is dropped, all tasks spawned by it are
    /// immediately aborted.
    pub fn spawn_task<F>(&mut self, task: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.tasks.spawn(task);
    }

    /// Returns a new receiver for the shutdown signal.
    ///
    /// Each background task should subscribe to this signal to know when to
    /// begin its graceful termination.
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Immediately aborts all tasks managed by the `ShutdownManager`.
    ///
    /// This is a forced shutdown and does not wait for tasks to complete their
    /// cleanup. It is useful for situations where a quick, non-graceful
    /// termination is required.
    pub fn abort_all(&mut self) {
        self.tasks.abort_all();
    }

    /// Initiates a graceful shutdown of all managed tasks.
    ///
    /// This method first broadcasts the shutdown signal. It then waits for all
    /// tasks to complete, up to the specified timeout.
    ///
    /// This method consumes the `ShutdownManager`, preventing it from being used again.
    ///
    /// Returns `Ok(())` if all tasks shut down cleanly within the timeout.
    /// Returns `Err(ShutdownError)` if a task panicked or if the timeout is reached.
    pub async fn graceful_shutdown(self, timeout: Duration) -> Result<(), ShutdownError> {
        // Destructure self to take ownership of its fields. This avoids the
        // partial move error when we drop `shutdown_tx`.
        let ShutdownManager { mut tasks, shutdown_tx } = self;

        info!("Broadcasting shutdown signal to all {} background tasks...", tasks.len());
        // Drop the sender to signal all receivers.
        drop(shutdown_tx);

        info!("Waiting for tasks to complete...");

        let join_all_logic = async {
            while let Some(res) = tasks.join_next().await {
                // If any task returns an error (i.e., panics), we propagate it.
                res?;
            }
            Ok(())
        };

        match tokio::time::timeout(timeout, join_all_logic).await {
            Ok(Ok(_)) => {
                info!("All background tasks completed gracefully.");
                Ok(())
            }
            Ok(Err(e)) => {
                error!(error = %e, "A background task panicked during shutdown.");
                Err(ShutdownError::Panic(e))
            }
            Err(_) => {
                error!("Shutdown timeout of {:?} exceeded. Aborting remaining tasks.", timeout);
                tasks.abort_all();
                Err(ShutdownError::Timeout(timeout))
            }
        }
    }
}

impl Default for ShutdownManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, Duration};
    use tracing::info;

    #[tokio::test]
    async fn test_basic_shutdown() {
        let mut manager = ShutdownManager::new();
        let mut rx = manager.subscribe();
        manager.spawn_task(async move {
            info!("Task started, waiting for shutdown...");
            let _ = rx.changed().await;
            info!("Task received shutdown signal.");
        });
        let res = manager.graceful_shutdown(Duration::from_secs(1)).await;
        assert!(res.is_ok(), "Expected graceful shutdown to succeed");
    }

    #[tokio::test]
    async fn test_timeout() {
        let mut manager = ShutdownManager::new();
        manager.spawn_task(async {
            info!("Long task started...");
            sleep(Duration::from_secs(10)).await;
        });
        let res = manager.graceful_shutdown(Duration::from_millis(100)).await;
        assert!(res.is_err(), "Expected shutdown to return error due to timeout");
        assert!(matches!(res, Err(ShutdownError::Timeout(_))), "Expected a timeout error");
    }

    #[tokio::test]
    async fn test_panic_propagation() {
        let mut manager = ShutdownManager::new();
        manager.spawn_task(async {
            info!("Task about to panic...");
            panic!("Simulated panic");
        });
        let res = manager.graceful_shutdown(Duration::from_secs(1)).await;
        assert!(res.is_err(), "Expected shutdown to return error due to task panic");
        assert!(matches!(res, Err(ShutdownError::Panic(_))), "Expected a panic error");
    }

    #[tokio::test]
    async fn test_multiple_tasks() {
        let mut manager = ShutdownManager::new();
        let mut rx1 = manager.subscribe();
        let mut rx2 = manager.subscribe();
        manager.spawn_task(async move {
            info!("Task 1 waiting for shutdown...");
            let _ = rx1.changed().await;
            info!("Task 1 shutdown complete");
        });
        manager.spawn_task(async move {
            info!("Task 2 waiting for shutdown...");
            let _ = rx2.changed().await;
            info!("Task 2 shutdown complete");
        });
        let res = manager.graceful_shutdown(Duration::from_secs(1)).await;
        assert!(res.is_ok(), "Expected all tasks to shutdown gracefully");
    }

    #[tokio::test]
    async fn test_shutdown_with_no_tasks() {
        let manager = ShutdownManager::new();
        let res = manager.graceful_shutdown(Duration::from_secs(1)).await;
        assert!(res.is_ok(), "Shutdown should succeed immediately with no tasks");
    }

    #[tokio::test]
    async fn test_drop_does_not_panic() {
        let mut manager = ShutdownManager::new();
        manager.spawn_task(async { sleep(Duration::from_secs(10)).await });
        // Manager is dropped here; test passes if it doesn't hang or panic.
    }

    #[tokio::test]
    async fn test_task_ignores_shutdown() {
        let mut manager = ShutdownManager::new();
        manager.spawn_task(async {
            info!("Task ignoring shutdown...");
            sleep(Duration::from_secs(10)).await;
        });
        let res = manager.graceful_shutdown(Duration::from_millis(100)).await;
        assert!(res.is_err(), "Expected timeout error");
        assert!(matches!(res, Err(ShutdownError::Timeout(_))), "Expected a timeout error");
    }

    #[tokio::test]
    async fn test_abort_all() {
        let mut manager = ShutdownManager::new();
        manager.spawn_task(async {
            // This task will sleep indefinitely if not aborted.
            sleep(Duration::from_secs(60)).await;
        });

        manager.abort_all();

        let res = manager.tasks.join_next().await;
        assert!(res.is_some(), "Expected a result from the aborted task");
        let task_res = res.unwrap();
        assert!(task_res.is_err(), "Expected the task result to be an error");
        assert!(
            task_res.unwrap_err().is_cancelled(),
            "Expected the JoinError to be of type 'cancelled'"
        );
    }

    #[tokio::test]
    async fn test_task_finishes_before_shutdown() {
        let mut manager = ShutdownManager::new();
        manager.spawn_task(async {
            info!("Task that finishes early has started and finished.");
        });
        // Give the task a moment to complete.
        sleep(Duration::from_millis(50)).await;
        let res = manager.graceful_shutdown(Duration::from_secs(1)).await;
        assert!(res.is_ok(), "Shutdown should succeed even if tasks are already complete");
    }

    #[tokio::test]
    async fn test_partial_panic_scenario() {
        let mut manager = ShutdownManager::new();
        let mut rx = manager.subscribe();

        // Task that will complete normally
        manager.spawn_task(async move {
            info!("Normal task waiting for shutdown...");
            let _ = rx.changed().await;
            info!("Normal task completed");
        });

        // Task that will panic
        manager.spawn_task(async {
            info!("Task about to panic...");
            panic!("Simulated panic in mixed scenario");
        });

        let res = manager.graceful_shutdown(Duration::from_secs(1)).await;
        assert!(res.is_err(), "Expected error due to panic");
        assert!(
            matches!(res, Err(ShutdownError::Panic(_))),
            "Expected panic error even with other tasks completing normally"
        );
    }

    #[tokio::test]
    async fn test_task_spawns_subtask() {
        let mut manager = ShutdownManager::new();
        let mut rx = manager.subscribe();

        manager.spawn_task(async move {
            info!("Parent task starting...");

            // Spawn a subtask (not managed by ShutdownManager)
            let subtask = tokio::spawn(async {
                sleep(Duration::from_millis(100)).await;
                info!("Subtask completed");
            });

            // Wait for shutdown signal
            let _ = rx.changed().await;

            // Wait for subtask to complete during shutdown
            let _ = subtask.await;
            info!("Parent task completed after subtask");
        });

        let res = manager.graceful_shutdown(Duration::from_secs(1)).await;
        assert!(res.is_ok(), "Should handle tasks that spawn their own subtasks");
    }

    #[tokio::test]
    async fn test_multiple_receivers_same_task() {
        let mut manager = ShutdownManager::new();
        let mut rx1 = manager.subscribe();
        let mut rx2 = manager.subscribe();

        manager.spawn_task(async move {
            info!("Task with multiple receivers starting...");

            // Wait on first receiver
            let _ = rx1.changed().await;
            info!("First receiver got signal");

            // Second receiver should also be signaled
            let _ = rx2.changed().await;
            info!("Second receiver got signal");
        });

        let res = manager.graceful_shutdown(Duration::from_secs(1)).await;
        assert!(res.is_ok(), "Task using multiple receivers should complete");
    }
}
