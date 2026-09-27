//! The action gate: the only code that deletes, trashes or signals.
//! Front ends build a [`Plan`], show it, get confirmation, then call
//! [`Gate::execute`], which re-checks every item immediately before acting.

pub mod dedupe;
pub mod kill;
pub mod log;
pub mod safety;
pub mod trash;

use crate::disk::Scan;
use crate::model::{EntryKind, Finding, now_epoch};
use crate::paths::JsonPath;
use crate::platform::Platform;
use crate::rules::{Rule, RuleSet};
use crate::session::Session;
use crate::{Error, Result};
use log::{ActionLog, LogEntry};
use rayon::prelude::*;
use safety::{InUse, contained, git_state, measure, protection};
use schemars::JsonSchema;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Move to the trash (default; reversible with `fagia undo`).
    Trash,
    /// Delete for good (`--permanent`, second confirmation).
    Permanent,
}

#[derive(Debug, Clone)]
pub struct PlanItem {
    pub finding: Finding,
    /// Size measured by the gate at plan time; compared again before acting.
    pub measured: u64,
    /// Why this item can never be acted on, if so.
    pub refusal: Option<String>,
    /// Preselected (regenerable, low risk, not refused).
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub root: PathBuf,
    pub mode: Mode,
    pub items: Vec<PlanItem>,
}

impl Plan {
    pub fn selected_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.selected)
            .map(|i| i.finding.reclaimable)
            .sum()
    }

    /// Selects exactly the given indices (refused items stay unselected).
    pub fn select(&mut self, indices: &[usize]) {
        for (i, item) in self.items.iter_mut().enumerate() {
            item.selected = item.refusal.is_none() && indices.contains(&i);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ItemOutcome {
    Done,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ItemResult {
    #[serde(flatten)]
    pub path: JsonPath,
    pub outcome: ItemOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash_path: Option<JsonPath>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CleanResult {
    pub run: String,
    pub mode: Mode,
    pub results: Vec<ItemResult>,
    /// Sum of reclaimable bytes of the items acted on.
    pub estimate: u64,
    /// Change in available space on the root's filesystem, measured.
    pub freed_measured: i64,
    pub interrupted: bool,
}

impl CleanResult {
    pub fn all_done(&self) -> bool {
        self.results.iter().all(|r| r.outcome == ItemOutcome::Done)
    }
}

pub struct Gate<'a> {
    platform: &'a dyn Platform,
    rules: &'a RuleSet,
    root: PathBuf,
    /// Protect themselves and their ancestors only (`/`, home).
    anchors: Vec<PathBuf>,
    /// Protect themselves, their ancestors and everything inside.
    protected: Vec<PathBuf>,
    log: ActionLog,
}

impl<'a> Gate<'a> {
    /// A gate for cleaning under `root`. Refuses to exist when running as
    /// root without `as_root`: under sudo, home, trash and log belong to
    /// someone else.
    pub fn new(
        session: &'a Session,
        root: &Path,
        scan: Option<&Scan>,
        as_root: bool,
    ) -> Result<Self> {
        let project = scan
            .map(|s| s.project_protect.as_slice())
            .unwrap_or_default();
        Self::with_project_protect(session, root, project, as_root)
    }

    /// Like [`Gate::new`], with the scan's project protections passed in
    /// directly, so a gate can be built on another thread than the scan.
    pub fn with_project_protect(
        session: &'a Session,
        root: &Path,
        project_protect: &[PathBuf],
        as_root: bool,
    ) -> Result<Self> {
        let p = session.platform.as_ref();
        let home = session.home().to_path_buf();
        let mut all = session.protected_paths(None);
        all.extend(project_protect.iter().cloned());
        let anchors = vec![PathBuf::from("/"), home.clone()];
        let protected = all.into_iter().filter(|x| !anchors.contains(x)).collect();
        Self::with_parts(
            p,
            &session.rules,
            root,
            anchors,
            protected,
            ActionLog::new(ActionLog::default_path(&p.dirs().state)),
            as_root,
        )
    }

    pub fn with_parts(
        platform: &'a dyn Platform,
        rules: &'a RuleSet,
        root: &Path,
        anchors: Vec<PathBuf>,
        protected: Vec<PathBuf>,
        log: ActionLog,
        as_root: bool,
    ) -> Result<Self> {
        if platform.effective_uid() == 0 && !as_root {
            return Err(Error::Refused(
                "running as root: under sudo the home folder, trash and action log belong to root; pass --as-root if you mean it"
                    .into(),
            ));
        }
        let root = std::fs::canonicalize(root).map_err(|source| Error::Io {
            path: root.to_path_buf(),
            source,
        })?;
        Ok(Self {
            platform,
            rules,
            root,
            anchors,
            protected,
            log,
        })
    }

    pub fn log(&self) -> &ActionLog {
        &self.log
    }

    fn in_trash(&self, path: &Path) -> bool {
        path.starts_with(self.platform.dirs().home_trash())
            || path.components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                s == ".Trash" || s.starts_with(".Trash-")
            })
    }

    /// Checks that hold for the life of the plan.
    fn refusal(&self, f: &Finding, mode: Mode, in_use: &InUse) -> Option<String> {
        if f.kind == EntryKind::Provider {
            return Some(format!(
                "measured by a provider; {}",
                f.note.as_deref().unwrap_or("reclaim with its own tool")
            ));
        }
        self.path_refusal(&f.path, mode == Mode::Trash, in_use)
    }

    /// The path checks every destructive action shares: containment,
    /// symlinks, protected paths, trash, processes using it, git state.
    pub(crate) fn path_refusal(
        &self,
        path: &Path,
        trashing: bool,
        in_use: &InUse,
    ) -> Option<String> {
        let path = match contained(path, &self.root) {
            Ok(p) => p,
            Err(why) => return Some(why),
        };
        if let Some(why) = protection(&path, &self.anchors, &self.protected) {
            return Some(why);
        }
        if trashing && self.in_trash(&path) {
            return Some("already in the trash; empty it with `fagia trash --empty`".into());
        }
        if let Some(pid) = in_use.user_of(&path) {
            return Some(format!("in use by process {pid}"));
        }
        git_state(&path).refusal()
    }

    pub(crate) fn platform(&self) -> &dyn Platform {
        self.platform
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The dry-run plan: every finding with its refusal (if any), measured
    /// size and preselection.
    pub fn plan(&self, findings: Vec<Finding>, mode: Mode) -> Plan {
        let in_use = InUse::snapshot(self.platform);
        let mut items: Vec<PlanItem> = findings
            .into_par_iter()
            .map(|finding| {
                let refusal = self.refusal(&finding, mode, &in_use);
                let measured = if refusal.is_none() {
                    measure(&finding.path)
                } else {
                    0
                };
                let selected =
                    refusal.is_none() && Rule::auto_select(finding.regenerable, finding.risk);
                PlanItem {
                    finding,
                    measured,
                    refusal,
                    selected,
                }
            })
            .collect();
        items.sort_by(|a, b| {
            a.refusal
                .is_some()
                .cmp(&b.refusal.is_some())
                .then(b.finding.reclaimable.cmp(&a.finding.reclaimable))
                .then(a.finding.path.cmp(&b.finding.path))
        });
        Plan {
            root: self.root.clone(),
            mode,
            items,
        }
    }

    /// Re-checks run immediately before acting on one item: anything may
    /// have changed since the dry run.
    fn recheck(
        &self,
        item: &PlanItem,
        mode: Mode,
        in_use: &InUse,
    ) -> std::result::Result<(), String> {
        if let Some(why) = self.refusal(&item.finding, mode, in_use) {
            return Err(why);
        }
        let evidence_holds = self
            .rules
            .explain(&item.finding.path)
            .iter()
            .any(|c| c.matched && c.rule_id == *item.finding.rule_id);
        if !evidence_holds {
            return Err(format!(
                "evidence for rule {} is gone",
                item.finding.rule_id
            ));
        }
        let now = measure(&item.finding.path);
        if now > item.measured + item.measured / 10 {
            return Err(format!(
                "grew from {} to {} since the dry run",
                crate::size::format_size(item.measured),
                crate::size::format_size(now)
            ));
        }
        Ok(())
    }

    #[allow(clippy::disallowed_methods)] // the gate is the one place that deletes
    fn delete(path: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(path)?.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
    }

    /// Acts on the selected items, in order, one at a time. Each item is
    /// done completely or not at all; `cancel` stops before the next item.
    pub fn execute(
        &self,
        plan: &Plan,
        cancel: &AtomicBool,
        on_item: &mut dyn FnMut(&ItemResult),
    ) -> CleanResult {
        let run = format!("{}-{}", now_epoch(), self.platform.current_pid());
        let in_use = InUse::snapshot(self.platform);
        let before = self.platform.fs_stats(&plan.root).ok();
        let mut results = Vec::new();
        let mut interrupted = false;
        for item in plan.items.iter().filter(|i| i.selected) {
            let f = &item.finding;
            let action = match plan.mode {
                Mode::Trash => "trash",
                Mode::Permanent => "delete",
            };
            let mut entry = LogEntry::new(&run, action, &f.path, f.reclaimable, "ok");
            let mut result = ItemResult {
                path: JsonPath::new(&f.path),
                outcome: ItemOutcome::Done,
                detail: None,
                bytes: f.reclaimable,
                trash_path: None,
            };
            if cancel.load(Ordering::SeqCst) {
                interrupted = true;
                result.outcome = ItemOutcome::Skipped;
                result.detail = Some("interrupted".into());
            } else if let Err(why) = self.recheck(item, plan.mode, &in_use) {
                result.outcome = ItemOutcome::Skipped;
                result.detail = Some(why);
            } else {
                let done = match plan.mode {
                    Mode::Trash => trash::choose(self.platform, &f.path)
                        .and_then(|t| trash::move_to(&t, &f.path, now_epoch())),
                    Mode::Permanent => Self::delete(&f.path)
                        .map(|()| trash::Trashed {
                            trash_path: PathBuf::new(),
                            info_path: PathBuf::new(),
                        })
                        .map_err(|source| Error::Io {
                            path: f.path.clone(),
                            source,
                        }),
                };
                match done {
                    Ok(t) if plan.mode == Mode::Trash => {
                        entry.trash_path = Some(t.trash_path.to_string_lossy().into_owned());
                        entry.trash_info = Some(t.info_path.to_string_lossy().into_owned());
                        result.trash_path = Some(JsonPath::new(&t.trash_path));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        result.outcome = match e {
                            Error::Refused(_) => ItemOutcome::Skipped,
                            _ => ItemOutcome::Failed,
                        };
                        result.detail = Some(e.to_string());
                    }
                }
            }
            if result.outcome != ItemOutcome::Done {
                result.bytes = 0;
                entry.outcome = format!("{:?}", result.outcome).to_lowercase();
                entry.detail = result.detail.clone();
            }
            if let Err(e) = self.log.append(&entry) {
                result.detail = Some(format!(
                    "{}; action log not written: {e}",
                    result.detail.as_deref().unwrap_or("done")
                ));
            }
            on_item(&result);
            results.push(result);
        }
        let after = self.platform.fs_stats(&plan.root).ok();
        let freed_measured = match (before, after) {
            (Some(b), Some(a)) => a.available as i64 - b.available as i64,
            _ => 0,
        };
        CleanResult {
            run,
            mode: plan.mode,
            estimate: results.iter().map(|r| r.bytes).sum(),
            results,
            freed_measured,
            interrupted,
        }
    }
}

/// Permanently deletes trashed items, one at a time, logging each. The
/// caller must have shown the list and had it confirmed.
pub fn empty_trash(
    log: &ActionLog,
    entries: &[trash::TrashEntry],
    cancel: &AtomicBool,
    on_item: &mut dyn FnMut(&ItemResult),
) -> Vec<ItemResult> {
    let run = format!("{}-{}", now_epoch(), std::process::id());
    let mut results = Vec::new();
    for e in entries {
        let mut result = ItemResult {
            path: JsonPath::new(&e.path),
            outcome: ItemOutcome::Done,
            detail: e
                .original
                .as_deref()
                .map(|o| format!("was {}", o.display())),
            bytes: e.size,
            trash_path: None,
        };
        if cancel.load(Ordering::SeqCst) {
            result.outcome = ItemOutcome::Skipped;
            result.detail = Some("interrupted".into());
            result.bytes = 0;
        } else if let Err(err) = trash::purge(e) {
            result.outcome = if matches!(err, Error::Refused(_)) {
                ItemOutcome::Skipped
            } else {
                ItemOutcome::Failed
            };
            result.detail = Some(err.to_string());
            result.bytes = 0;
        }
        let mut entry = LogEntry::new(&run, "empty-trash", &e.path, result.bytes, "ok");
        if result.outcome != ItemOutcome::Done {
            entry.outcome = format!("{:?}", result.outcome).to_lowercase();
            entry.detail = result.detail.clone();
        }
        let _ = log.append(&entry);
        on_item(&result);
        results.push(result);
    }
    results
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunSummary {
    pub run: String,
    pub time: i64,
    pub items: usize,
    pub bytes: u64,
    /// Trashed items not yet restored.
    pub restorable: usize,
}

fn restorable(entries: &[LogEntry]) -> Vec<&LogEntry> {
    entries
        .iter()
        .filter(|e| e.action == "trash" && e.outcome == "ok" && e.trash_path.is_some())
        // Gone when the trash was emptied since.
        .filter(|e| {
            e.trash_path
                .as_deref()
                .is_some_and(|p| std::fs::symlink_metadata(p).is_ok())
        })
        .filter(|e| {
            !entries
                .iter()
                .any(|r| r.action == "restore" && r.outcome == "ok" && r.trash_path == e.trash_path)
        })
        .collect()
}

/// Clean runs in the action log, newest first.
pub fn runs(log: &ActionLog) -> Vec<RunSummary> {
    let entries = log.read();
    let left = restorable(&entries);
    let mut out: Vec<RunSummary> = Vec::new();
    for e in entries
        .iter()
        .filter(|e| e.action == "trash" || e.action == "delete")
    {
        let s = match out.iter_mut().find(|s| s.run == e.run) {
            Some(s) => s,
            None => {
                out.push(RunSummary {
                    run: e.run.clone(),
                    time: e.time,
                    items: 0,
                    bytes: 0,
                    restorable: 0,
                });
                out.last_mut().expect("just pushed")
            }
        };
        if e.outcome == "ok" {
            s.items += 1;
            s.bytes += e.size;
        }
    }
    for s in out.iter_mut() {
        s.restorable = left.iter().filter(|e| e.run == s.run).count();
    }
    out.reverse();
    out
}

/// What `undo` would restore for `run` (default: the newest run with
/// anything left), newest item first.
pub fn pending_restores(log: &ActionLog, run: Option<&str>) -> Result<(String, Vec<LogEntry>)> {
    let entries = log.read();
    let left = restorable(&entries);
    let run = match run {
        Some(r) => r.to_string(),
        None => left.last().map(|e| e.run.clone()).ok_or_else(|| {
            Error::Other("nothing to undo: no trashed items in the action log".into())
        })?,
    };
    let items: Vec<LogEntry> = left
        .into_iter()
        .rev()
        .filter(|e| e.run == run)
        .cloned()
        .collect();
    if items.is_empty() {
        return Err(Error::Other(format!(
            "run {run} has nothing left to restore"
        )));
    }
    Ok((run, items))
}

/// Restores what run `run` (default: the newest with anything left)
/// moved to the trash.
pub fn undo(log: &ActionLog, run: Option<&str>) -> Result<(String, Vec<ItemResult>)> {
    let (run, pending) = pending_restores(log, run)?;
    let mut results = Vec::new();
    for e in &pending {
        let original = JsonPath {
            path: e.target.clone(),
            path_bytes: e.target_bytes.clone(),
        }
        .to_path();
        let trash_path = PathBuf::from(e.trash_path.as_deref().unwrap_or_default());
        let info = e.trash_info.as_deref().map(PathBuf::from);
        let outcome = trash::restore(&trash_path, info.as_deref(), &original);
        let mut entry = LogEntry::new(&run, "restore", &original, e.size, "ok");
        entry.trash_path = e.trash_path.clone();
        let mut result = ItemResult {
            path: JsonPath::new(&original),
            outcome: ItemOutcome::Done,
            detail: None,
            bytes: e.size,
            trash_path: Some(JsonPath::new(&trash_path)),
        };
        if let Err(err) = outcome {
            result.outcome = ItemOutcome::Skipped;
            result.detail = Some(err.to_string());
            entry.outcome = "skipped".into();
            entry.detail = result.detail.clone();
        }
        log.append(&entry)?;
        results.push(result);
    }
    Ok((run, results))
}

#[cfg(test)]
mod tests;
