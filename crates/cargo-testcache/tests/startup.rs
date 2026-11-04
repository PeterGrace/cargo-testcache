//! Starts the real binary, sends it a shutdown signal, and checks it exits cleanly.
//!
//! Unit tests cover the pieces; this covers the wiring in `main.rs`: configuration, logging,
//! the signal handlers and the drain, in a process of its own.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// How long the binary may take to log that it started, or to exit after the signal.
const DEADLINE: Duration = Duration::from_secs(20);

/// Spawns the binary, waits for its `started` log line, sends `signal`, and returns whether
/// it exited successfully.
fn exits_cleanly_on(signal: Signal) -> bool {
    // `CARGO_BIN_EXE_<name>` is the path cargo built the binary at, set for integration tests.
    let mut child = Command::new(env!("CARGO_BIN_EXE_cargo-testcache"))
        // A clean environment and an empty directory: no `.env` or `RUST_LOG` leaks in.
        .env_clear()
        .current_dir(std::env::temp_dir())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the binary");
    let stderr = child.stderr.take().expect("stderr is piped");

    // Read stderr on a thread, so the test can stop waiting after `DEADLINE`.
    let (started_tx, started_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("started") {
                let _ = started_tx.send(());
            }
        }
    });
    started_rx
        .recv_timeout(DEADLINE)
        .expect("the binary logs that it started");

    let pid = Pid::from_raw(i32::try_from(child.id()).expect("pid fits in i32"));
    kill(pid, signal).expect("send the signal");
    let (exit_tx, exit_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = exit_tx.send(child.wait().expect("wait for the binary"));
    });
    exit_rx
        .recv_timeout(DEADLINE)
        .expect("the binary exits after the signal")
        .success()
}

use hegel::TestCase;
use hegel::generators::{self as gs, Generator};

// Property: either shutdown signal stops the binary with exit code 0. Few cases, since each
// one starts a process.
#[hegel::test(test_cases = 4)]
fn exits_cleanly_on_a_shutdown_signal(tc: TestCase) {
    let signal = tc.draw(gs::sampled_from(&[Signal::SIGTERM, Signal::SIGINT]).print_as_debug());

    assert!(exits_cleanly_on(signal), "exit status after {signal}");
}
