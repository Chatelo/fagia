//! `fagia trash`: what is in the trash, and emptying it. Emptying is a
//! permanent delete, so it needs a typed confirmation that `-y` does not
//! skip, the same rule as `clean --permanent`.

use super::{Ctx, Outcome};
use crate::output::{Align, Table};
use anyhow::{Result, bail};
use dialoguer::{Input, theme::ColorfulTheme};
use fagia_core::actions::log::ActionLog;
use fagia_core::actions::trash::{self, TrashEntry};
use fagia_core::actions::{self, ItemOutcome};
use fagia_core::model::now_epoch;
use fagia_core::paths::JsonPath;
use fagia_core::report::{TrashItemOut, TrashReport};
use fagia_core::size::{format_delta, format_size};
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn item_out(e: &TrashEntry) -> TrashItemOut {
    TrashItemOut {
        path: JsonPath::new(&e.path),
        original: e.original.as_deref().map(JsonPath::new),
        deleted_at: e.deleted_at,
        size: e.size,
    }
}

pub fn run(ctx: &Ctx, empty: bool, limit: usize) -> Result<Outcome> {
    let platform = ctx.session.platform.as_ref();
    let dirs = trash::trash_dirs(platform);
    let mut entries = trash::list(platform);
    let now = now_epoch();
    if let Some(days) = ctx.older_days()? {
        // Unknown deletion dates are kept: older-than cannot be proven.
        entries.retain(|e| {
            e.deleted_at
                .is_some_and(|t| now - t >= (days * 86_400) as i64)
        });
    }
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let out = &ctx.out;
    if out.json {
        if empty {
            bail!("--empty needs a typed confirmation on a terminal and cannot run with --json");
        }
        out.print_json(
            "trash",
            TrashReport {
                trash_dirs: dirs.iter().map(|d| JsonPath::new(&d.root)).collect(),
                total,
                items: entries.iter().map(item_out).collect(),
                emptied: None,
            },
        )?;
        return Ok(Outcome::Ok);
    }
    if entries.is_empty() {
        println!(
            "{}",
            if ctx.g.older.is_some() {
                "Nothing in the trash is that old."
            } else {
                "The trash is empty."
            }
        );
        return Ok(Outcome::Ok);
    }
    println!(
        "{} {}",
        out.title("Trash"),
        out.dim(
            &dirs
                .iter()
                .map(|d| out.path(&d.root))
                .collect::<Vec<_>>()
                .join(", ")
        )
    );
    let mut t = Table::new(&[
        ("SIZE", Align::Right),
        ("TRASHED", Align::Right),
        ("ORIGINALLY", Align::Left),
    ]);
    let show = if out.verbose || empty && entries.len() <= 200 {
        entries.len()
    } else {
        limit
    };
    for e in entries.iter().take(show) {
        let when = e
            .deleted_at
            .map(|d| {
                format!(
                    "{} ago",
                    fagia_core::size::format_age((now - d).max(0) as u64)
                )
            })
            .unwrap_or_else(|| "?".into());
        let what = match &e.original {
            Some(o) => out.path_styled(o),
            None => out.path(&e.path),
        };
        t.row(vec![out.size(e.size), out.dim(&when), what]);
    }
    t.print(out);
    if entries.len() > show {
        println!(
            "{}",
            out.dim(&format!(
                "…and {} more (--verbose lists all)",
                entries.len() - show
            ))
        );
    }
    println!(
        "\n{} in {} item(s).",
        out.bold(&out.size(total)),
        entries.len()
    );
    if !empty {
        println!(
            "{}",
            out.dim(
                "Empty it with `fagia trash --empty` (permanent; `--older 30d` keeps recent items)."
            )
        );
        return Ok(Outcome::Ok);
    }
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        bail!("emptying the trash needs a typed confirmation on a terminal");
    }
    // Never skipped by -y: this cannot be undone.
    let typed: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Permanently delete {} item(s), {}? This cannot be undone. Type `empty` to confirm",
            entries.len(),
            format_size(total)
        ))
        .allow_empty(true)
        .interact_text()?;
    if typed.trim() != "empty" {
        return Ok(Outcome::Declined);
    }
    let log = ActionLog::new(ActionLog::default_path(&platform.dirs().state));
    let before = platform.fs_stats(&platform.dirs().home).ok();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    let results = actions::empty_trash(&log, &entries, &cancel, &mut |r| {
        if r.outcome != ItemOutcome::Done {
            println!(
                "  {}  {}: {}",
                out.skip_mark(),
                out.path(std::path::Path::new(&r.path.path)),
                out.yellow(r.detail.as_deref().unwrap_or(""))
            );
        }
    });
    let after = platform.fs_stats(&platform.dirs().home).ok();
    let done = results
        .iter()
        .filter(|r| r.outcome == ItemOutcome::Done)
        .count();
    let freed: u64 = results.iter().map(|r| r.bytes).sum();
    println!(
        "{} {done} of {} item(s): {} estimated, {} measured change in free space on ~.",
        out.bold("Deleted"),
        results.len(),
        out.green(&out.bold(&format_size(freed))),
        out.bold(&format_delta(match (before, after) {
            (Some(b), Some(a)) => a.available as i64 - b.available as i64,
            _ => 0,
        }))
    );
    Ok(if done == results.len() {
        Outcome::Ok
    } else {
        Outcome::Partial
    })
}
