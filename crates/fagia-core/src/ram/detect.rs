//! Forgotten processes and leak suspects.

use super::group::Group;
use crate::config::RamConfig;
use crate::model::now_epoch;
use crate::platform::Platform;
use std::path::Path;

/// Folders skipped when judging whether a project is still being worked
/// on: they change on every build, not when a person edits.
const GENERATED: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
    ".cache",
];

/// Newest modification time of files under `dir`, looking at no more than
/// `budget` entries (a dev process's working folder can be huge).
pub fn newest_source_mtime(dir: &Path, budget: usize) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    let mut stack = vec![dir.to_path_buf()];
    let mut newest = None;
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            seen += 1;
            if seen > budget {
                return newest;
            }
            let Ok(m) = e.metadata() else {
                continue;
            };
            if m.is_dir() {
                if !GENERATED.iter().any(|g| e.file_name() == *g) {
                    stack.push(e.path());
                }
            } else if m.is_file() {
                newest = Some(newest.map_or(m.mtime(), |n: i64| n.max(m.mtime())));
            }
        }
    }
    newest
}

/// Why a group counts as forgotten, if it does: a dev tool running longer
/// than the threshold with no terminal, or whose project went quiet.
pub fn forgotten(g: &Group, platform: &dyn Platform, cfg: &RamConfig) -> Option<String> {
    if !g.dev_suspect {
        return None;
    }
    let now = now_epoch();
    let leader = g.leader();
    let age = now - leader.info.started_at;
    if age < (cfg.forgotten_hours * 3600) as i64 {
        return None;
    }
    let hours = age / 3600;
    match leader.info.tty {
        None => return Some(format!("running {hours}h with no terminal")),
        Some(t) if !platform.tty_exists(t) => {
            return Some(format!("running {hours}h; its terminal is gone"));
        }
        _ => {}
    }
    let cwd = g.cwd.as_ref().or(leader.info.cwd.as_ref())?;
    let home = &platform.dirs().home;
    if cwd == home || cwd.as_os_str() == "/" || !cwd.starts_with(home) {
        return None;
    }
    let newest = newest_source_mtime(cwd, 20_000)?;
    let idle_days = (now - newest) / 86_400;
    (idle_days >= cfg.forgotten_idle_days as i64).then(|| {
        format!(
            "running {hours}h; project {} untouched for {idle_days} days",
            crate::paths::display_path(cwd, Some(home))
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeakSettings {
    pub warmup_secs: f64,
    pub min_window_secs: f64,
    /// Bytes per second.
    pub min_rate: f64,
    /// Minimum fit of the linear trend (R²).
    pub min_r2: f64,
    /// Largest allowed drop below the running peak, as a fraction of it.
    pub max_drop: f64,
}

impl LeakSettings {
    pub fn from_config(cfg: &RamConfig) -> Self {
        Self {
            warmup_secs: cfg.leak_warmup_minutes as f64 * 60.0,
            min_window_secs: cfg.leak_min_minutes as f64 * 60.0,
            min_rate: cfg.leak_rate_mib_per_min * 1024.0 * 1024.0 / 60.0,
            min_r2: 0.8,
            max_drop: 0.1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leak {
    /// Bytes per second.
    pub rate: f64,
    pub r2: f64,
    pub start: u64,
    pub end: u64,
    pub window_secs: f64,
}

impl Leak {
    pub fn mib_per_min(&self) -> f64 {
        self.rate * 60.0 / (1024.0 * 1024.0)
    }
}

/// Least-squares fit over `(seconds, bytes)` samples taken after the
/// warm-up. A leak rises steadily: positive slope above the rate, a good
/// linear fit and no significant drop (garbage collectors and caches
/// release memory; leaks do not).
pub fn leak(samples: &[(f64, u64)], s: &LeakSettings) -> Option<Leak> {
    let t0 = samples.first()?.0;
    let pts: Vec<(f64, f64)> = samples
        .iter()
        .filter(|(t, _)| t - t0 >= s.warmup_secs)
        .map(|(t, b)| (*t, *b as f64))
        .collect();
    if pts.len() < 3 {
        return None;
    }
    let window = pts.last()?.0 - pts[0].0;
    if window < s.min_window_secs {
        return None;
    }
    let n = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let sxy: f64 = pts.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let sxx: f64 = pts.iter().map(|(x, _)| (x - mx).powi(2)).sum();
    let syy: f64 = pts.iter().map(|(_, y)| (y - my).powi(2)).sum();
    if sxx == 0.0 || syy == 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    let r2 = sxy * sxy / (sxx * syy);
    let mut peak = 0f64;
    let mut worst_drop = 0f64;
    for (_, y) in &pts {
        peak = peak.max(*y);
        worst_drop = worst_drop.max((peak - y) / peak.max(1.0));
    }
    (slope >= s.min_rate && r2 >= s.min_r2 && worst_drop <= s.max_drop).then(|| Leak {
        rate: slope,
        r2,
        start: pts[0].1 as u64,
        end: pts.last().map_or(0, |p| p.1 as u64),
        window_secs: window,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::linux::LinuxPlatform;
    use crate::platform::{Dirs, ProcInfo, ProcKey, ProcMem};
    use crate::ram::group::{Group, Member};
    use std::fs::{self, File, FileTimes};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn dev_group(started_days_ago: i64, tty: Option<u64>, cwd: PathBuf) -> Group {
        Group {
            name: "Node".into(),
            category: None,
            rule_id: Some("node-dev".into()),
            uid: 1000,
            cwd: Some(cwd.clone()),
            members: vec![Member {
                info: ProcInfo {
                    key: ProcKey {
                        pid: 42,
                        start_ticks: 1,
                    },
                    ppid: 1,
                    uid: 1000,
                    comm: "node".into(),
                    exe: None,
                    cmdline: vec!["node".into()],
                    started_at: now_epoch() - started_days_ago * 86_400,
                    tty,
                    kernel_thread: false,
                    state: 'S',
                    cgroup_leaf: None,
                    cwd: Some(cwd),
                    rss: 0,
                },
                mem: ProcMem::default(),
            }],
            dev_suspect: true,
            system: false,
            unsaved_work: false,
            reason: None,
        }
    }

    #[test]
    fn forgotten_signals() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = home.join("web");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("server.js"), "x").unwrap();
        fs::create_dir_all(tmp.path().join("dev/pts")).unwrap();
        fs::write(tmp.path().join("dev/pts/3"), "").unwrap();
        let dirs = Dirs {
            cache: home.join(".cache"),
            data: home.join(".local/share"),
            config: home.join(".config"),
            state: home.join(".local/state"),
            cargo_home: home.join(".cargo"),
            home: home.clone(),
        };
        let plat = LinuxPlatform::with_roots("/proc", "/sys", tmp.path().join("dev"), dirs);
        let cfg = RamConfig::default();
        let pts3 = rustix::fs::makedev(136, 3);
        let pts9 = rustix::fs::makedev(136, 9);

        assert!(
            forgotten(&dev_group(2, None, proj.clone()), &plat, &cfg)
                .unwrap()
                .contains("no terminal")
        );
        assert!(
            forgotten(&dev_group(2, Some(pts9), proj.clone()), &plat, &cfg)
                .unwrap()
                .contains("terminal is gone")
        );
        // Terminal alive and the project was just edited: not forgotten.
        assert_eq!(
            forgotten(&dev_group(2, Some(pts3), proj.clone()), &plat, &cfg),
            None
        );
        // Too young.
        assert_eq!(
            forgotten(&dev_group(0, None, proj.clone()), &plat, &cfg),
            None
        );
        // Terminal alive but the project has been idle for 10 days.
        let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
        File::options()
            .write(true)
            .open(proj.join("server.js"))
            .unwrap()
            .set_times(FileTimes::new().set_modified(old))
            .unwrap();
        assert!(
            forgotten(&dev_group(2, Some(pts3), proj.clone()), &plat, &cfg)
                .unwrap()
                .contains("untouched for 10 days")
        );
        // Not a dev tool: never forgotten.
        let mut g = dev_group(2, None, proj);
        g.dev_suspect = false;
        assert_eq!(forgotten(&g, &plat, &cfg), None);
    }

    fn settings() -> LeakSettings {
        LeakSettings {
            warmup_secs: 60.0,
            min_window_secs: 600.0,
            min_rate: 1024.0 * 1024.0 / 60.0,
            min_r2: 0.8,
            max_drop: 0.1,
        }
    }

    const MIB: f64 = 1024.0 * 1024.0;

    #[test]
    fn steady_growth_is_a_leak() {
        // 5 MiB/min with a little noise, sampled every 5 s for 15 min.
        let s: Vec<(f64, u64)> = (0..180)
            .map(|i| {
                let t = i as f64 * 5.0;
                let noise = if i % 2 == 0 { 0.2 } else { -0.2 } * MIB;
                (t, (200.0 * MIB + t / 60.0 * 5.0 * MIB + noise) as u64)
            })
            .collect();
        let l = leak(&s, &settings()).unwrap();
        assert!((l.mib_per_min() - 5.0).abs() < 0.2, "{}", l.mib_per_min());
    }

    #[test]
    fn gc_sawtooth_and_warmup_are_not_leaks() {
        // Grows, collects back down, repeats.
        let saw: Vec<(f64, u64)> = (0..180)
            .map(|i| {
                (
                    i as f64 * 5.0,
                    ((200.0 + (i % 30) as f64 * 3.0) * MIB) as u64,
                )
            })
            .collect();
        assert_eq!(leak(&saw, &settings()), None);
        // Fast growth only during warm-up, flat afterwards.
        let warm: Vec<(f64, u64)> = (0..180)
            .map(|i| {
                let t = i as f64 * 5.0;
                (t, ((200.0 + t.min(60.0) * 10.0) * MIB) as u64)
            })
            .collect();
        assert_eq!(leak(&warm, &settings()), None);
        // Too short a window.
        let short: Vec<(f64, u64)> = (0..30)
            .map(|i| (i as f64 * 5.0, (i as f64 * 10.0 * MIB) as u64))
            .collect();
        assert_eq!(leak(&short, &settings()), None);
    }
}
