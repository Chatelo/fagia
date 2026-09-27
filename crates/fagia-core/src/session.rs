//! Everything a front end needs to run commands: platform, config and
//! rules, loaded once.

use crate::config::Config;
use crate::disk::{Progress, Scan, ScanOptions, scan};
use crate::model::Finding;
use crate::paths::expand_tilde;
use crate::platform::{self, Platform};
use crate::providers;
use crate::rules::RuleSet;
use crate::store::{Snapshot, Store};
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Session {
    pub platform: Arc<dyn Platform>,
    pub config: Config,
    pub rules: RuleSet,
    pub config_path: PathBuf,
}

/// Scan settings a front end collects from its flags.
#[derive(Debug, Clone, Default)]
pub struct ScanFlags {
    pub cross_fs: bool,
    pub network_fs: bool,
    pub excludes: Vec<String>,
    pub record_min: Option<u64>,
}

impl Session {
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        Self::with_platform(platform::current(), config_path)
    }

    pub fn with_platform(platform: Arc<dyn Platform>, config_path: Option<&Path>) -> Result<Self> {
        let config_path = config_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Config::default_path(platform.dirs()));
        let config = Config::load(&config_path)?;
        let rules = RuleSet::load(&config, platform.dirs())?;
        let s = Self {
            platform,
            config,
            rules,
            config_path,
        };
        let problems = s.rules.validate(&s.protected_paths(None));
        if !problems.is_empty() {
            return Err(Error::Config {
                path: s.config_path.clone(),
                message: problems.join("; "),
            });
        }
        Ok(s)
    }

    pub fn home(&self) -> &Path {
        &self.platform.dirs().home
    }

    /// Resolves a user-given root (default: home) to an absolute path.
    pub fn resolve_root(&self, path: Option<&Path>) -> Result<PathBuf> {
        let raw = path.map_or_else(
            || self.home().to_path_buf(),
            |p| expand_tilde(&p.to_string_lossy(), self.home()),
        );
        std::fs::canonicalize(&raw).map_err(|source| Error::Io { path: raw, source })
    }

    pub fn scan_options(&self, root: PathBuf, flags: &ScanFlags) -> ScanOptions {
        let mut excludes = self.config.exclude_globs(self.home());
        excludes.extend(
            flags
                .excludes
                .iter()
                .map(|g| expand_tilde(g, self.home()).to_string_lossy().into_owned()),
        );
        let mut o = ScanOptions::new(root);
        o.cross_fs = flags.cross_fs;
        o.network_fs = flags.network_fs;
        o.excludes = excludes;
        if let Some(m) = flags.record_min {
            o.record_min = m;
        }
        o
    }

    pub fn scan(&self, opts: &ScanOptions, progress: &Progress) -> Result<Scan> {
        scan(self.platform.as_ref(), Some(&self.rules), opts, progress)
    }

    /// Paths that may never be cleaned: system folders, home itself, the
    /// user's protect list, and protections from project files.
    pub fn protected_paths(&self, scan: Option<&Scan>) -> Vec<PathBuf> {
        let mut p = self.platform.system_protected_paths();
        p.push(self.home().to_path_buf());
        p.extend(self.config.protect_paths(self.home()));
        if let Some(s) = scan {
            p.extend(s.project_protect.iter().cloned());
        }
        p
    }

    pub fn store(&self) -> Result<Store> {
        Store::open(&Store::default_path(&self.platform.dirs().state))
    }

    /// Saves a snapshot of `scan` for `diff`, unless history is off or the
    /// scan was cancelled (a partial total would read as a huge shrink).
    pub fn save_snapshot(&self, scan: &Scan) -> Result<bool> {
        if !self.config.general.save_history || scan.cancelled {
            return Ok(false);
        }
        let snap = Snapshot::from_scan(scan, &self.platform.hostname());
        self.store()?
            .save(&snap, self.config.general.history_keep)?;
        Ok(true)
    }

    /// Findings from providers (Docker, ...), plus errors from those that
    /// could not run.
    pub fn provider_findings(&self) -> (Vec<Finding>, Vec<String>) {
        let mut found = Vec::new();
        let mut errors = Vec::new();
        for p in providers::all() {
            match p.findings() {
                Ok(f) => found.extend(f),
                Err(e) => errors.push(format!("{}: {e}", p.name())),
            }
        }
        (found, errors)
    }
}
