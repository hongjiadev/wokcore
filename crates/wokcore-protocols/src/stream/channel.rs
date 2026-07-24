use std::future::Future;

use tokio::sync::{Semaphore, mpsc};

use super::ProtocolError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ReceiveError {
    #[error("event receive was cancelled")]
    Cancelled,
}

pub struct EventReceiver<T> {
    inner: mpsc::Receiver<T>,
}

impl<T> EventReceiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.inner.recv().await
    }

    pub async fn recv_or_cancel<F>(&mut self, cancellation: F) -> Result<Option<T>, ReceiveError>
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(cancellation);
        tokio::select! {
            biased;
            _ = &mut cancellation => Err(ReceiveError::Cancelled),
            event = self.inner.recv() => Ok(event),
        }
    }
}

pub fn bounded_event_channel<T>(
    capacity: usize,
) -> Result<(mpsc::Sender<T>, EventReceiver<T>), ProtocolError> {
    if capacity == 0 || capacity > Semaphore::MAX_PERMITS {
        return Err(ProtocolError::InvalidChannelCapacity);
    }

    let (sender, receiver) = mpsc::channel(capacity);
    Ok((sender, EventReceiver { inner: receiver }))
}
