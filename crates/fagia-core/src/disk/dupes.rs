//! Duplicate files: same size, then same head and tail, then same full
//! blake3 hash. Hard links to one inode are one file, not duplicates.

use crate::disk::{FileRec, Scan};
use rayon::prelude::*;
use regex::Regex;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const EDGE: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub struct DupeSet {
    pub size: u64,
    pub real: u64,
    pub paths: Vec<PathBuf>,
}

impl DupeSet {
    /// Space freed by keeping one copy.
    pub fn wasted(&self) -> u64 {
        self.real * (self.paths.len() as u64 - 1)
    }
}

fn edges(path: &Path, size: u64) -> std::io::Result<blake3::Hash> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; EDGE.min(size) as usize];
    f.read_exact(&mut buf)?;
    h.update(&buf);
    if size > EDGE {
        let tail = EDGE.min(size - EDGE);
        f.seek(SeekFrom::End(-(tail as i64)))?;
        buf.resize(tail as usize, 0);
        f.read_exact(&mut buf)?;
        h.update(&buf);
    }
    Ok(h.finalize())
}

/// Full blake3 hash of a file's content.
pub fn full(path: &Path) -> std::io::Result<blake3::Hash> {
    let mut h = blake3::Hasher::new();
    h.update_reader(File::open(path)?)?;
    Ok(h.finalize())
}

fn group_by<K, F>(items: Vec<(PathBuf, u64)>, key: F) -> Vec<Vec<(PathBuf, u64)>>
where
    K: std::hash::Hash + Eq + Send,
    F: Fn(&Path, u64) -> Option<K> + Sync,
{
    let keyed: Vec<(K, (PathBuf, u64))> = items
        .into_par_iter()
        .filter_map(|(p, real)| key(&p, real).map(|k| (k, (p, real))))
        .collect();
    let mut map: HashMap<K, Vec<(PathBuf, u64)>> = HashMap::new();
    for (k, v) in keyed {
        map.entry(k).or_default().push(v);
    }
    map.into_values().filter(|g| g.len() > 1).collect()
}

/// A file name with copy markers removed, for comparing names:
/// `Book (1).PDF`, `book - Copy.pdf`, `Copy of book.pdf` and
/// `book (another copy).pdf` all become `book.pdf`.
pub fn name_key(name: &OsStr) -> String {
    static COPY_OF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^copy of\s+").expect("valid"));
    static COPY_SUFFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(^|[\s_-]+)copy(\s*\(?\d+\)?)?$").expect("valid"));
    static PAREN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\s*[\(\[](\d+|copy|another copy|\d+(st|nd|rd|th) copy)[\)\]]$").expect("valid")
    });
    let lower = name.to_string_lossy().to_lowercase();
    let (mut stem, mut ext) = match lower.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (lower.clone(), String::new()),
    };
    if let Some(s) = stem.strip_suffix(".tar") {
        ext = format!(".tar{ext}");
        stem = s.to_string();
    }
    loop {
        let before = stem.clone();
        stem = COPY_OF.replace(&stem, "").into_owned();
        stem = PAREN.replace(&stem, "").into_owned();
        stem = COPY_SUFFIX.replace(&stem, "").into_owned();
        stem = stem.trim_end_matches([' ', '_', '-']).trim().to_string();
        if stem == before {
            break;
        }
    }
    let stem = stem.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{stem}{ext}")
}

/// A looser name key: also drops trailing tags in brackets such as
/// `(z-library.sk, 1lib.sk)` or `[1080p]`, and every character that is not
/// a letter or digit, so `What to Eat_ (z-lib.sk).pdf` and
/// `What-to-eat.pdf` agree.
pub fn loose_name_key(name: &OsStr) -> String {
    static TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s*[\(\[\{][^\(\)\[\]\{\}]*[\)\]\}]\s*$").expect("valid"));
    let strict = name_key(name);
    let (mut stem, ext) = match strict.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), e.to_string()),
        _ => (strict.clone(), String::new()),
    };
    loop {
        let next = TAG
            .replace(&stem, "")
            .trim_end_matches(['_', '-', ' ', '.'])
            .to_string();
        if next == stem || next.is_empty() {
            break;
        }
        stem = next;
    }
    let alnum: String = stem.chars().filter(|c| c.is_alphanumeric()).collect();
    format!("{alnum}.{ext}")
}

/// How duplicates are recognised. Content is always compared byte for
/// byte (size, then head and tail, then a full hash).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MatchBy {
    /// Same content and the same name once copy markers are removed.
    NameAndContent,
    /// Same content and a loosely equal name (tags and punctuation ignored).
    LooseNameAndContent,
    /// Same content, whatever the names.
    Content,
}

/// Finds duplicate sets among recorded files of at least `min_size` bytes.
pub fn find(scan: &Scan, min_size: u64, by: MatchBy) -> Vec<DupeSet> {
    // One representative per inode: hard links are not duplicates.
    let mut by_size: HashMap<(u64, String), Vec<&FileRec>> = HashMap::new();
    for f in scan
        .files
        .iter()
        .filter(|f| f.counted && !f.stat.placeholder && f.stat.apparent >= min_size.max(1))
    {
        let name = match by {
            MatchBy::NameAndContent => name_key(&f.name),
            MatchBy::LooseNameAndContent => loose_name_key(&f.name),
            MatchBy::Content => String::new(),
        };
        by_size.entry((f.stat.apparent, name)).or_default().push(f);
    }
    let mut sets = Vec::new();
    for ((size, _), group) in by_size.into_iter().filter(|(_, g)| g.len() > 1) {
        let items: Vec<(PathBuf, u64)> = group
            .iter()
            .map(|f| (scan.file_path(f), f.stat.real))
            .collect();
        for g in group_by(items, |p, _| edges(p, size).ok()) {
            let g = if size > 2 * EDGE {
                group_by(g, |p, _| full(p).ok())
            } else {
                // Head and tail already cover the whole file.
                vec![g]
            };
            for set in g {
                let real = set.iter().map(|(_, r)| *r).max().unwrap_or(0);
                let mut paths: Vec<PathBuf> = set.into_iter().map(|(p, _)| p).collect();
                paths.sort();
                sets.push(DupeSet { size, real, paths });
            }
        }
    }
    sets.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.paths.cmp(&b.paths)));
    sets
}

#[cfg(test)]
mod tests {
    use crate::disk::{Progress, ScanOptions, scan};
    use crate::platform::linux::LinuxPlatform;
    use std::fs;

    #[test]
    fn finds_copies_but_not_hard_links_or_near_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(r.join("a.bin"), &data).unwrap();
        fs::write(r.join("b.bin"), &data).unwrap();
        fs::create_dir(r.join("sub")).unwrap();
        fs::write(r.join("sub/c.bin"), &data).unwrap();
        // Same size, same edges, one byte different in the middle.
        let mut near = data.clone();
        near[150_000] ^= 1;
        fs::write(r.join("near.bin"), &near).unwrap();
        fs::hard_link(r.join("a.bin"), r.join("a-link.bin")).unwrap();
        let mut opts = ScanOptions::new(r);
        opts.record_min = 1;
        let s = scan(&LinuxPlatform::new(), None, &opts, &Progress::default()).unwrap();
        let sets = super::find(&s, 1, super::MatchBy::Content);
        assert_eq!(sets.len(), 1, "{sets:?}");
        let names: Vec<_> = sets[0]
            .paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(!names.contains(&"near.bin".to_string()));
        assert_eq!(sets[0].wasted(), 2 * sets[0].real);
    }

    #[test]
    fn copy_markers_are_removed_from_names() {
        use super::name_key;
        use std::ffi::OsStr;
        let k = |s: &str| name_key(OsStr::new(s));
        for n in [
            "book.pdf",
            "Book (1).PDF",
            "book(2).pdf",
            "book - Copy.pdf",
            "book - Copy (3).pdf",
            "book_copy.pdf",
            "Copy of book.pdf",
            "book (another copy).pdf",
            "book (3rd copy).pdf",
            "book [1].pdf",
            "book  .pdf",
        ] {
            assert_eq!(k(n), "book.pdf", "{n}");
        }
        assert_eq!(k("photocopy.pdf"), "photocopy.pdf");
        assert_eq!(k("report-1.pdf"), "report-1.pdf");
        assert_ne!(k("book.pdf"), k("book.epub"));
        assert_eq!(k("a (1).tar.gz"), "a.tar.gz");
        assert_eq!(k("README"), "readme");
    }

    #[test]
    fn names_must_match_unless_content_only() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 249) as u8).collect();
        for n in ["a/book.pdf", "b/book (1).pdf", "c/unrelated.pdf"] {
            fs::create_dir_all(r.join(n).parent().unwrap()).unwrap();
            fs::write(r.join(n), &data).unwrap();
        }
        let mut opts = ScanOptions::new(r);
        opts.record_min = 1;
        let s = scan(&LinuxPlatform::new(), None, &opts, &Progress::default()).unwrap();
        let named = super::find(&s, 1, super::MatchBy::NameAndContent);
        assert_eq!(named.len(), 1);
        assert_eq!(
            named[0].paths.len(),
            2,
            "unrelated.pdf is not a copy of book.pdf"
        );
        let any = super::find(&s, 1, super::MatchBy::Content);
        assert_eq!(any[0].paths.len(), 3);
    }

    #[test]
    fn loose_names_ignore_tags_and_punctuation() {
        use super::loose_name_key;
        use std::ffi::OsStr;
        let k = |s: &str| loose_name_key(OsStr::new(s));
        assert_eq!(
            k("What to Eat During Cancer T_ (z-library.sk, 1lib.sk, z-lib.sk).pdf"),
            k("What to Eat During Cancer T.pdf")
        );
        assert_eq!(k("Movie.Name.2020 [1080p].mkv"), k("movie name 2020.mkv"));
        assert_eq!(k("report (1).pdf"), k("Report.pdf"));
        assert_ne!(k("report.pdf"), k("report.docx"));
        assert_ne!(k("book one.pdf"), k("book two.pdf"));
    }
}
