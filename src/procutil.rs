//! Bounded subprocess execution helpers (strict byte caps and wall-clock timeout).

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

/// Captured stdout, stderr, and exit metadata returned by [`run_command_output_bounded`].
#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) status: Option<ExitStatus>,
    pub(crate) stdout_truncated: bool,
}

/// Error returned by bounded subprocess execution helpers.
#[derive(Debug)]
pub(crate) enum BoundedCommandError {
    Io(io::Error),
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Stdout,
    Stderr,
}

/// Short grace window to collect the peer pipe after killing a child on truncation.
const POST_KILL_DRAIN_GRACE: Duration = Duration::from_millis(50);

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_bounded<R: Read>(reader: R, max_bytes: u64) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let limit = max_bytes.saturating_add(1);
    reader.take(limit).read_to_end(&mut buf)?;
    let truncated = (buf.len() as u64) > max_bytes;
    if truncated {
        buf.truncate(max_bytes as usize);
    }
    Ok((buf, truncated))
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
        let _ = tx.send(read_bounded(stdout, max_bytes));
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

/// Spawns `cmd` with null stdin and independent bounded stdout/stderr reader threads,
/// enforcing strict per-stream byte caps and a wall-clock `timeout`.
///
/// Independent reader threads prevent a chatty stderr from deadlocking stdout (or vice
/// versa). If either stream exceeds its byte cap, the child is killed and reaped
/// immediately and `Ok(BoundedOutput { status: None, .. })` is returned with the
/// captured prefixes.
pub(crate) fn run_command_output_bounded(
    cmd: &mut Command,
    timeout: Duration,
    max_stdout_bytes: u64,
    max_stderr_bytes: u64,
) -> Result<BoundedOutput, BoundedCommandError> {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(BoundedCommandError::Io)?;

    let (Some(stdout_pipe), Some(stderr_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        kill_and_reap(&mut child);
        return Err(BoundedCommandError::Io(io::Error::other(
            "child stdout/stderr pipes unavailable",
        )));
    };

    let (tx, rx) = mpsc::sync_channel::<(StreamKind, io::Result<(Vec<u8>, bool)>)>(2);
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        let _ = tx_out.send((StreamKind::Stdout, read_bounded(stdout_pipe, max_stdout_bytes)));
    });
    std::thread::spawn(move || {
        let _ = tx.send((StreamKind::Stderr, read_bounded(stderr_pipe, max_stderr_bytes)));
    });

    let mut stdout_res: Option<(Vec<u8>, bool)> = None;
    let mut stderr_res: Option<(Vec<u8>, bool)> = None;

    while stdout_res.is_none() || stderr_res.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok((kind, Ok((bytes, truncated)))) => {
                match kind {
                    StreamKind::Stdout => stdout_res = Some((bytes, truncated)),
                    StreamKind::Stderr => stderr_res = Some((bytes, truncated)),
                }
                if truncated {
                    kill_and_reap(&mut child);
                    // Opportunistically collect the peer stream now that the child is dead.
                    if stdout_res.is_none() || stderr_res.is_none() {
                        let grace = deadline
                            .saturating_duration_since(Instant::now())
                            .min(POST_KILL_DRAIN_GRACE);
                        if let Ok((peer_kind, Ok(peer_val))) = rx.recv_timeout(grace) {
                            match peer_kind {
                                StreamKind::Stdout => stdout_res = Some(peer_val),
                                StreamKind::Stderr => stderr_res = Some(peer_val),
                            }
                        }
                    }
                    let (stdout, stdout_truncated) = stdout_res.unwrap_or_default();
                    let (stderr, _) = stderr_res.unwrap_or_default();
                    return Ok(BoundedOutput {
                        stdout,
                        stderr,
                        status: None,
                        stdout_truncated,
                    });
                }
            }
            Ok((_, Err(err))) => {
                kill_and_reap(&mut child);
                return Err(BoundedCommandError::Io(err));
            }
            Err(_) => {
                kill_and_reap(&mut child);
                return Err(BoundedCommandError::TimedOut);
            }
        }
    }

    let (stdout, stdout_truncated) = stdout_res.unwrap_or_default();
    let (stderr, _) = stderr_res.unwrap_or_default();
    match wait_child_bounded(&mut child, deadline) {
        Some(status) => Ok(BoundedOutput {
            stdout,
            stderr,
            status: Some(status),
            stdout_truncated,
        }),
        None => Err(BoundedCommandError::TimedOut),
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

    #[test]
    fn output_bounded_captures_both_stdout_and_stderr() {
        let mut cmd = sh_command("printf 'out-text'; printf 'err-text' >&2");
        let out = run_command_output_bounded(&mut cmd, Duration::from_secs(5), 1024, 1024).unwrap();

        assert_eq!(out.stdout, b"out-text");
        assert_eq!(out.stderr, b"err-text");
        assert!(!out.stdout_truncated);
        assert!(out.status.unwrap().success());
    }

    #[test]
    fn output_bounded_captures_nonzero_exit_with_stderr() {
        let mut cmd = sh_command("printf 'boom stderr' >&2; exit 42");
        let out = run_command_output_bounded(&mut cmd, Duration::from_secs(5), 1024, 1024).unwrap();

        assert!(out.stdout.is_empty());
        assert_eq!(out.stderr, b"boom stderr");
        assert!(!out.stdout_truncated);
        assert_eq!(out.status.unwrap().code(), Some(42));
    }

    #[test]
    fn output_bounded_truncates_stdout_without_sigpipe_error() {
        let mut cmd = sh_command("while printf '0123456789abcdef'; do :; done");
        let out = run_command_output_bounded(&mut cmd, Duration::from_secs(5), 32, 1024).unwrap();

        assert_eq!(out.stdout, b"0123456789abcdef0123456789abcdef");
        assert!(out.stdout_truncated);
        assert!(out.status.is_none());
    }

    #[test]
    fn output_bounded_truncates_stderr_without_deadlock() {
        let mut cmd = sh_command("while printf '0123456789abcdef' >&2; do :; done");
        let out = run_command_output_bounded(&mut cmd, Duration::from_secs(5), 1024, 32).unwrap();

        assert_eq!(out.stderr, b"0123456789abcdef0123456789abcdef");
        assert!(!out.stdout_truncated);
        assert!(out.status.is_none());
    }

    #[test]
    fn output_bounded_times_out_stalled_command() {
        let mut cmd = sh_command("exec sleep 30");
        let err =
            run_command_output_bounded(&mut cmd, Duration::from_millis(50), 64, 64).unwrap_err();

        assert!(matches!(err, BoundedCommandError::TimedOut));
    }
}
