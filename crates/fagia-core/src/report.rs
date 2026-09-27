//! Machine-readable reports. Every JSON document carries `schema_version`;
//! fields may be added within a version, never removed or renamed.

use crate::disk::walk::{FLAG_COLLAPSED, NONE};
use crate::disk::{FileRec, Scan};
use crate::model::{EntryKind, Finding, Risk};
use crate::paths::JsonPath;
use crate::platform::FsStats;
use crate::rules::Rule;
use schemars::JsonSchema;
use serde::Serialize;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, JsonSchema)]
pub struct Envelope<T> {
    pub schema_version: u32,
    pub command: String,
    #[serde(flatten)]
    pub data: T,
}

impl<T> Envelope<T> {
    pub fn new(command: &str, data: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            command: command.to_string(),
            data,
        }
    }
}

/// Facts about the scan every disk report includes.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ScanInfo {
    #[serde(flatten)]
    pub root: JsonPath,
    pub total_real: u64,
    pub total_apparent: u64,
    pub elapsed_ms: u64,
    /// Entries that could not be read (permission errors).
    pub errors: u64,
    pub error_samples: Vec<String>,
    /// Mount points not entered.
    pub skipped: Vec<JsonPath>,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    pub cancelled: bool,
}

impl ScanInfo {
    pub fn from_scan(s: &Scan) -> Self {
        Self {
            root: JsonPath::new(s.tree.root_path()),
            total_real: s.total_real(),
            total_apparent: s.total_apparent(),
            elapsed_ms: s.elapsed.as_millis() as u64,
            errors: s.errors,
            error_samples: s.error_samples.clone(),
            skipped: s.skipped_mounts.iter().map(|p| JsonPath::new(p)).collect(),
            notes: s.notes.clone(),
            warnings: s.warnings.clone(),
            cancelled: s.cancelled,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FindingOut {
    #[serde(flatten)]
    pub path: JsonPath,
    pub kind: EntryKind,
    pub category: String,
    pub rule_id: String,
    pub evidence: String,
    pub regenerable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regenerate: Option<String>,
    pub risk: Risk,
    pub real: u64,
    pub apparent: u64,
    pub reclaimable: u64,
    pub files: u64,
    pub modified: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_days: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<JsonPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Preselected by `clean` (regenerable and low risk).
    pub auto_select: bool,
}

impl From<&Finding> for FindingOut {
    fn from(f: &Finding) -> Self {
        Self {
            path: JsonPath::new(&f.path),
            kind: f.kind,
            category: f.category.to_string(),
            rule_id: f.rule_id.to_string(),
            evidence: f.evidence.clone(),
            regenerable: f.regenerable,
            regenerate: f.regenerate.as_deref().map(str::to_string),
            risk: f.risk,
            real: f.real,
            apparent: f.apparent,
            reclaimable: f.reclaimable,
            files: f.files,
            modified: f.mtime,
            stale_days: f.stale_days,
            project: f.project_dir.as_deref().map(JsonPath::new),
            note: f.note.clone(),
            auto_select: Rule::auto_select(f.regenerable, f.risk),
        }
    }
}

// ---- top ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TopEntry {
    #[serde(flatten)]
    pub path: JsonPath,
    pub real: u64,
    pub apparent: u64,
    pub files: u64,
    /// Fraction of the scanned total.
    pub share: f64,
    pub depth: usize,
    /// Files directly inside the parent, shown as one row.
    pub loose_files: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TopReport {
    pub scan: ScanInfo,
    pub depth: usize,
    pub entries: Vec<TopEntry>,
}

/// Largest folders exactly `depth` levels below the root.
pub fn top(scan: &Scan, depth: usize, limit: usize, min_size: u64, apparent: bool) -> TopReport {
    let t = &scan.tree;
    let total = if apparent {
        scan.total_apparent()
    } else {
        scan.total_real()
    }
    .max(1);
    let size = |real: u64, app: u64| if apparent { app } else { real };
    let mut level = vec![0u32];
    for _ in 0..depth {
        level = level.iter().flat_map(|&id| t.children(id)).collect();
    }
    let mut entries: Vec<TopEntry> = level
        .iter()
        .map(|&id| {
            let n = t.node(id);
            TopEntry {
                path: JsonPath::new(&t.path(id)),
                real: n.real,
                apparent: n.apparent,
                files: n.files,
                share: size(n.real, n.apparent) as f64 / total as f64,
                depth,
                loose_files: false,
                category: (n.finding != NONE && n.flags & FLAG_COLLAPSED != 0)
                    .then(|| scan.findings[n.finding as usize].category.to_string()),
            }
        })
        .collect();
    if depth == 1 {
        // Bytes of the root's own files, so the rows add up to the total.
        let root = t.node(0);
        let (cr, ca, cf) = t.children(0).fold((0, 0, 0), |(r, a, f), c| {
            let n = t.node(c);
            (r + n.real, a + n.apparent, f + n.files)
        });
        let (lr, la) = (
            root.real.saturating_sub(cr),
            root.apparent.saturating_sub(ca),
        );
        if root.files > cf {
            entries.push(TopEntry {
                path: JsonPath::new(t.root_path()),
                real: lr,
                apparent: la,
                files: root.files - cf,
                share: size(lr, la) as f64 / total as f64,
                depth: 0,
                loose_files: true,
                category: None,
            });
        }
    }
    entries.retain(|e| size(e.real, e.apparent) >= min_size);
    entries.sort_by(|a, b| {
        size(b.real, b.apparent)
            .cmp(&size(a.real, a.apparent))
            .then(a.path.path.cmp(&b.path.path))
    });
    entries.truncate(limit);
    TopReport {
        scan: ScanInfo::from_scan(scan),
        depth,
        entries,
    }
}

// ---- big ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BigFile {
    #[serde(flatten)]
    pub path: JsonPath,
    pub real: u64,
    pub apparent: u64,
    pub modified: i64,
    pub hard_links: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BigReport {
    pub scan: ScanInfo,
    pub files: Vec<BigFile>,
}

pub fn big_file(scan: &Scan, f: &FileRec) -> BigFile {
    BigFile {
        path: JsonPath::new(&scan.file_path(f)),
        real: f.stat.real,
        apparent: f.stat.apparent,
        modified: f.stat.mtime,
        hard_links: f.stat.nlink,
        category: f.rule.as_ref().map(|m| m.category.to_string()),
    }
}

/// Largest single files; each hard-linked inode appears once.
pub fn big(scan: &Scan, limit: usize, min_size: u64, apparent: bool) -> BigReport {
    let key = |f: &FileRec| {
        if apparent {
            f.stat.apparent
        } else {
            f.stat.real
        }
    };
    let mut files: Vec<&FileRec> = scan
        .files
        .iter()
        .filter(|f| f.counted && key(f) >= min_size)
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(key(f)));
    files.truncate(limit);
    BigReport {
        scan: ScanInfo::from_scan(scan),
        files: files.into_iter().map(|f| big_file(scan, f)).collect(),
    }
}

// ---- summary ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiskSummary {
    #[serde(flatten)]
    pub path: JsonPath,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs_type: Option<String>,
    pub stats: FsStats,
    pub reserved: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MemorySummary {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeletedOpenOut {
    #[serde(flatten)]
    pub path: JsonPath,
    pub real: u64,
    pub pid: u32,
    pub process: String,
}

/// Space `du` cannot see: why `df` and `du` disagree.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct Unseen {
    pub deleted_open: Vec<DeletedOpenOut>,
    pub deleted_open_total: u64,
    pub reserved: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_note: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SummaryReport {
    pub disk: DiskSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySummary>,
    pub scan: ScanInfo,
    pub top_suspects: Vec<FindingOut>,
    pub suspects_total: u64,
    pub unseen: Unseen,
}

// ---- suspects / stale ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CategoryRow {
    pub category: String,
    pub count: u64,
    pub real: u64,
    pub reclaimable: u64,
    /// Reclaimable bytes in items at least `stale_days` old.
    pub stale: u64,
    pub regenerable: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SuspectsReport {
    pub scan: ScanInfo,
    pub stale_days: u64,
    pub categories: Vec<CategoryRow>,
    pub findings: Vec<FindingOut>,
    /// Providers that could not run, with the reason.
    pub provider_errors: Vec<String>,
}

/// Groups findings by category, regenerable categories first, each sorted
/// by reclaimable size.
pub fn categories(findings: &[&Finding], stale_days: u64) -> Vec<CategoryRow> {
    let mut rows: Vec<CategoryRow> = Vec::new();
    for f in findings {
        let row = match rows.iter_mut().find(|r| r.category == *f.category) {
            Some(r) => r,
            None => {
                rows.push(CategoryRow {
                    category: f.category.to_string(),
                    count: 0,
                    real: 0,
                    reclaimable: 0,
                    stale: 0,
                    regenerable: f.regenerable,
                });
                rows.last_mut().expect("just pushed")
            }
        };
        row.count += 1;
        row.real += f.real;
        row.reclaimable += f.reclaimable;
        if f.stale_days.is_some_and(|d| d >= stale_days) {
            row.stale += f.reclaimable;
        }
    }
    rows.sort_by(|a, b| {
        b.regenerable
            .cmp(&a.regenerable)
            .then(b.reclaimable.cmp(&a.reclaimable))
    });
    rows
}

// ---- media ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MediaFile {
    #[serde(flatten)]
    pub path: JsonPath,
    pub real: u64,
    pub modified: i64,
    /// Detected from the file header rather than the extension.
    pub sniffed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MediaFolder {
    #[serde(flatten)]
    pub path: JsonPath,
    pub real: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MediaGroup {
    pub kind: crate::disk::media::MediaKind,
    pub label: String,
    pub count: u64,
    pub real: u64,
    pub largest: Vec<MediaFile>,
    pub folders: Vec<MediaFolder>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MediaReport {
    pub scan: ScanInfo,
    pub total: u64,
    pub groups: Vec<MediaGroup>,
}

// ---- dupes ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DupeSetOut {
    pub size: u64,
    pub wasted: u64,
    pub copies: Vec<JsonPath>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DupeDirOut {
    /// Disk bytes of one copy.
    pub real: u64,
    pub files: u64,
    pub wasted: u64,
    pub copies: Vec<JsonPath>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DupesReport {
    pub scan: ScanInfo,
    pub min_size: u64,
    /// How names had to match; content always had to be identical.
    pub match_by: crate::disk::dupes::MatchBy,
    /// Copies had to share a name (ignoring copy markers) as well as content.
    pub match_names: bool,
    /// Wasted by identical files and folders together.
    pub total_wasted: u64,
    /// Identical folders (outermost only).
    pub folders: Vec<DupeDirOut>,
    /// Identical files outside those folders.
    pub sets: Vec<DupeSetOut>,
    /// Near-duplicates: reported, never removed automatically.
    pub similar: Vec<crate::disk::similar::SimilarGroup>,
    /// Analyses that could not run, and why.
    pub notes: Vec<String>,
}

// ---- diff ----

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiffReport {
    #[serde(flatten)]
    pub root: JsonPath,
    pub snapshots: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<crate::store::Diff>,
}

// ---- rules ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuleOut {
    pub id: String,
    pub category: String,
    pub kind: crate::rules::RuleKind,
    pub names: Vec<String>,
    pub extensions: Vec<String>,
    pub paths: Vec<String>,
    pub require_sibling: Vec<String>,
    pub require_inside: Vec<String>,
    pub regenerable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regenerate: Option<String>,
    pub risk: Risk,
    pub priority: i32,
    pub enabled: bool,
    pub origin: crate::rules::Origin,
}

impl From<&Rule> for RuleOut {
    fn from(r: &Rule) -> Self {
        Self {
            id: r.id.to_string(),
            category: r.category.to_string(),
            kind: r.kind,
            names: r.names.clone(),
            extensions: r.extensions.clone(),
            paths: r.raw_paths.clone(),
            require_sibling: r.require_sibling.patterns.clone(),
            require_inside: r.require_inside.patterns.clone(),
            regenerable: r.regenerable,
            regenerate: r.regenerate.as_deref().map(str::to_string),
            risk: r.risk,
            priority: r.priority,
            enabled: r.enabled,
            origin: r.origin,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RulesReport {
    pub rules: Vec<RuleOut>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RuleTestReport {
    #[serde(flatten)]
    pub path: JsonPath,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner: Option<String>,
    pub checks: Vec<crate::rules::RuleCheck>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ValidateReport {
    pub ok: bool,
    pub rules: usize,
    pub mem_rules: usize,
    pub problems: Vec<String>,
}

// ---- clean / undo ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlanItemOut {
    #[serde(flatten)]
    pub finding: FindingOut,
    pub measured: u64,
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CleanReport {
    #[serde(flatten)]
    pub root: JsonPath,
    pub mode: crate::actions::Mode,
    pub dry_run: bool,
    pub selected_bytes: u64,
    pub items: Vec<PlanItemOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<crate::actions::CleanResult>,
}

impl CleanReport {
    pub fn new(plan: &crate::actions::Plan, dry_run: bool) -> Self {
        Self {
            root: JsonPath::new(&plan.root),
            mode: plan.mode,
            dry_run,
            selected_bytes: plan.selected_bytes(),
            items: plan
                .items
                .iter()
                .map(|i| PlanItemOut {
                    finding: FindingOut::from(&i.finding),
                    measured: i.measured,
                    selected: i.selected,
                    refusal: i.refusal.clone(),
                })
                .collect(),
            result: None,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UndoReport {
    pub run: String,
    pub dry_run: bool,
    pub runs: Vec<crate::actions::RunSummary>,
    pub results: Vec<crate::actions::ItemResult>,
}

// ---- memory ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SystemMemOut {
    pub total: u64,
    /// What programs can still get without swapping: the number to watch.
    pub available: u64,
    pub used_by_programs: u64,
    pub file_cache: u64,
    pub shared: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// Pages per second.
    pub swap_in_rate: f64,
    pub swap_out_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zram: Option<crate::platform::Zram>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShmOut {
    #[serde(flatten)]
    pub mount: JsonPath,
    pub used: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MemberOut {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub command: String,
    pub fair: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique: Option<u64>,
    pub resident: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap: Option<u64>,
    pub started_at: i64,
    /// `pss` or `rss` (fair share not readable, usually another user's).
    pub measure: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LeakOut {
    pub mib_per_min: f64,
    pub start: u64,
    pub end: u64,
    pub window_secs: f64,
    pub fit: f64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GroupOut {
    pub name: String,
    pub display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub uid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<JsonPath>,
    pub processes: usize,
    /// Fair share (PSS); resident for members that could not be measured.
    pub fair: u64,
    /// Memory only this app holds: roughly what quitting it frees.
    pub unique: u64,
    pub resident: u64,
    pub swap: u64,
    pub fully_measured: bool,
    pub age_secs: u64,
    pub dev_suspect: bool,
    pub system: bool,
    pub unsaved_work: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forgotten: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leak: Option<LeakOut>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trend: Vec<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberOut>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MemReport {
    pub system: SystemMemOut,
    pub processes: usize,
    /// Processes shown at resident size because fair share was unreadable.
    pub unmeasured: usize,
    pub groups: Vec<GroupOut>,
    pub shm: Vec<ShmOut>,
    pub containers: Vec<crate::providers::docker::Container>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watched_secs: Option<f64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SignalReport {
    pub plan: crate::actions::kill::SignalPlan,
    pub action: crate::actions::kill::SignalKind,
    pub results: Vec<crate::actions::kill::SignalResult>,
    /// PIDs still running after the grace period.
    pub still_running: Vec<u32>,
}

// ---- dedupe / trash ----

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DedupeSetOut {
    pub kind: crate::actions::dedupe::CopyKind,
    pub files: u64,
    pub size: u64,
    pub real: u64,
    pub keep: Vec<JsonPath>,
    pub remove: Vec<JsonPath>,
    pub skipped: Vec<crate::actions::dedupe::SkippedCopy>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DedupeReport {
    #[serde(flatten)]
    pub root: JsonPath,
    pub mode: crate::actions::dedupe::DedupeMode,
    pub dry_run: bool,
    pub reclaimable: u64,
    pub removals: usize,
    pub sets: Vec<DedupeSetOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<crate::actions::dedupe::DedupeResult>,
}

impl DedupeReport {
    pub fn new(plan: &crate::actions::dedupe::DedupePlan, dry_run: bool) -> Self {
        Self {
            root: JsonPath::new(&plan.root),
            mode: plan.mode,
            dry_run,
            reclaimable: plan.reclaimable(),
            removals: plan.removals(),
            sets: plan
                .sets
                .iter()
                .map(|s| DedupeSetOut {
                    kind: s.kind,
                    files: s.files,
                    size: s.size,
                    real: s.real,
                    keep: s.keep.iter().map(|p| JsonPath::new(p)).collect(),
                    remove: s.remove.iter().map(|p| JsonPath::new(p)).collect(),
                    skipped: s
                        .skipped
                        .iter()
                        .map(|(p, r)| crate::actions::dedupe::SkippedCopy {
                            path: JsonPath::new(p),
                            reason: r.clone(),
                        })
                        .collect(),
                })
                .collect(),
            result: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TrashItemOut {
    #[serde(flatten)]
    pub path: JsonPath,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original: Option<JsonPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
    pub size: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TrashReport {
    pub trash_dirs: Vec<JsonPath>,
    pub total: u64,
    pub items: Vec<TrashItemOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emptied: Option<Vec<crate::actions::ItemResult>>,
}

/// JSON Schemas for every report, keyed by command. `docs/schema/` holds
/// the committed copies; a test keeps them in step.
pub fn schemas() -> Vec<(&'static str, serde_json::Value)> {
    fn s<T: JsonSchema>() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Envelope<T>)).unwrap_or_default()
    }
    vec![
        ("top", s::<TopReport>()),
        ("big", s::<BigReport>()),
        ("summary", s::<SummaryReport>()),
        ("suspects", s::<SuspectsReport>()),
        ("media", s::<MediaReport>()),
        ("dupes", s::<DupesReport>()),
        ("diff", s::<DiffReport>()),
        ("rules", s::<RulesReport>()),
        ("rules-test", s::<RuleTestReport>()),
        ("rules-validate", s::<ValidateReport>()),
        ("clean", s::<CleanReport>()),
        ("undo", s::<UndoReport>()),
        ("dedupe", s::<DedupeReport>()),
        ("trash", s::<TrashReport>()),
        ("mem", s::<MemReport>()),
        ("signal", s::<SignalReport>()),
    ]
}
