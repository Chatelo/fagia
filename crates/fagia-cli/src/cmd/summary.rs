use super::{Ctx, Outcome};
use crate::output::{Align, Table};
use anyhow::Result;
use fagia_core::disk::unseen;
use fagia_core::paths::JsonPath;
use fagia_core::report::{DiskSummary, FindingOut, MemorySummary, ScanInfo, SummaryReport};
use fagia_core::size::format_size;
use std::path::Path;

pub fn run(ctx: &Ctx) -> Result<Outcome> {
    let p = ctx.session.platform.as_ref();
    let home = ctx.session.home().to_path_buf();
    let stats = p.fs_stats(&home)?;
    let disk = DiskSummary {
        path: JsonPath::new(&home),
        fs_type: p.mount_for(&home).map(|m| m.fs_type),
        reserved: stats.reserved(),
        stats,
    };
    let memory = p.system_memory().ok().map(|m| MemorySummary {
        total: m.total,
        available: m.available,
        swap_total: m.swap_total,
        swap_used: m.swap_total.saturating_sub(m.swap_free),
    });
    let scan = ctx.scan(None, ctx.flags())?;
    let outcome = ctx.scan_issues(&scan);
    let mut suspects: Vec<_> = scan
        .findings
        .iter()
        .filter(|f| f.regenerable && ctx.wants_category(f))
        .collect();
    let suspects_total = suspects.iter().map(|f| f.reclaimable).sum();
    suspects.sort_by_key(|f| std::cmp::Reverse(f.reclaimable));
    let rep = SummaryReport {
        disk,
        memory,
        scan: ScanInfo::from_scan(&scan),
        top_suspects: suspects
            .iter()
            .take(5)
            .map(|f| FindingOut::from(*f))
            .collect(),
        suspects_total,
        unseen: unseen::collect(p, &home, &stats),
    };
    if ctx.out.json {
        ctx.out.print_json("summary", &rep)?;
        return Ok(outcome);
    }
    print_human(ctx, &rep);
    Ok(outcome)
}

fn print_human(ctx: &Ctx, rep: &SummaryReport) {
    let out = &ctx.out;
    let d = &rep.disk;
    let used = 1.0 - d.stats.available as f64 / d.stats.total.max(1) as f64;
    println!(
        "{}  {} {}  {} free of {}{}",
        out.bold("Disk"),
        out.usage_bar(used, 24),
        out.bold(&format!("{:.0}%", used * 100.0)),
        out.bold(&format_size(d.stats.available)),
        format_size(d.stats.total),
        out.dim(&format!(
            "  {} {}{}",
            out.path(Path::new(&d.path.path)),
            d.fs_type.as_deref().unwrap_or(""),
            if d.reserved > 0 {
                format!(" · {} reserved for root", format_size(d.reserved))
            } else {
                String::new()
            }
        ))
    );
    if let Some(m) = &rep.memory {
        let used = 1.0 - m.available as f64 / m.total.max(1) as f64;
        println!(
            "{}   {} {}  {} available of {}{}",
            out.bold("RAM"),
            out.usage_bar(used, 24),
            out.bold(&format!("{:.0}%", used * 100.0)),
            out.bold(&format_size(m.available)),
            format_size(m.total),
            if m.swap_total > 0 {
                out.dim(&format!(
                    "  swap {} of {}",
                    format_size(m.swap_used),
                    format_size(m.swap_total)
                ))
            } else {
                String::new()
            }
        );
    }
    println!();
    if rep.top_suspects.is_empty() {
        println!(
            "No regenerable suspects found in {}.",
            out.path(Path::new(&rep.scan.root.path))
        );
    } else {
        println!(
            "{} {}",
            out.title("Top suspects"),
            out.dim(&format!(
                "({} scanned in {:.1} s)",
                format_size(rep.scan.total_real),
                rep.scan.elapsed_ms as f64 / 1000.0
            ))
        );
        let mut t = Table::new(&[
            ("SIZE", Align::Right),
            ("CATEGORY", Align::Left),
            ("PATH", Align::Left),
            ("EVIDENCE", Align::Left),
        ]);
        for f in &rep.top_suspects {
            t.row(vec![
                out.size(f.reclaimable),
                out.accent(&f.category),
                out.path_styled(Path::new(&f.path.path)),
                out.italic(&f.evidence),
            ]);
        }
        t.print(out);
        println!(
            "\n{} regenerable in total. Run {} for details, {} to reclaim.",
            out.green(&out.bold(&format_size(rep.suspects_total))),
            out.accent("`fagia suspects`"),
            out.accent("`fagia clean`")
        );
    }
    let u = &rep.unseen;
    if u.deleted_open_total > 0 || u.reserved > 0 || u.snapshot_note.is_some() {
        println!("\n{}", out.title("Space folder sizes cannot show"));
        if u.deleted_open_total > 0 {
            println!(
                "  {}  held by deleted files that are still open; restart the process to free it",
                out.size(u.deleted_open_total)
            );
            for d in u.deleted_open.iter().take(if out.verbose { 20 } else { 3 }) {
                println!(
                    "    {}  {} {}  {}",
                    out.size(d.real),
                    fagia_core::paths::escape_control(&d.process),
                    out.dim(&format!("(pid {})", d.pid)),
                    out.dim(&out.path(Path::new(&d.path.path)))
                );
            }
        }
        if u.reserved > 0 {
            println!(
                "  {}  reserved for root by the filesystem",
                out.size(u.reserved)
            );
        }
        if let Some(n) = &u.snapshot_note {
            println!("  {}", out.dim(n));
        }
    }
}
