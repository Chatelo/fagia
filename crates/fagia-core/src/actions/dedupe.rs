//! Removing duplicate copies: keep one copy of each set and move the rest
//! to the trash, or replace them with hard links to the kept copy.
//!
//! Only the user's own files are touched by default (documents, media,
//! archives, office files outside hidden and dependency folders): a copy
//! inside `.git`, a package store or an app's data looks identical but
//! deleting it breaks that program. Every removal is re-checked right
//! before it happens, including a full hash against the kept copy.

use super::log::LogEntry;
use super::safety::InUse;
use super::{Gate, ItemOutcome, ItemResult, trash};
use crate::disk::dupe_dirs::{DirSet, inside_any};
use crate::disk::dupes::DupeSet;
use crate::disk::media;
use crate::disk::scope::{PROGRAM_FOLDERS as APP_FOLDERS, PROJECT_MARKERS};
use crate::model::now_epoch;
use crate::paths::JsonPath;
use crate::{Error, Result};
use rayon::prelude::*;
use schemars::JsonSchema;
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DedupeMode {
    /// Move extra copies to the trash (restorable with `fagia undo`).
    Trash,
    /// Replace extra copies with hard links to the kept copy: space is
    /// freed and every path keeps working.
    Link,
}

#[derive(Debug, Clone)]
pub struct DedupeOptions {
    pub mode: DedupeMode,
    /// Prefer keeping copies under these folders.
    pub keep_under: Vec<PathBuf>,
    /// Also act on files that are not documents or media.
    pub any_type: bool,
}

/// File types a person keeps copies of by hand.
const OWN_TYPES: &[&str] = &[
    "pdf", "epub", "mobi", "azw3", "djvu", "doc", "docx", "odt", "rtf", "xls", "xlsx", "ods",
    "csv", "ppt", "pptx", "odp", "zip", "tar", "gz", "tgz", "xz", "bz2", "zst", "7z", "rar", "iso",
    "deb", "rpm", "appimage", "dmg", "apk",
];

/// Why a copy must not be removed on type grounds, if so.
pub fn own_file_refusal(path: &Path, root: &Path, any_type: bool) -> Option<String> {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let comps: Vec<String> = rel
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if comps.iter().any(|c| c == ".git") {
        return Some("inside git's own storage".into());
    }
    if any_type {
        return None;
    }
    if let Some(c) = comps.iter().find(|c| c.starts_with('.')) {
        return Some(format!(
            "inside hidden folder {c} (app data; --any-type to include)"
        ));
    }
    if let Some(c) = comps.iter().find(|c| APP_FOLDERS.contains(&c.as_str())) {
        return Some(format!("inside {c} (program files; --any-type to include)"));
    }
    // Files inside a code project may be read by that project's code.
    if let Some(project) = path
        .ancestors()
        .skip(1)
        .take_while(|a| a.starts_with(root) && *a != root)
        .find(|a| PROJECT_MARKERS.iter().any(|m| a.join(m).exists()))
    {
        return Some(format!(
            "inside project {} (its code may use it; --any-type to include)",
            project.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let name = path.file_name().unwrap_or_default();
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let own =
        media::kind_of(name).is_some() || ext.as_deref().is_some_and(|e| OWN_TYPES.contains(&e));
    (!own).then(|| "not a document or media file (--any-type to include)".to_string())
}

/// Lower is a better copy to keep.
fn keep_rank(path: &Path, root: &Path, keep_under: &[PathBuf]) -> (bool, u8, i64, usize, PathBuf) {
    const TRANSIENT: &[&str] = &[
        "backup",
        "backups",
        "Backup",
        "Backups",
        "Downloads",
        "Download",
        "tmp",
        "Temp",
        "temp",
    ];
    let preferred = keep_under.iter().any(|k| path.starts_with(k));
    let transient = path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .any(|c| TRANSIENT.contains(&c.as_os_str().to_string_lossy().as_ref()));
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let looks_copied = [" (1)", " (2)", " (3)", "copy", " - copy", "_copy"]
        .iter()
        .any(|m| name.contains(m));
    let mtime = std::fs::metadata(path)
        .map(|m| m.mtime())
        .unwrap_or(i64::MAX);
    (
        !preferred,
        u8::from(transient) + u8::from(looks_copied),
        mtime,
        path.as_os_str().len(),
        path.to_path_buf(),
    )
}

/// Why a duplicate folder copy must not be removed on type grounds.
pub fn own_folder_refusal(path: &Path, root: &Path, any_type: bool) -> Option<String> {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if comps.iter().any(|c| c == ".git") {
        return Some("inside git's own storage".into());
    }
    if any_type {
        return None;
    }
    if let Some(c) = comps.iter().find(|c| c.starts_with('.')) {
        return Some(format!(
            "hidden folder {c} (app data; --any-type to include)"
        ));
    }
    if let Some(c) = comps.iter().find(|c| APP_FOLDERS.contains(&c.as_str())) {
        return Some(format!("{c} (program files; --any-type to include)"));
    }
    if let Some(project) = path
        .ancestors()
        .skip(1)
        .take_while(|a| a.starts_with(root) && *a != root)
        .find(|a| PROJECT_MARKERS.iter().any(|m| a.join(m).exists()))
    {
        return Some(format!(
            "inside project {} (its code may use it; --any-type to include)",
            project.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let listing = crate::disk::dupe_dirs::listing(path)?;
    listing
        .iter()
        .any(|e| {
            e.rel
                .file_name()
                .is_some_and(|n| PROJECT_MARKERS.iter().any(|m| n == *m))
        })
        .then(|| "contains a code project (--any-type to include)".to_string())
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SkippedCopy {
    #[serde(flatten)]
    pub path: JsonPath,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CopyKind {
    File,
    Folder,
}

#[derive(Debug, Clone)]
pub struct DedupeSetPlan {
    pub kind: CopyKind,
    pub size: u64,
    /// Disk bytes one copy uses.
    pub real: u64,
    /// Files in one copy (1 for a file).
    pub files: u64,
    /// For folders: the content manifest every copy must still have.
    pub manifest: Option<[u8; 32]>,
    pub keep: Vec<PathBuf>,
    pub remove: Vec<PathBuf>,
    pub skipped: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone)]
pub struct DedupePlan {
    pub root: PathBuf,
    pub mode: DedupeMode,
    pub sets: Vec<DedupeSetPlan>,
}

impl DedupePlan {
    pub fn reclaimable(&self) -> u64 {
        self.sets
            .iter()
            .map(|s| s.real * s.remove.len() as u64)
            .sum()
    }

    pub fn removals(&self) -> usize {
        self.sets.iter().map(|s| s.remove.len()).sum()
    }
}

struct Candidate {
    kind: CopyKind,
    size: u64,
    real: u64,
    files: u64,
    manifest: Option<[u8; 32]>,
    paths: Vec<PathBuf>,
}

/// Decides, per set, what to keep and what to remove. Duplicate folders
/// come first; file sets lying wholly inside them are handled with them.
pub fn plan(gate: &Gate, sets: &[DupeSet], dirs: &[DirSet], opts: &DedupeOptions) -> DedupePlan {
    let in_use = InUse::snapshot(gate.platform());
    let trashing = opts.mode == DedupeMode::Trash;
    let mut cands: Vec<Candidate> = dirs
        .iter()
        .map(|d| Candidate {
            kind: CopyKind::Folder,
            size: d.real,
            real: d.real,
            files: d.files,
            manifest: Some(d.manifest),
            paths: d.paths.clone(),
        })
        .collect();
    for s in sets {
        let paths: Vec<PathBuf> = s
            .paths
            .iter()
            .filter(|p| !inside_any(p, dirs))
            .cloned()
            .collect();
        if paths.len() > 1 {
            cands.push(Candidate {
                kind: CopyKind::File,
                size: s.size,
                real: s.real,
                files: 1,
                manifest: None,
                paths,
            });
        }
    }
    let mut planned: Vec<DedupeSetPlan> = cands
        .par_iter()
        .map(|c| {
            let mut ok = Vec::new();
            let mut skipped = Vec::new();
            for p in &c.paths {
                let type_rule = match c.kind {
                    CopyKind::File => own_file_refusal(p, gate.root(), opts.any_type),
                    CopyKind::Folder => own_folder_refusal(p, gate.root(), opts.any_type),
                };
                match type_rule.or_else(|| gate.path_refusal(p, trashing, &in_use)) {
                    Some(w) => skipped.push((p.clone(), w)),
                    None => ok.push(p.clone()),
                }
            }
            ok.sort_by_cached_key(|p| keep_rank(p, gate.root(), &opts.keep_under));
            // Always keep the best of the user's own copies, even when
            // other copies must stay anyway (a project or app may delete
            // its copy later; the user's copy is the one to rely on).
            let keep: Vec<PathBuf> = ok.first().cloned().into_iter().collect();
            let mut remove: Vec<PathBuf> = ok.into_iter().skip(1).collect();
            if opts.mode == DedupeMode::Link {
                // A hard link cannot cross filesystems.
                let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
                let target = keep.first().and_then(|k| dev(k));
                let (same, other): (Vec<_>, Vec<_>) =
                    remove.into_iter().partition(|p| dev(p) == target);
                remove = same;
                skipped.extend(
                    other
                        .into_iter()
                        .map(|p| (p, "on another filesystem than the kept copy".to_string())),
                );
            }
            DedupeSetPlan {
                kind: c.kind,
                size: c.size,
                real: c.real,
                files: c.files,
                manifest: c.manifest,
                keep,
                remove,
                skipped,
            }
        })
        .collect();
    planned.sort_by_key(|s| std::cmp::Reverse(s.real * s.remove.len() as u64));
    DedupePlan {
        root: gate.root().to_path_buf(),
        mode: opts.mode,
        sets: planned,
    }
}

fn full_hash(path: &Path) -> std::io::Result<blake3::Hash> {
    let mut h = blake3::Hasher::new();
    h.update_reader(std::fs::File::open(path)?)?;
    Ok(h.finalize())
}

/// Replaces `dup` with a hard link to `keep`, atomically: the link is made
/// under a temporary name in the same folder, then renamed over `dup`.
fn link_over(keep: &Path, dup: &Path) -> Result<()> {
    let io = |source| Error::Io {
        path: dup.to_path_buf(),
        source,
    };
    let dir = dup
        .parent()
        .ok_or_else(|| Error::Refused("no parent folder".into()))?;
    let tmp = dir.join(format!(
        ".{}.fagia-link-{}",
        dup.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::hard_link(keep, &tmp).map_err(io)?;
    if let Err(e) = std::fs::rename(&tmp, dup) {
        #[allow(clippy::disallowed_methods)] // our own temporary link
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok(())
}

/// Links every file of folder copy `dup` to its twin in `keep`.
fn link_folder(keep: &Path, dup: &Path) -> Result<()> {
    let entries = crate::disk::dupe_dirs::listing(dup)
        .ok_or_else(|| Error::Refused("cannot list the copy".into()))?;
    let files: Vec<_> = entries.iter().filter(|e| e.kind == 'f').collect();
    for (n, e) in files.iter().enumerate() {
        let (k, d) = (keep.join(&e.rel), dup.join(&e.rel));
        let same = matches!(
            (std::fs::metadata(&k), std::fs::metadata(&d)),
            (Ok(a), Ok(b)) if a.dev() == b.dev() && a.ino() == b.ino()
        );
        if !same && let Err(err) = link_over(&k, &d) {
            return Err(Error::Other(format!(
                "linked {n} of {} files, then: {err}",
                files.len()
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DedupeResult {
    pub run: String,
    pub mode: DedupeMode,
    pub results: Vec<ItemResult>,
    pub estimate: u64,
    pub freed_measured: i64,
    pub interrupted: bool,
}

/// Removes the planned copies set by set. Before each one: the path checks
/// again, the kept copy still exists, and both still hash the same (a file
/// hash, or a folder's whole content manifest).
pub fn execute(
    gate: &Gate,
    plan: &DedupePlan,
    cancel: &AtomicBool,
    on_item: &mut dyn FnMut(&ItemResult),
) -> DedupeResult {
    let platform = gate.platform();
    let run = format!("{}-{}", now_epoch(), platform.current_pid());
    let in_use = InUse::snapshot(platform);
    let before = platform.fs_stats(&plan.root).ok();
    let trashing = plan.mode == DedupeMode::Trash;
    let mut results = Vec::new();
    let mut interrupted = false;
    for set in plan.sets.iter().filter(|s| !s.remove.is_empty()) {
        let keep = &set.keep[0];
        let folder = set.kind == CopyKind::Folder;
        let keep_hash = if folder { None } else { full_hash(keep).ok() };
        let keep_ok = !folder || crate::disk::dupe_dirs::manifest(keep) == set.manifest;
        for dup in &set.remove {
            let mut result = ItemResult {
                path: JsonPath::new(dup),
                outcome: ItemOutcome::Done,
                detail: Some(format!(
                    "copy of {}",
                    crate::paths::display_path(keep, Some(&platform.dirs().home))
                )),
                bytes: set.real,
                trash_path: None,
            };
            let action = match (trashing, folder) {
                (true, _) => "trash",
                (false, false) => "dedupe-link",
                (false, true) => "dedupe-link-folder",
            };
            let mut entry = LogEntry::new(&run, action, dup, set.real, "ok");
            let check = || -> std::result::Result<(), String> {
                if cancel.load(Ordering::SeqCst) {
                    return Err("interrupted".into());
                }
                if let Some(why) = gate.path_refusal(dup, trashing, &in_use) {
                    return Err(why);
                }
                if folder {
                    if !keep_ok {
                        return Err("the kept folder changed since the dry run".into());
                    }
                    return if crate::disk::dupe_dirs::manifest(dup) == set.manifest {
                        Ok(())
                    } else {
                        Err("this folder changed since the dry run".into())
                    };
                }
                let (Ok(km), Ok(dm)) = (
                    std::fs::symlink_metadata(keep),
                    std::fs::symlink_metadata(dup),
                ) else {
                    return Err("the kept copy or this copy is gone".into());
                };
                if !km.is_file() || !dm.is_file() || km.len() != set.size || dm.len() != set.size {
                    return Err("changed since the dry run".into());
                }
                if km.dev() == dm.dev() && km.ino() == dm.ino() {
                    return Err("already the same file".into());
                }
                match (keep_hash, full_hash(dup).ok()) {
                    (Some(a), Some(b)) if a == b => Ok(()),
                    _ => Err("content differs from the kept copy now".into()),
                }
            };
            match check() {
                Err(why) => {
                    interrupted |= why == "interrupted";
                    result.outcome = ItemOutcome::Skipped;
                    result.detail = Some(why);
                }
                Ok(()) => {
                    let done = if trashing {
                        trash::choose(platform, dup)
                            .and_then(|t| trash::move_to(&t, dup, now_epoch()))
                            .map(|t| {
                                entry.trash_path =
                                    Some(t.trash_path.to_string_lossy().into_owned());
                                entry.trash_info = Some(t.info_path.to_string_lossy().into_owned());
                                result.trash_path = Some(JsonPath::new(&t.trash_path));
                            })
                    } else if folder {
                        link_folder(keep, dup)
                    } else {
                        link_over(keep, dup)
                    };
                    if let Err(e) = done {
                        result.outcome = if matches!(e, Error::Refused(_)) {
                            ItemOutcome::Skipped
                        } else {
                            ItemOutcome::Failed
                        };
                        result.detail = Some(e.to_string());
                    }
                }
            }
            if result.outcome != ItemOutcome::Done {
                result.bytes = 0;
                entry.outcome = format!("{:?}", result.outcome).to_lowercase();
                entry.detail = result.detail.clone();
            }
            let _ = gate.log().append(&entry);
            on_item(&result);
            results.push(result);
        }
    }
    let after = platform.fs_stats(&plan.root).ok();
    DedupeResult {
        run,
        mode: plan.mode,
        estimate: results.iter().map(|r| r.bytes).sum(),
        freed_measured: match (before, after) {
            (Some(b), Some(a)) => a.available as i64 - b.available as i64,
            _ => 0,
        },
        results,
        interrupted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::log::ActionLog;
    use crate::disk::{Progress, ScanOptions, dupes, scan};
    use crate::platform::Dirs;
    use crate::platform::linux::LinuxPlatform;
    use crate::rules::RuleSet;
    use std::fs;

    struct Fx {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        plat: LinuxPlatform,
        rules: RuleSet,
    }

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap().join("home");
        fs::create_dir_all(&home).unwrap();
        let dirs = Dirs {
            cache: home.join(".cache"),
            data: home.join(".local/share"),
            config: home.join(".config"),
            state: home.join(".local/state"),
            cargo_home: home.join(".cargo"),
            home: home.clone(),
        };
        let rules = RuleSet::builtin(&dirs).unwrap();
        Fx {
            plat: LinuxPlatform::with_roots("/proc", "/sys", "/dev", dirs),
            _tmp: tmp,
            home,
            rules,
        }
    }

    fn put(fx: &Fx, rel: &str, data: &[u8]) -> PathBuf {
        let p = fx.home.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        p
    }

    fn gate(fx: &Fx) -> Gate<'_> {
        Gate::with_parts(
            &fx.plat,
            &fx.rules,
            &fx.home,
            vec![PathBuf::from("/"), fx.home.clone()],
            vec![],
            ActionLog::new(fx.home.join(".local/state/fagia/actions.jsonl")),
            false,
        )
        .unwrap()
    }

    fn sets(fx: &Fx) -> Vec<DupeSet> {
        let mut o = ScanOptions::new(&fx.home);
        o.record_min = 1;
        let s = scan(&fx.plat, None, &o, &Progress::default()).unwrap();
        dupes::find(&s, 1, dupes::MatchBy::NameAndContent)
    }

    fn opts(mode: DedupeMode) -> DedupeOptions {
        DedupeOptions {
            mode,
            keep_under: vec![],
            any_type: false,
        }
    }

    fn data() -> Vec<u8> {
        (0..200_000u32).map(|i| (i % 241) as u8).collect()
    }

    #[test]
    fn keeps_the_best_copy_and_skips_app_data() {
        let fx = fx();
        let d = data();
        put(&fx, "Documents/book.pdf", &d);
        put(&fx, "Downloads/book (1).pdf", &d);
        put(&fx, "backup/Documents/book.pdf", &d);
        let blob = data().into_iter().rev().collect::<Vec<u8>>();
        put(&fx, "app/.cache/blob", &blob);
        put(&fx, "other/.cache/blob", &blob);
        let g = gate(&fx);
        let p = plan(&g, &sets(&fx), &[], &opts(DedupeMode::Trash));
        let books = p
            .sets
            .iter()
            .find(|s| s.keep.iter().any(|k| k.ends_with("book.pdf")))
            .unwrap();
        assert_eq!(books.keep, vec![fx.home.join("Documents/book.pdf")]);
        assert_eq!(books.remove.len(), 2);
        let blobs = p
            .sets
            .iter()
            .find(|s| s.skipped.iter().any(|(p, _)| p.ends_with("blob")))
            .unwrap();
        assert!(
            blobs.remove.is_empty(),
            "hidden app data must not be removed"
        );
        assert!(
            blobs
                .skipped
                .iter()
                .all(|(_, why)| why.contains("hidden folder"))
        );
        // --keep-under wins over the default preference.
        let mut o = opts(DedupeMode::Trash);
        o.keep_under = vec![fx.home.join("backup")];
        let p = plan(&g, &sets(&fx), &[], &o);
        let books = p
            .sets
            .iter()
            .find(|s| s.keep.iter().any(|k| k.ends_with("book.pdf")))
            .unwrap();
        assert_eq!(books.keep, vec![fx.home.join("backup/Documents/book.pdf")]);
    }

    #[test]
    fn trash_mode_moves_extras_and_undo_restores() {
        let fx = fx();
        let d = data();
        let keep = put(&fx, "Documents/a.pdf", &d);
        let extra = put(&fx, "Downloads/a.pdf", &d);
        let g = gate(&fx);
        let p = plan(&g, &sets(&fx), &[], &opts(DedupeMode::Trash));
        let r = execute(&g, &p, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].outcome, ItemOutcome::Done);
        assert!(keep.exists() && !extra.exists());
        crate::actions::undo(g.log(), None).unwrap();
        assert!(extra.exists());
    }

    #[test]
    fn link_mode_keeps_every_path_working() {
        let fx = fx();
        let d = data();
        let keep = put(&fx, "Documents/a.pdf", &d);
        let extra = put(&fx, "Downloads/a.pdf", &d);
        let g = gate(&fx);
        let p = plan(&g, &sets(&fx), &[], &opts(DedupeMode::Link));
        let r = execute(&g, &p, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(r.results[0].outcome, ItemOutcome::Done, "{:?}", r.results);
        let (a, b) = (fs::metadata(&keep).unwrap(), fs::metadata(&extra).unwrap());
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
        assert_eq!(fs::read(&extra).unwrap(), d);
        assert!(
            fs::read_dir(extra.parent().unwrap()).unwrap().count() == 1,
            "temporary link left behind"
        );
    }

    #[test]
    fn changed_copy_is_skipped_at_action_time() {
        let fx = fx();
        let d = data();
        put(&fx, "Documents/a.pdf", &d);
        let extra = put(&fx, "Downloads/a.pdf", &d);
        let g = gate(&fx);
        let p = plan(&g, &sets(&fx), &[], &opts(DedupeMode::Trash));
        let mut changed = d.clone();
        changed[100_000] ^= 0xff;
        fs::write(&extra, &changed).unwrap();
        let r = execute(&g, &p, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(r.results[0].outcome, ItemOutcome::Skipped);
        assert!(r.results[0].detail.as_ref().unwrap().contains("differs"));
        assert!(extra.exists());
    }

    #[test]
    fn type_and_git_rules() {
        let root = Path::new("/h");
        assert!(own_file_refusal(Path::new("/h/Documents/x.pdf"), root, false).is_none());
        assert!(own_file_refusal(Path::new("/h/Music/x.flac"), root, false).is_none());
        assert!(
            own_file_refusal(Path::new("/h/data/page_0.dat"), root, false)
                .unwrap()
                .contains("not a document")
        );
        assert!(own_file_refusal(Path::new("/h/p/node_modules/x.pdf"), root, false).is_some());
        assert!(
            own_file_refusal(Path::new("/h/r/.git/objects/4e/09"), root, true)
                .unwrap()
                .contains("git")
        );
        assert!(own_file_refusal(Path::new("/h/data/page_0.dat"), root, true).is_none());
    }

    #[test]
    fn project_files_are_left_alone() {
        let fx = fx();
        let d = data();
        put(&fx, "Documents/book.pdf", &d);
        put(&fx, "backup/book.pdf", &d);
        put(&fx, "projs/app/package.json", b"{}");
        let in_project = put(&fx, "projs/app/pdfs/book.pdf", &d);
        let g = gate(&fx);
        let p = plan(&g, &sets(&fx), &[], &opts(DedupeMode::Trash));
        let s = &p.sets[0];
        assert!(
            s.skipped
                .iter()
                .any(|(p, why)| *p == in_project && why.contains("inside project app"))
        );
        assert_eq!(s.remove, vec![fx.home.join("backup/book.pdf")]);
        assert_eq!(
            s.keep,
            vec![fx.home.join("Documents/book.pdf")],
            "the user's own copy always stays"
        );
    }

    #[test]
    fn duplicate_folders_are_trashed_or_linked_whole() {
        for mode in [DedupeMode::Trash, DedupeMode::Link] {
            let fx = fx();
            let d = data();
            put(&fx, "Documents/books/a.pdf", &d);
            put(&fx, "Documents/books/sub/b.pdf", &d[..150_000]);
            put(&fx, "backup/Documents/books/a.pdf", &d);
            put(&fx, "backup/Documents/books/sub/b.pdf", &d[..150_000]);
            let mut o = crate::disk::ScanOptions::new(&fx.home);
            o.record_min = 1;
            let s =
                crate::disk::scan(&fx.plat, None, &o, &crate::disk::Progress::default()).unwrap();
            let dirs = crate::disk::dupe_dirs::find(&s, 1, true);
            let files = dupes::find(&s, 1, dupes::MatchBy::NameAndContent);
            let g = gate(&fx);
            let p = plan(&g, &files, &dirs, &opts(mode));
            // One folder set; the file sets inside it are not planned twice.
            assert_eq!(
                p.sets.iter().filter(|s| !s.remove.is_empty()).count(),
                1,
                "{mode:?}"
            );
            assert_eq!(p.sets[0].kind, CopyKind::Folder);
            assert_eq!(p.sets[0].keep, vec![fx.home.join("Documents")]);
            let r = execute(&g, &p, &AtomicBool::new(false), &mut |_| {});
            assert_eq!(r.results[0].outcome, ItemOutcome::Done, "{:?}", r.results);
            assert!(fx.home.join("Documents/books/sub/b.pdf").exists());
            match mode {
                DedupeMode::Trash => assert!(!fx.home.join("backup/Documents").exists()),
                DedupeMode::Link => {
                    let ino = |p: &str| fs::metadata(fx.home.join(p)).unwrap().ino();
                    assert_eq!(
                        ino("Documents/books/a.pdf"),
                        ino("backup/Documents/books/a.pdf")
                    );
                }
            }
        }
    }

    #[test]
    fn changed_folder_copy_is_skipped() {
        let fx = fx();
        let d = data();
        put(&fx, "Documents/books/a.pdf", &d);
        put(&fx, "backup/Documents/books/a.pdf", &d);
        put(&fx, "Documents/books/b.pdf", &d[..100_000]);
        put(&fx, "backup/Documents/books/b.pdf", &d[..100_000]);
        let mut o = crate::disk::ScanOptions::new(&fx.home);
        o.record_min = 1;
        let s = crate::disk::scan(&fx.plat, None, &o, &crate::disk::Progress::default()).unwrap();
        let dirs = crate::disk::dupe_dirs::find(&s, 1, true);
        let g = gate(&fx);
        let p = plan(&g, &[], &dirs, &opts(DedupeMode::Trash));
        put(
            &fx,
            "backup/Documents/books/new-notes.pdf",
            b"written after the dry run",
        );
        let r = execute(&g, &p, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(r.results[0].outcome, ItemOutcome::Skipped);
        assert!(
            fx.home
                .join("backup/Documents/books/new-notes.pdf")
                .exists()
        );
    }

    #[test]
    fn folders_holding_projects_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("backup/app/.git")).unwrap();
        fs::write(r.join("backup/app/main.rs"), "x").unwrap();
        assert!(
            own_folder_refusal(&r.join("backup"), r, false)
                .unwrap()
                .contains("code project")
        );
        assert!(
            own_folder_refusal(&r.join("backup/app/.git"), r, true)
                .unwrap()
                .contains("git")
        );
    }
}
