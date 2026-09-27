use super::{Ctx, Outcome};
use crate::output::{Align, Table, pct};
use anyhow::Result;
use fagia_core::report;
use std::path::Path;

pub fn run(ctx: &Ctx, path: Option<&Path>, depth: usize, limit: usize) -> Result<Outcome> {
    let scan = ctx.scan(path, ctx.flags())?;
    let outcome = ctx.scan_issues(&scan);
    let rep = report::top(&scan, depth.max(1), limit, ctx.min_size()?, ctx.g.apparent);
    if ctx.out.json {
        ctx.out.print_json("top", &rep)?;
        return Ok(outcome);
    }
    let out = &ctx.out;
    let mut headers = vec![("SIZE", Align::Right), ("SHARE", Align::Right)];
    if out.fancy {
        headers.push(("", Align::Left));
    }
    headers.extend([("FILES", Align::Right), ("PATH", Align::Left)]);
    let mut t = Table::new(&headers);
    let max_share = rep
        .entries
        .iter()
        .map(|e| e.share)
        .fold(0.0, f64::max)
        .max(f64::EPSILON);
    for e in &rep.entries {
        let size = if ctx.g.apparent { e.apparent } else { e.real };
        let mut p = out.path_styled(Path::new(&e.path.path));
        if e.loose_files {
            p = format!("{p} {}", out.dim("(files directly inside)"));
        } else if let Some(c) = &e.category {
            p = format!("{p}  {}", out.badge(c, "30;42"));
        }
        let mut row = vec![out.size(size), out.dim(&pct(e.share))];
        if out.fancy {
            row.push(out.bar(e.share / max_share, 20));
        }
        row.extend([out.dim(&e.files.to_string()), p]);
        t.row(row);
    }
    let total = if ctx.g.apparent {
        rep.scan.total_apparent
    } else {
        rep.scan.total_real
    };
    if !out.csv {
        let mut row = vec![out.size(total), "100%".into()];
        if out.fancy {
            row.push(String::new());
        }
        row.extend([
            String::new(),
            format!("TOTAL {}", out.path(scan.tree.root_path())),
        ]);
        t.total(row);
    }
    t.print(out);
    Ok(outcome)
}
