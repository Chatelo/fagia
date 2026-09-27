//! Space `du` cannot see: deleted-but-open files, reserved blocks and
//! snapshots. This explains why `df` reports less free space than the
//! folder totals suggest.

use crate::paths::JsonPath;
use crate::platform::{FsStats, Platform, is_cow_fs};
use crate::report::{DeletedOpenOut, Unseen};
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Hidden space on the filesystem holding `root`.
pub fn collect(platform: &dyn Platform, root: &Path, stats: &FsStats) -> Unseen {
    let dev = std::fs::metadata(root).map(|m| m.dev()).ok();
    let mut seen = HashSet::new();
    let mut deleted: Vec<DeletedOpenOut> = platform
        .open_paths()
        .deleted
        .into_iter()
        .filter(|d| Some(d.dev) == dev && d.real > 0)
        .filter(|d| seen.insert((d.dev, d.path.clone())))
        .map(|d| DeletedOpenOut {
            path: JsonPath::new(&d.path),
            real: d.real,
            pid: d.pid,
            process: platform
                .process(d.pid)
                .map(|p| p.exe_name())
                .unwrap_or_else(|| "?".into()),
        })
        .collect();
    deleted.sort_by_key(|d| std::cmp::Reverse(d.real));
    let fs_type = platform.mount_for(root).map(|m| m.fs_type);
    Unseen {
        deleted_open_total: deleted.iter().map(|d| d.real).sum(),
        deleted_open: deleted,
        reserved: stats.reserved(),
        snapshot_note: fs_type.filter(|t| is_cow_fs(t)).map(|t| {
            format!("{t}: snapshots may hold space that no folder shows; check your snapshot tool")
        }),
    }
}
