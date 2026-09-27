use super::{Ctx, Outcome};
use crate::output::{Align, Table};
use anyhow::Result;
use fagia_core::model::now_epoch;
use fagia_core::report;
use fagia_core::session::ScanFlags;
use fagia_core::size::format_size;
use std::path::Path;

pub fn run(ctx: &Ctx, path: Option<&Path>, limit: usize) -> Result<Outcome> {
    let min = ctx.min_size()?;
    // Keep every file at least as large as the display threshold.
    let flags = ScanFlags {
        record_min: Some(min.max(1)),
        ..ctx.flags()
    };
    let scan = ctx.scan(path, flags)?;
    let outcome = ctx.scan_issues(&scan);
    let rep = report::big(&scan, limit, min, ctx.g.apparent);
    if ctx.out.json {
        ctx.out.print_json("big", &rep)?;
        return Ok(outcome);
    }
    let out = &ctx.out;
    let now = now_epoch();
    let mut t = Table::new(&[
        ("SIZE", Align::Right),
        ("AGE", Align::Right),
        ("PATH", Align::Left),
    ]);
    for f in &rep.files {
        let size = if ctx.g.apparent { f.apparent } else { f.real };
        let mut p = out.path_styled(Path::new(&f.path.path));
        if let Some(c) = &f.category {
            p = format!("{p}  {}", out.badge(c, "30;43"));
        }
        if f.hard_links > 1 {
            p = format!(
                "{p}  {}",
                out.dim(&format!("({} hard links)", f.hard_links))
            );
        }
        t.row(vec![
            out.size(size),
            out.age(now.saturating_sub(f.modified).max(0) as u64),
            p,
        ]);
    }
    if t.is_empty() && !out.csv {
        println!(
            "No files of at least {} under {}.",
            format_size(min),
            out.path(scan.tree.root_path())
        );
    } else {
        t.print(out);
    }
    Ok(outcome)
}
