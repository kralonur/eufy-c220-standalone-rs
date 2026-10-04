//! Only public service handles own senders. Background tasks hold receivers, so
//! dropping the last handle is a durable shutdown signal, even before first poll.

use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct Shutdown(watch::Receiver<()>);

impl Shutdown {
    pub fn channel() -> (watch::Sender<()>, Self) {
        let (owner, receiver) = watch::channel(());
        (owner, Self(receiver))
    }

    pub fn is_stopped(&self) -> bool {
        self.0.has_changed().is_err()
    }

    pub async fn wait(&mut self) {
        // Nothing sends values; closure is the only transition. It cannot be
        // lost before polling, unlike Notify::notify_waiters.
        let _ = self.0.changed().await;
    }
}
