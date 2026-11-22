//! Bounded subprocess execution helpers (strict stdout byte cap and wall-clock timeout).

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Error returned by [`run_command_stdout_bounded`].
pub(crate) enum BoundedCommandError {
    Io(io::Error),
    TimedOut,
    NonZero(ExitStatus),
}

fn wait_child_bounded(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Spawns `cmd` with null stdin/stderr and piped stdout, reading at most `max_bytes`
/// within `timeout` and killing/reaping the child if the deadline expires.
pub(crate) fn run_command_stdout_bounded(
    cmd: &mut Command,
    timeout: Duration,
    max_bytes: u64,
) -> Result<Vec<u8>, BoundedCommandError> {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(BoundedCommandError::Io)?;

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(BoundedCommandError::Io(io::Error::other(
            "child stdout pipe unavailable",
        )));
    };

    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stdout.take(max_bytes).read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send(res);
    });

    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(Ok(bytes)) => match wait_child_bounded(&mut child, deadline) {
            Some(status) if status.success() => Ok(bytes),
            Some(status) => Err(BoundedCommandError::NonZero(status)),
            None => Err(BoundedCommandError::TimedOut),
        },
        Ok(Err(err)) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(BoundedCommandError::Io(err))
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(BoundedCommandError::TimedOut)
        }
    }
}
