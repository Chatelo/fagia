//! Scan history in SQLite: one snapshot per saved scan, a rolling window
//! per root, and diffs between the last two.

use crate::disk::Scan;
use crate::model::now_epoch;
use crate::paths::JsonPath;
use crate::{Error, Result};
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Folders kept per snapshot, largest first, down to this depth.
const TOP_DEPTH: usize = 3;
const TOP_KEEP: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Snapshot {
    pub id: i64,
    pub taken_at: i64,
    pub host: String,
    pub root: String,
    pub total_real: u64,
    pub categories: BTreeMap<String, u64>,
    /// Largest folders: path and real size.
    pub top: Vec<(String, u64)>,
}

impl Snapshot {
    pub fn from_scan(scan: &Scan, host: &str) -> Self {
        let t = &scan.tree;
        let mut categories: BTreeMap<String, u64> = BTreeMap::new();
        for f in &scan.findings {
            *categories.entry(f.category.to_string()).or_default() += f.real;
        }
        let mut top: Vec<(String, u64)> = t
            .ids()
            .filter(|&id| id != 0 && t.depth(id) <= TOP_DEPTH)
            .map(|id| (t.path(id).to_string_lossy().into_owned(), t.node(id).real))
            .collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        top.truncate(TOP_KEEP);
        Self {
            id: 0,
            taken_at: now_epoch(),
            host: host.to_string(),
            root: t.root_path().to_string_lossy().into_owned(),
            total_real: scan.total_real(),
            categories,
            top,
        }
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn default_path(state_dir: &Path) -> PathBuf {
        state_dir.join("fagia/history.sqlite")
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS snapshots (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 taken_at INTEGER NOT NULL,
                 host TEXT NOT NULL,
                 root TEXT NOT NULL,
                 total_real INTEGER NOT NULL,
                 categories TEXT NOT NULL,
                 top TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS snapshots_root ON snapshots (host, root, taken_at);",
        )?;
        Ok(Self { conn })
    }

    /// Saves a snapshot and keeps only the newest `keep` for its root.
    pub fn save(&self, snap: &Snapshot, keep: usize) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO snapshots (taken_at, host, root, total_real, categories, top) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                snap.taken_at,
                snap.host,
                snap.root,
                snap.total_real as i64,
                json(&snap.categories),
                json(&snap.top)
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "DELETE FROM snapshots WHERE host = ?1 AND root = ?2 AND id NOT IN (
                 SELECT id FROM snapshots WHERE host = ?1 AND root = ?2 ORDER BY taken_at DESC, id DESC LIMIT ?3)",
            params![snap.host, snap.root, keep.max(1) as i64],
        )?;
        Ok(id)
    }

    /// Snapshots of `root`, newest first.
    pub fn list(&self, host: &str, root: &str, limit: usize) -> Result<Vec<Snapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, taken_at, host, root, total_real, categories, top FROM snapshots
             WHERE host = ?1 AND root = ?2 ORDER BY taken_at DESC, id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![host, root, limit as i64], |r| {
            Ok(Snapshot {
                id: r.get(0)?,
                taken_at: r.get(1)?,
                host: r.get(2)?,
                root: r.get(3)?,
                total_real: r.get::<_, i64>(4)? as u64,
                categories: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                top: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Every root with history on this host, with its snapshot count.
    pub fn roots(&self, host: &str) -> Result<Vec<(String, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT root, COUNT(*) FROM snapshots WHERE host = ?1 GROUP BY root ORDER BY root",
        )?;
        let rows = stmt.query_map(params![host], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Grew,
    Shrank,
    Appeared,
    Vanished,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffEntry {
    #[serde(flatten)]
    pub path: JsonPath,
    pub change: Change,
    pub before: u64,
    pub after: u64,
    pub delta: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CategoryDelta {
    pub category: String,
    pub before: u64,
    pub after: u64,
    pub delta: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Diff {
    pub before_taken_at: i64,
    pub after_taken_at: i64,
    pub total_before: u64,
    pub total_after: u64,
    pub total_delta: i64,
    pub categories: Vec<CategoryDelta>,
    pub entries: Vec<DiffEntry>,
}

/// What grew, shrank, appeared or vanished between two snapshots. Only the
/// most specific changed folder is kept: when `a/b` accounts for all of
/// `a`'s growth, `a` is not repeated.
pub fn diff(before: &Snapshot, after: &Snapshot, min_change: u64) -> Diff {
    let b: HashMap<&str, u64> = before.top.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let a: HashMap<&str, u64> = after.top.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let mut entries: Vec<DiffEntry> = Vec::new();
    let mut paths: Vec<&str> = b.keys().chain(a.keys()).copied().collect();
    paths.sort_unstable();
    paths.dedup();
    for p in paths {
        let (old, new) = (b.get(p).copied(), a.get(p).copied());
        let (change, before, after) = match (old, new) {
            (Some(o), Some(n)) if n > o => (Change::Grew, o, n),
            (Some(o), Some(n)) if n < o => (Change::Shrank, o, n),
            (None, Some(n)) => (Change::Appeared, 0, n),
            (Some(o), None) => (Change::Vanished, o, 0),
            _ => continue,
        };
        if before.abs_diff(after) < min_change.max(1) {
            continue;
        }
        entries.push(DiffEntry {
            path: JsonPath::new(Path::new(p)),
            change,
            before,
            after,
            delta: after as i64 - before as i64,
        });
    }
    // Drop a parent whose change is explained by one listed child.
    let explained: Vec<bool> = entries
        .iter()
        .map(|e| {
            entries.iter().any(|c| {
                c.path.path.len() > e.path.path.len()
                    && Path::new(&c.path.path).starts_with(&e.path.path)
                    && c.delta.signum() == e.delta.signum()
                    && (c.delta - e.delta).unsigned_abs() < min_change.max(1)
            })
        })
        .collect();
    let mut entries: Vec<DiffEntry> = entries
        .into_iter()
        .zip(explained)
        .filter(|(_, x)| !x)
        .map(|(e, _)| e)
        .collect();
    entries.sort_by(|x, y| {
        y.delta
            .unsigned_abs()
            .cmp(&x.delta.unsigned_abs())
            .then(x.path.path.cmp(&y.path.path))
    });
    let mut cats: Vec<&String> = before
        .categories
        .keys()
        .chain(after.categories.keys())
        .collect();
    cats.sort();
    cats.dedup();
    let mut categories: Vec<CategoryDelta> = cats
        .into_iter()
        .map(|c| {
            let (o, n) = (
                before.categories.get(c).copied().unwrap_or(0),
                after.categories.get(c).copied().unwrap_or(0),
            );
            CategoryDelta {
                category: c.clone(),
                before: o,
                after: n,
                delta: n as i64 - o as i64,
            }
        })
        .filter(|c| c.delta != 0)
        .collect();
    categories.sort_by_key(|c| std::cmp::Reverse(c.delta.unsigned_abs()));
    Diff {
        before_taken_at: before.taken_at,
        after_taken_at: after.taken_at,
        total_before: before.total_real,
        total_after: after.total_real,
        total_delta: after.total_real as i64 - before.total_real as i64,
        categories,
        entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(t: i64, top: &[(&str, u64)], cats: &[(&str, u64)]) -> Snapshot {
        Snapshot {
            id: 0,
            taken_at: t,
            host: "h".into(),
            root: "/r".into(),
            total_real: top.iter().map(|x| x.1).sum(),
            categories: cats.iter().map(|(c, s)| (c.to_string(), *s)).collect(),
            top: top.iter().map(|(p, s)| (p.to_string(), *s)).collect(),
        }
    }

    #[test]
    fn store_round_trip_and_rolling_window() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("h.sqlite")).unwrap();
        for t in 0..5 {
            store
                .save(
                    &snap(t, &[("/r/a", 10 * t as u64)], &[("Rust build", 5)]),
                    3,
                )
                .unwrap();
        }
        let list = store.list("h", "/r", 10).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].taken_at, 4);
        assert_eq!(list[0].categories["Rust build"], 5);
        assert_eq!(store.roots("h").unwrap(), vec![("/r".to_string(), 3)]);
    }

    #[test]
    fn diff_classifies_changes() {
        let before = snap(
            1,
            &[("/r/a", 100), ("/r/a/x", 90), ("/r/b", 50), ("/r/gone", 30)],
            &[("Node deps", 10)],
        );
        let after = snap(
            2,
            &[("/r/a", 300), ("/r/a/x", 290), ("/r/b", 20), ("/r/new", 40)],
            &[("Node deps", 60)],
        );
        let d = diff(&before, &after, 1);
        let got: Vec<(&str, Change)> = d
            .entries
            .iter()
            .map(|e| (e.path.path.as_str(), e.change))
            .collect();
        assert_eq!(
            got,
            vec![
                ("/r/a/x", Change::Grew),
                ("/r/new", Change::Appeared),
                ("/r/b", Change::Shrank),
                ("/r/gone", Change::Vanished)
            ]
        );
        assert_eq!(d.categories[0].delta, 50);
    }
}
