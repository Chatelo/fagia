//! Memory overview, kill, pause and resume.

mod common;

use common::Env;
use predicates::prelude::*;
use std::process::Command;
use std::time::Duration;

fn state(pid: u32) -> char {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    s.rsplit(')')
        .next()
        .and_then(|r| r.trim().chars().next())
        .unwrap_or('?')
}

#[test]
fn mem_json_has_system_and_groups() {
    let env = Env::new();
    let v = env.json(&["mem", "--expand"]);
    assert_eq!(v["command"], "mem");
    assert!(v["system"]["available"].as_u64().unwrap() > 0);
    assert!(v["system"]["total"].as_u64().unwrap() >= v["system"]["available"].as_u64().unwrap());
    let groups = v["groups"].as_array().unwrap();
    assert!(!groups.is_empty());
    let fair: Vec<u64> = groups.iter().map(|g| g["fair"].as_u64().unwrap()).collect();
    assert!(
        fair.windows(2).all(|w| w[0] >= w[1]),
        "groups sorted by fair share"
    );
    assert!(
        groups
            .iter()
            .any(|g| !g["members"].as_array().unwrap().is_empty())
    );
}

#[test]
fn mem_dev_filters_to_dev_suspects() {
    let env = Env::new();
    let v = env.json(&["mem", "--dev"]);
    assert!(
        v["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["dev_suspect"] == true)
    );
}

#[test]
fn kill_by_pid_quits_politely() {
    let env = Env::new();
    let mut child = Command::new("sleep").arg("300").spawn().unwrap();
    let pid = child.id().to_string();
    let v = env.json(&["kill", &pid, "-y"]);
    assert_eq!(v["action"], "quit");
    assert!(v["still_running"].as_array().unwrap().is_empty());
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(15), "SIGTERM, not SIGKILL");
    let log = std::fs::read_to_string(env.home().join(".local/state/fagia/actions.jsonl")).unwrap();
    assert!(log.contains("signal-term"));
}

#[test]
fn pause_and_resume_by_pid() {
    let env = Env::new();
    let mut child = Command::new("sleep").arg("300").spawn().unwrap();
    let pid = child.id();
    env.cmd()
        .args(["pause", &pid.to_string(), "-y"])
        .assert()
        .success();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(state(pid), 'T');
    env.cmd()
        .args(["resume", &pid.to_string(), "-y"])
        .assert()
        .success();
    std::thread::sleep(Duration::from_millis(100));
    assert_ne!(state(pid), 'T');
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn refusals() {
    let env = Env::new();
    env.cmd()
        .args(["kill", "1", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("may be signalled"));
    let mut child = Command::new("sleep").arg("300").spawn().unwrap();
    // No terminal and no -y: nothing happens.
    env.cmd()
        .args(["kill", &child.id().to_string()])
        .assert()
        .code(1);
    assert!(
        matches!(state(child.id()), 'S' | 'R'),
        "process must be untouched"
    );
    env.cmd()
        .args(["kill", &child.id().to_string(), "--force", "--json", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("second confirmation"));
    let _ = child.kill();
    let _ = child.wait();
    env.cmd()
        .args(["kill", "no-such-app-anywhere", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no app matches"));
}

#[test]
fn watch_records_and_reports() {
    let env = Env::new();
    let cfg = env.home().join(".config/fagia/config.toml");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(
        &cfg,
        "[ram]\nsample_seconds = 1\nleak_min_minutes = 0\nleak_warmup_minutes = 0\n",
    )
    .unwrap();
    let v = env.json(&["mem", "--watch", "2s", "--dev"]);
    assert!(v["watched_secs"].as_f64().unwrap() >= 2.0);
}
