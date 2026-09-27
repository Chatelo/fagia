//! Freedesktop trash that never copies: items move by `rename`, which
//! cannot cross filesystems, into the home trash when it shares the
//! target's filesystem, else into `$topdir/.Trash-$uid` on the target's
//! own filesystem. When neither works the move is refused; there is no
//! fallback to deleting.

use super::log::format_utc;
use crate::platform::Platform;
use crate::{Error, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashDir {
    pub root: PathBuf,
}

impl TrashDir {
    pub fn files(&self) -> PathBuf {
        self.root.join("files")
    }
    pub fn info(&self) -> PathBuf {
        self.root.join("info")
    }
}

#[derive(Debug, Clone)]
pub struct Trashed {
    pub trash_path: PathBuf,
    pub info_path: PathBuf,
}

fn dev_of(p: &Path) -> Option<u64> {
    p.ancestors()
        .find_map(|a| fs::metadata(a).ok())
        .map(|m| m.dev())
}

/// The trash to use for `target`, or why there is none.
pub fn choose(platform: &dyn Platform, target: &Path) -> Result<TrashDir> {
    let target_dev = fs::symlink_metadata(target)
        .map_err(|source| Error::Io {
            path: target.to_path_buf(),
            source,
        })?
        .dev();
    let home_trash = platform.dirs().home_trash();
    if dev_of(&home_trash) == Some(target_dev) {
        return Ok(TrashDir { root: home_trash });
    }
    let uid = platform.current_uid();
    let top = platform
        .mount_for(target)
        .map(|m| m.mount_point)
        .ok_or_else(|| Error::Refused(format!("no mount point found for {}", target.display())))?;
    // $topdir/.Trash/$uid is allowed only when .Trash is a real, sticky
    // directory (so other users cannot tamper with it).
    let shared = top.join(".Trash");
    if let Ok(m) = fs::symlink_metadata(&shared)
        && m.is_dir()
        && m.permissions().mode() & 0o1000 != 0
        && m.dev() == target_dev
    {
        return Ok(TrashDir {
            root: shared.join(uid.to_string()),
        });
    }
    let own = top.join(format!(".Trash-{uid}"));
    match fs::symlink_metadata(&own) {
        Ok(m) if m.is_dir() && m.uid() == uid && m.dev() == target_dev => {
            Ok(TrashDir { root: own })
        }
        Ok(_) => Err(Error::Refused(format!(
            "{} is not a trash directory owned by you",
            own.display()
        ))),
        Err(_) if dev_of(&top) == Some(target_dev) => Ok(TrashDir { root: own }),
        Err(_) => Err(Error::Refused(format!(
            "no trash on the filesystem of {}; use --permanent to delete",
            target.display()
        ))),
    }
}

/// Percent-encodes a path for a `.trashinfo` file.
fn url_encode(p: &Path) -> String {
    let mut s = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn ensure_dirs(t: &TrashDir) -> std::io::Result<()> {
    for d in [&t.root, &t.files(), &t.info()] {
        if !d.exists() {
            fs::create_dir_all(d)?;
            fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Moves `target` into `trash`. The `.trashinfo` is written first (with
/// O_EXCL, which also reserves the name); if the rename fails it is
/// removed again and nothing else has changed.
pub(super) fn move_to(trash: &TrashDir, target: &Path, now: i64) -> Result<Trashed> {
    let io = |source| Error::Io {
        path: target.to_path_buf(),
        source,
    };
    ensure_dirs(trash).map_err(io)?;
    let base = target
        .file_name()
        .ok_or_else(|| Error::Refused(format!("{} has no file name", target.display())))?
        .to_os_string();
    for n in 0..10_000 {
        let mut name = base.clone();
        if n > 0 {
            name.push(format!(".{n}"));
        }
        let files = trash.files().join(&name);
        let mut info_name = name.clone();
        info_name.push(".trashinfo");
        let info = trash.info().join(info_name);
        if files.exists() {
            continue;
        }
        let mut f = match OpenOptions::new().write(true).create_new(true).open(&info) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(e)),
        };
        let text = format!(
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            url_encode(target),
            format_utc(now)
        );
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(io)?;
        return match fs::rename(target, &files) {
            Ok(()) => Ok(Trashed {
                trash_path: files,
                info_path: info,
            }),
            Err(e) => {
                #[allow(clippy::disallowed_methods)] // our own .trashinfo, just created
                let _ = fs::remove_file(&info);
                if e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) {
                    Err(Error::Refused(format!(
                        "{} is on another filesystem than the trash; refusing to copy",
                        target.display()
                    )))
                } else {
                    Err(io(e))
                }
            }
        };
    }
    Err(Error::Refused(
        "too many items with this name in the trash".into(),
    ))
}

/// Moves a trashed item back to where it came from.
pub(super) fn restore(trash_path: &Path, info_path: Option<&Path>, original: &Path) -> Result<()> {
    if fs::symlink_metadata(original).is_ok() {
        return Err(Error::Refused(format!(
            "{} exists again; not overwriting",
            original.display()
        )));
    }
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::rename(trash_path, original).map_err(|source| Error::Io {
        path: trash_path.to_path_buf(),
        source,
    })?;
    if let Some(info) = info_path {
        #[allow(clippy::disallowed_methods)] // the entry's own .trashinfo
        let _ = fs::remove_file(info);
    }
    Ok(())
}

/// Reverses [`url_encode`] for `Path=` lines written by any trash tool.
fn url_decode(s: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// `YYYY-MM-DDThh:mm:ss` as seconds since the epoch, read as UTC (other
/// tools write local time; an hour or two does not matter for ages).
fn parse_deletion_date(s: &str) -> Option<i64> {
    let (date, time) = s.trim().split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time
        .split(':')
        .map(|x| x.get(..2).unwrap_or(x).parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next().flatten().unwrap_or(0));
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// One item sitting in a trash folder.
#[derive(Debug, Clone)]
pub struct TrashEntry {
    pub trash: TrashDir,
    /// The item itself, `<trash>/files/<name>`.
    pub path: PathBuf,
    pub info: PathBuf,
    pub original: Option<PathBuf>,
    pub deleted_at: Option<i64>,
    pub size: u64,
}

/// Every trash folder of this user: the home trash plus the per-filesystem
/// ones (`$topdir/.Trash/$uid`, `$topdir/.Trash-$uid`) that exist.
pub fn trash_dirs(platform: &dyn Platform) -> Vec<TrashDir> {
    let uid = platform.current_uid();
    let mut out = vec![TrashDir {
        root: platform.dirs().home_trash(),
    }];
    for m in platform.mounts() {
        if crate::platform::is_network_fs(&m.fs_type) || platform.is_virtual_path(&m.mount_point) {
            continue;
        }
        for root in [
            m.mount_point.join(".Trash").join(uid.to_string()),
            m.mount_point.join(format!(".Trash-{uid}")),
        ] {
            if fs::symlink_metadata(&root).is_ok_and(|md| md.is_dir() && md.uid() == uid)
                && !out.iter().any(|t| t.root == root)
            {
                out.push(TrashDir { root });
            }
        }
    }
    out.retain(|t| t.files().is_dir());
    out
}

/// What is in the trash, largest first.
pub fn list(platform: &dyn Platform) -> Vec<TrashEntry> {
    let mut out = Vec::new();
    for t in trash_dirs(platform) {
        let Ok(rd) = fs::read_dir(t.files()) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name();
            let mut info_name = name.clone();
            info_name.push(".trashinfo");
            let info = t.info().join(info_name);
            let text = fs::read_to_string(&info).unwrap_or_default();
            let field = |k: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(k))
                    .map(str::to_string)
            };
            out.push(TrashEntry {
                path: e.path(),
                original: field("Path=").map(|p| url_decode(&p)),
                deleted_at: field("DeletionDate=").and_then(|d| parse_deletion_date(&d)),
                size: super::safety::measure(&e.path()),
                info,
                trash: t.clone(),
            });
        }
    }
    out.sort_by(|a, b| b.size.cmp(&a.size).then(a.path.cmp(&b.path)));
    out
}

/// Permanently deletes one trashed item and its `.trashinfo`. Refuses
/// anything that is not directly inside a trash `files/` folder; never
/// follows symlinks (a symlink in the trash is removed, not its target).
pub(super) fn purge(entry: &TrashEntry) -> Result<()> {
    let refuse = |why: &str| Err(Error::Refused(format!("{}: {why}", entry.path.display())));
    if entry.path.parent() != Some(entry.trash.files().as_path()) {
        return refuse("not directly inside a trash folder");
    }
    let meta = match fs::symlink_metadata(&entry.path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return refuse("already gone"),
        Err(source) => {
            return Err(Error::Io {
                path: entry.path.clone(),
                source,
            });
        }
    };
    #[allow(clippy::disallowed_methods)] // emptying the trash is a permanent delete by definition
    let res = if meta.is_dir() {
        fs::remove_dir_all(&entry.path)
    } else {
        fs::remove_file(&entry.path)
    };
    res.map_err(|source| Error::Io {
        path: entry.path.clone(),
        source,
    })?;
    #[allow(clippy::disallowed_methods)] // the item's own .trashinfo
    let _ = fs::remove_file(&entry.info);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_trashinfo_paths() {
        assert_eq!(url_encode(Path::new("/a b/c%d/ü")), "/a%20b/c%25d/%C3%BC");
    }

    #[test]
    fn move_and_restore_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = TrashDir {
            root: tmp.path().join("Trash"),
        };
        let target = tmp.path().join("proj/target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(target.join("debug/app"), b"x").unwrap();
        let t = move_to(&trash, &target, 0).unwrap();
        assert!(!target.exists());
        assert!(t.trash_path.join("debug/app").exists());
        let info = fs::read_to_string(&t.info_path).unwrap();
        assert!(info.contains("Path=") && info.contains("DeletionDate=1970-01-01T00:00:00"));
        // Same name again gets a suffix instead of clobbering.
        fs::create_dir_all(&target).unwrap();
        let t2 = move_to(&trash, &target, 0).unwrap();
        assert!(t2.trash_path.to_string_lossy().ends_with("target.1"));
        // Something new sits at the original path: restore must not clobber it.
        fs::create_dir_all(&target).unwrap();
        restore(&t.trash_path, Some(&t.info_path), &target).unwrap_err();
        let other = tmp.path().join("proj/restored");
        restore(&t.trash_path, Some(&t.info_path), &other).unwrap();
        assert!(other.join("debug/app").exists());
        assert!(!t.info_path.exists());
    }

    #[test]
    fn cross_filesystem_move_is_refused_not_copied() {
        let shm = Path::new("/dev/shm");
        let tmp = tempfile::tempdir().unwrap();
        if !shm.is_dir() || dev_of(shm) == dev_of(tmp.path()) {
            return; // needs two filesystems
        }
        let Ok(elsewhere) = tempfile::tempdir_in(shm) else {
            return;
        };
        let target = elsewhere.path().join("big");
        fs::write(&target, vec![0u8; 4096]).unwrap();
        let trash = TrashDir {
            root: tmp.path().join("Trash"),
        };
        let err = move_to(&trash, &target, 0).unwrap_err();
        assert!(err.to_string().contains("refusing to copy"), "{err}");
        assert!(target.exists(), "target must be untouched");
        assert_eq!(
            fs::read_dir(trash.info()).unwrap().count(),
            0,
            "stale trashinfo left behind"
        );
    }

    #[test]
    fn trashinfo_round_trip() {
        let p = Path::new("/a b/c%d/ü");
        assert_eq!(url_decode(&url_encode(p)), p);
        assert_eq!(parse_deletion_date("1970-01-02T00:00:10"), Some(86_410));
        assert_eq!(
            parse_deletion_date(&format_utc(1_790_000_000)),
            Some(1_790_000_000)
        );
    }

    #[test]
    fn purge_only_inside_files_dir_and_not_through_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = TrashDir {
            root: tmp.path().join("Trash"),
        };
        let outside = tmp.path().join("precious");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "x").unwrap();
        let target = tmp.path().join("old");
        fs::create_dir_all(target.join("sub")).unwrap();
        let t = move_to(&trash, &target, 0).unwrap();
        // A symlink in the trash pointing at real data.
        ensure_dirs(&trash).unwrap();
        std::os::unix::fs::symlink(&outside, trash.files().join("link")).unwrap();
        let entry = |path: PathBuf, info: PathBuf| TrashEntry {
            trash: trash.clone(),
            path,
            info,
            original: None,
            deleted_at: None,
            size: 0,
        };
        purge(&entry(t.trash_path.clone(), t.info_path.clone())).unwrap();
        assert!(!t.trash_path.exists() && !t.info_path.exists());
        purge(&entry(
            trash.files().join("link"),
            trash.info().join("link.trashinfo"),
        ))
        .unwrap();
        assert!(
            outside.join("keep.txt").exists(),
            "symlink target must survive"
        );
        assert!(purge(&entry(outside.clone(), trash.info().join("x"))).is_err());
        assert!(outside.exists());
    }
}
