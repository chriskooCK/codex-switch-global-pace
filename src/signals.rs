//! Signal listeners that survive the gaps between polls.
//!
//! Tokio *discards* a delivered signal when nothing is registered for it at
//! broadcast time — it does not defer it. So the registration has to exist
//! before the window it protects, and it has to outlive every future built
//! from it: a listener created inside a `select!` arm is rebuilt on every loop
//! iteration, and one created after the critical section starts leaves that
//! section running under the default terminate action.

use anyhow::Result;

/// Listens for the signals that mean "wind down now".
pub(crate) struct ShutdownListener {
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
    #[cfg(not(unix))]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl ShutdownListener {
    /// SIGTERM and SIGINT (Ctrl+C on Windows), for a process that owns its own
    /// lifetime and should shut down cleanly on either.
    pub(crate) fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                sigterm: signal(SignalKind::terminate())?,
                sigint: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    /// Resolves once a signal has been received. Safe to cancel: the
    /// registration lives in `self`, so a signal that arrives while this future
    /// is not being polled is still observed by the next call.
    pub(crate) async fn recv(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.sigterm.recv() => {},
                _ = self.sigint.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            self.ctrl_c.recv().await;
        }
    }
}

/// Serialises tests that raise a signal at this process.
#[cfg(all(test, unix))]
pub(crate) static RAISE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(all(test, unix))]
mod tests {
    use super::{RAISE_LOCK, ShutdownListener};
    use std::time::Duration;
    use tokio::signal::unix::{SignalKind, signal};

    /// `daemon stop` sends a single SIGTERM. The daemon's select loop spends a
    /// large share of every second inside a branch body (the poll branch does
    /// HTTP round trips), and during that time nothing polls the shutdown
    /// branch. Tokio drops a delivered signal outright when no listener is
    /// registered at broadcast time, so the listener has to survive across
    /// loop iterations rather than be rebuilt inside `select!`.
    #[tokio::test]
    async fn shutdown_listener_catches_a_signal_raised_while_it_is_not_polled() {
        let _raise = RAISE_LOCK.lock().await;
        let mut listener = ShutdownListener::new().expect("shutdown listener");

        // A second listener registered up front turns "tokio has finished
        // broadcasting the signal" into an awaitable event, so the assertion
        // below never depends on sleeping long enough.
        let mut witness = signal(SignalKind::terminate()).expect("witness listener");

        // SAFETY: raising SIGTERM at our own process. Both listeners above are
        // registered first, so tokio's handler is installed and the default
        // terminate action cannot fire.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        witness.recv().await;

        // `listener` was never polled before the broadcast -- exactly the state
        // the daemon loop is in while a poll body runs.
        tokio::time::timeout(Duration::from_secs(5), listener.recv())
            .await
            .expect("a SIGTERM delivered while the loop was busy must not be lost");
    }
}
