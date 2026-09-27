//! Near-duplicates: files that are not byte-identical but are probably the
//! same thing. These are reported, never removed automatically: without
//! identical bytes there is no guarantee nothing is lost.
//!
//! - text: same text once whitespace, line endings and a BOM are ignored
//! - image: two perceptual hashes (gradient and average) agree, and the
//!   aspect ratio matches: the same picture resized or recompressed
//! - media: the same name (ignoring copy markers) and duration, and the
//!   same title when both have one; with `fpcalc` installed, also audio
//!   fingerprints, which find one recording under any name
//! - content: the documents' words compared (PDF via `pdftotext`, office
//!   and EPUB files from their XML), for edited or re-saved versions
//!
//! Groups are built around a seed (the largest file) and every member must
//! match the seed itself, so similarity never chains from one file to the
//! next and on to something unrelated.

use crate::disk::dupes::name_key;
use crate::disk::scope::Scope;
use crate::disk::{FileRec, Scan, media};
use rayon::prelude::*;
use regex::Regex;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SimilarKind {
    Text,
    Image,
    Media,
    Content,
}

impl SimilarKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Same text",
            Self::Image => "Similar images",
            Self::Media => "Same recording",
            Self::Content => "Similar documents",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SimilarFile {
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SimilarGroup {
    pub kind: SimilarKind,
    /// 0 to 1; the weakest match between the seed and a member.
    pub similarity: f64,
    pub note: String,
    pub files: Vec<SimilarFile>,
}

#[derive(Debug, Clone)]
pub struct SimilarOptions {
    pub min_size: u64,
    /// Largest Hamming distance (of 64 bits) for both image hashes.
    pub image_distance: u32,
    /// Smallest estimated overlap of words for similar documents.
    pub content_threshold: f64,
    /// Also look inside hidden, program and project folders.
    pub any_type: bool,
}

impl Default for SimilarOptions {
    fn default() -> Self {
        Self {
            min_size: 4096,
            image_distance: 5,
            content_threshold: 0.8,
            any_type: false,
        }
    }
}

pub const TEXT_EXT: &[&str] = &[
    "txt", "md", "markdown", "rst", "csv", "tsv", "json", "xml", "html", "htm", "yaml", "yml",
    "toml", "ini", "cfg", "conf", "log", "srt", "vtt", "tex",
];
const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];
const DOC_EXT: &[&str] = &["pdf", "docx", "odt", "ods", "odp", "pptx", "xlsx", "epub"];

fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Groups around seeds: items in `order` (strongest first) each start a
/// group unless already taken; a group holds the seed's untaken matches.
fn star_groups(
    n: usize,
    order: &[usize],
    neighbours: &HashMap<usize, Vec<(usize, f64)>>,
) -> Vec<(Vec<usize>, f64)> {
    let mut taken = vec![false; n];
    let mut out = Vec::new();
    for &seed in order {
        if taken[seed] {
            continue;
        }
        let members: Vec<(usize, f64)> = neighbours
            .get(&seed)
            .into_iter()
            .flatten()
            .filter(|(j, _)| !taken[*j])
            .copied()
            .collect();
        if members.is_empty() {
            continue;
        }
        taken[seed] = true;
        let mut g = vec![seed];
        let mut low = 1.0f64;
        for (j, s) in members {
            taken[j] = true;
            g.push(j);
            low = low.min(s);
        }
        out.push((g, low));
    }
    out
}

fn add_edge(nb: &mut HashMap<usize, Vec<(usize, f64)>>, a: usize, b: usize, s: f64) {
    nb.entry(a).or_default().push((b, s));
    nb.entry(b).or_default().push((a, s));
}

struct Item {
    path: PathBuf,
    real: u64,
    detail: Option<String>,
}

fn candidates<'a>(
    scan: &'a Scan,
    opts: &SimilarOptions,
    exts: &[&str],
    max_size: u64,
) -> Vec<(PathBuf, &'a FileRec)> {
    let mut scope = Scope::new(scan.tree.root_path(), opts.any_type);
    scan.files
        .iter()
        .filter(|f| {
            f.counted
                && !f.stat.placeholder
                && f.stat.apparent >= opts.min_size
                && f.stat.apparent <= max_size
        })
        .filter_map(|f| {
            let path = scan.file_path(f);
            (exts.is_empty() || exts.contains(&ext_of(&path).as_str())).then_some((path, f))
        })
        .filter(|(p, _)| scope.exclusion(p).is_none())
        .collect()
}

/// Turns groups into reports, dropping groups whose files are all
/// byte-identical (those are exact duplicates, reported elsewhere).
fn finish(
    kind: SimilarKind,
    items: &[Item],
    groups: Vec<(Vec<usize>, f64)>,
    identical: &HashMap<PathBuf, usize>,
    note: &str,
) -> Vec<SimilarGroup> {
    let mut out: Vec<SimilarGroup> = groups
        .into_iter()
        .filter(|(g, _)| {
            let ids: HashSet<Option<&usize>> =
                g.iter().map(|&i| identical.get(&items[i].path)).collect();
            !(ids.len() == 1 && ids.iter().next().is_some_and(Option::is_some))
        })
        .map(|(g, sim)| SimilarGroup {
            kind,
            similarity: (sim * 1000.0).round() / 1000.0,
            note: note.to_string(),
            files: g
                .iter()
                .map(|&i| SimilarFile {
                    path: items[i].path.to_string_lossy().into_owned(),
                    size: items[i].real,
                    detail: items[i].detail.clone(),
                })
                .collect(),
        })
        .collect();
    out.sort_by_key(|g| std::cmp::Reverse(g.files.iter().map(|f| f.size).sum::<u64>()));
    out
}

// ---- text ----

/// Hash of the text with a BOM, trailing whitespace, line-ending style and
/// trailing blank lines removed. `None` for files that are not UTF-8.
pub fn normalized_text_hash(bytes: &[u8]) -> Option<blake3::Hash> {
    let s = std::str::from_utf8(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)).ok()?;
    let mut h = blake3::Hasher::new();
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);
    for l in &lines[..end] {
        h.update(l.as_bytes());
        h.update(b"\n");
    }
    Some(h.finalize())
}

fn text(
    scan: &Scan,
    opts: &SimilarOptions,
    identical: &HashMap<PathBuf, usize>,
    done: &AtomicU64,
) -> Vec<SimilarGroup> {
    let hashed: Vec<(Item, blake3::Hash)> = candidates(scan, opts, TEXT_EXT, 32 << 20)
        .into_par_iter()
        .filter_map(|(path, f)| {
            done.fetch_add(1, Ordering::Relaxed);
            let h = normalized_text_hash(&std::fs::read(&path).ok()?)?;
            Some((
                Item {
                    path,
                    real: f.stat.real,
                    detail: None,
                },
                h,
            ))
        })
        .collect();
    let mut by: HashMap<blake3::Hash, Vec<usize>> = HashMap::new();
    for (i, (_, h)) in hashed.iter().enumerate() {
        by.entry(*h).or_default().push(i);
    }
    let groups = by
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|g| (g, 1.0))
        .collect();
    let items: Vec<Item> = hashed.into_iter().map(|(i, _)| i).collect();
    finish(
        SimilarKind::Text,
        &items,
        groups,
        identical,
        "same text; differs only in whitespace, line endings or a BOM",
    )
}

// ---- images ----

/// Decodes a picture small: JPEGs at 1/2 to 1/8 scale straight from the
/// DCT (much faster than full size), everything else in full. Returns the
/// image and the original width and height.
fn decode_small(path: &Path) -> Option<(image::DynamicImage, u32, u32)> {
    if matches!(ext_of(path).as_str(), "jpg" | "jpeg") {
        let file = std::fs::File::open(path).ok()?;
        let mut dec = jpeg_decoder::Decoder::new(std::io::BufReader::new(file));
        dec.read_info().ok()?;
        let full = dec.info()?;
        dec.scale(64, 64).ok()?;
        let pixels = dec.decode().ok()?;
        let info = dec.info()?;
        let (w, h) = (u32::from(info.width), u32::from(info.height));
        let img = match info.pixel_format {
            jpeg_decoder::PixelFormat::L8 => {
                image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(w, h, pixels)?)
            }
            jpeg_decoder::PixelFormat::RGB24 => {
                image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, pixels)?)
            }
            jpeg_decoder::PixelFormat::CMYK32 => {
                image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(
                    w,
                    h,
                    pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|p| 255 - p[3])
                        .collect(),
                )?)
            }
            jpeg_decoder::PixelFormat::L16 => {
                image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(
                    w,
                    h,
                    pixels.as_chunks::<2>().0.iter().map(|p| p[0]).collect(),
                )?)
            }
        };
        return Some((img, u32::from(full.width), u32::from(full.height)));
    }
    let img = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let (w, h) = (img.width(), img.height());
    Some((img, w, h))
}

/// 64-bit gradient hash: brightness differences in a 9×8 thumbnail.
pub fn dhash(img: &image::DynamicImage) -> u64 {
    let small = img.thumbnail_exact(9, 8).to_luma8();
    let mut bits = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            bits = (bits << 1) | u64::from(small.get_pixel(x, y)[0] < small.get_pixel(x + 1, y)[0]);
        }
    }
    bits
}

/// 64-bit average hash: which pixels of an 8×8 thumbnail are brighter than
/// the mean. Catches different mistakes than the gradient hash.
pub fn ahash(img: &image::DynamicImage) -> u64 {
    let small = img.thumbnail_exact(8, 8).to_luma8();
    let mean = small.pixels().map(|p| u32::from(p[0])).sum::<u32>() / 64;
    small.pixels().fold(0u64, |bits, p| {
        (bits << 1) | u64::from(u32::from(p[0]) > mean)
    })
}

struct Pic {
    d: u64,
    a: u64,
    w: u32,
    h: u32,
}

fn pictures_match(x: &Pic, y: &Pic, max: u32) -> Option<f64> {
    let (rx, ry) = (
        x.w as f64 / x.h.max(1) as f64,
        y.w as f64 / y.h.max(1) as f64,
    );
    if rx.min(ry) / rx.max(ry) < 0.9 {
        return None;
    }
    let (dd, da) = ((x.d ^ y.d).count_ones(), (x.a ^ y.a).count_ones());
    (dd <= max && da <= max).then(|| 1.0 - f64::from(dd.max(da)) / 64.0)
}

fn images(
    scan: &Scan,
    opts: &SimilarOptions,
    identical: &HashMap<PathBuf, usize>,
    done: &AtomicU64,
) -> Vec<SimilarGroup> {
    let hashed: Vec<(Item, Pic)> = candidates(scan, opts, IMAGE_EXT, 256 << 20)
        .into_par_iter()
        .filter_map(|(path, f)| {
            done.fetch_add(1, Ordering::Relaxed);
            let (img, w, h) = decode_small(&path)?;
            let pic = Pic {
                d: dhash(&img),
                a: ahash(&img),
                w,
                h,
            };
            Some((
                Item {
                    path,
                    real: f.stat.real,
                    detail: Some(format!("{w}×{h}")),
                },
                pic,
            ))
        })
        .collect();
    // Hashes within distance d share one of d+1 equal slices; 8 slices of
    // the gradient hash cover distances up to 7.
    let mut nb: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for band in 0..8u32 {
        let mut buckets: HashMap<u8, Vec<usize>> = HashMap::new();
        for (i, (_, p)) in hashed.iter().enumerate() {
            buckets
                .entry((p.d >> (band * 8)) as u8)
                .or_default()
                .push(i);
        }
        for b in buckets.values().filter(|b| b.len() > 1 && b.len() < 5000) {
            for (x, &i) in b.iter().enumerate() {
                for &j in &b[x + 1..] {
                    if seen.insert((i.min(j), i.max(j)))
                        && let Some(s) =
                            pictures_match(&hashed[i].1, &hashed[j].1, opts.image_distance)
                    {
                        add_edge(&mut nb, i, j, s);
                    }
                }
            }
        }
    }
    // Seeds: the largest pictures (usually the originals) first.
    let mut order: Vec<usize> = (0..hashed.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(u64::from(hashed[i].1.w) * u64::from(hashed[i].1.h)));
    let groups = star_groups(hashed.len(), &order, &nb);
    let items: Vec<Item> = hashed.into_iter().map(|(i, _)| i).collect();
    finish(
        SimilarKind::Image,
        &items,
        groups,
        identical,
        "look alike: the same picture resized or recompressed, or shots taken seconds apart",
    )
}

// ---- media ----

struct Probe {
    duration: f64,
    title: Option<String>,
    detail: String,
    video: bool,
}

fn probe(path: &Path) -> Option<Probe> {
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration,bit_rate:format_tags=title,artist:stream=codec_type,codec_name,width,height:stream_disposition=attached_pic",
        "-of",
        "json",
    ])
    .arg(path);
    let out =
        crate::providers::run_with_timeout(&mut cmd, std::time::Duration::from_secs(10)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let duration: f64 = v["format"]["duration"].as_str()?.parse().ok()?;
    let streams = v["streams"].as_array()?;
    // Cover art in audio files is a still "video" stream; it is not video.
    let video = streams.iter().find(|s| {
        s["codec_type"] == "video"
            && s["width"].as_u64().is_some_and(|w| w > 0)
            && s["disposition"]["attached_pic"].as_u64() != Some(1)
    });
    let audio = streams.iter().find(|s| s["codec_type"] == "audio");
    let tags = &v["format"]["tags"];
    let tag = |k: &str| {
        tags[k]
            .as_str()
            .or_else(|| tags[k.to_uppercase()].as_str())
            .map(str::to_string)
    };
    let title = match (tag("title"), tag("artist")) {
        (Some(t), Some(a)) => Some(format!("{a} - {t}")),
        (t, _) => t,
    };
    let kbps = v["format"]["bit_rate"]
        .as_str()
        .and_then(|b| b.parse::<u64>().ok())
        .map(|b| b / 1000);
    let mins = format!("{}:{:02}", (duration / 60.0) as u64, duration as u64 % 60);
    let detail = match video {
        Some(s) => format!(
            "{mins} · {}×{} {}",
            s["width"].as_u64().unwrap_or(0),
            s["height"].as_u64().unwrap_or(0),
            s["codec_name"].as_str().unwrap_or("?")
        ),
        None => format!(
            "{mins} · {}{}",
            audio.and_then(|a| a["codec_name"].as_str()).unwrap_or("?"),
            kbps.map(|k| format!(" {k} kb/s")).unwrap_or_default()
        ),
    };
    Some(Probe {
        duration,
        title,
        detail,
        video: video.is_some(),
    })
}

/// Raw Chromaprint fingerprint (one 32-bit word per ~0.12 s of audio).
fn fingerprint(path: &Path) -> Option<Vec<u32>> {
    let mut cmd = std::process::Command::new("fpcalc");
    cmd.args(["-raw", "-json", "-length", "120"]).arg(path);
    let out =
        crate::providers::run_with_timeout(&mut cmd, std::time::Duration::from_secs(30)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let fp: Vec<u32> = v["fingerprint"]
        .as_array()?
        .iter()
        .filter_map(|x| x.as_u64().map(|n| n as u32))
        .collect();
    (fp.len() > 30).then_some(fp)
}

/// Share of equal bits between two fingerprints at the best alignment
/// within a few seconds (encoders add different amounts of padding).
pub fn fingerprint_similarity(a: &[u32], b: &[u32]) -> f64 {
    let mut best = 0.0f64;
    for shift in -24i64..=24 {
        let (mut same, mut total) = (0u64, 0u64);
        for (i, &x) in a.iter().enumerate() {
            let j = i as i64 + shift;
            if j < 0 || j as usize >= b.len() {
                continue;
            }
            same += u64::from(32 - (x ^ b[j as usize]).count_ones());
            total += 32;
        }
        if total >= 32 * 30 {
            best = best.max(same as f64 / total as f64);
        }
    }
    best
}

fn norm_title(t: &str) -> String {
    t.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn media_groups(
    scan: &Scan,
    opts: &SimilarOptions,
    identical: &HashMap<PathBuf, usize>,
    done: &AtomicU64,
) -> Option<Vec<SimilarGroup>> {
    if !crate::providers::on_path("ffprobe") {
        return None;
    }
    let mut scope = Scope::new(scan.tree.root_path(), opts.any_type);
    let cands: Vec<(PathBuf, u64)> = scan
        .files
        .iter()
        .filter(|f| f.counted && f.stat.apparent >= opts.min_size.max(64 << 10))
        .filter(|f| {
            matches!(
                media::kind_of(&f.name),
                Some(media::MediaKind::Audio | media::MediaKind::Video)
            )
        })
        .map(|f| (scan.file_path(f), f.stat.real))
        .filter(|(p, _)| scope.exclusion(p).is_none())
        .collect();
    let fingerprints = crate::providers::on_path("fpcalc");
    let probed: Vec<(Item, Probe, Option<Vec<u32>>)> = cands
        .into_par_iter()
        .filter_map(|(path, real)| {
            done.fetch_add(1, Ordering::Relaxed);
            let pr = probe(&path)?;
            let fp = (fingerprints && !pr.video)
                .then(|| fingerprint(&path))
                .flatten();
            Some((
                Item {
                    path,
                    real,
                    detail: Some(pr.detail.clone()),
                },
                pr,
                fp,
            ))
        })
        .collect();
    let mut nb: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    // By name: the same name once copy markers are gone (so "Song (Lead
    // Vocal)" is not "Song"), same duration, and titles agree if present.
    let stem = |p: &Path| {
        let k = name_key(p.file_name().unwrap_or_default());
        k.rsplit_once('.').map_or(k.clone(), |(s, _)| s.to_string())
    };
    let mut by_name: HashMap<(bool, String), Vec<usize>> = HashMap::new();
    for (i, (it, pr, _)) in probed.iter().enumerate() {
        by_name
            .entry((pr.video, stem(&it.path)))
            .or_default()
            .push(i);
    }
    for g in by_name.values().filter(|g| g.len() > 1) {
        for (x, &i) in g.iter().enumerate() {
            for &j in &g[x + 1..] {
                let (a, b) = (&probed[i].1, &probed[j].1);
                let titles_agree = match (&a.title, &b.title) {
                    (Some(t), Some(u)) => norm_title(t) == norm_title(u),
                    _ => true,
                };
                if titles_agree && (a.duration - b.duration).abs() <= 1.0 {
                    add_edge(&mut nb, i, j, 1.0);
                }
            }
        }
    }
    // By sound: audio fingerprints of similar length, whatever the names.
    if fingerprints {
        let with_fp: Vec<usize> = (0..probed.len())
            .filter(|&i| probed[i].2.is_some())
            .collect();
        for (x, &i) in with_fp.iter().enumerate() {
            for &j in &with_fp[x + 1..] {
                if (probed[i].1.duration - probed[j].1.duration).abs() > 3.0 {
                    continue;
                }
                if let (Some(a), Some(b)) = (&probed[i].2, &probed[j].2) {
                    let s = fingerprint_similarity(a, b);
                    if s >= 0.9 {
                        add_edge(&mut nb, i, j, s);
                    }
                }
            }
        }
    }
    let mut order: Vec<usize> = (0..probed.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(probed[i].0.real));
    let groups = star_groups(probed.len(), &order, &nb);
    let items: Vec<Item> = probed.into_iter().map(|(i, _, _)| i).collect();
    Some(finish(
        SimilarKind::Media,
        &items,
        groups,
        identical,
        "same recording in a different encoding",
    ))
}

// ---- documents ----

static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("valid"));

/// Text from the XML parts of a zipped office or EPUB file.
fn zipped_text(path: &Path, parts: impl Fn(&str) -> bool) -> Option<String> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let mut out = String::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).ok()?;
        if !parts(f.name()) || f.size() > 64 << 20 {
            continue;
        }
        let mut xml = String::new();
        if f.read_to_string(&mut xml).is_ok() {
            out.push_str(&TAGS.replace_all(&xml, " "));
            out.push(' ');
        }
    }
    Some(out)
}

/// The words of a document, or `None` if they cannot be read.
fn document_text(path: &Path, pdftotext: bool) -> Option<String> {
    let ext = ext_of(path);
    match ext.as_str() {
        "pdf" if pdftotext => {
            let mut cmd = std::process::Command::new("pdftotext");
            // The first 50 pages tell documents apart; whole books take long.
            cmd.args(["-q", "-enc", "UTF-8", "-l", "50"])
                .arg(path)
                .arg("-");
            crate::providers::run_with_timeout(&mut cmd, std::time::Duration::from_secs(30)).ok()
        }
        "docx" => zipped_text(path, |n| n == "word/document.xml"),
        "odt" | "ods" | "odp" => zipped_text(path, |n| n == "content.xml"),
        "pptx" => zipped_text(path, |n| {
            n.starts_with("ppt/slides/slide") && n.ends_with(".xml")
        }),
        "xlsx" => zipped_text(path, |n| n == "xl/sharedStrings.xml"),
        "epub" => zipped_text(path, |n| {
            n.ends_with(".xhtml") || n.ends_with(".html") || n.ends_with(".htm")
        }),
        e if TEXT_EXT.contains(&e) => String::from_utf8(std::fs::read(path).ok()?).ok(),
        _ => None,
    }
}

const MINHASHES: usize = 64;

/// Five-word shingles of the normalized text, hashed.
pub fn shingles(text: &str) -> HashSet<u64> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    words
        .windows(5)
        .map(|w| {
            u64::from_le_bytes(
                blake3::hash(w.join(" ").as_bytes()).as_bytes()[..8]
                    .try_into()
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// MinHash signature: for each of 64 hash functions, the smallest value
/// over the set. The share of equal positions estimates overlap.
pub fn minhash(set: &HashSet<u64>) -> [u64; MINHASHES] {
    let mut sig = [u64::MAX; MINHASHES];
    for &c in set {
        for (i, s) in sig.iter_mut().enumerate() {
            let a = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let v = c.wrapping_mul(a).rotate_left(i as u32 % 64)
                ^ (i as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93);
            if v < *s {
                *s = v;
            }
        }
    }
    sig
}

pub fn estimate(a: &[u64; MINHASHES], b: &[u64; MINHASHES]) -> f64 {
    a.iter().zip(b).filter(|(x, y)| x == y).count() as f64 / MINHASHES as f64
}

fn documents(
    scan: &Scan,
    opts: &SimilarOptions,
    identical: &HashMap<PathBuf, usize>,
    done: &AtomicU64,
    notes: &mut Vec<String>,
) -> Vec<SimilarGroup> {
    let pdftotext = crate::providers::on_path("pdftotext");
    if !pdftotext {
        notes.push("PDFs skipped by the similar-documents check: pdftotext (poppler-utils) is not installed".into());
    }
    let exts: Vec<&str> = DOC_EXT.iter().chain(TEXT_EXT).copied().collect();
    let sigs: Vec<(Item, usize, [u64; MINHASHES])> = candidates(scan, opts, &exts, 256 << 20)
        .into_par_iter()
        .filter_map(|(path, f)| {
            done.fetch_add(1, Ordering::Relaxed);
            let sh = shingles(&document_text(&path, pdftotext)?);
            // Too few words to tell documents apart.
            (sh.len() >= 40).then(|| {
                let sig = minhash(&sh);
                (
                    Item {
                        path,
                        real: f.stat.real,
                        detail: Some(format!("{} words compared", sh.len() + 4)),
                    },
                    sh.len(),
                    sig,
                )
            })
        })
        .collect();
    // LSH: 16 bands of 4 rows; similar signatures collide in some band.
    let mut nb: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for band in 0..16 {
        let mut buckets: HashMap<[u64; 4], Vec<usize>> = HashMap::new();
        for (i, s) in sigs.iter().enumerate() {
            let mut k = [0u64; 4];
            k.copy_from_slice(&s.2[band * 4..band * 4 + 4]);
            buckets.entry(k).or_default().push(i);
        }
        for b in buckets.values().filter(|b| b.len() > 1 && b.len() < 2000) {
            for (x, &i) in b.iter().enumerate() {
                for &j in &b[x + 1..] {
                    if !seen.insert((i.min(j), i.max(j))) {
                        continue;
                    }
                    let (li, lj) = (sigs[i].1 as f64, sigs[j].1 as f64);
                    if li.max(lj) > 1.5 * li.min(lj) {
                        continue;
                    }
                    let e = estimate(&sigs[i].2, &sigs[j].2);
                    if e >= opts.content_threshold {
                        add_edge(&mut nb, i, j, e);
                    }
                }
            }
        }
    }
    let mut order: Vec<usize> = (0..sigs.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(sigs[i].1));
    let groups = star_groups(sigs.len(), &order, &nb);
    let items: Vec<Item> = sigs.into_iter().map(|(i, _, _)| i).collect();
    finish(
        SimilarKind::Content,
        &items,
        groups,
        identical,
        "mostly the same words (edited, re-saved or another version)",
    )
}

/// Extensions the walk must keep whatever their size for these analyses.
pub fn record_ext(kinds: &[SimilarKind]) -> Vec<String> {
    let mut v: Vec<&str> = Vec::new();
    if kinds.contains(&SimilarKind::Text) || kinds.contains(&SimilarKind::Content) {
        v.extend(TEXT_EXT);
    }
    if kinds.contains(&SimilarKind::Content) {
        v.extend(DOC_EXT);
    }
    if kinds.contains(&SimilarKind::Image) {
        v.extend(IMAGE_EXT);
    }
    v.into_iter().map(str::to_string).collect()
}

/// Runs the requested analyses. `identical` maps each byte-identical file
/// to its exact set, so groups of identical files are left out. Returns
/// the groups and notes about analyses that could not fully run.
pub fn find(
    scan: &Scan,
    kinds: &[SimilarKind],
    opts: &SimilarOptions,
    identical: &HashMap<PathBuf, usize>,
    done: &AtomicU64,
) -> (Vec<SimilarGroup>, Vec<String>) {
    let mut groups = Vec::new();
    let mut notes = Vec::new();
    for k in kinds {
        match k {
            SimilarKind::Text => groups.extend(text(scan, opts, identical, done)),
            SimilarKind::Image => groups.extend(images(scan, opts, identical, done)),
            SimilarKind::Media => match media_groups(scan, opts, identical, done) {
                Some(g) => groups.extend(g),
                None => notes.push(
                    "same-recording check skipped: ffprobe (from ffmpeg) is not installed".into(),
                ),
            },
            SimilarKind::Content => {
                groups.extend(documents(scan, opts, identical, done, &mut notes))
            }
        }
    }
    // One set of files reported once, by the first analysis that found it.
    let mut seen: HashSet<Vec<String>> = HashSet::new();
    groups.retain(|g| {
        let mut k: Vec<String> = g.files.iter().map(|f| f.path.clone()).collect();
        k.sort();
        seen.insert(k)
    });
    (groups, notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::{Progress, ScanOptions, scan};
    use crate::platform::linux::LinuxPlatform;
    use std::fs;

    fn put(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    fn scanned(root: &Path, kinds: &[SimilarKind]) -> Scan {
        let mut o = ScanOptions::new(root);
        o.record_ext = record_ext(kinds);
        scan(&LinuxPlatform::new(), None, &o, &Progress::default()).unwrap()
    }

    fn opts() -> SimilarOptions {
        SimilarOptions {
            min_size: 1,
            ..SimilarOptions::default()
        }
    }

    fn names(g: &SimilarGroup) -> Vec<&str> {
        g.files
            .iter()
            .map(|f| f.path.rsplit('/').next().unwrap())
            .collect()
    }

    #[test]
    fn text_ignores_whitespace_and_line_endings() {
        assert_eq!(
            normalized_text_hash(b"a\nb\n"),
            normalized_text_hash(b"\xEF\xBB\xBFa  \r\nb\r\n\r\n")
        );
        assert_ne!(normalized_text_hash(b"a\nb"), normalized_text_hash(b"a\nc"));
        assert_eq!(normalized_text_hash(b"\xff\xfe"), None);
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        put(r, "unix/notes.txt", b"hello\nworld\n");
        put(r, "win/notes.txt", b"hello  \r\nworld\r\n");
        put(r, "other/notes.txt", b"hello\nthere\n");
        let s = scanned(r, &[SimilarKind::Text]);
        let (g, _) = find(
            &s,
            &[SimilarKind::Text],
            &opts(),
            &HashMap::new(),
            &AtomicU64::new(0),
        );
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].files.len(), 2);
    }

    /// A photo-like picture: smooth shapes that depend on `seed`, so
    /// different seeds give different pictures.
    fn picture(w: u32, h: u32, seed: u32) -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let v = ((fx * (3 + seed % 5) as f32 * 3.1).sin()
                * (fy * (2 + seed % 7) as f32 * 2.3).cos()
                * 110.0
                + 128.0
                + fx * 40.0 * (seed % 3) as f32) as u8;
            image::Rgb([v, v / 2 + 30, 255 - v])
        })
    }

    #[test]
    fn resized_images_match_and_different_ones_do_not() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        for d in ["a", "b", "c", "d"] {
            fs::create_dir_all(r.join(d)).unwrap();
        }
        picture(800, 600, 1).save(r.join("a/photo.png")).unwrap();
        picture(400, 300, 1)
            .save(r.join("b/photo-small.jpg"))
            .unwrap();
        picture(800, 600, 4).save(r.join("c/other.png")).unwrap();
        // The same picture squashed to another aspect ratio: not a copy.
        picture(800, 300, 1)
            .save(r.join("d/stretched.png"))
            .unwrap();
        let s = scanned(r, &[SimilarKind::Image]);
        let (g, _) = find(
            &s,
            &[SimilarKind::Image],
            &opts(),
            &HashMap::new(),
            &AtomicU64::new(0),
        );
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(
            names(&g[0]),
            ["photo.png", "photo-small.jpg"],
            "seed (largest) first"
        );
        assert_eq!(g[0].files[1].detail.as_deref(), Some("400×300"));
    }

    #[test]
    fn star_groups_do_not_chain() {
        // 0~1 and 1~2 but not 0~2: one group of two, never all three.
        let mut nb = HashMap::new();
        add_edge(&mut nb, 0, 1, 0.9);
        add_edge(&mut nb, 1, 2, 0.9);
        let g = star_groups(3, &[0, 1, 2], &nb);
        assert_eq!(g, vec![(vec![0, 1], 0.9)]);
    }

    #[test]
    fn edited_documents_share_most_words() {
        let words: Vec<String> = (0..3000)
            .map(|i| format!("w{}", (i * 7919) % 1543))
            .collect();
        let base = words.join(" ");
        let mut edited_words = words.clone();
        edited_words.insert(1500, "an entirely new sentence goes right here".into());
        edited_words[200] = "changed".into();
        let edited = edited_words.join(" ");
        let other: String = (0..3000)
            .map(|i| format!("z{} ", (i * 104_729) % 2011))
            .collect();
        let (a, b, c) = (
            minhash(&shingles(&base)),
            minhash(&shingles(&edited)),
            minhash(&shingles(&other)),
        );
        assert!(estimate(&a, &b) >= 0.8, "{}", estimate(&a, &b));
        assert!(estimate(&a, &c) < 0.1, "{}", estimate(&a, &c));

        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        put(r, "v1/report.md", base.as_bytes());
        put(r, "v2/report-final.md", edited.as_bytes());
        put(r, "x/other.md", other.as_bytes());
        let s = scanned(r, &[SimilarKind::Content]);
        let (g, _) = find(
            &s,
            &[SimilarKind::Content],
            &opts(),
            &HashMap::new(),
            &AtomicU64::new(0),
        );
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0].files.len(), 2);
        assert!(g[0].similarity >= 0.8);
    }

    #[test]
    fn office_text_is_read_from_the_zip() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.docx");
        let mut z = zip::ZipWriter::new(fs::File::create(&p).unwrap());
        z.start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        z.write_all(
            b"<w:document><w:p><w:t>Hello brave</w:t><w:t> new world</w:t></w:p></w:document>",
        )
        .unwrap();
        z.finish().unwrap();
        let t = document_text(&p, false).unwrap();
        assert!(t.contains("Hello brave") && t.contains("new world"), "{t}");
    }

    #[test]
    fn fingerprints_compare_across_padding() {
        let a: Vec<u32> = (0..200u32).map(|i| i.wrapping_mul(2_654_435_761)).collect();
        let mut b = vec![0u32; 5];
        b.extend(&a);
        assert!(fingerprint_similarity(&a, &b) > 0.99);
        let c: Vec<u32> = (0..200u32)
            .map(|i| i.wrapping_mul(40503).rotate_left(7))
            .collect();
        assert!(fingerprint_similarity(&a, &c) < 0.7);
    }

    #[test]
    fn one_recording_in_two_encodings_but_not_its_stems() {
        if !crate::providers::on_path("ffmpeg") || !crate::providers::on_path("ffprobe") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let make = |out: &str, secs: &str, title: &str| {
            let ok = std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("sine=frequency=440:duration={secs}"),
                ])
                .args(["-metadata", &format!("title={title}")])
                .arg(r.join(out))
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "ffmpeg failed for {out}");
        };
        fs::create_dir_all(r.join("a")).unwrap();
        fs::create_dir_all(r.join("b")).unwrap();
        make("a/song.wav", "12", "Tutam");
        make("b/song (1).flac", "12", "Tutam");
        make("b/song (Lead Vocal).wav", "12", "Tutam");
        make("b/other.flac", "30", "Another");
        let s = scanned(r, &[SimilarKind::Media]);
        let (g, notes) = find(
            &s,
            &[SimilarKind::Media],
            &opts(),
            &HashMap::new(),
            &AtomicU64::new(0),
        );
        assert!(notes.is_empty());
        assert_eq!(g.len(), 1, "{g:?}");
        let mut n = names(&g[0]);
        n.sort();
        assert_eq!(
            n,
            ["song (1).flac", "song.wav"],
            "a stem is another recording"
        );
        assert!(
            g[0].files
                .iter()
                .all(|f| f.detail.as_deref().is_some_and(|d| d.starts_with("0:12")))
        );
    }
}
