//! `fagia dupes`: identical folders, identical files and (with --similar)
//! near-duplicates.

use super::{Ctx, Outcome};
use crate::cli::{DupesCli, MatchArg, SimilarArg};
use crate::output::{Align, Out, Table};
use crate::progress::Spinner;
use anyhow::Result;
use fagia_core::disk::dupe_dirs::{self, DirSet};
use fagia_core::disk::dupes::{self, DupeSet, MatchBy};
use fagia_core::disk::similar::{self, SimilarGroup, SimilarKind, SimilarOptions};
use fagia_core::paths::JsonPath;
use fagia_core::report::{DupeDirOut, DupeSetOut, DupesReport, ScanInfo};
use fagia_core::session::ScanFlags;
use fagia_core::size::{format_size, parse_size};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

/// Hashing many small files costs more than it finds.
const DEFAULT_MIN: u64 = 1 << 20;
/// Near-duplicate checks also look at smaller files (notes, pictures).
const SIMILAR_MIN: u64 = 4 << 10;

fn match_by(a: &DupesCli) -> MatchBy {
    match (a.any_name, a.match_by) {
        (true, _) | (_, MatchArg::Content) => MatchBy::Content,
        (_, MatchArg::Loose) => MatchBy::LooseNameAndContent,
        (_, MatchArg::Name) => MatchBy::NameAndContent,
    }
}

fn similar_kinds(a: &DupesCli) -> Vec<SimilarKind> {
    let map = |k: &SimilarArg| match k {
        SimilarArg::Text => SimilarKind::Text,
        SimilarArg::Images => SimilarKind::Image,
        SimilarArg::Media => SimilarKind::Media,
        SimilarArg::Content => SimilarKind::Content,
    };
    match &a.similar {
        None => Vec::new(),
        Some(v) if v.is_empty() => vec![
            SimilarKind::Text,
            SimilarKind::Image,
            SimilarKind::Media,
            SimilarKind::Content,
        ],
        Some(v) => v.iter().map(map).collect(),
    }
}

pub fn run(ctx: &Ctx, a: &DupesCli) -> Result<Outcome> {
    let user_min = ctx.g.min_size.as_deref().map(parse_size).transpose()?;
    let min = user_min.unwrap_or(DEFAULT_MIN).max(1);
    let kinds = similar_kinds(a);
    let by = match_by(a);
    let flags = ScanFlags {
        record_min: Some(min),
        ..ctx.flags()
    };
    let root = ctx.session.resolve_root(a.path.as_deref())?;
    let mut opts = ctx.session.scan_options(root.clone(), &flags);
    opts.record_ext = similar::record_ext(&kinds);
    let progress = Arc::new(fagia_core::disk::Progress::default());
    let scan = {
        let _s = Spinner::start(
            progress.clone(),
            &format!("Scanning {}", ctx.out.path(&root)),
        );
        ctx.session.scan(&opts, &progress)?
    };
    if !ctx.g.no_save
        && let Err(e) = ctx.session.save_snapshot(&scan)
    {
        ctx.out
            .note(&format!("warning: could not save scan history: {e}"));
    }
    let outcome = ctx.scan_issues(&scan);

    let counter = Arc::new(AtomicU64::new(0));
    let (dirs, sets) = {
        let _s = Spinner::counter(counter.clone(), "Comparing identical files");
        let dirs = if a.no_folders {
            Vec::new()
        } else {
            dupe_dirs::find(&scan, min, by != MatchBy::Content)
        };
        (dirs, dupes::find(&scan, min, by))
    };
    // File sets wholly inside duplicate folders are part of those.
    let sets: Vec<DupeSet> = sets
        .into_iter()
        .filter(|s| !s.paths.iter().all(|p| dupe_dirs::inside_any(p, &dirs)))
        .collect();

    if a.clean || a.link {
        if !kinds.is_empty() && !ctx.out.json {
            ctx.out.note(
                "note: near-duplicates are never removed; --clean acts on identical copies only",
            );
        }
        return super::dedupe::run(ctx, &scan, &sets, &dirs, a, outcome);
    }

    let (similar_groups, notes) = if kinds.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        // Identical content under any name, to leave byte-identical groups
        // out of the near-duplicate report.
        let exact_any = if by == MatchBy::Content {
            sets.clone()
        } else {
            dupes::find(&scan, 1, MatchBy::Content)
        };
        let identical: HashMap<PathBuf, usize> = exact_any
            .iter()
            .enumerate()
            .flat_map(|(i, s)| s.paths.iter().map(move |p| (p.clone(), i)))
            .collect();
        let sopts = SimilarOptions {
            min_size: user_min.unwrap_or(SIMILAR_MIN).max(1),
            image_distance: a.image_distance.min(64),
            content_threshold: a.similarity.clamp(0.0, 1.0),
            any_type: a.any_type,
        };
        let _s = Spinner::counter(counter.clone(), "Comparing similar files");
        similar::find(&scan, &kinds, &sopts, &identical, &counter)
    };

    let rep = DupesReport {
        scan: ScanInfo::from_scan(&scan),
        min_size: min,
        match_by: by,
        match_names: by != MatchBy::Content,
        total_wasted: sets.iter().map(DupeSet::wasted).sum::<u64>()
            + dirs.iter().map(DirSet::wasted).sum::<u64>(),
        folders: dirs
            .iter()
            .take(a.limit)
            .map(|d| DupeDirOut {
                real: d.real,
                files: d.files,
                wasted: d.wasted(),
                copies: d.paths.iter().map(|p| JsonPath::new(p)).collect(),
            })
            .collect(),
        sets: sets
            .iter()
            .take(a.limit)
            .map(|s| DupeSetOut {
                size: s.size,
                wasted: s.wasted(),
                copies: s.paths.iter().map(|p| JsonPath::new(p)).collect(),
            })
            .collect(),
        similar: similar_groups.clone(),
        notes: notes.clone(),
    };
    if ctx.out.json {
        ctx.out.print_json("dupes", &rep)?;
        return Ok(outcome);
    }
    let out = &ctx.out;
    for n in &notes {
        out.note(&format!("note: {n}"));
    }
    if out.csv {
        let mut t = Table::new(&[
            ("SET", Align::Right),
            ("KIND", Align::Left),
            ("SIZE", Align::Right),
            ("PATH", Align::Left),
        ]);
        let mut n = 0;
        for d in &rep.folders {
            n += 1;
            for c in &d.copies {
                t.row(vec![
                    n.to_string(),
                    "folder".into(),
                    d.real.to_string(),
                    c.path.clone(),
                ]);
            }
        }
        for s in &rep.sets {
            n += 1;
            for c in &s.copies {
                t.row(vec![
                    n.to_string(),
                    "file".into(),
                    s.size.to_string(),
                    c.path.clone(),
                ]);
            }
        }
        for g in &similar_groups {
            n += 1;
            for f in &g.files {
                t.row(vec![
                    n.to_string(),
                    format!("similar-{:?}", g.kind).to_lowercase(),
                    f.size.to_string(),
                    f.path.clone(),
                ]);
            }
        }
        t.print(out);
        return Ok(outcome);
    }
    print_human(out, a, &rep, &sets, &dirs, &similar_groups, min);
    Ok(outcome)
}

fn tree(out: &Out, paths: &[&str]) {
    for (i, p) in paths.iter().enumerate() {
        let branch = if out.fancy {
            if i + 1 == paths.len() {
                "└─"
            } else {
                "├─"
            }
        } else {
            ""
        };
        println!("  {} {}", out.dim(branch), out.path_styled(Path::new(p)));
    }
}

fn print_human(
    out: &Out,
    a: &DupesCli,
    rep: &DupesReport,
    sets: &[DupeSet],
    dirs: &[DirSet],
    similar_groups: &[SimilarGroup],
    min: u64,
) {
    let nothing = rep.folders.is_empty() && rep.sets.is_empty() && similar_groups.is_empty();
    if nothing {
        println!(
            "No duplicates of at least {} in {}.",
            format_size(min),
            out.path(Path::new(&rep.scan.root.path))
        );
    }
    if !rep.folders.is_empty() {
        println!("{}", out.title("Identical folders"));
        for d in &rep.folders {
            println!(
                "{} {} {}",
                out.size(d.wasted),
                out.dim("wasted ·"),
                out.dim(&format!(
                    "{} copies of {} ({} files)",
                    d.copies.len(),
                    format_size(d.real),
                    d.files
                ))
            );
            tree(
                out,
                &d.copies.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            );
        }
        if dirs.len() > a.limit {
            println!(
                "{}",
                out.dim(&format!("…and {} more folder sets", dirs.len() - a.limit))
            );
        }
        println!();
    }
    if !rep.sets.is_empty() {
        println!("{}", out.title("Identical files"));
        for s in &rep.sets {
            println!(
                "{} {} {}",
                out.size(s.wasted),
                out.dim("wasted ·"),
                out.dim(&format!(
                    "{} copies of {}",
                    s.copies.len(),
                    format_size(s.size)
                ))
            );
            tree(
                out,
                &s.copies.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            );
        }
        if sets.len() > a.limit {
            println!(
                "{}",
                out.dim(&format!("…and {} more file sets", sets.len() - a.limit))
            );
        }
        println!();
    }
    if !similar_groups.is_empty() {
        println!(
            "{} {}",
            out.title("Similar files"),
            out.dim("(not identical: listed, never removed automatically)")
        );
        let mut shown: HashMap<SimilarKind, usize> = HashMap::new();
        for g in similar_groups {
            let n = shown.entry(g.kind).or_default();
            *n += 1;
            if *n > a.limit {
                continue;
            }
            println!(
                "{} {} {}",
                out.badge(g.kind.label(), "30;46"),
                out.bold(&format!("{:.0}% alike", g.similarity * 100.0)),
                out.dim(&format!("· {}", g.note))
            );
            for (i, f) in g.files.iter().enumerate() {
                let branch = if out.fancy {
                    if i + 1 == g.files.len() {
                        "└─"
                    } else {
                        "├─"
                    }
                } else {
                    ""
                };
                println!(
                    "  {} {}  {}{}",
                    out.dim(branch),
                    out.size_pad(f.size, 10),
                    out.path_styled(Path::new(&f.path)),
                    f.detail
                        .as_deref()
                        .map(|d| out.dim(&format!("  {d}")))
                        .unwrap_or_default()
                );
            }
        }
        for (k, n) in &shown {
            if *n > a.limit {
                println!(
                    "{}",
                    out.dim(&format!(
                        "…and {} more {} groups",
                        n - a.limit,
                        k.label().to_lowercase()
                    ))
                );
            }
        }
        println!();
    }
    if !nothing {
        println!(
            "{} wasted by identical copies: {} folder set(s), {} file set(s).{}",
            out.green(&out.bold(&format_size(rep.total_wasted))),
            dirs.len(),
            sets.len(),
            if rep.total_wasted > 0 {
                " Remove extras with `fagia dupes --clean` (or --link)."
            } else {
                ""
            }
        );
    }
    let how = match rep.match_by {
        MatchBy::NameAndContent => {
            "same name (ignoring \"(1)\", \"- Copy\"…), size and identical content"
        }
        MatchBy::LooseNameAndContent => {
            "loosely equal names (tags and punctuation ignored), size and identical content"
        }
        MatchBy::Content => "identical content, any name",
    };
    println!(
        "{}",
        out.dim(&format!(
            "Matched by {how}.{}",
            if a.similar.is_none() {
                " Add --similar for near-duplicates (text, images, media, edited files)."
            } else {
                ""
            }
        ))
    );
}
