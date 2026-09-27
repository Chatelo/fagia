use super::Outcome;
use crate::cli::{ConfigCmd, GlobalArgs};
use anyhow::{Context, Result, bail};
use fagia_core::config::{Config, TEMPLATE};
use fagia_core::platform;

pub fn run(g: &GlobalArgs, action: Option<&ConfigCmd>) -> Result<Outcome> {
    let plat = platform::current();
    let path = g
        .config
        .clone()
        .unwrap_or_else(|| Config::default_path(plat.dirs()));
    match action.unwrap_or(&ConfigCmd::Path) {
        ConfigCmd::Path => {
            if g.json {
                println!(
                    "{}",
                    serde_json::json!({ "schema_version": 1, "command": "config", "path": path, "exists": path.exists() })
                );
            } else {
                println!(
                    "{}{}",
                    path.display(),
                    if path.exists() {
                        ""
                    } else {
                        "  (not created yet; `fagia config init`)"
                    }
                );
            }
        }
        ConfigCmd::Init => {
            if path.exists() {
                bail!("{} already exists; not overwriting", path.display());
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            std::fs::write(&path, TEMPLATE)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("Wrote {}", path.display());
        }
        ConfigCmd::Edit => {
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, TEMPLATE)?;
            }
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".into());
            let status = std::process::Command::new(&editor)
                .arg(&path)
                .status()
                .with_context(|| format!("starting {editor}"))?;
            if !status.success() {
                bail!("{editor} exited with {status}");
            }
            // Report problems right away rather than on the next command.
            Config::load(&path)?;
        }
        ConfigCmd::Show => {
            let cfg = Config::load(&path)?;
            let home = &plat.dirs().home;
            let v = serde_json::json!({
                "schema_version": 1,
                "command": "config",
                "path": path,
                "exists": path.exists(),
                "min_size": cfg.general.min_size,
                "stale_days": cfg.general.stale_days,
                "save_history": cfg.general.save_history,
                "history_keep": cfg.general.history_keep,
                "protect": cfg.protect_paths(home),
                "exclude": cfg.exclude_globs(home),
                "ram": {
                    "sample_seconds": cfg.ram.sample_seconds,
                    "forgotten_hours": cfg.ram.forgotten_hours,
                    "forgotten_idle_days": cfg.ram.forgotten_idle_days,
                    "grace_seconds": cfg.ram.grace_seconds,
                },
                "user_rules": cfg.rule.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
                "user_mem_rules": cfg.mem_rule.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(Outcome::Ok)
}
