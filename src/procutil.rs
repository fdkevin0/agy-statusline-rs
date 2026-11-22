//! Bounded subprocess execution helpers (strict stdout byte cap and wall-clock timeout).

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Captured stdout and exit metadata returned by [`run_command_stdout_bounded`].
#[derive(Debug)]
pub(crate) struct BoundedStdout {
    pub(crate) stdout: Vec<u8>,
    pub(crate) status: Option<ExitStatus>,
    pub(crate) truncated: bool,
}

/// Error returned by [`run_command_stdout_bounded`].
#[derive(Debug)]
pub(crate) enum BoundedCommandError {
    Io(io::Error),
    TimedOut,
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn wait_child_bounded(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    kill_and_reap(child);
                    return None;
                }
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            Err(_) => {
                kill_and_reap(child);
                return None;
            }
        }
    }
}

/// Spawns `cmd` with null stdin/stderr and a bounded stdout pipe, enforcing a strict
/// byte cap and wall-clock `timeout`.
///
/// When stdout exceeds `max_bytes`, the child is killed and reaped immediately and
/// `Ok(BoundedStdout { status: None, truncated: true, .. })` is returned with the
/// truncated prefix so callers avoid `SIGPIPE` false-failures and slow draining.
pub(crate) fn run_command_stdout_bounded(
    cmd: &mut Command,
    timeout: Duration,
    max_bytes: u64,
) -> Result<BoundedStdout, BoundedCommandError> {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(BoundedCommandError::Io)?;

    let Some(stdout) = child.stdout.take() else {
        kill_and_reap(&mut child);
        return Err(BoundedCommandError::Io(io::Error::other(
            "child stdout pipe unavailable",
        )));
    };

    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let limit = max_bytes.saturating_add(1);
        let res = stdout.take(limit).read_to_end(&mut buf).map(|_| {
            let truncated = (buf.len() as u64) > max_bytes;
            if truncated {
                buf.truncate(max_bytes as usize);
            }
            (buf, truncated)
        });
        let _ = tx.send(res);
    });

    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(Ok((stdout, true))) => {
            // Kill and reap immediately to avoid SIGPIPE false-failures or slow
            // draining of unbounded output after hitting the byte cap.
            kill_and_reap(&mut child);
            Ok(BoundedStdout {
                stdout,
                status: None,
                truncated: true,
            })
        }
        Ok(Ok((stdout, false))) => match wait_child_bounded(&mut child, deadline) {
            Some(status) => Ok(BoundedStdout {
                stdout,
                status: Some(status),
                truncated: false,
            }),
            None => Err(BoundedCommandError::TimedOut),
        },
        Ok(Err(err)) => {
            kill_and_reap(&mut child);
            Err(BoundedCommandError::Io(err))
        }
        Err(_) => {
            kill_and_reap(&mut child);
            Err(BoundedCommandError::TimedOut)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh_command(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn captures_stdout_on_normal_exit() {
        let mut cmd = sh_command("printf 'hello bounded'");
        let out = run_command_stdout_bounded(&mut cmd, Duration::from_secs(5), 1024).unwrap();

        assert_eq!(out.stdout, b"hello bounded");
        assert!(!out.truncated);
        assert!(out.status.unwrap().success());
    }

    #[test]
    fn returns_nonzero_exit_status() {
        let mut cmd = sh_command("printf 'out-body'; exit 7");
        let out = run_command_stdout_bounded(&mut cmd, Duration::from_secs(5), 1024).unwrap();

        assert_eq!(out.stdout, b"out-body");
        assert!(!out.truncated);
        assert_eq!(out.status.unwrap().code(), Some(7));
    }

    #[test]
    fn truncates_stdout_without_sigpipe_error_and_allows_exact_cap() {
        let mut exact_cmd = sh_command("printf '12345'");
        let exact = run_command_stdout_bounded(&mut exact_cmd, Duration::from_secs(5), 5).unwrap();
        assert_eq!(exact.stdout, b"12345");
        assert!(!exact.truncated);
        assert!(exact.status.unwrap().success());

        let mut flood_cmd = sh_command("while printf '0123456789abcdef'; do :; done");
        let truncated =
            run_command_stdout_bounded(&mut flood_cmd, Duration::from_secs(5), 32).unwrap();
        assert_eq!(truncated.stdout, b"0123456789abcdef0123456789abcdef");
        assert!(truncated.truncated);
        assert!(truncated.status.is_none());
    }

    #[test]
    fn kills_and_times_out_stalled_command() {
        let mut cmd = sh_command("exec sleep 30");
        let err = run_command_stdout_bounded(&mut cmd, Duration::from_millis(50), 64).unwrap_err();

        assert!(matches!(err, BoundedCommandError::TimedOut));
    }
}
