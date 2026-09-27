//! `fagia dupes --clean` / `--link`: keep one copy of each duplicate set.

use super::{Ctx, Outcome};
use crate::cli::DupesCli;
use crate::output::Out;
use anyhow::{Result, bail};
use dialoguer::{Confirm, theme::ColorfulTheme};
use fagia_core::actions::Gate;
use fagia_core::actions::dedupe::CopyKind;
use fagia_core::actions::dedupe::{self, DedupeMode, DedupeOptions, DedupePlan};
use fagia_core::actions::{ItemOutcome, ItemResult};
use fagia_core::disk::Scan;
use fagia_core::disk::dupe_dirs::DirSet;
use fagia_core::disk::dupes::DupeSet;
use fagia_core::report::DedupeReport;
use fagia_core::size::{format_delta, format_size};
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub fn run(
    ctx: &Ctx,
    scan: &Scan,
    sets: &[DupeSet],
    dirs: &[DirSet],
    a: &DupesCli,
    outcome: Outcome,
) -> Result<Outcome> {
    let mode = if a.link {
        DedupeMode::Link
    } else {
        DedupeMode::Trash
    };
    let gate = Gate::new(&ctx.session, scan.tree.root_path(), Some(scan), a.as_root)?;
    let keep_under = a
        .keep_under
        .iter()
        .map(|p| ctx.session.resolve_root(Some(p)))
        .collect::<fagia_core::Result<Vec<_>>>()?;
    let plan = dedupe::plan(
        &gate,
        sets,
        dirs,
        &DedupeOptions {
            mode,
            keep_under,
            any_type: a.any_type,
        },
    );
    let out = &ctx.out;
    if out.json {
        if a.dry_run {
            out.print_json("dedupe", DedupeReport::new(&plan, true))?;
            return Ok(outcome);
        }
        if !ctx.g.yes {
            bail!("--json needs -y (or --dry-run): there is no one to confirm with");
        }
        let res = dedupe::execute(&gate, &plan, &AtomicBool::new(false), &mut |_| {});
        let partial = res.results.iter().any(|r| r.outcome != ItemOutcome::Done);
        let mut rep = DedupeReport::new(&plan, false);
        rep.result = Some(res);
        out.print_json("dedupe", rep)?;
        return Ok(if partial { Outcome::Partial } else { outcome });
    }

    print_plan(out, &plan);
    if plan.removals() == 0 || a.dry_run {
        return Ok(outcome);
    }
    if !ctx.g.yes {
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            bail!("no terminal to confirm on; pass -y, or --dry-run to only look");
        }
        let prompt = match mode {
            DedupeMode::Trash => format!(
                "Move {} extra copies ({}) to the trash?",
                plan.removals(),
                format_size(plan.reclaimable())
            ),
            DedupeMode::Link => format!(
                "Replace {} extra copies with hard links (frees {})?",
                plan.removals(),
                format_size(plan.reclaimable())
            ),
        };
        if !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .default(false)
            .interact()?
        {
            return Ok(Outcome::Declined);
        }
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    // Finish the current file on Ctrl-C, then stop.
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    let res = dedupe::execute(&gate, &plan, &cancel, &mut |r: &ItemResult| {
        print_result(out, r)
    });
    let done = res
        .results
        .iter()
        .filter(|r| r.outcome == ItemOutcome::Done)
        .count();
    println!();
    println!(
        "{} {done} of {} copies: {} estimated, {} measured change in free space.",
        out.bold(match mode {
            DedupeMode::Trash => "Trashed",
            DedupeMode::Link => "Linked",
        }),
        res.results.len(),
        out.green(&out.bold(&format_size(res.estimate))),
        out.bold(&format_delta(res.freed_measured)),
    );
    if mode == DedupeMode::Trash && done > 0 {
        println!(
            "{}",
            out.dim(&format!(
                "Space is freed when the trash is emptied (`fagia trash --empty`). Restore with `fagia undo` (run {}).",
                res.run
            ))
        );
    }
    if res.interrupted {
        println!(
            "{}",
            out.yellow("Interrupted: the remaining copies were left untouched.")
        );
    }
    Ok(
        if res.results.iter().all(|r| r.outcome == ItemOutcome::Done) {
            outcome
        } else {
            Outcome::Partial
        },
    )
}

fn print_plan(out: &Out, plan: &DedupePlan) {
    let verb = match plan.mode {
        DedupeMode::Trash => "trash",
        DedupeMode::Link => "link",
    };
    let (acting, idle): (Vec<_>, Vec<_>) = plan.sets.iter().partition(|s| !s.remove.is_empty());
    if acting.is_empty() {
        println!("Nothing to remove: every duplicate is app data, protected or in use.");
    } else {
        println!(
            "{} {}",
            out.title("Dry run"),
            out.dim(match plan.mode {
                DedupeMode::Trash => "(extra copies move to the trash)",
                DedupeMode::Link => "(extra copies become hard links to the kept one)",
            })
        );
    }
    for s in &acting {
        let copies = s.keep.len() + s.remove.len() + s.skipped.len();
        println!(
            "\n{}  {}",
            out.size(s.real * s.remove.len() as u64),
            out.dim(&match s.kind {
                CopyKind::File => format!("{copies} copies of {}", format_size(s.size)),
                CopyKind::Folder => format!(
                    "{copies} copies of a folder: {} files, {}",
                    s.files,
                    format_size(s.real)
                ),
            })
        );
        for k in &s.keep {
            println!(
                "  {} {}",
                if out.fancy {
                    out.green("✔ keep  ")
                } else {
                    "keep    ".into()
                },
                out.path_styled(k)
            );
        }
        for r in &s.remove {
            let mark = if out.fancy {
                out.red(&format!("✖ {verb:<6}"))
            } else {
                format!("{verb:<8}")
            };
            println!("  {mark} {}", out.path(r));
        }
        for (p, why) in &s.skipped {
            let mark = if out.fancy {
                out.yellow("⊘ skip  ")
            } else {
                "skip    ".into()
            };
            println!("  {mark} {}  {}", out.dim(&out.path(p)), out.yellow(why));
        }
    }
    if !idle.is_empty() {
        println!(
            "\n{}",
            out.dim(&format!(
                "{} set(s) left alone entirely (app data, git storage, protected or in use){}",
                idle.len(),
                if out.verbose {
                    ":"
                } else {
                    "; --verbose lists them."
                }
            ))
        );
        if out.verbose {
            for s in &idle {
                for (p, why) in &s.skipped {
                    println!(
                        "  {} {}  {}",
                        out.skip_mark(),
                        out.dim(&out.path(p)),
                        out.dim(why)
                    );
                }
            }
        }
    }
    if !acting.is_empty() {
        println!(
            "\nWill {verb} {} copies in {} set(s), freeing {}.",
            plan.removals(),
            acting.len(),
            out.green(&out.bold(&format_size(plan.reclaimable())))
        );
    }
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
