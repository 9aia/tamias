//! Bounded subprocess execution — `Command::output` blocks forever on a
//! hung child (a stalled `git` remote, a wedged driver), so callers that
//! can be reached by untrusted refs go through [`output_with_timeout`].

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::Error;

/// How often the wait loop polls `try_wait` — small enough that the
/// timeout lands close to `timeout`, large enough to stay cheap.
const POLL: Duration = Duration::from_millis(20);

/// `cmd.output()` with a wall-clock bound. stdout/stderr are drained on
/// threads while the child runs, so a chatty child can't deadlock on a
/// full pipe; on timeout the child is killed and waited on before the
/// error returns. Only the direct child is killed — grandchildren that
/// outlive it are left to exit on their own (their inherited pipes keep
/// the detached drain threads alive until EOF).
///
/// # Errors
/// Propagates spawn/wait failures; a child still running at `timeout`
/// is killed and reported as [`Error::Failed`].
pub fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, Error> {
    let prog = cmd.get_program().to_string_lossy().to_string();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Error::io(format!("spawn {prog}")))?;
    // stdout and stderr are different types — drain each explicitly.
    let stdout_thr = drain_stdout(child.stdout.take());
    let stderr_thr = drain_stderr(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Don't join the drain threads: killed children's
                    // grandchildren can inherit the pipes and hold them
                    // open — joining would block until they exit. The
                    // detached threads finish on their own at EOF.
                    drop(stdout_thr);
                    drop(stderr_thr);
                    return Err(Error::Failed(format!("{prog} timed out after {timeout:?}")));
                }
                thread::sleep(POLL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stdout_thr);
                drop(stderr_thr);
                return Err(Error::io(format!("wait {prog}"))(e));
            }
        }
    };
    let stdout = stdout_thr.join().unwrap_or_default();
    let stderr = stderr_thr.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn drain_stdout(pipe: Option<std::process::ChildStdout>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

fn drain_stderr(pipe: Option<std::process::ChildStderr>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn fast_command_returns_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hi; echo err >&2"]);
        let out = output_with_timeout(&mut cmd, Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "err");
    }

    #[test]
    fn hung_command_is_killed() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let start = Instant::now();
        let err = output_with_timeout(&mut cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn chatty_child_does_not_deadlock() {
        // >64KiB of output — more than a pipe buffer — while the child
        // keeps running briefly, so the drain threads must be live.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "i=0; while [ $i -lt 200 ]; do head -c 1024 /dev/zero | tr '\\0' 'x'; i=$((i+1)); done",
        ]);
        let out = output_with_timeout(&mut cmd, Duration::from_secs(30)).unwrap();
        assert!(out.status.success());
        assert!(out.stdout.len() > 64 * 1024);
    }
}
