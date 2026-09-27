//! `fagia clean`: dry-run list, pick, confirm, then act through the gate.

use super::{Ctx, Outcome, suspects};
use crate::output::{Align, Out, Table};
use anyhow::{Result, bail};
use dialoguer::{Confirm, Input, MultiSelect, theme::ColorfulTheme};
use fagia_core::actions::{Gate, ItemOutcome, ItemResult, Mode, Plan};
use fagia_core::report::CleanReport;
use fagia_core::size::{format_delta, format_size};
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct CleanArgs<'a> {
    pub path: Option<&'a Path>,
    pub permanent: bool,
    pub as_root: bool,
    pub dry_run: bool,
}

pub fn run(ctx: &Ctx, a: CleanArgs) -> Result<Outcome> {
    let mode = if a.permanent {
        Mode::Permanent
    } else {
        Mode::Trash
    };
    let root = ctx.session.resolve_root(a.path)?;
    // Refuse before scanning when running as root without --as-root.
    Gate::new(&ctx.session, &root, None, a.as_root)?;
    let (scan, findings, _) = suspects::collect(ctx, Some(&root))?;
    let outcome = ctx.scan_issues(&scan);
    let gate = Gate::new(&ctx.session, &root, Some(&scan), a.as_root)?;
    let mut plan = gate.plan(findings, mode);
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    if ctx.out.json {
        if a.dry_run {
            ctx.out.print_json("clean", CleanReport::new(&plan, true))?;
            return Ok(outcome);
        }
        if !ctx.g.yes {
            bail!("--json needs -y (or --dry-run): there is no one to confirm with");
        }
        if mode == Mode::Permanent {
            bail!("--permanent needs a typed confirmation and cannot run with --json");
        }
        let res = gate.execute(&plan, &AtomicBool::new(false), &mut |_| {});
        let partial = !res.all_done();
        let mut rep = CleanReport::new(&plan, false);
        rep.result = Some(res);
        ctx.out.print_json("clean", rep)?;
        return Ok(if partial { Outcome::Partial } else { outcome });
    }

    let out = &ctx.out;
    print_plan(out, &plan);
    if plan.items.is_empty() || a.dry_run {
        return Ok(outcome);
    }
    if !ctx.g.yes {
        if !interactive {
            bail!(
                "no terminal to confirm on; pass -y to act on the preselected items, or --dry-run"
            );
        }
        if !pick(out, &mut plan)? {
            return Ok(Outcome::Declined);
        }
    }
    let count = plan.items.iter().filter(|i| i.selected).count();
    if count == 0 {
        println!("Nothing selected.");
        return Ok(outcome);
    }
    let verb = match mode {
        Mode::Trash => "Move",
        Mode::Permanent => "Permanently delete",
    };
    let question = format!(
        "{verb} {count} item(s), {}{}?",
        format_size(plan.selected_bytes()),
        if mode == Mode::Trash {
            " to the trash"
        } else {
            ""
        }
    );
    if !ctx.g.yes
        && !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(question)
            .default(false)
            .interact()?
    {
        return Ok(Outcome::Declined);
    }
    if mode == Mode::Permanent {
        // The second confirmation is never skipped, not even by -y.
        if !interactive {
            bail!("--permanent needs a typed confirmation on a terminal");
        }
        let typed: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("This cannot be undone. Type `delete` to confirm")
            .allow_empty(true)
            .interact_text()?;
        if typed.trim() != "delete" {
            return Ok(Outcome::Declined);
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    // Finish the current item on Ctrl-C, then stop; never leave half an item.
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    let res = gate.execute(&plan, &cancel, &mut |r: &ItemResult| print_result(out, r));
    println!();
    let done = res
        .results
        .iter()
        .filter(|r| r.outcome == ItemOutcome::Done)
        .count();
    println!(
        "{} {done} of {} item(s): {} estimated, {} measured change in free space.",
        out.bold(match mode {
            Mode::Trash => "Trashed",
            Mode::Permanent => "Deleted",
        }),
        res.results.len(),
        out.green(&out.bold(&format_size(res.estimate))),
        out.bold(&format_delta(res.freed_measured)),
    );
    if mode == Mode::Trash && done > 0 {
        println!(
            "{}",
            out.dim(&format!(
                "Trashed items still use disk space until the trash is emptied. Restore with `fagia undo` (run {}).",
                res.run
            ))
        );
    }
    if res.interrupted {
        println!(
            "{}",
            out.yellow("Interrupted: the remaining items were left untouched.")
        );
    }
    Ok(if res.all_done() {
        outcome
    } else {
        Outcome::Partial
    })
}

fn print_plan(out: &Out, plan: &Plan) {
    if plan.items.is_empty() {
        println!("Nothing to clean under {}.", out.path(&plan.root));
        return;
    }
    println!(
        "{} under {} {}",
        out.title("Dry run"),
        out.bold(&out.path(&plan.root)),
        match plan.mode {
            Mode::Trash => out.dim("(move to trash)"),
            Mode::Permanent => out.red(&out.bold("(PERMANENT delete)")),
        }
    );
    let mut t = Table::new(&[
        ("", Align::Left),
        ("SIZE", Align::Right),
        ("STALE", Align::Right),
        ("CATEGORY", Align::Left),
        ("PATH", Align::Left),
        ("EVIDENCE", Align::Left),
    ]);
    for i in &plan.items {
        let f = &i.finding;
        let mark = match (&i.refusal, i.selected, out.fancy) {
            (Some(_), _, false) => "skip".to_string(),
            (None, true, false) => "[x]".to_string(),
            (None, false, false) => "[ ]".to_string(),
            (Some(_), _, true) => out.yellow("⊘"),
            (None, true, true) => out.green("✔"),
            (None, false, true) => out.dim("○"),
        };
        let evidence = match &i.refusal {
            Some(why) => out.yellow(&format!("skipped: {why}")),
            None => out.italic(&f.evidence),
        };
        let path = if f.kind == fagia_core::model::EntryKind::Provider {
            f.path.to_string_lossy().into_owned()
        } else if i.refusal.is_some() {
            out.dim(&out.path(&f.path))
        } else {
            out.path_styled(&f.path)
        };
        t.row(vec![
            mark,
            out.size(f.reclaimable),
            f.stale_days.map(|d| out.age_days(d)).unwrap_or_default(),
            if f.regenerable {
                out.accent(&f.category)
            } else {
                out.yellow(&f.category)
            },
            path,
            evidence,
        ]);
    }
    t.print(out);
    println!(
        "Selected: {} in {} item(s). {} = preselected (regenerable, low risk).",
        out.green(&out.bold(&format_size(plan.selected_bytes()))),
        plan.items.iter().filter(|i| i.selected).count(),
        if out.fancy {
            out.green("✔")
        } else {
            "[x]".into()
        }
    );
}

/// Lets the user change the selection. Returns false when they abort.
fn pick(out: &Out, plan: &mut Plan) -> Result<bool> {
    let choices: Vec<usize> = plan
        .items
        .iter()
        .enumerate()
        .filter(|(_, i)| i.refusal.is_none())
        .map(|(n, _)| n)
        .collect();
    if choices.is_empty() {
        return Ok(true);
    }
    let labels: Vec<String> = choices
        .iter()
        .map(|&n| {
            let f = &plan.items[n].finding;
            format!(
                "{:>10}  {:<14} {}",
                format_size(f.reclaimable),
                f.category,
                out.path(&f.path)
            )
        })
        .collect();
    let defaults: Vec<bool> = choices.iter().map(|&n| plan.items[n].selected).collect();
    let picked = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Space toggles, Enter accepts, Esc cancels")
        .items(&labels)
        .defaults(&defaults)
        .interact_opt()?;
    let Some(picked) = picked else {
        return Ok(false);
    };
    let indices: Vec<usize> = picked.into_iter().map(|p| choices[p]).collect();
    plan.select(&indices);
    Ok(true)
}

fn print_result(out: &Out, r: &ItemResult) {
    let p = out.path_styled(Path::new(&r.path.path));
    let detail = r.detail.as_deref().unwrap_or("");
    match r.outcome {
        ItemOutcome::Done => println!("  {}  {}  {p}", out.ok_mark(), out.size_pad(r.bytes, 10)),
        ItemOutcome::Skipped => println!("  {}  {p}: {}", out.skip_mark(), out.yellow(detail)),
        ItemOutcome::Failed => println!("  {}  {p}: {}", out.fail_mark(), out.red(detail)),
    }
}
