use std::future::Future;

use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelErr {
    Cancelled,
}

pub(crate) trait OrCancelExt: Future + Sized {
    fn or_cancel<'a>(
        self,
        token: &'a CancellationToken,
    ) -> impl Future<Output = Result<Self::Output, CancelErr>> + Send + 'a
    where
        Self: Send + 'a,
        Self::Output: Send + 'a,
    {
        async move {
            tokio::select! {
                _ = token.cancelled() => Err(CancelErr::Cancelled),
                output = self => Ok(output),
            }
        }
    }
}

impl<F> OrCancelExt for F where F: Future + Sized {}
