//! RAM engine: honest system numbers, processes grouped into apps, and
//! detectors for forgotten and leaking apps.

pub mod detect;
pub mod group;

use crate::config::RamConfig;
use crate::platform::{Platform, SystemMemory, is_ram_fs};
use crate::rules::MemRule;
use crate::{Error, Result};
use detect::{Leak, LeakSettings};
use group::Group;
use rayon::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ShmUse {
    pub mount: PathBuf,
    pub used: u64,
    pub total: u64,
}

#[derive(Debug, Clone)]
pub struct MemSnapshot {
    pub system: SystemMemory,
    /// Pages per second moved to and from swap during the snapshot.
    pub swap_in_rate: f64,
    pub swap_out_rate: f64,
    pub groups: Vec<Group>,
    pub shm: Vec<ShmUse>,
    /// Processes whose fair share could not be read (resident shown).
    pub unmeasured: usize,
    pub processes: usize,
}

/// Swap rates need two readings; this is the shortest gap used.
const MIN_RATE_WINDOW: Duration = Duration::from_millis(250);

/// Reuses fair-share readings between refreshes. Reading `smaps_rollup`
/// walks page tables and costs far more than everything else; a process
/// whose resident size barely moved has barely moved in fair share too.
#[derive(Debug, Default)]
pub struct MemCache {
    mem: HashMap<crate::platform::ProcKey, (u64, crate::platform::ProcMem)>,
    /// Static process details (command line, executable, cgroup), which
    /// only change when the PID belongs to a new process.
    info: HashMap<crate::platform::ProcKey, crate::platform::ProcInfo>,
    last_full: Option<Instant>,
    last_swap: Option<(Instant, u64, u64)>,
}

/// Re-read everything at least this often.
const FULL_REFRESH: Duration = Duration::from_secs(60);

fn moved(then: u64, now: u64) -> bool {
    then.abs_diff(now) > (then / 20).max(1 << 20)
}

pub fn snapshot(platform: &dyn Platform, rules: &[MemRule]) -> Result<MemSnapshot> {
    snapshot_with(platform, rules, None)
}

pub fn snapshot_with(
    platform: &dyn Platform,
    rules: &[MemRule],
    mut cache: Option<&mut MemCache>,
) -> Result<MemSnapshot> {
    let started = Instant::now();
    let first = platform.system_memory()?;
    let full = cache
        .as_ref()
        .is_none_or(|c| c.last_full.is_none_or(|t| t.elapsed() >= FULL_REFRESH));
    let procs = match cache.as_deref_mut() {
        Some(c) if !full => {
            let boot_info = &mut c.info;
            let stats = platform.process_stats();
            let procs: Vec<crate::platform::ProcInfo> = stats
                .iter()
                .filter_map(|st| {
                    let mut info = match boot_info.get(&st.key) {
                        Some(i) => i.clone(),
                        None => platform.process(st.key.pid).filter(|i| i.key == st.key)?,
                    };
                    info.ppid = st.ppid;
                    info.state = st.state;
                    info.rss = st.rss;
                    Some(info)
                })
                .collect();
            *boot_info = procs.iter().map(|p| (p.key, p.clone())).collect();
            procs
        }
        Some(c) => {
            let procs = platform.processes();
            c.info = procs.iter().map(|p| (p.key, p.clone())).collect();
            procs
        }
        None => platform.processes(),
    };
    let cached = cache.as_ref().map(|c| &c.mem);
    let with_mem: Vec<_> = procs
        .into_par_iter()
        .map(|p| {
            let reuse = if full {
                None
            } else {
                cached
                    .and_then(|c| c.get(&p.key))
                    .filter(|(rss, _)| !moved(*rss, p.rss))
                    .map(|(_, m)| crate::platform::ProcMem { rss: p.rss, ..*m })
            };
            let m = reuse.unwrap_or_else(|| platform.process_memory(p.key.pid));
            (p, m)
        })
        .collect();
    if let Some(c) = cache.as_deref_mut() {
        c.mem = with_mem.iter().map(|(p, m)| (p.key, (p.rss, *m))).collect();
        if full {
            c.last_full = Some(Instant::now());
        }
    }
    let processes = with_mem.iter().filter(|(p, _)| !p.kernel_thread).count();
    let unmeasured = with_mem
        .iter()
        .filter(|(p, m)| !p.kernel_thread && m.pss.is_none() && m.rss > 0)
        .count();
    let groups = group::group(with_mem, rules);
    // Swap rates: against the previous refresh when there is one, else
    // over a short wait.
    let (since, base_in, base_out) = match cache.as_ref().and_then(|c| c.last_swap) {
        Some(prev) => prev,
        None => {
            let elapsed = started.elapsed();
            if elapsed < MIN_RATE_WINDOW {
                std::thread::sleep(MIN_RATE_WINDOW - elapsed);
            }
            (started, first.swap_in_pages, first.swap_out_pages)
        }
    };
    let second = platform.system_memory()?;
    let secs = since.elapsed().as_secs_f64().max(0.001);
    if let Some(c) = cache {
        c.last_swap = Some((Instant::now(), second.swap_in_pages, second.swap_out_pages));
    }
    let shm = platform
        .mounts()
        .into_iter()
        .filter(|m| is_ram_fs(&m.fs_type))
        .filter(|m| {
            let p = m.mount_point.to_string_lossy();
            p == "/dev/shm" || p == "/tmp" || p.starts_with("/run/user/")
        })
        .filter_map(|m| {
            let st = platform.fs_stats(&m.mount_point).ok()?;
            Some(ShmUse {
                used: st.total.saturating_sub(st.free),
                total: st.total,
                mount: m.mount_point,
            })
        })
        .collect();
    Ok(MemSnapshot {
        swap_in_rate: second.swap_in_pages.saturating_sub(base_in) as f64 / secs,
        swap_out_rate: second.swap_out_pages.saturating_sub(base_out) as f64 / secs,
        system: second,
        groups,
        shm,
        unmeasured,
        processes,
    })
}

/// Memory per group over time, in a ring buffer per group.
#[derive(Debug, Clone, Default)]
pub struct Sampler {
    cap: usize,
    start: Option<Instant>,
    series: HashMap<String, VecDeque<(f64, u64)>>,
}

impl Sampler {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(2),
            start: None,
            series: HashMap::new(),
        }
    }

    pub fn record(&mut self, groups: &[Group]) {
        let start = *self.start.get_or_insert_with(Instant::now);
        self.record_at(start.elapsed().as_secs_f64(), groups);
    }

    pub fn record_at(&mut self, t: f64, groups: &[Group]) {
        for g in groups {
            let s = self.series.entry(g.key()).or_default();
            if s.len() == self.cap {
                s.pop_front();
            }
            s.push_back((t, g.fair()));
        }
    }

    pub fn series(&self, key: &str) -> Vec<(f64, u64)> {
        self.series
            .get(key)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Leak suspects among the recorded groups.
    pub fn leaks(&self, settings: &LeakSettings) -> HashMap<String, Leak> {
        self.series
            .iter()
            .filter_map(|(k, s)| {
                let v: Vec<(f64, u64)> = s.iter().copied().collect();
                detect::leak(&v, settings).map(|l| (k.clone(), l))
            })
            .collect()
    }
}

/// Ring buffer size that covers the leak window at the sample interval.
pub fn sampler_capacity(cfg: &RamConfig, duration: Duration) -> usize {
    let every = cfg.sample_seconds.max(1);
    (duration.as_secs() / every + 2) as usize
}

/// Finds groups by name (case-insensitive, with or without the working
/// directory). An exact name match wins over partial ones. A PID selects
/// just that process, keeping its group's flags (system, unsaved work).
pub fn find_groups(
    groups: &[Group],
    query: &str,
    home: Option<&std::path::Path>,
) -> Result<Vec<Group>> {
    if let Ok(pid) = query.parse::<u32>() {
        return groups
            .iter()
            .find_map(|g| {
                let m = g.members.iter().find(|m| m.info.key.pid == pid)?;
                let mut one = g.clone();
                one.name = format!("{} (pid {pid})", m.info.exe_name());
                one.cwd = None;
                one.members = vec![m.clone()];
                Some(vec![one])
            })
            .ok_or_else(|| Error::Other(format!("no process with pid {pid}")));
    }
    let q = query.to_lowercase();
    let exact: Vec<&Group> = groups
        .iter()
        .filter(|g| g.display_name(home).to_lowercase() == q || g.name.to_lowercase() == q)
        .collect();
    if !exact.is_empty() {
        return Ok(exact.into_iter().cloned().collect());
    }
    let partial: Vec<&Group> = groups
        .iter()
        .filter(|g| g.display_name(home).to_lowercase().contains(&q))
        .collect();
    if partial.is_empty() {
        return Err(Error::Other(format!(
            "no app matches {query:?}; see `fagia mem`"
        )));
    }
    Ok(partial.into_iter().cloned().collect())
}

/// Builds the report shared by the CLI and TUI.
pub struct ReportOptions<'a> {
    pub platform: &'a dyn Platform,
    pub cfg: &'a RamConfig,
    pub sampler: Option<&'a Sampler>,
    pub leaks: &'a HashMap<String, Leak>,
    pub expand: bool,
    pub with_forgotten: bool,
}

pub fn report(snap: &MemSnapshot, o: &ReportOptions) -> crate::report::MemReport {
    use crate::paths::JsonPath;
    use crate::report::{GroupOut, LeakOut, MemReport, MemberOut, ShmOut, SystemMemOut};
    let home = o.platform.dirs().home.clone();
    let now = crate::model::now_epoch();
    let me = o.platform.current_uid();
    let sys = &snap.system;
    let groups = snap
        .groups
        .iter()
        .map(|g| {
            let forgotten = if o.with_forgotten {
                detect::forgotten(g, o.platform, o.cfg)
            } else {
                None
            };
            let leak = o.leaks.get(&g.key()).map(|l| LeakOut {
                mib_per_min: l.mib_per_min(),
                start: l.start,
                end: l.end,
                window_secs: l.window_secs,
                fit: l.r2,
            });
            let mut flags = Vec::new();
            if forgotten.is_some() {
                flags.push("forgotten".to_string());
            }
            if leak.is_some() {
                flags.push("leaking".to_string());
            }
            GroupOut {
                name: g.name.clone(),
                display: if g.uid == me {
                    g.display_name(Some(&home))
                } else {
                    format!(
                        "{} ({})",
                        g.display_name(Some(&home)),
                        o.platform.user_name(g.uid)
                    )
                },
                category: g.category.clone(),
                rule_id: g.rule_id.clone(),
                uid: g.uid,
                cwd: g.cwd.as_deref().map(JsonPath::new),
                processes: g.members.len(),
                fair: g.fair(),
                unique: g.uss(),
                resident: g.rss(),
                swap: g.swap(),
                fully_measured: g.fully_measured(),
                age_secs: (now - g.started_at()).max(0) as u64,
                dev_suspect: g.dev_suspect,
                system: g.system,
                unsaved_work: g.unsaved_work,
                reason: g.reason.clone(),
                flags,
                forgotten,
                leak,
                trend: o
                    .sampler
                    .map(|s| s.series(&g.key()).into_iter().map(|(_, b)| b).collect())
                    .unwrap_or_default(),
                members: if o.expand {
                    let mut m: Vec<MemberOut> = g
                        .members
                        .iter()
                        .map(|m| MemberOut {
                            pid: m.info.key.pid,
                            ppid: m.info.ppid,
                            name: m.info.exe_name(),
                            command: crate::paths::escape_control(&m.info.cmdline.join(" ")),
                            fair: m.fair(),
                            unique: m.mem.uss,
                            resident: m.mem.rss,
                            swap: m.mem.swap,
                            started_at: m.info.started_at,
                            measure: if m.mem.pss.is_some() {
                                "pss".into()
                            } else {
                                "rss".into()
                            },
                        })
                        .collect();
                    m.sort_by_key(|x| std::cmp::Reverse(x.fair));
                    m
                } else {
                    Vec::new()
                },
            }
        })
        .collect();
    MemReport {
        system: SystemMemOut {
            total: sys.total,
            available: sys.available,
            used_by_programs: sys.used_by_programs(),
            file_cache: sys.file_cache + sys.buffers,
            shared: sys.shmem,
            swap_total: sys.swap_total,
            swap_used: sys.swap_total.saturating_sub(sys.swap_free),
            swap_in_rate: snap.swap_in_rate,
            swap_out_rate: snap.swap_out_rate,
            zram: sys.zram,
        },
        processes: snap.processes,
        unmeasured: snap.unmeasured,
        groups,
        shm: snap
            .shm
            .iter()
            .map(|s| ShmOut {
                mount: JsonPath::new(&s.mount),
                used: s.used,
                total: s.total,
            })
            .collect(),
        containers: Vec::new(),
        watched_secs: None,
    }
}
