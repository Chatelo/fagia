//! Leak detection against real processes: this test binary re-runs itself
//! as a child that either leaks steadily or holds a fixed amount.

use fagia_core::platform::Platform;
use fagia_core::platform::linux::LinuxPlatform;
use fagia_core::ram::detect::{LeakSettings, leak};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;

/// Not a real test: the body run by the child processes.
#[test]
#[ignore]
fn leak_child_helper() {
    let Ok(mode) = std::env::var("FAGIA_LEAK_MODE") else {
        return;
    };
    let mut hold: Vec<Vec<u8>> = Vec::new();
    if mode == "steady" {
        hold.push(vec![1u8; 40 * MIB]);
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if mode == "leak" {
            // Touched pages count as resident; 2 MiB every 50 ms.
            hold.push(vec![1u8; 2 * MIB]);
        } else {
            for v in hold.iter_mut() {
                v[0] = v[0].wrapping_add(1);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::hint::black_box(&hold);
}

fn sample(mode: &str) -> Vec<(f64, u64)> {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "leak_child_helper", "--nocapture"])
        .env("FAGIA_LEAK_MODE", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let plat = LinuxPlatform::new();
    let start = Instant::now();
    let mut samples = Vec::new();
    while start.elapsed() < Duration::from_secs(4) {
        let m = plat.process_memory(child.id());
        samples.push((start.elapsed().as_secs_f64(), m.pss.unwrap_or(m.rss)));
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    samples
}

#[test]
fn leaking_process_is_flagged_and_steady_is_not() {
    // The production defaults scaled down from minutes to seconds.
    let s = LeakSettings {
        warmup_secs: 0.5,
        min_window_secs: 2.0,
        min_rate: 5.0 * MIB as f64,
        min_r2: 0.8,
        max_drop: 0.1,
    };
    let leaking = sample("leak");
    let l = leak(&leaking, &s).unwrap_or_else(|| panic!("leak not detected: {leaking:?}"));
    assert!(l.end > l.start);
    let steady = sample("steady");
    assert_eq!(
        leak(&steady, &s),
        None,
        "steady process flagged: {steady:?}"
    );
}
