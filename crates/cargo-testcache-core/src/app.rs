//! The application's main task.
//!
//! The binary runs [`run`] on the tokio runtime and, on SIGTERM or SIGINT, tells it to stop
//! through a `watch` channel, then waits for it at most
//! [`Config::shutdown_timeout`](crate::Config::shutdown_timeout).

use tokio::sync::watch;

use crate::config::Config;

/// Runs the application until `shutdown` turns `true` or its sender is dropped.
///
/// This skeleton only logs and waits. Put the application's work here, and make every loop or
/// long wait also watch `shutdown` (with `tokio::select!`) so the task stops promptly.
///
/// # Arguments
///
/// * `config` - The application's settings.
/// * `shutdown` - Turns `true` when the process is asked to stop. A `watch` channel is like a
///   shared variable that receivers can await changes to.
pub async fn run(config: &Config, mut shutdown: watch::Receiver<bool>) {
    tracing::info!(log_format = %config.log_format(), "started");
    // `wait_for` resolves once the value satisfies the predicate. It errs only when the
    // sender is gone, which can never send `true` any more, so that also means stop.
    let _ = shutdown.wait_for(|stop| *stop).await;
    tracing::info!("stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use hegel::TestCase;
    use hegel::generators as gs;

    // Property: `run` returns once told to stop, or once the sender is dropped, either way.
    #[tokio::test]
    #[hegel::test(test_cases = 20)]
    async fn run_stops_on_signal_or_dropped_sender(tc: TestCase) {
        let drop_sender = tc.draw(gs::booleans());
        let (stop_tx, stop_rx) = watch::channel(false);
        let config = Config::default();
        let task = run(&config, stop_rx);

        if drop_sender {
            drop(stop_tx);
        } else {
            stop_tx
                .send(true)
                .expect("receiver is alive until run returns");
        }

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("run returns promptly");
    }
}
