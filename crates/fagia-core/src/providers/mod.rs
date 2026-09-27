//! Providers: code for what a rule cannot express. Each returns findings
//! in the shared model so every front end shows them.

pub mod docker;

use crate::model::Finding;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Findings, or an explanation of why the provider could not run.
    fn findings(&self) -> Result<Vec<Finding>, String>;
}

pub fn all() -> Vec<Box<dyn Provider>> {
    vec![Box::new(docker::Docker)]
}

/// Runs a command with a deadline; a hung daemon must not hang fagia.
pub(crate) fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<String, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Drain both pipes while the command runs: a command writing more than
    // a pipe buffer (64 KiB) would otherwise block until the timeout.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    if !status.success() {
        return Err(String::from_utf8_lossy(&err)
            .lines()
            .next()
            .unwrap_or("failed")
            .to_string());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

pub(crate) fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_output_does_not_block_until_the_timeout() {
        let started = Instant::now();
        // About 1 MiB on stdout: far beyond a pipe buffer.
        let out = run_with_timeout(
            Command::new("sh").args(["-c", "head -c 1048576 /dev/zero | tr '\\0' x"]),
            Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(out.len(), 1 << 20);
        assert!(started.elapsed() < Duration::from_secs(5));
        let err = run_with_timeout(
            Command::new("sh").args(["-c", "echo boom >&2; exit 3"]),
            Duration::from_secs(5),
        );
        assert_eq!(err.unwrap_err(), "boom");
        let slow = run_with_timeout(Command::new("sleep").arg("5"), Duration::from_millis(200));
        assert!(slow.unwrap_err().contains("timed out"));
    }
}
