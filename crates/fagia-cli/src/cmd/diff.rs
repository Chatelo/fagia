use super::{Ctx, Outcome};
use crate::output::{Align, Table};
use anyhow::Result;
use fagia_core::paths::JsonPath;
use fagia_core::report::DiffReport;
use fagia_core::size::{format_delta, format_size};
use fagia_core::store::{Change, diff};
use std::path::Path;

/// Scans now (saving a snapshot) and compares with the previous snapshot
/// of the same root.
pub fn run(ctx: &Ctx, path: Option<&Path>, limit: usize) -> Result<Outcome> {
    let root = ctx.session.resolve_root(path)?;
    let mut outcome = Outcome::Ok;
    if !ctx.g.no_save {
        let scan = ctx.scan(Some(&root), ctx.flags())?;
        outcome = ctx.scan_issues(&scan);
        if !ctx.session.config.general.save_history {
            anyhow::bail!("history is off (general.save_history = false); diff needs saved scans");
        }
    }
    let host = ctx.session.platform.hostname();
    let snaps = ctx
        .session
        .store()?
        .list(&host, &root.to_string_lossy(), 2)?;
    let min_change = ctx
        .g
        .min_size
        .as_deref()
        .map(fagia_core::size::parse_size)
        .transpose()?
        .unwrap_or(1 << 20);
    let mut d = (snaps.len() == 2).then(|| diff(&snaps[1], &snaps[0], min_change));
    if let Some(d) = d.as_mut() {
        d.entries.truncate(limit);
    }
    let rep = DiffReport {
        root: JsonPath::new(&root),
        snapshots: snaps.len(),
        diff: d,
    };
    if ctx.out.json {
        ctx.out.print_json("diff", &rep)?;
        return Ok(outcome);
    }
    let out = &ctx.out;
    let Some(d) = &rep.diff else {
        match rep.snapshots {
            0 => println!(
                "No saved scans of {} yet; run `fagia diff` without --no-save.",
                out.path(&root)
            ),
            _ => println!(
                "Only one saved scan of {}. Run `fagia diff` again later to see what changed.",
                out.path(&root)
            ),
        }
        return Ok(outcome);
    };
    let age = (d.after_taken_at - d.before_taken_at).max(0) as u64;
    let delta = if d.total_delta > 0 {
        out.yellow(&format_delta(d.total_delta))
    } else {
        out.green(&format_delta(d.total_delta))
    };
    println!(
        "{} {} → {} {} {}",
        out.title("Total"),
        format_size(d.total_before),
        out.bold(&format_size(d.total_after)),
        out.bold(&delta),
        out.dim(&format!("since {} ago", fagia_core::size::format_age(age)))
    );
    if !d.categories.is_empty() {
        println!();
        let mut t = Table::new(&[
            ("CATEGORY", Align::Left),
            ("BEFORE", Align::Right),
            ("AFTER", Align::Right),
            ("CHANGE", Align::Right),
        ]);
        for c in &d.categories {
            let delta = format_delta(c.delta);
            t.row(vec![
                out.accent(&c.category),
                out.dim(&format_size(c.before)),
                format_size(c.after),
                if c.delta > 0 {
                    out.yellow(&delta)
                } else {
                    out.green(&delta)
                },
            ]);
        }
        t.print(out);
    }
    println!();
    if d.entries.is_empty() {
        println!("No folder changed by at least {}.", format_size(min_change));
        return Ok(outcome);
    }
    let mut t = Table::new(&[
        ("CHANGE", Align::Right),
        ("", Align::Left),
        ("NOW", Align::Right),
        ("PATH", Align::Left),
    ]);
    for e in &d.entries {
        let (label, grew) = match e.change {
            Change::Grew => (if out.fancy { "▲ grew" } else { "grew" }, true),
            Change::Shrank => (if out.fancy { "▼ shrank" } else { "shrank" }, false),
            Change::Appeared => (if out.fancy { "+ new" } else { "new" }, true),
            Change::Vanished => (if out.fancy { "− gone" } else { "gone" }, false),
        };
        let paint = |s: &str| if grew { out.yellow(s) } else { out.green(s) };
        t.row(vec![
            paint(&format_delta(e.delta)),
            paint(label),
            out.dim(&format_size(e.after)),
            out.path_styled(Path::new(&e.path.path)),
        ]);
    }
    t.print(out);
    Ok(outcome)
}
