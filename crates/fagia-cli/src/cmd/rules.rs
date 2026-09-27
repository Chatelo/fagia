use super::{Ctx, Outcome};
use crate::cli::{GlobalArgs, RulesCmd};
use crate::output::{Align, Out, Table};
use anyhow::Result;
use fagia_core::config::Config;
use fagia_core::paths::JsonPath;
use fagia_core::platform;
use fagia_core::report::{RuleOut, RuleTestReport, RulesReport, ValidateReport};
use fagia_core::rules::{Origin, RuleSet};
use fagia_core::session::Session;

pub fn run(ctx: &Ctx, action: Option<RulesCmd>) -> Result<Outcome> {
    match action.unwrap_or(RulesCmd::List) {
        RulesCmd::List => list(ctx),
        RulesCmd::Test { path } => test(ctx, &path),
        RulesCmd::Validate => validate(&ctx.g),
    }
}

fn list(ctx: &Ctx) -> Result<Outcome> {
    let rep = RulesReport {
        rules: ctx
            .session
            .rules
            .rules()
            .iter()
            .map(RuleOut::from)
            .collect(),
    };
    if ctx.out.json {
        ctx.out.print_json("rules", &rep)?;
        return Ok(Outcome::Ok);
    }
    let mut t = Table::new(&[
        ("ID", Align::Left),
        ("CATEGORY", Align::Left),
        ("MATCH", Align::Left),
        ("EVIDENCE", Align::Left),
        ("REGEN", Align::Left),
        ("RISK", Align::Left),
        ("PRIO", Align::Right),
        ("SOURCE", Align::Left),
    ]);
    for r in &rep.rules {
        let what = [
            r.names.join(" "),
            r.extensions
                .iter()
                .map(|e| format!("*.{e}"))
                .collect::<Vec<_>>()
                .join(" "),
            r.paths.join(" "),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
        let mut ev = Vec::new();
        if !r.require_sibling.is_empty() {
            ev.push(format!("sibling {}", r.require_sibling.join("|")));
        }
        if !r.require_inside.is_empty() {
            ev.push(format!("inside {}", r.require_inside.join("|")));
        }
        let mut source = match r.origin {
            Origin::Builtin => "built-in",
            Origin::User => "config",
            Origin::Overridden => "built-in+config",
        }
        .to_string();
        if !r.enabled {
            source.push_str(" (disabled)");
        }
        t.row(vec![
            r.id.clone(),
            r.category.clone(),
            what,
            if ev.is_empty() {
                "-".into()
            } else {
                ev.join(", ")
            },
            if r.regenerable {
                "yes".into()
            } else {
                "no".into()
            },
            format!("{:?}", r.risk).to_lowercase(),
            r.priority.to_string(),
            source,
        ]);
    }
    t.print(&ctx.out);
    Ok(Outcome::Ok)
}

fn test(ctx: &Ctx, raw: &std::path::Path) -> Result<Outcome> {
    let path = ctx.session.resolve_root(Some(raw))?;
    let checks = ctx.session.rules.explain(&path);
    let winner = checks.iter().find(|c| c.matched).map(|c| c.rule_id.clone());
    let rep = RuleTestReport {
        path: JsonPath::new(&path),
        winner,
        checks,
    };
    if ctx.out.json {
        ctx.out.print_json("rules-test", &rep)?;
        return Ok(Outcome::Ok);
    }
    let out = &ctx.out;
    match &rep.winner {
        Some(w) => println!("{} matches rule {}", out.path(&path), out.bold(w)),
        None => println!("{} matches no rule", out.path(&path)),
    }
    if rep.checks.is_empty() {
        println!("No rule names this path, extension or location.");
    }
    for c in &rep.checks {
        let status = if Some(&c.rule_id) == rep.winner.as_ref() {
            out.green("wins ")
        } else if c.matched {
            out.yellow("loses")
        } else {
            out.dim("no   ")
        };
        println!(
            "  {status}  {} ({}, priority {}): {}",
            c.rule_id, c.category, c.priority, c.reason
        );
    }
    Ok(Outcome::Ok)
}

/// Checks the config and rules; exit code 1 when anything is wrong.
pub fn validate(g: &GlobalArgs) -> Result<Outcome> {
    let plat = platform::current();
    let out = Out::new(g.json, g.csv, g.verbose, plat.dirs().home.clone());
    let path = g
        .config
        .clone()
        .unwrap_or_else(|| Config::default_path(plat.dirs()));
    let mut problems = Vec::new();
    let mut counts = (0, 0);
    match Session::with_platform(plat.clone(), Some(&path)) {
        Ok(s) => counts = (s.rules.rules().len(), s.rules.mem_rules().len()),
        Err(e) => problems.push(e.to_string()),
    }
    if problems.is_empty() {
        // Also catch unsafe rules that would only matter with protections.
        let cfg = Config::load(&path)?;
        let rs = RuleSet::load(&cfg, plat.dirs())?;
        let mut protected = plat.system_protected_paths();
        protected.push(plat.dirs().home.clone());
        protected.extend(cfg.protect_paths(&plat.dirs().home));
        problems.extend(rs.validate(&protected));
    }
    let rep = ValidateReport {
        ok: problems.is_empty(),
        rules: counts.0,
        mem_rules: counts.1,
        problems,
    };
    if out.json {
        out.print_json("rules-validate", &rep)?;
    } else if rep.ok {
        println!(
            "{} {} rules and {} memory rules are valid ({}).",
            out.green("ok:"),
            rep.rules,
            rep.mem_rules,
            out.path(&path)
        );
    } else {
        for p in &rep.problems {
            eprintln!("problem: {}", fagia_core::paths::escape_control(p));
        }
    }
    if rep.ok {
        Ok(Outcome::Ok)
    } else {
        anyhow::bail!("{} problem(s) found", rep.problems.len())
    }
}
