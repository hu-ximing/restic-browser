use std::future::Future;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{AppError, Result};

pub struct JobHandle<T> {
    token: CancellationToken,
    handle: JoinHandle<Result<T>>,
}

impl<T: Send + 'static> JobHandle<T> {
    pub fn spawn<F>(future: F) -> Self
    where
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let token = CancellationToken::new();
        let handle = tokio::spawn(future);
        Self { token, handle }
    }

    pub fn spawn_cancellable<F, Fut>(factory: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let token = CancellationToken::new();
        let child = token.child_token();
        let handle = tokio::spawn(factory(child));
        Self { token, handle }
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub async fn finish(self) -> Result<T> {
        self.handle
            .await
            .map_err(|error| AppError::Other(format!("background job failed: {error}")))?
    }
}
