//! Linux implementation. Reads `/proc` and `/sys` through configurable
//! roots so tests can run the parsers against fake trees.

use super::{
    DeletedOpen, Dirs, FileStat, FsStats, Mount, OpenPaths, Platform, ProcInfo, ProcKey, ProcMem,
    Signal, SystemMemory, Zram,
};
use crate::{Error, Result};
use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const PF_KTHREAD: u64 = 0x0020_0000;

pub struct LinuxPlatform {
    proc_root: PathBuf,
    sys_root: PathBuf,
    dev_root: PathBuf,
    dirs: Dirs,
    ticks_per_sec: u64,
    page_size: u64,
    fake_euid: Option<u32>,
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxPlatform {
    pub fn new() -> Self {
        Self::with_roots("/proc", "/sys", "/dev", dirs_from_env())
    }

    /// For tests: point the parsers at a fake `/proc`, `/sys` and `/dev`.
    pub fn with_roots(
        proc_root: impl Into<PathBuf>,
        sys_root: impl Into<PathBuf>,
        dev_root: impl Into<PathBuf>,
        dirs: Dirs,
    ) -> Self {
        Self {
            proc_root: proc_root.into(),
            sys_root: sys_root.into(),
            dev_root: dev_root.into(),
            dirs,
            ticks_per_sec: rustix::param::clock_ticks_per_second().max(1),
            page_size: rustix::param::page_size() as u64,
            fake_euid: None,
        }
    }

    /// For tests of the run-as-root refusal.
    pub fn with_effective_uid(mut self, euid: u32) -> Self {
        self.fake_euid = Some(euid);
        self
    }

    fn pid_dir(&self, pid: u32) -> PathBuf {
        self.proc_root.join(pid.to_string())
    }

    fn boot_time(&self) -> i64 {
        fs::read_to_string(self.proc_root.join("stat"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
            })
            .unwrap_or(0)
    }

    fn read_process(&self, pid: u32, boot: i64) -> Option<ProcInfo> {
        let dir = self.pid_dir(pid);
        let stat = parse_stat(&fs::read_to_string(dir.join("stat")).ok()?)?;
        let uid = fs::read_to_string(dir.join("status"))
            .ok()
            .and_then(|s| parse_status_uid(&s))
            .unwrap_or(u32::MAX);
        let cmdline = fs::read(dir.join("cmdline"))
            .map(|b| parse_cmdline(&b))
            .unwrap_or_default();
        let cgroup_leaf = fs::read_to_string(dir.join("cgroup"))
            .ok()
            .and_then(|s| parse_cgroup_leaf(&s));
        Some(ProcInfo {
            key: ProcKey {
                pid,
                start_ticks: stat.start_ticks,
            },
            ppid: stat.ppid,
            uid,
            comm: stat.comm,
            exe: fs::read_link(dir.join("exe")).ok(),
            cmdline,
            started_at: boot + (stat.start_ticks / self.ticks_per_sec) as i64,
            tty: (stat.tty_nr != 0).then_some(stat.tty_nr),
            kernel_thread: stat.flags & PF_KTHREAD != 0 || pid == 2 || stat.ppid == 2,
            state: stat.state,
            cgroup_leaf,
            cwd: fs::read_link(dir.join("cwd")).ok(),
            rss: stat.rss_pages * self.page_size,
        })
    }

    fn pids(&self) -> Vec<u32> {
        let Ok(rd) = fs::read_dir(&self.proc_root) else {
            return Vec::new();
        };
        let mut pids: Vec<u32> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        pids.sort_unstable();
        pids
    }

    fn start_ticks(&self, pid: u32) -> Option<(u64, char)> {
        let s = parse_stat(&fs::read_to_string(self.pid_dir(pid).join("stat")).ok()?)?;
        Some((s.start_ticks, s.state))
    }

    fn zram(&self) -> Option<Zram> {
        let rd = fs::read_dir(self.sys_root.join("block")).ok()?;
        let mut total: Option<Zram> = None;
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with("zram") {
                continue;
            }
            if let Some(z) = fs::read_to_string(e.path().join("mm_stat"))
                .ok()
                .and_then(|s| parse_mm_stat(&s))
            {
                let t = total.get_or_insert_with(Zram::default);
                t.original += z.original;
                t.compressed += z.compressed;
                t.used += z.used;
            }
        }
        total
    }
}

pub fn dirs_from_env() -> Dirs {
    let env_path = |k: &str| {
        std::env::var_os(k)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let home = env_path("HOME").unwrap_or_else(|| PathBuf::from("/"));
    Dirs {
        cache: env_path("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache")),
        data: env_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
        config: env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
        state: env_path("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state")),
        cargo_home: env_path("CARGO_HOME").unwrap_or_else(|| home.join(".cargo")),
        home,
    }
}

impl Platform for LinuxPlatform {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn dirs(&self) -> &Dirs {
        &self.dirs
    }

    fn file_stat(&self, meta: &Metadata) -> FileStat {
        FileStat {
            real: meta.blocks() * 512,
            apparent: meta.len(),
            dev: meta.dev(),
            ino: meta.ino(),
            nlink: meta.nlink(),
            mtime: meta.mtime(),
            placeholder: false,
        }
    }

    fn is_virtual_path(&self, path: &Path) -> bool {
        ["/proc", "/sys", "/dev", "/run"]
            .iter()
            .any(|v| path.starts_with(v))
    }

    fn mounts(&self) -> Vec<Mount> {
        fs::read_to_string(self.proc_root.join("self/mountinfo"))
            .map(|s| parse_mountinfo(&s))
            .unwrap_or_default()
    }

    fn fs_stats(&self, path: &Path) -> Result<FsStats> {
        let st = rustix::fs::statvfs(path)
            .map_err(|e| Error::Platform(format!("statvfs {}: {e}", path.display())))?;
        Ok(FsStats {
            total: st.f_blocks * st.f_frsize,
            free: st.f_bfree * st.f_frsize,
            available: st.f_bavail * st.f_frsize,
        })
    }

    fn effective_uid(&self) -> u32 {
        self.fake_euid
            .unwrap_or_else(|| rustix::process::geteuid().as_raw())
    }

    fn current_uid(&self) -> u32 {
        rustix::process::getuid().as_raw()
    }

    fn current_pid(&self) -> u32 {
        rustix::process::getpid().as_raw_nonzero().get() as u32
    }

    fn hostname(&self) -> String {
        rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned()
    }

    fn system_protected_paths(&self) -> Vec<PathBuf> {
        [
            "/", "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/opt", "/proc",
            "/root", "/run", "/sbin", "/snap", "/srv", "/sys", "/usr", "/var",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }

    fn processes(&self) -> Vec<ProcInfo> {
        let boot = self.boot_time();
        self.pids()
            .into_iter()
            .filter_map(|pid| self.read_process(pid, boot))
            .collect()
    }

    fn process_stats(&self) -> Vec<super::ProcStat> {
        self.pids()
            .into_iter()
            .filter_map(|pid| {
                let s = parse_stat(&fs::read_to_string(self.pid_dir(pid).join("stat")).ok()?)?;
                Some(super::ProcStat {
                    key: ProcKey {
                        pid,
                        start_ticks: s.start_ticks,
                    },
                    ppid: s.ppid,
                    state: s.state,
                    rss: s.rss_pages * self.page_size,
                })
            })
            .collect()
    }

    fn process(&self, pid: u32) -> Option<ProcInfo> {
        self.read_process(pid, self.boot_time())
    }

    fn process_memory(&self, pid: u32) -> ProcMem {
        let dir = self.pid_dir(pid);
        let rss = fs::read_to_string(dir.join("stat"))
            .ok()
            .and_then(|s| parse_stat(&s))
            .map(|s| s.rss_pages * self.page_size)
            .unwrap_or(0);
        match fs::read_to_string(dir.join("smaps_rollup"))
            .ok()
            .and_then(|s| parse_smaps_rollup(&s))
        {
            Some(m) => m,
            // No permission (another user's process) or a kernel thread.
            None => ProcMem {
                rss,
                ..ProcMem::default()
            },
        }
    }

    fn system_memory(&self) -> Result<SystemMemory> {
        let path = self.proc_root.join("meminfo");
        let text = fs::read_to_string(&path)
            .map_err(|e| Error::Platform(format!("{}: {e}", path.display())))?;
        let mut mem = parse_meminfo(&text);
        if let Ok(v) = fs::read_to_string(self.proc_root.join("vmstat")) {
            let (i, o) = parse_vmstat_swap(&v);
            mem.swap_in_pages = i;
            mem.swap_out_pages = o;
        }
        mem.zram = self.zram();
        mem.page_size = self.page_size;
        Ok(mem)
    }

    fn open_paths(&self) -> OpenPaths {
        let mut out = OpenPaths::default();
        for pid in self.pids() {
            let dir = self.pid_dir(pid);
            if let Ok(cwd) = fs::read_link(dir.join("cwd")) {
                out.cwds.push((pid, cwd));
            }
            let Ok(fds) = fs::read_dir(dir.join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(target) = fs::read_link(fd.path()) else {
                    continue;
                };
                let s = target.as_os_str().to_string_lossy();
                if !s.starts_with('/') || s.starts_with("/memfd:") || s.starts_with("/dev/") {
                    continue;
                }
                if let Some(orig) = s.strip_suffix(" (deleted)") {
                    // Following the fd link reaches the still-open inode.
                    if let Some(meta) = fs::metadata(fd.path()).ok().filter(|m| m.is_file()) {
                        out.deleted.push(DeletedOpen {
                            pid,
                            path: PathBuf::from(orig),
                            real: meta.blocks() * 512,
                            dev: meta.dev(),
                        });
                    }
                } else {
                    out.files.push((pid, target));
                }
            }
        }
        out
    }

    fn tty_exists(&self, tty: u64) -> bool {
        let major = rustix::fs::major(tty);
        let minor = rustix::fs::minor(tty);
        match major {
            136..=143 => self
                .dev_root
                .join(format!("pts/{}", (major - 136) * 256 + minor))
                .exists(),
            4 if minor < 64 => self.dev_root.join(format!("tty{minor}")).exists(),
            _ => true,
        }
    }

    fn user_name(&self, uid: u32) -> String {
        std::fs::read_to_string("/etc/passwd")
            .ok()
            .and_then(|t| {
                t.lines().find_map(|l| {
                    let mut f = l.split(':');
                    let name = f.next()?;
                    (f.nth(1)?.parse::<u32>().ok()? == uid).then(|| name.to_string())
                })
            })
            .unwrap_or_else(|| format!("uid {uid}"))
    }

    fn cgroup_memory(&self, rel: &str) -> Option<u64> {
        if rel.split('/').any(|c| c == "..") {
            return None;
        }
        fs::read_to_string(
            self.sys_root
                .join("fs/cgroup")
                .join(rel)
                .join("memory.current"),
        )
        .ok()?
        .trim()
        .parse()
        .ok()
    }

    fn is_alive(&self, key: ProcKey) -> bool {
        matches!(self.start_ticks(key.pid), Some((t, s)) if t == key.start_ticks && s != 'Z' && s != 'X')
    }

    #[allow(clippy::disallowed_methods)] // the one signal primitive, called only via actions
    fn signal(&self, key: ProcKey, sig: Signal) -> Result<()> {
        use rustix::process::{Pid, PidfdFlags, Signal as S, pidfd_open, pidfd_send_signal};
        let pid = Pid::from_raw(key.pid as i32)
            .ok_or_else(|| Error::Refused(format!("invalid pid {}", key.pid)))?;
        // The pidfd pins this exact process, so the start-time check below
        // cannot race with PID reuse.
        let fd = pidfd_open(pid, PidfdFlags::empty())
            .map_err(|e| Error::Platform(format!("pid {}: {e}", key.pid)))?;
        match self.start_ticks(key.pid) {
            Some((t, _)) if t == key.start_ticks => {}
            _ => {
                return Err(Error::Refused(format!(
                    "pid {} now belongs to a different process",
                    key.pid
                )));
            }
        }
        let s = match sig {
            Signal::Terminate => S::TERM,
            Signal::Kill => S::KILL,
            Signal::Stop => S::STOP,
            Signal::Continue => S::CONT,
        };
        pidfd_send_signal(&fd, s).map_err(|e| Error::Platform(format!("pid {}: {e}", key.pid)))
    }
}

// ---- parsers (pure, tested on fixture text) ----

#[derive(Debug, PartialEq)]
pub(crate) struct Stat {
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub tty_nr: u64,
    pub flags: u64,
    pub start_ticks: u64,
    pub rss_pages: u64,
}

pub(crate) fn parse_stat(text: &str) -> Option<Stat> {
    // comm may contain spaces and parentheses; it ends at the last ')'.
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let f: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    Some(Stat {
        comm,
        state: f.first()?.chars().next()?,
        ppid: f.get(1)?.parse().ok()?,
        tty_nr: f.get(4)?.parse::<i64>().ok()? as u64,
        flags: f.get(6)?.parse().ok()?,
        start_ticks: f.get(19)?.parse().ok()?,
        rss_pages: f.get(21)?.parse::<i64>().ok()?.max(0) as u64,
    })
}

pub(crate) fn parse_status_uid(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
}

pub(crate) fn parse_cmdline(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

pub(crate) fn parse_cgroup_leaf(text: &str) -> Option<String> {
    let line = text
        .lines()
        .find(|l| l.starts_with("0::"))
        .or_else(|| text.lines().next())?;
    let path = line.rsplit(':').next()?;
    let leaf = path.rsplit('/').next()?;
    (!leaf.is_empty()).then(|| leaf.to_string())
}

fn kb_field(line: &str) -> Option<u64> {
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}

pub(crate) fn parse_smaps_rollup(text: &str) -> Option<ProcMem> {
    let mut fields: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        if let Some((key, _)) = line.split_once(':')
            && let Some(v) = kb_field(line)
        {
            fields.insert(key, v);
        }
    }
    let pss = *fields.get("Pss")?;
    let uss = fields.get("Private_Clean").copied().unwrap_or(0)
        + fields.get("Private_Dirty").copied().unwrap_or(0);
    Some(ProcMem {
        rss: fields.get("Rss").copied().unwrap_or(0),
        pss: Some(pss),
        uss: Some(uss),
        swap: Some(fields.get("Swap").copied().unwrap_or(0)),
    })
}

pub(crate) fn parse_meminfo(text: &str) -> SystemMemory {
    let mut m = SystemMemory::default();
    for line in text.lines() {
        let Some((key, _)) = line.split_once(':') else {
            continue;
        };
        let v = kb_field(line).unwrap_or(0);
        match key {
            "MemTotal" => m.total = v,
            "MemFree" => m.free = v,
            "MemAvailable" => m.available = v,
            "Buffers" => m.buffers = v,
            "Cached" => m.file_cache = v,
            "Shmem" => m.shmem = v,
            "SReclaimable" => m.reclaimable_slab = v,
            "SwapTotal" => m.swap_total = v,
            "SwapFree" => m.swap_free = v,
            _ => {}
        }
    }
    // Shared memory is counted in Cached but cannot be dropped like cache.
    m.file_cache = m.file_cache.saturating_sub(m.shmem);
    m
}

pub(crate) fn parse_vmstat_swap(text: &str) -> (u64, u64) {
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix(' ')?.trim().parse().ok())
            .unwrap_or(0)
    };
    (get("pswpin"), get("pswpout"))
}

pub(crate) fn parse_mm_stat(text: &str) -> Option<Zram> {
    let f: Vec<u64> = text
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    Some(Zram {
        original: *f.first()?,
        compressed: *f.get(1)?,
        used: *f.get(2)?,
    })
}

fn unescape_mount(s: &str) -> String {
    // mountinfo escapes space, tab, newline and backslash as \ooo.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 4])
                .ok()
                .and_then(|o| u8::from_str_radix(o, 8).ok())
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mount_point = left.split_whitespace().nth(4)?;
            let fs_type = right.split_whitespace().next()?;
            Some(Mount {
                mount_point: PathBuf::from(unescape_mount(mount_point)),
                fs_type: fs_type.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "1234 (my (odd) prog) S 1 1234 1234 34816 1234 4194560 100 0 0 0 5 3 0 0 20 0 4 0 987654 123456789 2500 18446744073709551615";

    #[test]
    fn stat_handles_parens_in_comm() {
        let s = parse_stat(STAT).unwrap();
        assert_eq!(s.comm, "my (odd) prog");
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 1);
        assert_eq!(s.tty_nr, 34816);
        assert_eq!(s.start_ticks, 987654);
        assert_eq!(s.rss_pages, 2500);
    }

    #[test]
    fn smaps_rollup_gives_pss_and_uss() {
        let text = "55d0-7ffd ---p 00000000 00:00 0  [rollup]\nRss:  10240 kB\nPss:  6144 kB\nPss_Anon: 4096 kB\nShared_Clean: 2048 kB\nPrivate_Clean: 1024 kB\nPrivate_Dirty: 3072 kB\nSwap: 512 kB\n";
        let m = parse_smaps_rollup(text).unwrap();
        assert_eq!(m.rss, 10240 * 1024);
        assert_eq!(m.pss, Some(6144 * 1024));
        assert_eq!(m.uss, Some(4096 * 1024));
        assert_eq!(m.swap, Some(512 * 1024));
        assert_eq!(parse_smaps_rollup(""), None);
    }

    #[test]
    fn meminfo_separates_shmem_from_cache() {
        let text = "MemTotal: 16000000 kB\nMemFree: 1000000 kB\nMemAvailable: 9000000 kB\nBuffers: 100000 kB\nCached: 6000000 kB\nShmem: 500000 kB\nSReclaimable: 200000 kB\nSwapTotal: 2000000 kB\nSwapFree: 1500000 kB\n";
        let m = parse_meminfo(text);
        assert_eq!(m.total, 16_000_000 * 1024);
        assert_eq!(m.available, 9_000_000 * 1024);
        assert_eq!(m.file_cache, 5_500_000 * 1024);
        assert_eq!(
            m.used_by_programs(),
            (16_000_000 - 1_000_000 - 5_500_000 - 100_000 - 200_000) * 1024
        );
    }

    #[test]
    fn mountinfo_and_cgroup() {
        let text = "36 35 98:0 / /mnt/my\\040disk rw,noatime shared:1 - ext4 /dev/sda1 rw\n40 35 0:50 / /home/u/remote rw - fuse.sshfs host: rw\n";
        let m = parse_mountinfo(text);
        assert_eq!(m[0].mount_point, PathBuf::from("/mnt/my disk"));
        assert_eq!(m[0].fs_type, "ext4");
        assert!(super::super::is_network_fs(&m[1].fs_type));
        assert_eq!(
            parse_cgroup_leaf("0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-firefox-4242.scope\n").as_deref(),
            Some("app-gnome-firefox-4242.scope")
        );
        assert_eq!(parse_vmstat_swap("pgpgin 5\npswpin 7\npswpout 9\n"), (7, 9));
        assert_eq!(
            parse_status_uid("Name:\tx\nUid:\t1000\t1000\t1000\t1000\n"),
            Some(1000)
        );
    }

    #[test]
    fn fake_proc_tree_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        fs::create_dir_all(p.join("proc/77")).unwrap();
        fs::write(p.join("proc/stat"), "cpu 1 2 3\nbtime 1700000000\n").unwrap();
        fs::write(p.join("proc/77/stat"), STAT.replacen("1234", "77", 1)).unwrap();
        fs::write(p.join("proc/77/status"), "Uid:\t1000\t1000\t1000\t1000\n").unwrap();
        fs::write(p.join("proc/77/cmdline"), b"node\0server.js\0").unwrap();
        fs::write(
            p.join("proc/77/smaps_rollup"),
            "Rss: 8 kB\nPss: 4 kB\nPrivate_Dirty: 2 kB\n",
        )
        .unwrap();
        fs::write(
            p.join("proc/meminfo"),
            "MemTotal: 100 kB\nMemAvailable: 60 kB\n",
        )
        .unwrap();
        let dirs = dirs_from_env();
        let plat = LinuxPlatform::with_roots(p.join("proc"), p.join("sys"), p.join("dev"), dirs);
        let procs = plat.processes();
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].cmdline, vec!["node", "server.js"]);
        assert_eq!(procs[0].uid, 1000);
        assert!(procs[0].started_at >= 1_700_000_000);
        assert_eq!(plat.process_memory(77).uss, Some(2048));
        assert_eq!(plat.system_memory().unwrap().available, 60 * 1024);
    }
}
