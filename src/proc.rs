//! Running a program that isn't git (the agent's CLI, `jail`) with every wait bounded: how long
//! it may take, and how much of its output is kept. The bounds cover the whole exchange, the
//! pipes included, so a child that hands its stdout to something long-lived can't hold the
//! caller past the deadline.

use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `cmd` with `input` on its stdin. Fails if it can't start, runs past `timeout`, or
/// writes more than `cap` bytes to stdout (more is refused, not cut: half an answer would
/// read as a whole one).
pub fn run(mut cmd: Command, input: Option<String>, cap: u64, timeout: Duration) -> Result<Output, String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run {name}: {e}"))?;
    // Each pipe has its own thread, none of them joined: one that never finishes (a pipe held
    // open by a grandchild) is left behind when the deadline passes.
    if let (Some(mut pipe), Some(input)) = (child.stdin.take(), input) {
        std::thread::spawn(move || {
            let _ = pipe.write_all(input.as_bytes());
        });
    }
    fn reader(pipe: impl Read + Send + 'static, cap: u64) -> Receiver<Vec<u8>> {
        let (send, got) = channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.take(cap + 1).read_to_end(&mut buf);
            let _ = send.send(buf);
        });
        got
    }
    let out = reader(child.stdout.take().expect("stdout is piped"), cap);
    let err = reader(child.stderr.take().expect("stderr is piped"), 1 << 20);
    let until = Instant::now() + timeout;
    let late = |child: &mut std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
        format!("{name}: no answer in {} s", timeout.as_secs())
    };
    // stdout first: a child that has written past the cap, or all it has, is done with.
    let stdout = match out.recv_timeout(until.saturating_duration_since(Instant::now())) {
        Ok(buf) if buf.len() as u64 > cap => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{name}: more than {} KB of output", cap / 1000));
        }
        Ok(buf) => buf,
        Err(_) => return Err(late(&mut child)),
    };
    let status = loop {
        match child.try_wait().map_err(|e| format!("{name}: {e}"))? {
            Some(status) => break status,
            None if Instant::now() > until => return Err(late(&mut child)),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // The child is gone; its stderr is given a moment, no longer.
    let stderr = err.recv_timeout(Duration::from_millis(500)).unwrap_or_default();
    Ok(Output { status, stdout: String::from_utf8_lossy(&stdout).into_owned(), stderr: String::from_utf8_lossy(&stderr).into_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn output_and_status_come_back() {
        let out = run(sh("cat; echo oops >&2; exit 3"), Some("hello".into()), 1000, Duration::from_secs(10)).unwrap();
        assert_eq!((out.stdout.as_str(), out.stderr.trim(), out.status.code()), ("hello", "oops", Some(3)));
    }

    #[test]
    fn a_program_that_takes_too_long_or_says_too_much_is_stopped() {
        let started = Instant::now();
        let slow = run(sh("sleep 30"), None, 1000, Duration::from_millis(200));
        assert!(slow.is_err_and(|e| e.contains("no answer")));
        let loud = run(sh("yes | head -c 100000"), None, 1000, Duration::from_secs(10));
        assert!(loud.is_err_and(|e| e.contains("more than")));
        // One that exits at once but leaves its stdout with something long-lived: the
        // deadline still holds.
        let held = run(sh("sleep 30 & exit 0"), None, 1000, Duration::from_millis(300));
        assert!(held.is_err_and(|e| e.contains("no answer")));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
