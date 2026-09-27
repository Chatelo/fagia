//! Media detection: extension first, file header for large files whose
//! extension says nothing (renamed or extension-less media).

use schemars::JsonSchema;
use serde::Serialize;
use std::ffi::OsStr;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Video,
    Audio,
    Image,
    Raw,
    Design,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Image => "Images",
            Self::Raw => "Camera raw",
            Self::Design => "Design files",
        }
    }
}

const VIDEO: &[&str] = &[
    "mp4", "mkv", "mov", "avi", "webm", "m4v", "wmv", "flv", "mpg", "mpeg", "m2ts", "mts", "3gp",
];
const AUDIO: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "wma", "aiff", "alac",
];
const IMAGE: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "bmp", "tif", "tiff", "svg",
];
const RAW: &[&str] = &[
    "cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2", "pef", "srw",
];
const DESIGN: &[&str] = &[
    "psd", "xcf", "kra", "ai", "sketch", "fig", "blend", "afdesign", "afphoto",
];

pub fn kind_of(name: &OsStr) -> Option<MediaKind> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    let e = ext.as_str();
    [
        (VIDEO, MediaKind::Video),
        (AUDIO, MediaKind::Audio),
        (IMAGE, MediaKind::Image),
        (RAW, MediaKind::Raw),
        (DESIGN, MediaKind::Design),
    ]
    .into_iter()
    .find(|(list, _)| list.contains(&e))
    .map(|(_, k)| k)
}

/// `.ts` is both MPEG transport stream video and TypeScript source; such
/// files count as video only when their header says so.
const AMBIGUOUS_VIDEO: &[&str] = &["ts"];

fn ext_lower(name: &OsStr) -> Option<String> {
    Some(Path::new(name).extension()?.to_str()?.to_ascii_lowercase())
}

/// Worth recording during a scan: a media extension, or an ambiguous one
/// that needs its header checked.
pub fn is_candidate(name: &OsStr) -> bool {
    kind_of(name).is_some()
        || ext_lower(name).is_some_and(|e| AMBIGUOUS_VIDEO.contains(&e.as_str()))
}

/// Media kind of a file, reading the header only for ambiguous
/// extensions. The flag is true when the header decided.
pub fn kind_of_file(path: &Path, name: &OsStr, placeholder: bool) -> Option<(MediaKind, bool)> {
    if let Some(k) = kind_of(name) {
        return Some((k, false));
    }
    let ext = ext_lower(name)?;
    if placeholder || !AMBIGUOUS_VIDEO.contains(&ext.as_str()) {
        return None;
    }
    is_mpeg_ts(path).then_some((MediaKind::Video, true))
}

/// MPEG-TS packets are 188 bytes, each starting with the sync byte 0x47.
fn is_mpeg_ts(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 377];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok_and(|()| buf[0] == 0x47 && buf[188] == 0x47 && buf[376] == 0x47)
}

/// Sniffs the file header. Only used for files without a known extension,
/// and never for cloud placeholders (reading those downloads them).
pub fn sniff(path: &Path) -> Option<MediaKind> {
    let kind = infer::get_from_path(path).ok()??;
    match kind.matcher_type() {
        infer::MatcherType::Video => Some(MediaKind::Video),
        infer::MatcherType::Audio => Some(MediaKind::Audio),
        infer::MatcherType::Image => Some(MediaKind::Image),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_case_insensitive() {
        assert_eq!(kind_of(OsStr::new("a.MKV")), Some(MediaKind::Video));
        assert_eq!(kind_of(OsStr::new("b.nef")), Some(MediaKind::Raw));
        assert_eq!(kind_of(OsStr::new("c.txt")), None);
        assert_eq!(kind_of(OsStr::new("noext")), None);
        assert!(is_candidate(OsStr::new("main.ts")));
        assert_eq!(kind_of(OsStr::new("main.ts")), None);
    }

    #[test]
    fn ts_counts_as_video_only_with_mpeg_ts_header() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("main.ts");
        std::fs::write(&code, "export const x = 1;\n".repeat(40)).unwrap();
        assert_eq!(kind_of_file(&code, OsStr::new("main.ts"), false), None);
        let video = dir.path().join("clip.ts");
        let mut data = vec![0u8; 188 * 4];
        for i in 0..4 {
            data[i * 188] = 0x47;
        }
        std::fs::write(&video, data).unwrap();
        assert_eq!(
            kind_of_file(&video, OsStr::new("clip.ts"), false),
            Some((MediaKind::Video, true))
        );
    }

    #[test]
    fn sniffs_renamed_png() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("picture.dat");
        let png = [
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        std::fs::write(&p, png).unwrap();
        assert_eq!(sniff(&p), Some(MediaKind::Image));
    }
}

/// Per-type totals, largest files and the folders holding the most.
pub mod analyze {
    use super::{MediaKind, kind_of_file, sniff};
    use crate::disk::Scan;
    use crate::paths::JsonPath;
    use crate::report::{MediaFile, MediaFolder, MediaGroup, MediaReport, ScanInfo};
    use rayon::prelude::*;
    use std::collections::{BTreeMap, HashMap};
    use std::path::{Path, PathBuf};

    /// Files at least this large without a known extension get their header
    /// read, to catch renamed or extension-less media.
    const SNIFF_MIN: u64 = 1 << 20;

    pub fn media(scan: &Scan, limit: usize, min_size: u64) -> MediaReport {
        let classified: Vec<(MediaKind, bool, usize)> = scan
            .files
            .par_iter()
            .enumerate()
            .filter(|(_, f)| f.counted && f.stat.real >= min_size)
            .filter_map(|(i, f)| {
                match kind_of_file(&scan.file_path(f), &f.name, f.stat.placeholder) {
                    Some((k, sniffed)) => Some((k, sniffed, i)),
                    None if f.stat.apparent >= SNIFF_MIN
                        && !f.stat.placeholder
                        && Path::new(&*f.name).extension().is_none() =>
                    {
                        sniff(&scan.file_path(f)).map(|k| (k, true, i))
                    }
                    None => None,
                }
            })
            .collect();
        let mut by_kind: BTreeMap<MediaKind, Vec<(bool, usize)>> = BTreeMap::new();
        for (k, sniffed, i) in classified {
            by_kind.entry(k).or_default().push((sniffed, i));
        }
        let mut groups: Vec<MediaGroup> = by_kind
            .into_iter()
            .map(|(kind, mut items)| {
                items.sort_by_key(|(_, i)| std::cmp::Reverse(scan.files[*i].stat.real));
                let mut folders: HashMap<u32, (u64, u64)> = HashMap::new();
                for (_, i) in &items {
                    let f = &scan.files[*i];
                    let e = folders.entry(f.dir).or_default();
                    e.0 += f.stat.real;
                    e.1 += 1;
                }
                let mut folders: Vec<(PathBuf, u64, u64)> = folders
                    .into_iter()
                    .map(|(d, (r, c))| (scan.tree.path(d), r, c))
                    .collect();
                folders.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                folders.truncate(limit);
                MediaGroup {
                    kind,
                    label: kind.label().to_string(),
                    count: items.len() as u64,
                    real: items.iter().map(|(_, i)| scan.files[*i].stat.real).sum(),
                    largest: items
                        .iter()
                        .take(limit)
                        .map(|(sniffed, i)| {
                            let f = &scan.files[*i];
                            let path = scan.file_path(f);
                            let (duration_secs, resolution) = if kind == MediaKind::Video {
                                probe(&path)
                            } else {
                                (None, None)
                            };
                            MediaFile {
                                path: JsonPath::new(&path),
                                real: f.stat.real,
                                modified: f.stat.mtime,
                                sniffed: *sniffed,
                                duration_secs,
                                resolution,
                            }
                        })
                        .collect(),
                    folders: folders
                        .into_iter()
                        .map(|(p, real, count)| MediaFolder {
                            path: JsonPath::new(&p),
                            real,
                            count,
                        })
                        .collect(),
                }
            })
            .collect();
        groups.sort_by_key(|g| std::cmp::Reverse(g.real));
        MediaReport {
            scan: ScanInfo::from_scan(scan),
            total: groups.iter().map(|g| g.real).sum(),
            groups,
        }
    }

    /// Duration and resolution via `ffprobe`, when it is installed.
    fn probe(path: &Path) -> (Option<f64>, Option<String>) {
        if !crate::providers::on_path("ffprobe") {
            return (None, None);
        }
        let mut cmd = std::process::Command::new("ffprobe");
        cmd.args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height:format=duration",
            "-of",
            "json",
        ])
        .arg(path);
        let Ok(out) =
            crate::providers::run_with_timeout(&mut cmd, std::time::Duration::from_secs(5))
        else {
            return (None, None);
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
        let duration = v["format"]["duration"]
            .as_str()
            .and_then(|d| d.parse().ok());
        let res = match (
            v["streams"][0]["width"].as_u64(),
            v["streams"][0]["height"].as_u64(),
        ) {
            (Some(w), Some(h)) => Some(format!("{w}x{h}")),
            _ => None,
        };
        (duration, res)
    }
}
