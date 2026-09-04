use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
};

use thiserror::Error;

/// Limits one running Deployment or Rollback Deployment to this process session.
#[derive(Debug, Default)]
pub struct DeploymentSession {
    active: AtomicBool,
}

impl DeploymentSession {
    /// Runs an operation only when this session has no active Deployment.
    ///
    /// The permit is released when the future completes, fails, or is dropped
    /// because its task was cancelled.
    ///
    /// # Errors
    ///
    /// Returns [`SessionBusy`] without polling `operation` when another
    /// Deployment is active in this session.
    pub async fn run<F, T>(&self, operation: F) -> Result<T, SessionBusy>
    where
        F: Future<Output = T>,
    {
        let _permit = self.try_acquire()?;
        Ok(operation.await)
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn try_acquire(&self) -> Result<SessionPermit<'_>, SessionBusy> {
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| SessionPermit { session: self })
            .map_err(|_| SessionBusy)
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("this TUI session already has an active Deployment")]
pub struct SessionBusy;

#[derive(Debug)]
struct SessionPermit<'a> {
    session: &'a DeploymentSession,
}

impl Drop for SessionPermit<'_> {
    fn drop(&mut self) {
        self.session.active.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::sync::Notify;

    use super::*;

    #[tokio::test]
    async fn rejects_a_second_operation_without_polling_it() {
        let session = Arc::new(DeploymentSession::default());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = {
            let session = Arc::clone(&session);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                session
                    .run(async {
                        started.notify_one();
                        release.notified().await;
                    })
                    .await
            })
        };
        started.notified().await;

        let polls = AtomicUsize::new(0);
        let result = session
            .run(async {
                polls.fetch_add(1, Ordering::Relaxed);
            })
            .await;
        assert_eq!(result, Err(SessionBusy));
        assert_eq!(polls.load(Ordering::Relaxed), 0);

        release.notify_one();
        assert_eq!(first.await.unwrap(), Ok(()));
        assert!(!session.is_active());
    }

    #[tokio::test]
    async fn completion_and_operation_error_release_the_session() {
        let session = DeploymentSession::default();
        assert_eq!(session.run(async { 7 }).await, Ok(7));
        assert!(!session.is_active());

        let operation = session.run(async { Err::<(), _>("failed") }).await;
        assert_eq!(operation, Ok(Err("failed")));
        assert!(!session.is_active());
        assert_eq!(session.run(async { 9 }).await, Ok(9));
    }

    #[tokio::test]
    async fn task_cancellation_releases_the_session() {
        let session = Arc::new(DeploymentSession::default());
        let started = Arc::new(Notify::new());
        let never = Arc::new(Notify::new());
        let task = {
            let session = Arc::clone(&session);
            let started = Arc::clone(&started);
            let never = Arc::clone(&never);
            tokio::spawn(async move {
                session
                    .run(async {
                        started.notify_one();
                        never.notified().await;
                    })
                    .await
            })
        };
        started.notified().await;
        assert!(session.is_active());

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!session.is_active());
        assert_eq!(session.run(async { "next" }).await, Ok("next"));
    }
}
