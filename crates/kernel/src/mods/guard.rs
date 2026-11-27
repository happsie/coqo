// SPDX-License-Identifier: Apache-2.0

//! Runs mod code so that a panic becomes a [`ModError`] instead of taking
//! the daemon down.

use std::any::Any;
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll};

use coqo_mod_api::{BoxFuture, ModError};

/// Polls a future, catching panics.
struct CatchUnwind<F>(F);

impl<F: Future + Unpin> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.0;
        // The registries recover from poisoned locks, and a mod that
        // panicked is withdrawn, so observing its broken state is fine.
        match panic::catch_unwind(AssertUnwindSafe(|| Pin::new(&mut *inner).poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// Calls `start` and awaits the future it returns, turning a panic in
/// either into [`ModError::Failed`] mentioning `what`.
pub(super) async fn guarded<'a, T>(
    what: impl FnOnce() -> String,
    start: impl FnOnce() -> BoxFuture<'a, Result<T, ModError>>,
) -> Result<T, ModError> {
    let result = match panic::catch_unwind(AssertUnwindSafe(start)) {
        Ok(future) => CatchUnwind(future).await,
        Err(payload) => Err(payload),
    };
    result.unwrap_or_else(|payload| {
        Err(ModError::failed(format!(
            "{} panicked: {}",
            what(),
            panic_message(&*payload)
        )))
    })
}
