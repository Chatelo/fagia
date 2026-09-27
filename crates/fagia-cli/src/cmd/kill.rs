//! `fagia kill`, `pause`, `resume`: signals through the action gate.

use super::{Ctx, Outcome};
use anyhow::{Result, bail};
use dialoguer::{Confirm, theme::ColorfulTheme};
use fagia_core::actions::kill::{self, SignalKind, SignalPlan};
use fagia_core::actions::log::ActionLog;
use fagia_core::ram;
use fagia_core::report::SignalReport;
use fagia_core::size::format_size;
use std::io::IsTerminal;
use std::time::Duration;

pub struct KillOpts<'a> {
    pub app: &'a str,
    pub kind: SignalKind,
    pub force: bool,
    pub allow_other_users: bool,
    pub grace: Option<u64>,
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn confirm(prompt: &str) -> Result<bool> {
    Ok(Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(false)
        .interact()?)
}

pub fn run(ctx: &Ctx, o: KillOpts) -> Result<Outcome> {
    let platform = ctx.session.platform.as_ref();
    let log = ActionLog::new(ActionLog::default_path(&platform.dirs().state));
    let out = &ctx.out;
    if let Some(name) = o.app.strip_prefix("docker:") {
        if o.kind != SignalKind::Quit {
            bail!("containers can only be stopped (fagia kill docker:NAME)");
        }
        if !ctx.g.yes {
            if !interactive() {
                bail!("no terminal to confirm on; pass -y");
            }
            if !confirm(&format!("Stop Docker container {name}?"))? {
                return Ok(Outcome::Declined);
            }
        }
        kill::stop_container(&log, name)?;
        println!("Stopped container {name}.");
        return Ok(Outcome::Ok);
    }
    if o.force && ctx.out.json {
        bail!("--force needs a second confirmation on a terminal and cannot run with --json");
    }
    let snap = ram::snapshot(platform, ctx.session.rules.mem_rules())?;
    let home = platform.dirs().home.clone();
    let found = ram::find_groups(&snap.groups, o.app, Some(&home))?;
    if found.len() > 1 {
        let names: Vec<String> = found
            .iter()
            .map(|g| format!("\"{}\"", g.display_name(Some(&home))))
            .collect();
        bail!(
            "{} apps match {:?}: {}; use the full name or a PID",
            found.len(),
            o.app,
            names.join(", ")
        );
    }
    let group = &found[0];
    let plan = kill::plan(platform, group, Some(&home), o.allow_other_users);
    if !out.json {
        print_plan(ctx, &plan, o.kind);
    }
    if plan.allowed().count() == 0 {
        bail!("nothing in {} may be signalled", plan.group);
    }
    let verb = match o.kind {
        SignalKind::Quit | SignalKind::ForceKill => "Quit",
        SignalKind::Pause => "Pause",
        SignalKind::Resume => "Resume",
    };
    if !ctx.g.yes {
        if !interactive() {
            bail!("no terminal to confirm on; pass -y");
        }
        let frees: u64 = group.uss();
        let prompt = if o.kind == SignalKind::Quit {
            format!(
                "{verb} {} ({} processes, frees about {})?",
                plan.group,
                plan.allowed().count(),
                format_size(frees)
            )
        } else {
            format!(
                "{verb} {} ({} processes)?",
                plan.group,
                plan.allowed().count()
            )
        };
        if !confirm(&prompt)? {
            return Ok(Outcome::Declined);
        }
    }
    let mut results = kill::send(platform, &log, &plan, o.kind);
    let mut still: Vec<u32> = Vec::new();
    if o.kind == SignalKind::Quit {
        let grace = Duration::from_secs(o.grace.unwrap_or(ctx.session.config.ram.grace_seconds));
        let alive = kill::wait_for_exit(platform, &plan, grace);
        if !alive.is_empty() {
            if o.force && interactive() {
                println!(
                    "{} process(es) still running after {}s.",
                    alive.len(),
                    grace.as_secs()
                );
                // The second confirmation is never skipped by -y.
                if confirm("Force kill them (SIGKILL; unsaved work is lost)?")? {
                    let narrowed = kill::narrow(&plan, &alive);
                    results.extend(kill::send(platform, &log, &narrowed, SignalKind::ForceKill));
                    let left = kill::wait_for_exit(platform, &narrowed, Duration::from_secs(2));
                    still = left.iter().map(|k| k.pid).collect();
                } else {
                    still = alive.iter().map(|k| k.pid).collect();
                }
            } else {
                still = alive.iter().map(|k| k.pid).collect();
            }
        }
    }
    let failed = results.iter().any(|r| !r.ok);
    if out.json {
        out.print_json(
            "signal",
            SignalReport {
                plan,
                action: o.kind,
                results,
                still_running: still.clone(),
            },
        )?;
    } else {
        for r in results.iter().filter(|r| !r.ok) {
            println!(
                "  {} pid {}: {}",
                if out.fancy {
                    out.red("✖ failed")
                } else {
                    out.red("failed")
                },
                r.pid,
                r.detail.as_deref().unwrap_or("")
            );
        }
        if still.is_empty() {
            println!(
                "{} {}.",
                if out.fancy {
                    out.green("✔ Done:")
                } else {
                    out.green("Done:")
                },
                verb.to_lowercase() + "d"
            );
        } else {
            println!(
                "{} process(es) still running{}.",
                still.len(),
                if o.force {
                    ""
                } else {
                    "; rerun with --force to offer SIGKILL"
                }
            );
        }
    }
    Ok(if failed || !still.is_empty() {
        Outcome::Partial
    } else {
        Outcome::Ok
    })
}

fn print_plan(ctx: &Ctx, plan: &SignalPlan, kind: SignalKind) {
    let out = &ctx.out;
    println!("{} {}", out.title("Target"), out.bold(&plan.group));
    for t in &plan.targets {
        match &t.refusal {
            None => println!(
                "  {:>7}  {:>10}  {}",
                t.key.pid,
                format_size(t.fair),
                truncate(&t.command, 90)
            ),
            Some(why) => println!(
                "  {:>7}  {}",
                t.key.pid,
                out.yellow(&format!("skipped: {why} ({})", t.name))
            ),
        }
    }
    if plan.unsaved_work && kind == SignalKind::Quit {
        println!(
            "{}",
            out.yellow("warning: this app may hold unsaved work; save it first.")
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}
