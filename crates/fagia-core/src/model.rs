//! Shared data model. Every engine and provider produces these types so
//! all front ends can show them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    #[default]
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    /// Measured by a provider (for example Docker images); not a path on
    /// disk and never cleaned by deleting files.
    Provider,
}

/// Why a path was classified: the rule and the evidence that proved it.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub rule_id: Arc<str>,
    pub category: Arc<str>,
    pub evidence: String,
    pub regenerable: bool,
    pub regenerate: Option<Arc<str>>,
    pub risk: Risk,
    /// Folder holding the marker that proved the match; staleness is
    /// measured over this project, not the suspect folder itself.
    pub project_dir: Option<PathBuf>,
}

/// A classified entry plus its evidence.
#[derive(Debug, Clone)]
pub struct Finding {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub rule_id: Arc<str>,
    pub category: Arc<str>,
    pub evidence: String,
    pub regenerable: bool,
    pub regenerate: Option<Arc<str>>,
    pub risk: Risk,
    pub real: u64,
    pub apparent: u64,
    pub files: u64,
    /// What deleting it would actually free (hard links that survive
    /// elsewhere free nothing).
    pub reclaimable: u64,
    pub mtime: i64,
    pub project_dir: Option<PathBuf>,
    pub stale_days: Option<u64>,
    pub note: Option<String>,
}

impl Finding {
    pub fn from_match(path: PathBuf, kind: EntryKind, m: &Match) -> Self {
        Self {
            path,
            kind,
            rule_id: m.rule_id.clone(),
            category: m.category.clone(),
            evidence: m.evidence.clone(),
            regenerable: m.regenerable,
            regenerate: m.regenerate.clone(),
            risk: m.risk,
            real: 0,
            apparent: 0,
            files: 0,
            reclaimable: 0,
            mtime: 0,
            project_dir: m.project_dir.clone(),
            stale_days: None,
            note: None,
        }
    }

    /// Ranking score: large and long-untouched rises to the top.
    pub fn score(&self) -> u128 {
        u128::from(self.reclaimable) * u128::from(self.stale_days.unwrap_or(0).max(1))
    }
}

/// Seconds since the Unix epoch.
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
