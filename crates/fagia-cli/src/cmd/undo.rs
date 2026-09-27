//! `fagia undo`: restore what a clean moved to the trash.

use super::{Ctx, Outcome};
use crate::output::{Align, Table};
use anyhow::Result;
use dialoguer::{Confirm, theme::ColorfulTheme};
use fagia_core::actions::{self, ItemOutcome, log::ActionLog};
use fagia_core::report::UndoReport;
use fagia_core::size::{format_age, format_size};
use std::io::IsTerminal;
use std::path::PathBuf;

pub fn run(ctx: &Ctx, list: bool, run: Option<&str>) -> Result<Outcome> {
    let log = ActionLog::new(ActionLog::default_path(&ctx.session.platform.dirs().state));
    let out = &ctx.out;
    if list {
        let runs = actions::runs(&log);
        if out.json {
            out.print_json(
                "undo",
                UndoReport {
                    run: String::new(),
                    dry_run: true,
                    runs,
                    results: Vec::new(),
                },
            )?;
            return Ok(Outcome::Ok);
        }
        if runs.is_empty() {
            println!("No clean runs in {}.", out.path(log.path()));
            return Ok(Outcome::Ok);
        }
        let now = fagia_core::model::now_epoch();
        let mut t = Table::new(&[
            ("RUN", Align::Left),
            ("WHEN", Align::Right),
            ("ITEMS", Align::Right),
            ("SIZE", Align::Right),
            ("RESTORABLE", Align::Right),
        ]);
        for r in &runs {
            t.row(vec![
                r.run.clone(),
                format!("{} ago", format_age((now - r.time).max(0) as u64)),
                r.items.to_string(),
                format_size(r.bytes),
                r.restorable.to_string(),
            ]);
        }
        t.print(out);
        return Ok(Outcome::Ok);
    }
    let (run_id, pending) = actions::pending_restores(&log, run)?;
    if !out.json {
        println!(
            "{} {}",
            out.title("Restore"),
            out.dim(&format!("run {run_id}"))
        );
        for e in &pending {
            println!(
                "  {}  {}",
                format_size(e.size),
                out.path(&PathBuf::from(&e.target))
            );
        }
    }
    if !ctx.g.yes {
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            anyhow::bail!("no terminal to confirm on; pass -y");
        }
        let ok = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("Restore {} item(s)?", pending.len()))
            .default(false)
            .interact()?;
        if !ok {
            return Ok(Outcome::Declined);
        }
    }
    let (run_id, results) = actions::undo(&log, Some(&run_id))?;
    let all_done = results.iter().all(|r| r.outcome == ItemOutcome::Done);
    if out.json {
        out.print_json(
            "undo",
            UndoReport {
                run: run_id,
                dry_run: false,
                runs: Vec::new(),
                results,
            },
        )?;
    } else {
        for r in &results {
            let p = out.path(&PathBuf::from(&r.path.path));
            match r.outcome {
                ItemOutcome::Done => println!(
                    "  {}  {p}",
                    if out.fancy {
                        out.green("✔ restored")
                    } else {
                        out.green("restored")
                    }
                ),
                _ => println!(
                    "  {}  {p}: {}",
                    if out.fancy {
                        out.yellow("⊘ skipped")
                    } else {
                        out.yellow("skipped")
                    },
                    r.detail.as_deref().unwrap_or("")
                ),
            }
        }
    }
    Ok(if all_done {
        Outcome::Ok
    } else {
        Outcome::Partial
    })
}
