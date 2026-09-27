//! Command dispatch and the context every command shares.

mod big;
mod clean;
mod config;
mod dedupe;
mod diff;
mod dupes;
mod kill;
mod media;
mod mem;
mod rules;
mod summary;
mod suspects;
mod top;
mod trash;
mod undo;

use crate::cli::{Cli, Command, GlobalArgs};
use crate::output::Out;
use crate::progress::Spinner;
use anyhow::{Context, Result};
use clap::CommandFactory;
use fagia_core::disk::{Progress, Scan};
use fagia_core::model::Finding;
use fagia_core::session::{ScanFlags, Session};
use fagia_core::size::{parse_duration, parse_size};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

/// How a command finished; maps to the documented exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Finished, but some paths were skipped (permission errors, refusals).
    Partial,
    /// The user declined at the confirmation.
    Declined,
}

impl Outcome {
    pub fn code(self) -> ExitCode {
        ExitCode::from(match self {
            Self::Ok => 0,
            Self::Partial => 3,
            Self::Declined => 4,
        })
    }
}

pub struct Ctx {
    pub session: Session,
    pub out: Out,
    pub g: GlobalArgs,
}

impl Ctx {
    pub fn flags(&self) -> ScanFlags {
        ScanFlags {
            cross_fs: self.g.cross_fs,
            network_fs: self.g.network_fs,
            excludes: self.g.exclude.clone(),
            record_min: None,
        }
    }

    /// `--min-size`, else the config default.
    pub fn min_size(&self) -> Result<u64> {
        match &self.g.min_size {
            Some(s) => Ok(parse_size(s)?),
            None => Ok(self.session.config.min_size()?),
        }
    }

    /// `--older` in days, if given.
    pub fn older_days(&self) -> Result<Option<u64>> {
        self.g
            .older
            .as_deref()
            .map(|s| parse_duration(s).map(|d| d.as_secs() / 86_400))
            .transpose()
            .map_err(Into::into)
    }

    pub fn wants_category(&self, f: &Finding) -> bool {
        self.g.category.is_empty()
            || self.g.category.iter().any(|c| {
                let c = c.to_ascii_lowercase();
                f.category.to_ascii_lowercase().contains(&c)
                    || f.rule_id.to_ascii_lowercase().contains(&c)
            })
    }

    pub fn scan(&self, path: Option<&Path>, flags: ScanFlags) -> Result<Scan> {
        let root = self.session.resolve_root(path)?;
        let opts = self.session.scan_options(root.clone(), &flags);
        let progress = Arc::new(Progress::default());
        let spinner = Spinner::start(
            progress.clone(),
            &format!("Scanning {}", self.out.path(&root)),
        );
        let scan = self
            .session
            .scan(&opts, &progress)
            .with_context(|| format!("scanning {}", root.display()))?;
        drop(spinner);
        if !self.g.no_save
            && let Err(e) = self.session.save_snapshot(&scan)
        {
            self.out
                .note(&format!("warning: could not save scan history: {e}"));
        }
        Ok(scan)
    }

    /// Prints scan caveats to stderr (human mode) and returns the outcome
    /// they imply.
    pub fn scan_issues(&self, scan: &Scan) -> Outcome {
        if !self.out.json {
            for w in &scan.warnings {
                self.out.note(&format!("warning: {w}"));
            }
            for n in &scan.notes {
                self.out.note(&format!("note: {n}"));
            }
            if !scan.skipped_mounts.is_empty() {
                self.out.note(&format!(
                    "note: skipped {} other filesystem(s){}",
                    scan.skipped_mounts.len(),
                    if self.out.verbose {
                        format!(
                            ": {}",
                            scan.skipped_mounts
                                .iter()
                                .map(|p| self.out.path(p))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    } else {
                        " (--cross-fs to include, --verbose to list)".into()
                    }
                ));
            }
            if scan.errors > 0 {
                self.out.note(&format!(
                    "note: {} entr{} could not be read and {} not counted",
                    scan.errors,
                    if scan.errors == 1 { "y" } else { "ies" },
                    if scan.errors == 1 { "is" } else { "are" },
                ));
                if self.out.verbose {
                    for s in &scan.error_samples {
                        self.out.note(&format!("  {s}"));
                    }
                }
            }
        }
        if scan.errors > 0 || scan.cancelled {
            Outcome::Partial
        } else {
            Outcome::Ok
        }
    }
}

pub fn run(cli: Cli) -> Result<Outcome> {
    // Commands that need no session.
    match &cli.command {
        Some(Command::Completions { shell }) => {
            clap_complete::generate(*shell, &mut Cli::command(), "fagia", &mut std::io::stdout());
            return Ok(Outcome::Ok);
        }
        Some(Command::Man) => {
            clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
            return Ok(Outcome::Ok);
        }
        // These must work while the config is broken, to help fix it.
        Some(Command::Config { action }) => return config::run(&cli.global, action.as_ref()),
        Some(Command::Rules {
            action: Some(crate::cli::RulesCmd::Validate),
        }) => return rules::validate(&cli.global),
        _ => {}
    }
    let session = Session::load(cli.global.config.as_deref())?;
    let out = Out::new(
        cli.global.json,
        cli.global.csv,
        cli.global.verbose,
        session.home().to_path_buf(),
    );
    let ctx = Ctx {
        session,
        out,
        g: cli.global,
    };
    match cli.command {
        None => summary::run(&ctx),
        Some(Command::Top { path, depth, limit }) => top::run(&ctx, path.as_deref(), depth, limit),
        Some(Command::Big { path, limit }) => big::run(&ctx, path.as_deref(), limit),
        Some(Command::Suspects { path, list }) => suspects::run(&ctx, path.as_deref(), list),
        Some(Command::Stale { path }) => suspects::stale(&ctx, path.as_deref()),
        Some(Command::Media { path, limit }) => media::run(&ctx, path.as_deref(), limit),
        Some(Command::Dupes(a)) => dupes::run(&ctx, &a),
        Some(Command::Trash { empty, limit }) => trash::run(&ctx, empty, limit),
        Some(Command::Diff { path, limit }) => diff::run(&ctx, path.as_deref(), limit),
        Some(Command::Rules { action }) => rules::run(&ctx, action),
        Some(Command::Clean {
            path,
            permanent,
            as_root,
            dry_run,
        }) => clean::run(
            &ctx,
            clean::CleanArgs {
                path: path.as_deref(),
                permanent,
                as_root,
                dry_run,
            },
        ),
        Some(Command::Undo { list, run }) => undo::run(&ctx, list, run.as_deref()),
        Some(Command::Mem(a)) => mem::run(&ctx, &a),
        Some(Command::Kill(a)) => kill::run(
            &ctx,
            kill::KillOpts {
                app: &a.app,
                kind: fagia_core::actions::kill::SignalKind::Quit,
                force: a.force,
                allow_other_users: a.allow_other_users,
                grace: a.grace,
            },
        ),
        Some(Command::Pause(a)) => kill::run(
            &ctx,
            kill::KillOpts {
                app: &a.app,
                kind: fagia_core::actions::kill::SignalKind::Pause,
                force: false,
                allow_other_users: a.allow_other_users,
                grace: None,
            },
        ),
        Some(Command::Resume(a)) => kill::run(
            &ctx,
            kill::KillOpts {
                app: &a.app,
                kind: fagia_core::actions::kill::SignalKind::Resume,
                force: false,
                allow_other_users: a.allow_other_users,
                grace: None,
            },
        ),
        Some(Command::Ui { path, as_root }) => {
            let root = ctx.session.resolve_root(path.as_deref())?;
            let flags = ctx.flags();
            fagia_tui::run(ctx.session, root, flags, as_root)?;
            Ok(Outcome::Ok)
        }
        Some(Command::Completions { .. } | Command::Man | Command::Config { .. }) => {
            unreachable!("handled before the session loads")
        }
    }
}
