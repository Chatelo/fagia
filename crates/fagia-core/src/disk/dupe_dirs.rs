//! Duplicate folders: whole trees with the same files (relative paths,
//! sizes and content). One `~/backup/Documents` that mirrors
//! `~/Documents` is one finding instead of hundreds of file sets.
//!
//! Candidates share name (ignoring copy markers), size and file count;
//! a cheap listing of relative paths and sizes narrows them; a manifest
//! with every file's content hash confirms them. Only the outermost
//! duplicate folders are reported.

use super::dupes::{full, name_key};
use crate::disk::Scan;
use crate::disk::walk::{FLAG_COLLAPSED, FLAG_GIT, FLAG_UNREADABLE};
use rayon::prelude::*;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DirSet {
    /// Disk bytes of one copy.
    pub real: u64,
    pub files: u64,
    pub paths: Vec<PathBuf>,
    /// Manifest hash every copy had when found; checked again before acting.
    pub manifest: [u8; 32],
}

impl DirSet {
    pub fn wasted(&self) -> u64 {
        self.real * (self.paths.len() as u64 - 1)
    }
}

/// One entry of a folder listing: relative path, kind and size.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListEntry {
    pub rel: PathBuf,
    /// 'f' file, 'd' folder, 'l' symlink (its target text is the "content").
    pub kind: char,
    pub size: u64,
}

/// Every entry below `dir`, sorted, without following symlinks or leaving
/// the filesystem. `None` if anything cannot be read.
pub fn listing(dir: &Path) -> Option<Vec<ListEntry>> {
    fn walk(root: &Path, dir: &Path, dev: u64, out: &mut Vec<ListEntry>) -> Option<()> {
        for e in std::fs::read_dir(dir).ok()? {
            let e = e.ok()?;
            let m = e.metadata().ok()?;
            let rel = e.path().strip_prefix(root).ok()?.to_path_buf();
            if m.file_type().is_symlink() {
                let t = std::fs::read_link(e.path()).ok()?;
                out.push(ListEntry {
                    rel,
                    kind: 'l',
                    size: t.as_os_str().len() as u64,
                });
            } else if m.is_dir() {
                if m.dev() != dev {
                    return None;
                }
                out.push(ListEntry {
                    rel,
                    kind: 'd',
                    size: 0,
                });
                walk(root, &e.path(), dev, out)?;
            } else {
                out.push(ListEntry {
                    rel,
                    kind: 'f',
                    size: m.len(),
                });
            }
        }
        Some(())
    }
    let dev = std::fs::symlink_metadata(dir).ok()?.dev();
    let mut out = Vec::new();
    walk(dir, dir, dev, &mut out)?;
    out.sort();
    Some(out)
}

fn listing_hash(l: &[ListEntry]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for e in l {
        h.update(e.rel.as_os_str().as_encoded_bytes());
        h.update(&[0, e.kind as u8]);
        h.update(&e.size.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

/// Hash of every relative path, kind and file content below `dir`.
pub fn manifest(dir: &Path) -> Option<[u8; 32]> {
    let l = listing(dir)?;
    let mut h = blake3::Hasher::new();
    for e in &l {
        h.update(e.rel.as_os_str().as_encoded_bytes());
        h.update(&[0, e.kind as u8]);
        match e.kind {
            'f' => {
                h.update(full(&dir.join(&e.rel)).ok()?.as_bytes());
            }
            'l' => {
                h.update(
                    std::fs::read_link(dir.join(&e.rel))
                        .ok()?
                        .as_os_str()
                        .as_encoded_bytes(),
                );
            }
            _ => {}
        }
    }
    Some(*h.finalize().as_bytes())
}

/// Folders whose whole contents are identical.
pub fn find(scan: &Scan, min_size: u64, match_names: bool) -> Vec<DirSet> {
    let t = &scan.tree;
    let mut groups: HashMap<(u64, u64, String), Vec<u32>> = HashMap::new();
    for id in t.ids().skip(1) {
        let n = t.node(id);
        if n.flags & (FLAG_COLLAPSED | FLAG_GIT | FLAG_UNREADABLE) != 0
            || n.files < 2
            || n.real < min_size.max(1)
        {
            continue;
        }
        let name = if match_names {
            name_key(t.name(id))
        } else {
            String::new()
        };
        groups
            .entry((n.apparent, n.files, name))
            .or_default()
            .push(id);
    }
    let candidates: Vec<(u64, u64, Vec<PathBuf>)> = groups
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(_, ids)| {
            let n = t.node(ids[0]);
            // A folder and its own descendant cannot be copies.
            let mut paths: Vec<PathBuf> = ids.iter().map(|&i| t.path(i)).collect();
            paths.sort();
            paths.dedup();
            (n.real, n.files, paths)
        })
        .collect();

    let mut sets: Vec<DirSet> = candidates
        .into_par_iter()
        .flat_map_iter(|(real, files, paths)| {
            // Cheap pass: same relative paths and sizes.
            let mut by_listing: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
            for p in paths {
                if let Some(l) = listing(&p) {
                    by_listing.entry(listing_hash(&l)).or_default().push(p);
                }
            }
            // Confirming pass: same content.
            let mut out = Vec::new();
            for (_, ps) in by_listing.into_iter().filter(|(_, v)| v.len() > 1) {
                let mut by_manifest: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
                for p in ps {
                    if let Some(m) = manifest(&p) {
                        by_manifest.entry(m).or_default().push(p);
                    }
                }
                for (m, mut ps) in by_manifest.into_iter().filter(|(_, v)| v.len() > 1) {
                    ps.sort();
                    out.push(DirSet {
                        real,
                        files,
                        paths: ps,
                        manifest: m,
                    });
                }
            }
            out
        })
        .collect();

    // Keep only the outermost: a set whose every copy lies inside a copy
    // of a bigger set is already covered by it.
    sets.sort_by(|a, b| b.real.cmp(&a.real).then(a.paths.cmp(&b.paths)));
    let mut kept: Vec<DirSet> = Vec::new();
    for s in sets {
        let covered = s.paths.iter().all(|p| {
            kept.iter()
                .any(|k| k.paths.iter().any(|kp| p != kp && p.starts_with(kp)))
        });
        if !covered {
            kept.push(s);
        }
    }
    kept.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.paths.cmp(&b.paths)));
    kept
}

/// True if `path` lies inside (or is) any copy of any of these folders.
pub fn inside_any(path: &Path, sets: &[DirSet]) -> bool {
    sets.iter()
        .any(|s| s.paths.iter().any(|d| path.starts_with(d)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::{Progress, ScanOptions, scan};
    use crate::platform::linux::LinuxPlatform;
    use std::fs;

    fn write(p: &Path, data: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    #[test]
    fn finds_outermost_identical_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 233) as u8).collect();
        for base in ["Documents/books", "backup/Documents/books"] {
            write(&r.join(base).join("a.pdf"), &big);
            write(&r.join(base).join("sub/b.pdf"), &big[..200_000]);
            write(&r.join(base).join("sub/c.txt"), b"notes");
        }
        // Same names and sizes, one byte different: not a copy.
        write(&r.join("other/books/a.pdf"), &big);
        let mut changed = big[..200_000].to_vec();
        changed[5] ^= 1;
        write(&r.join("other/books/sub/b.pdf"), &changed);
        write(&r.join("other/books/sub/c.txt"), b"notes");

        let s = scan(
            &LinuxPlatform::new(),
            None,
            &ScanOptions::new(r),
            &Progress::default(),
        )
        .unwrap();
        let sets = find(&s, 1, true);
        assert_eq!(sets.len(), 1, "{sets:?}");
        let root = r.canonicalize().unwrap();
        assert_eq!(
            sets[0].paths,
            vec![root.join("Documents"), root.join("backup/Documents")],
            "outermost only (Documents holds nothing but books), and the near-copy is excluded"
        );
        assert_eq!(sets[0].files, 3);
        assert!(inside_any(
            &root.join("backup/Documents/books/sub/b.pdf"),
            &sets
        ));
        assert_eq!(manifest(&root.join("Documents")), Some(sets[0].manifest));
    }
}
