//! Graceful-shutdown signal handling.
//!
//! Kubernetes and systemd stop a process with SIGTERM (then SIGKILL after a grace period);
//! Ctrl-C in a terminal sends SIGINT. [`ShutdownListener`] turns either into a future.
//!
//! Installing the handlers and waiting on them are separate steps, so an installation error
//! surfaces at startup and no signal sent after startup can be missed.

use std::fmt;
use std::io;

/// Which signal asked the process to shut down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    /// SIGINT, typically Ctrl-C in a terminal.
    Interrupt,
    /// SIGTERM, sent by Kubernetes, systemd and `kill`.
    Terminate,
}

impl fmt::Display for ShutdownSignal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        })
    }
}

/// Listens for SIGTERM and SIGINT once installed.
///
/// Signals delivered between [`ShutdownListener::install`] and [`ShutdownListener::wait`] are
/// buffered, not lost.
#[derive(Debug)]
pub struct ShutdownListener {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl ShutdownListener {
    /// Installs the signal handlers. Must be called from within a tokio runtime.
    ///
    /// # Returns
    ///
    /// A listener whose [`wait`](Self::wait) resolves on the first shutdown signal.
    ///
    /// # Errors
    ///
    /// The OS error if a signal handler cannot be registered.
    pub fn install() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                terminate: signal(SignalKind::terminate())?,
                interrupt: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Waits until SIGTERM or SIGINT arrives.
    ///
    /// Takes `self` by value (consumes the listener), because a shutdown happens only once.
    ///
    /// # Returns
    ///
    /// The signal that triggered shutdown.
    ///
    /// # Errors
    ///
    /// Only on non-Unix targets, if waiting for Ctrl-C fails.
    pub async fn wait(self) -> io::Result<ShutdownSignal> {
        #[cfg(unix)]
        {
            // Destructure so each stream can be borrowed mutably at the same time.
            let Self {
                mut terminate,
                mut interrupt,
            } = self;
            // `select!` resumes on whichever future completes first, like
            // `asyncio.wait(..., return_when=FIRST_COMPLETED)`.
            let signal = tokio::select! {
                _ = terminate.recv() => ShutdownSignal::Terminate,
                _ = interrupt.recv() => ShutdownSignal::Interrupt,
            };
            Ok(signal)
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await?;
            Ok(ShutdownSignal::Interrupt)
        }
    }
}

// Signals are process-wide, so every case runs inside this one test, one after another;
// separate tests on parallel threads would see each other's signals.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use nix::sys::signal::{Signal, raise};
    use std::time::Duration;

    /// Raises `signal` in this process and returns what a freshly installed listener saw.
    async fn observe(signal: Signal) -> ShutdownSignal {
        let listener = ShutdownListener::install().expect("signal handlers install");
        raise(signal).expect("raise signal");
        tokio::time::timeout(Duration::from_secs(5), listener.wait())
            .await
            .expect("listener resolves within timeout")
            .expect("waiting for the signal succeeds")
    }

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    // Property: the listener resolves with the signal that was raised, and that signal
    // displays under its conventional name. `#[tokio::test]` must come before
    // `#[hegel::test]`: it provides the async runtime.
    #[tokio::test]
    #[hegel::test(test_cases = 20)]
    async fn resolves_with_the_signal_that_was_raised(tc: TestCase) {
        let (signal, expected) = tc.draw(
            gs::sampled_from(&[
                (Signal::SIGTERM, ShutdownSignal::Terminate),
                (Signal::SIGINT, ShutdownSignal::Interrupt),
            ])
            .print_as_debug(),
        );

        let observed = observe(signal).await;

        assert_eq!(observed, expected);
        assert_eq!(observed.to_string(), signal.as_str());
    }
}
