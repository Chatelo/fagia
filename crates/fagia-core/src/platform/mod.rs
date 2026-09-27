//! The only place OS-specific code lives. Everything else talks to the
//! [`Platform`] trait, so a new OS is one new implementation here.

use crate::Result;
use serde::Serialize;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(not(target_os = "linux"))]
compile_error!("fagia currently supports Linux only");

/// Standard per-user locations, resolved once.
#[derive(Debug, Clone)]
pub struct Dirs {
    pub home: PathBuf,
    pub cache: PathBuf,
    pub data: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub cargo_home: PathBuf,
}

impl Dirs {
    /// Value for a rule path variable such as `{cache_dir}`.
    pub fn var(&self, name: &str) -> Option<&Path> {
        Some(match name {
            "home" => &self.home,
            "cache_dir" => &self.cache,
            "data_dir" => &self.data,
            "config_dir" => &self.config,
            "state_dir" => &self.state,
            "cargo_home" => &self.cargo_home,
            _ => return None,
        })
    }

    pub fn home_trash(&self) -> PathBuf {
        self.data.join("Trash")
    }
}

/// Size and identity of one file, as the platform reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// Bytes of disk blocks actually used.
    pub real: u64,
    /// File length.
    pub apparent: u64,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    /// Seconds since the Unix epoch.
    pub mtime: i64,
    /// Cloud-only placeholder whose content must not be read.
    pub placeholder: bool,
}

impl FileStat {
    pub fn is_sparse(&self) -> bool {
        // A file using far fewer blocks than its length has holes.
        self.apparent > 1 << 20 && self.real < self.apparent / 2
    }
}

#[derive(Debug, Clone)]
pub struct Mount {
    pub mount_point: PathBuf,
    pub fs_type: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, schemars::JsonSchema)]
pub struct FsStats {
    pub total: u64,
    pub free: u64,
    /// Free space an unprivileged user can use.
    pub available: u64,
}

impl FsStats {
    /// Blocks reserved for root (ext4 keeps 5% by default).
    pub fn reserved(&self) -> u64 {
        self.free.saturating_sub(self.available)
    }
}

/// Identifies one process instance. The start time guards against a PID
/// being reused between confirmation and signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
pub struct ProcKey {
    pub pid: u32,
    pub start_ticks: u64,
}

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub key: ProcKey,
    pub ppid: u32,
    pub uid: u32,
    pub comm: String,
    pub exe: Option<PathBuf>,
    pub cmdline: Vec<String>,
    /// Seconds since the Unix epoch.
    pub started_at: i64,
    /// Controlling terminal device number, `None` when there is none.
    pub tty: Option<u64>,
    pub kernel_thread: bool,
    pub state: char,
    /// Last component of the process's cgroup path (for example an
    /// `app-*.scope` unit).
    pub cgroup_leaf: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Resident bytes from the cheap status line (no page-table walk).
    pub rss: u64,
}

impl ProcInfo {
    /// Executable file name, falling back to `comm` (which the kernel
    /// truncates to 15 bytes).
    pub fn exe_name(&self) -> String {
        self.exe
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| {
                n.to_string_lossy()
                    .trim_end_matches(" (deleted)")
                    .to_string()
            })
            // Versioned binaries (`~/.local/share/app/versions/2.1.3`) say
            // nothing; the command line's first word usually does.
            .filter(|n| !n.starts_with(|c: char| c.is_ascii_digit()))
            .or_else(|| {
                let arg0 = self.cmdline.first()?;
                let base = arg0.rsplit('/').next()?.split_whitespace().next()?;
                (!base.is_empty() && !base.starts_with(|c: char| c.is_ascii_digit()))
                    .then(|| base.to_string())
            })
            .unwrap_or_else(|| self.comm.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    pub key: ProcKey,
    pub ppid: u32,
    pub state: char,
    pub rss: u64,
}

/// Per-process memory. `pss`, `uss` and `swap` are `None` when the platform
/// cannot measure them (no permission, or unsupported), so callers label
/// the fallback instead of guessing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcMem {
    pub rss: u64,
    pub pss: Option<u64>,
    pub uss: Option<u64>,
    pub swap: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, schemars::JsonSchema)]
pub struct Zram {
    pub original: u64,
    pub compressed: u64,
    pub used: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SystemMemory {
    pub total: u64,
    pub available: u64,
    pub free: u64,
    /// Page cache that the kernel releases on demand.
    pub file_cache: u64,
    pub buffers: u64,
    pub shmem: u64,
    pub reclaimable_slab: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub zram: Option<Zram>,
    /// Cumulative pages swapped in and out since boot.
    pub swap_in_pages: u64,
    pub swap_out_pages: u64,
    pub page_size: u64,
}

impl SystemMemory {
    pub fn used_by_programs(&self) -> u64 {
        self.total
            .saturating_sub(self.free)
            .saturating_sub(self.file_cache)
            .saturating_sub(self.buffers)
            .saturating_sub(self.reclaimable_slab)
    }
}

/// A file that was deleted while a process still holds it open; its space
/// is not freed until the process closes it.
#[derive(Debug, Clone)]
pub struct DeletedOpen {
    pub pid: u32,
    pub path: PathBuf,
    pub real: u64,
    pub dev: u64,
}

#[derive(Debug, Clone, Default)]
pub struct OpenPaths {
    pub cwds: Vec<(u32, PathBuf)>,
    pub files: Vec<(u32, PathBuf)>,
    pub deleted: Vec<DeletedOpen>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Terminate,
    Kill,
    Stop,
    Continue,
}

pub trait Platform: Send + Sync {
    fn name(&self) -> &'static str;
    fn dirs(&self) -> &Dirs;
    fn file_stat(&self, meta: &Metadata) -> FileStat;
    /// Pseudo filesystems that are never scanned (`/proc`, `/sys`, ...).
    fn is_virtual_path(&self, path: &Path) -> bool;
    fn mounts(&self) -> Vec<Mount>;
    fn fs_stats(&self, path: &Path) -> Result<FsStats>;
    fn effective_uid(&self) -> u32;
    fn current_uid(&self) -> u32;
    fn current_pid(&self) -> u32;
    fn hostname(&self) -> String;
    /// Folders that can never be cleaned, whatever the config says.
    fn system_protected_paths(&self) -> Vec<PathBuf>;

    fn processes(&self) -> Vec<ProcInfo>;
    /// Cheap per-refresh view: identity, parent, state and resident size
    /// only, for every process. Callers cache the rest of [`ProcInfo`].
    fn process_stats(&self) -> Vec<ProcStat>;
    fn process(&self, pid: u32) -> Option<ProcInfo>;
    fn process_memory(&self, pid: u32) -> ProcMem;
    fn system_memory(&self) -> Result<SystemMemory>;
    fn open_paths(&self) -> OpenPaths;
    fn tty_exists(&self, tty: u64) -> bool;
    /// Current memory of a cgroup, `rel` relative to the cgroup root (for
    /// example `system.slice/docker-<id>.scope`).
    fn cgroup_memory(&self, rel: &str) -> Option<u64>;
    /// Login name for `uid`, or the number when unknown.
    fn user_name(&self, uid: u32) -> String;
    fn is_alive(&self, key: ProcKey) -> bool;
    /// Sends `sig` to exactly the process `key` names, or fails. Only the
    /// action gate may call this (enforced by clippy `disallowed-methods`).
    fn signal(&self, key: ProcKey, sig: Signal) -> Result<()>;

    fn mount_for(&self, path: &Path) -> Option<Mount> {
        self.mounts()
            .into_iter()
            .filter(|m| path.starts_with(&m.mount_point))
            .max_by_key(|m| m.mount_point.as_os_str().len())
    }
}

/// Network and FUSE filesystems are skipped by default: slow, and reading
/// them may cost bandwidth.
pub fn is_network_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "nfs"
            | "nfs4"
            | "cifs"
            | "smb3"
            | "smbfs"
            | "9p"
            | "afs"
            | "ceph"
            | "glusterfs"
            | "davfs"
            | "sshfs"
            | "fuse"
    ) || fs_type.starts_with("fuse.")
}

/// Filesystems where shared extents or snapshots make freed space lower
/// than the sizes shown.
pub fn is_cow_fs(fs_type: &str) -> bool {
    matches!(fs_type, "btrfs" | "zfs" | "bcachefs" | "xfs" | "apfs")
}

pub fn is_ram_fs(fs_type: &str) -> bool {
    matches!(fs_type, "tmpfs" | "ramfs")
}

/// The platform for the running OS.
pub fn current() -> Arc<dyn Platform> {
    Arc::new(linux::LinuxPlatform::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_name_skips_version_numbers() {
        let mut p = ProcInfo {
            key: ProcKey {
                pid: 9,
                start_ticks: 1,
            },
            ppid: 1,
            uid: 1000,
            comm: "2.1.283".into(),
            exe: Some(PathBuf::from(
                "/home/u/.local/share/someapp/versions/2.1.283",
            )),
            cmdline: vec!["someapp".into(), "--flag".into()],
            started_at: 0,
            tty: None,
            kernel_thread: false,
            state: 'S',
            cgroup_leaf: None,
            cwd: None,
            rss: 0,
        };
        assert_eq!(p.exe_name(), "someapp");
        p.exe = Some(PathBuf::from("/usr/bin/node"));
        assert_eq!(p.exe_name(), "node");
    }
}
