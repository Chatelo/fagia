//! `fagia mem`: overview, top apps, forgotten and leaking apps, live view.

use super::{Ctx, Outcome};
use crate::cli::MemArgs;
use crate::output::{Align, Out, Table};
use anyhow::Result;
use fagia_core::providers::docker;
use fagia_core::ram::detect::LeakSettings;
use fagia_core::ram::{self, ReportOptions, Sampler};
use fagia_core::report::MemReport;
use fagia_core::size::{format_age, format_size, parse_duration};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub fn run(ctx: &Ctx, a: &MemArgs) -> Result<Outcome> {
    if a.live {
        return live(ctx, a);
    }
    let cfg = &ctx.session.config.ram;
    let rules = ctx.session.rules.mem_rules();
    let platform = ctx.session.platform.as_ref();
    let mut sampler = None;
    let mut leaks = HashMap::new();
    let mut watched = None;
    let snap = if let Some(d) = &a.watch {
        let dur = parse_duration(d)?;
        let settings = LeakSettings::from_config(cfg);
        let need = Duration::from_secs_f64(settings.warmup_secs + settings.min_window_secs);
        if dur < need && !ctx.out.json {
            ctx.out.note(&format!(
                "note: leak detection needs at least {} of samples (warm-up plus window); set [ram] leak_* to shorten",
                format_age(need.as_secs())
            ));
        }
        let mut s = Sampler::new(ram::sampler_capacity(cfg, dur));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
        let started = Instant::now();
        let every = Duration::from_secs(cfg.sample_seconds.max(1));
        let mut cache = ram::MemCache::default();
        let mut last = ram::snapshot_with(platform, rules, Some(&mut cache))?;
        s.record(&last.groups);
        while started.elapsed() < dur && !stop.load(Ordering::SeqCst) {
            if std::io::stderr().is_terminal() {
                eprint!(
                    "\rwatching: {} of {} (Ctrl-C to stop early)   ",
                    format_age(started.elapsed().as_secs()),
                    format_age(dur.as_secs())
                );
            }
            let next = Instant::now() + every;
            while Instant::now() < next && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
            }
            last = ram::snapshot_with(platform, rules, Some(&mut cache))?;
            s.record(&last.groups);
        }
        if std::io::stderr().is_terminal() {
            eprintln!();
        }
        leaks = s.leaks(&settings);
        watched = Some(started.elapsed().as_secs_f64());
        sampler = Some(s);
        last
    } else {
        ram::snapshot(platform, rules)?
    };
    let mut rep = ram::report(
        &snap,
        &ReportOptions {
            platform,
            cfg,
            sampler: sampler.as_ref(),
            leaks: &leaks,
            expand: a.expand,
            with_forgotten: true,
        },
    );
    rep.watched_secs = watched;
    rep.containers = docker::containers(platform).unwrap_or_default();
    filter(&mut rep, a);
    if ctx.out.json {
        ctx.out.print_json("mem", &rep)?;
        return Ok(Outcome::Ok);
    }
    print(&ctx.out, &rep, a);
    Ok(Outcome::Ok)
}

fn filter(rep: &mut MemReport, a: &MemArgs) {
    if a.dev {
        rep.groups.retain(|g| g.dev_suspect);
    }
    if a.forgotten {
        rep.groups.retain(|g| g.forgotten.is_some());
    }
    if a.watch.is_some() {
        // Leak suspects first.
        rep.groups
            .sort_by_key(|g| (g.leak.is_none(), std::cmp::Reverse(g.fair)));
    }
}

fn sparkline(values: &[u64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.len() < 2 {
        return String::new();
    }
    let tail = &values[values.len().saturating_sub(width)..];
    let (lo, hi) = (
        *tail.iter().min().unwrap_or(&0),
        *tail.iter().max().unwrap_or(&0),
    );
    tail.iter()
        .map(|v| {
            let i = if hi == lo {
                0
            } else {
                ((v - lo) * 7 / (hi - lo)) as usize
            };
            BARS[i]
        })
        .collect()
}

pub fn print(out: &Out, rep: &MemReport, a: &MemArgs) {
    print!("{}", render(out, rep, a));
}

/// The whole memory view as text, so the live view can redraw it in one
/// write without flicker.
pub fn render(out: &Out, rep: &MemReport, a: &MemArgs) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let s = &rep.system;
    let used = 1.0 - s.available as f64 / s.total.max(1) as f64;
    let _ = writeln!(
        o,
        "{}   {} {}  {} available of {}",
        out.bold("RAM"),
        out.usage_bar(used, 24),
        out.bold(&format!("{:.0}%", used * 100.0)),
        out.bold(&format_size(s.available)),
        format_size(s.total)
    );
    let _ = writeln!(
        o,
        "      {} {} {} {} {} {}",
        out.dim("programs"),
        out.size(s.used_by_programs),
        out.dim("· file cache"),
        format_size(s.file_cache),
        out.dim("(released on demand) · shared"),
        format_size(s.shared)
    );
    if s.swap_total > 0 {
        let busy = s.swap_in_rate + s.swap_out_rate > 100.0;
        let activity = format!(
            "activity {:.0} in / {:.0} out pages/s",
            s.swap_in_rate, s.swap_out_rate
        );
        let _ = writeln!(
            o,
            "{}  {} used of {} · {}",
            out.bold("Swap"),
            format_size(s.swap_used),
            format_size(s.swap_total),
            if busy {
                out.red(&format!("{activity} (heavy: the machine is short of RAM)"))
            } else {
                out.dim(&activity)
            }
        );
    }
    if let Some(z) = &s.zram {
        let _ = writeln!(
            o,
            "{}  {} stored in {} of RAM",
            out.bold("zram"),
            format_size(z.original),
            format_size(z.used)
        );
    }
    for shm in rep.shm.iter().filter(|x| x.used > 0) {
        let _ = writeln!(
            o,
            "{}  {} {}",
            out.dim(&shm.mount.path),
            format_size(shm.used),
            out.dim("of files held in RAM")
        );
    }
    let _ = writeln!(o);
    if rep.groups.is_empty() {
        let _ = writeln!(
            o,
            "{}",
            if a.forgotten {
                "No forgotten dev processes."
            } else {
                "No matching apps."
            }
        );
    } else {
        let mut headers = vec![("FAIR", Align::Right)];
        if out.fancy {
            headers.push(("", Align::Left));
        }
        headers.extend([
            ("UNIQUE", Align::Right),
            ("PROCS", Align::Right),
            ("AGE", Align::Right),
            ("APP", Align::Left),
        ]);
        let mut t = Table::new(&headers);
        let max = rep.groups.iter().map(|g| g.fair).max().unwrap_or(1).max(1);
        for g in rep.groups.iter().take(a.limit) {
            let mut app = out.bold(&g.display);
            if let Some(c) = &g.category {
                app = format!("{app} {}", out.dim(&format!("[{c}]")));
            }
            let mut tags: Vec<String> = g
                .flags
                .iter()
                .map(|f| out.badge(f, if f == "leaking" { "30;41" } else { "30;43" }))
                .collect();
            if !g.fully_measured {
                tags.push(out.italic("resident"));
            }
            if g.system {
                tags.push(out.dim("system"));
            }
            if !tags.is_empty() {
                app = format!("{app}  {}", tags.join(" "));
            }
            let spark = sparkline(&g.trend, 16);
            if !spark.is_empty() {
                app = format!("{app}  {}", out.accent(&spark));
            }
            let mut row = vec![out.size(g.fair)];
            if out.fancy {
                row.push(out.bar(g.fair as f64 / max as f64, 10));
            }
            row.extend([
                out.dim(&format_size(g.unique)),
                out.dim(&g.processes.to_string()),
                out.dim(&format_age(g.age_secs)),
                app,
            ]);
            t.row(row);
            if a.expand {
                for m in &g.members {
                    let mut row = vec![out.dim(&format_size(m.fair))];
                    if out.fancy {
                        row.push(String::new());
                    }
                    row.extend([
                        out.dim(&m.unique.map(format_size).unwrap_or_default()),
                        String::new(),
                        String::new(),
                        out.dim(&format!("  {} {}", m.pid, truncate(&m.command, 80))),
                    ]);
                    t.row(row);
                }
            }
        }
        o.push_str(&t.render(out));
    }
    for g in rep
        .groups
        .iter()
        .filter(|g| g.forgotten.is_some() || g.leak.is_some())
    {
        if let Some(f) = &g.forgotten {
            let _ = writeln!(
                o,
                "{} {}: {f}",
                out.badge("forgotten", "30;43"),
                out.bold(&g.display)
            );
        }
        if let Some(l) = &g.leak {
            let _ = writeln!(
                o,
                "{} {}: grew {} → {} at {:.1} MiB/min over {}",
                out.badge("leaking", "30;41"),
                out.bold(&g.display),
                format_size(l.start),
                out.red(&format_size(l.end)),
                l.mib_per_min,
                format_age(l.window_secs as u64)
            );
        }
    }
    if !rep.containers.is_empty() {
        let _ = writeln!(o, "\n{}", out.title("Docker containers"));
        for c in &rep.containers {
            let _ = writeln!(
                o,
                "  {}  {}  {}",
                out.size_pad(c.memory, 10),
                out.bold(&c.name),
                out.dim(&format!("(stop: fagia kill docker:{})", c.name))
            );
        }
    }
    if rep.unmeasured > 0 {
        let _ = writeln!(
            o,
            "\n{}",
            out.dim(&format!(
                "{} process(es) of other users are shown at resident size (fair share needs root).",
                rep.unmeasured
            ))
        );
    }
    if rep.groups.iter().any(|g| g.forgotten.is_some()) {
        let _ = writeln!(o, "{}", out.dim("Quit an app with `fagia kill <APP>`."));
    }
    o
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

/// Redraws every two seconds until Ctrl-C.
fn live(ctx: &Ctx, a: &MemArgs) -> Result<Outcome> {
    let cfg = &ctx.session.config.ram;
    let platform = ctx.session.platform.as_ref();
    let rules = ctx.session.rules.mem_rules();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    let mut sampler = Sampler::new(120);
    let leaks = HashMap::new();
    let mut cache = ram::MemCache::default();
    // Alternate screen with a hidden cursor, restored however the loop ends.
    struct Screen;
    impl Drop for Screen {
        fn drop(&mut self) {
            print!("\x1b[?25h\x1b[?1049l");
            let _ = std::io::stdout().flush();
        }
    }
    print!("\x1b[?1049h\x1b[?25l");
    let _screen = Screen;
    while !stop.load(Ordering::SeqCst) {
        let snap = ram::snapshot_with(platform, rules, Some(&mut cache))?;
        sampler.record(&snap.groups);
        let mut rep = ram::report(
            &snap,
            &ReportOptions {
                platform,
                cfg,
                sampler: Some(&sampler),
                leaks: &leaks,
                expand: a.expand,
                with_forgotten: false,
            },
        );
        filter(&mut rep, a);
        // Redraw in place: home, each line cleared to its end, then clear
        // below. One write per frame, so no flicker.
        let mut frame = String::from("\x1b[H");
        for line in render(&ctx.out, &rep, a).lines() {
            frame.push_str(line);
            frame.push_str("\x1b[K\n");
        }
        frame.push_str(&format!(
            "\n{}\x1b[K\n\x1b[J",
            ctx.out
                .dim("live view, refreshed every 2 s · Ctrl-C to quit")
        ));
        {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(frame.as_bytes())?;
            stdout.flush()?;
        }
        let next = Instant::now() + Duration::from_secs(2);
        while Instant::now() < next && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(Outcome::Ok)
}
