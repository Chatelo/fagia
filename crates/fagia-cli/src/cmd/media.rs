use super::{Ctx, Outcome};
use crate::output::{Align, Table};
use anyhow::Result;
use fagia_core::disk::media::analyze;
use fagia_core::session::ScanFlags;
use fagia_core::size::format_size;
use std::path::Path;

pub fn run(ctx: &Ctx, path: Option<&Path>, limit: usize) -> Result<Outcome> {
    // Media files are always recorded; this also keeps large unknown files
    // for header sniffing.
    let flags = ScanFlags {
        record_min: Some(1 << 20),
        ..ctx.flags()
    };
    let scan = ctx.scan(path, flags)?;
    let outcome = ctx.scan_issues(&scan);
    let min = ctx
        .g
        .min_size
        .as_deref()
        .map(fagia_core::size::parse_size)
        .transpose()?
        .unwrap_or(0);
    let rep = analyze::media(&scan, limit, min);
    if ctx.out.json {
        ctx.out.print_json("media", &rep)?;
        return Ok(outcome);
    }
    let out = &ctx.out;
    if out.csv {
        let mut t = Table::new(&[
            ("TYPE", Align::Left),
            ("SIZE", Align::Right),
            ("PATH", Align::Left),
        ]);
        for g in &rep.groups {
            for f in &g.largest {
                t.row(vec![
                    g.label.clone(),
                    f.real.to_string(),
                    f.path.path.clone(),
                ]);
            }
        }
        t.print(out);
        return Ok(outcome);
    }
    if rep.groups.is_empty() {
        println!("No media files in {}.", out.path(scan.tree.root_path()));
        return Ok(outcome);
    }
    let mut headers = vec![
        ("TYPE", Align::Left),
        ("FILES", Align::Right),
        ("SIZE", Align::Right),
    ];
    if out.fancy {
        headers.push(("", Align::Left));
    }
    let mut t = Table::new(&headers);
    for g in &rep.groups {
        let mut row = vec![
            out.accent(&g.label),
            out.dim(&g.count.to_string()),
            out.size(g.real),
        ];
        if out.fancy {
            row.push(out.bar(g.real as f64 / rep.total.max(1) as f64, 20));
        }
        t.row(row);
    }
    let mut total = vec![
        "TOTAL".into(),
        rep.groups.iter().map(|g| g.count).sum::<u64>().to_string(),
        format_size(rep.total),
    ];
    if out.fancy {
        total.push(String::new());
    }
    t.total(total);
    t.print(out);
    for g in &rep.groups {
        println!("\n{} {}", out.title(&g.label), out.dim("largest files"));
        let mut t = Table::new(&[("SIZE", Align::Right), ("PATH", Align::Left)]);
        for f in &g.largest {
            let mut p = out.path_styled(Path::new(&f.path.path));
            let mut extra = Vec::new();
            if let Some(d) = f.duration_secs {
                extra.push(format!(
                    "{}:{:02}:{:02}",
                    (d / 3600.0) as u64,
                    (d / 60.0) as u64 % 60,
                    d as u64 % 60
                ));
            }
            if let Some(r) = &f.resolution {
                extra.push(r.clone());
            }
            if f.sniffed {
                extra.push("detected from header".into());
            }
            if !extra.is_empty() {
                p = format!("{p}  {}", out.dim(&format!("({})", extra.join(", "))));
            }
            t.row(vec![out.size(f.real), p]);
        }
        t.print(out);
        println!("{}", out.dim("by folder:"));
        for f in &g.folders {
            println!(
                "  {}  {} {}",
                out.size_pad(f.real, 10),
                out.path_styled(Path::new(&f.path.path)),
                out.dim(&format!("({} files)", f.count))
            );
        }
    }
    Ok(outcome)
}
