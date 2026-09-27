//! `fagia suspects` and `fagia stale`.

use super::{Ctx, Outcome};
use crate::output::{Align, Table};
use anyhow::Result;
use fagia_core::disk::{Scan, stale};
use fagia_core::model::{EntryKind, Finding};
use fagia_core::report::{self, FindingOut, ScanInfo, SuspectsReport};
use fagia_core::size::format_size;
use std::path::Path;

/// Scans, annotates staleness, adds provider findings for a home scan, and
/// applies the global filters.
pub fn collect(ctx: &Ctx, path: Option<&Path>) -> Result<(Scan, Vec<Finding>, Vec<String>)> {
    let mut scan = ctx.scan(path, ctx.flags())?;
    stale::annotate(&mut scan, true);
    let mut findings: Vec<Finding> = scan.findings.clone();
    let mut provider_errors = Vec::new();
    if scan.tree.root_path() == ctx.session.home() {
        let (found, errors) = ctx.session.provider_findings();
        findings.extend(found);
        provider_errors = errors;
    }
    let min = ctx.min_size()?;
    let older = ctx.older_days()?;
    findings.retain(|f| {
        ctx.wants_category(f)
            && f.reclaimable.max(f.real) >= min
            && older.is_none_or(|d| f.stale_days.is_some_and(|s| s >= d))
    });
    findings.sort_by(|a, b| b.reclaimable.cmp(&a.reclaimable).then(a.path.cmp(&b.path)));
    Ok((scan, findings, provider_errors))
}

fn stale_threshold(ctx: &Ctx) -> Result<u64> {
    Ok(ctx
        .older_days()?
        .unwrap_or(ctx.session.config.general.stale_days))
}

pub fn run(ctx: &Ctx, path: Option<&Path>, list: bool) -> Result<Outcome> {
    let (scan, findings, provider_errors) = collect(ctx, path)?;
    let outcome = ctx.scan_issues(&scan);
    let days = stale_threshold(ctx)?;
    let refs: Vec<&Finding> = findings.iter().collect();
    let rep = SuspectsReport {
        scan: ScanInfo::from_scan(&scan),
        stale_days: days,
        categories: report::categories(&refs, days),
        findings: findings.iter().map(FindingOut::from).collect(),
        provider_errors,
    };
    if ctx.out.json {
        ctx.out.print_json("suspects", &rep)?;
        return Ok(outcome);
    }
    let out = &ctx.out;
    if ctx.out.verbose {
        for e in &rep.provider_errors {
            out.note(&format!("note: provider {e}"));
        }
    }
    if out.csv {
        item_table(ctx, &findings).print(out);
        return Ok(outcome);
    }
    let stale_header = format!("STALE >{days}d");
    let (regen, other): (Vec<_>, Vec<_>) = rep.categories.iter().partition(|c| c.regenerable);
    if regen.is_empty() {
        println!(
            "No regenerable suspects in {}.",
            out.path(scan.tree.root_path())
        );
    } else {
        let mut headers = vec![
            ("CATEGORY", Align::Left),
            ("COUNT", Align::Right),
            ("SIZE", Align::Right),
        ];
        if out.fancy {
            headers.push(("", Align::Left));
        }
        headers.push((&stale_header, Align::Right));
        let mut t = Table::new(&headers);
        let biggest = regen
            .iter()
            .map(|c| c.reclaimable)
            .max()
            .unwrap_or(1)
            .max(1);
        let (mut count, mut size, mut stale) = (0, 0, 0);
        for c in &regen {
            let mut row = vec![
                out.accent(&c.category),
                out.dim(&c.count.to_string()),
                out.size(c.reclaimable),
            ];
            if out.fancy {
                row.push(out.bar(c.reclaimable as f64 / biggest as f64, 16));
            }
            row.push(if c.stale > 0 {
                out.yellow(&format_size(c.stale))
            } else {
                out.dim(&format_size(0))
            });
            t.row(row);
            count += c.count;
            size += c.reclaimable;
            stale += c.stale;
        }
        let mut total = vec!["TOTAL".into(), count.to_string(), format_size(size)];
        if out.fancy {
            total.push(String::new());
        }
        total.push(format_size(stale));
        t.total(total);
        t.print(out);
        if stale > 0 {
            println!(
                "\nRun {} to review {} of stale items.",
                out.accent(&format!(
                    "`fagia clean {} --older {days}d`",
                    out.path(scan.tree.root_path())
                )),
                out.green(&out.bold(&format_size(stale)))
            );
        }
    }
    if !other.is_empty() {
        println!(
            "\n{}",
            out.title("Other findings (not regenerable, never auto-selected)")
        );
        let mut t = Table::new(&[
            ("CATEGORY", Align::Left),
            ("COUNT", Align::Right),
            ("SIZE", Align::Right),
        ]);
        for c in &other {
            t.row(vec![
                out.yellow(&c.category),
                out.dim(&c.count.to_string()),
                out.size(c.real),
            ]);
        }
        t.print(out);
    }
    if list || out.verbose {
        println!();
        item_table(ctx, &findings).print(out);
    }
    Ok(outcome)
}

fn item_table(ctx: &Ctx, findings: &[Finding]) -> Table {
    let mut t = Table::new(&[
        ("SIZE", Align::Right),
        ("STALE", Align::Right),
        ("CATEGORY", Align::Left),
        ("PATH", Align::Left),
        ("EVIDENCE", Align::Left),
    ]);
    let out = &ctx.out;
    for f in findings {
        let path = if f.kind == EntryKind::Provider {
            f.path.to_string_lossy().into_owned()
        } else {
            out.path_styled(&f.path)
        };
        let mut evidence = f.evidence.clone();
        if let Some(n) = &f.note {
            evidence = format!("{evidence}; {n}");
        }
        let category = if f.regenerable {
            out.accent(&f.category)
        } else {
            out.yellow(&f.category)
        };
        t.row(vec![
            out.size(f.reclaimable),
            f.stale_days.map(|d| out.age_days(d)).unwrap_or_default(),
            category,
            path,
            out.italic(&evidence),
        ]);
    }
    t
}

pub fn stale(ctx: &Ctx, path: Option<&Path>) -> Result<Outcome> {
    let days = stale_threshold(ctx)?;
    let (scan, mut findings, provider_errors) = collect(ctx, path)?;
    let outcome = ctx.scan_issues(&scan);
    findings.retain(|f| f.regenerable && f.stale_days.is_some_and(|d| d >= days));
    // Large and long untouched first.
    findings.sort_by(|a, b| b.score().cmp(&a.score()).then(a.path.cmp(&b.path)));
    let refs: Vec<&Finding> = findings.iter().collect();
    let rep = SuspectsReport {
        scan: ScanInfo::from_scan(&scan),
        stale_days: days,
        categories: report::categories(&refs, days),
        findings: findings.iter().map(FindingOut::from).collect(),
        provider_errors,
    };
    if ctx.out.json {
        ctx.out.print_json("stale", &rep)?;
        return Ok(outcome);
    }
    if findings.is_empty() && !ctx.out.csv {
        println!(
            "Nothing regenerable untouched for {days}+ days in {}.",
            ctx.out.path(scan.tree.root_path())
        );
        return Ok(outcome);
    }
    item_table(ctx, &findings).print(&ctx.out);
    if !ctx.out.csv {
        let total: u64 = findings.iter().map(|f| f.reclaimable).sum();
        let out = &ctx.out;
        println!(
            "\n{} in {} item(s) untouched for {days}+ days. Review with {}.",
            out.green(&out.bold(&format_size(total))),
            findings.len(),
            out.accent(&format!(
                "`fagia clean {} --older {days}d`",
                out.path(scan.tree.root_path())
            ))
        );
    }
    Ok(outcome)
}
