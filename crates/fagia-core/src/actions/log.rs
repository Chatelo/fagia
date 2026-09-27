//! The action log: one JSON object per line for every clean, restore and
//! signal, so what fagia did can always be reviewed (and trashed items
//! restored with `fagia undo`).

use crate::model::now_epoch;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Rotate to `actions.jsonl.1` beyond this size.
const ROTATE_AT: u64 = 5 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct LogEntry {
    pub time: i64,
    pub run: String,
    pub action: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_bytes: Option<String>,
    pub size: u64,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Where a trashed item went, for restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash_info: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl LogEntry {
    pub fn new(run: &str, action: &str, target: &Path, size: u64, outcome: &str) -> Self {
        let j = crate::paths::JsonPath::new(target);
        Self {
            time: now_epoch(),
            run: run.to_string(),
            action: action.to_string(),
            target: j.path,
            target_bytes: j.path_bytes,
            size,
            outcome: outcome.to_string(),
            detail: None,
            trash_path: None,
            trash_info: None,
            pid: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActionLog {
    path: PathBuf,
}

impl ActionLog {
    pub fn default_path(state_dir: &Path) -> PathBuf {
        state_dir.join("fagia/actions.jsonl")
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: &LogEntry) -> Result<()> {
        let io = |source| Error::Io {
            path: self.path.clone(),
            source,
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > ROTATE_AT) {
            // rename() replaces the previous rotation; nothing is deleted.
            std::fs::rename(&self.path, self.path.with_extension("jsonl.1")).map_err(io)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(io)?;
        let line = serde_json::to_string(entry).map_err(|e| Error::Other(e.to_string()))?;
        writeln!(f, "{line}").map_err(io)?;
        f.sync_data().map_err(io)
    }

    /// All entries, oldest first (rotated file included).
    pub fn read(&self) -> Vec<LogEntry> {
        let mut out = Vec::new();
        for p in [self.path.with_extension("jsonl.1"), self.path.clone()] {
            if let Ok(f) = std::fs::File::open(&p) {
                out.extend(
                    BufReader::new(f)
                        .lines()
                        .map_while(std::io::Result::ok)
                        .filter_map(|l| serde_json::from_str(&l).ok()),
                );
            }
        }
        out
    }
}

/// `YYYY-MM-DDThh:mm:ss` in UTC.
pub fn format_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00");
        assert_eq!(format_utc(1_790_000_000), "2026-09-21T14:13:20");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00");
    }

    #[test]
    fn append_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ActionLog::new(tmp.path().join("s/actions.jsonl"));
        let mut e = LogEntry::new("r1", "trash", Path::new("/x/target"), 42, "ok");
        e.trash_path = Some("/t/files/target".into());
        log.append(&e).unwrap();
        log.append(&LogEntry::new(
            "r1",
            "trash",
            Path::new("/x/y"),
            1,
            "skipped",
        ))
        .unwrap();
        let back = log.read();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], e);
    }
}
