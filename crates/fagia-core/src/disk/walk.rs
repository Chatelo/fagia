//! Parallel directory walker. Sizes by real disk blocks, counts each inode
//! once (attributed deterministically), stays on one filesystem, never
//! follows symlinks, and stops listing inside folders a rule claims.

use crate::config::{PROJECT_FILE, ProjectConfig};
use crate::disk::media;
use crate::error::IoContext;
use crate::model::{EntryKind, Finding, Match};
use crate::platform::{FileStat, Platform, is_cow_fs, is_network_fs};
use crate::{Error, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Decides whether a path belongs to a category. Implemented by the rule
/// set; the walker only calls it.
pub trait Classifier: Sync {
    fn classify_dir(
        &self,
        path: &Path,
        name: &OsStr,
        siblings: &[OsString],
        inside: &mut dyn FnMut() -> Option<Vec<OsString>>,
    ) -> Option<Match>;
    fn classify_file(&self, path: &Path, name: &OsStr, siblings: &[OsString]) -> Option<Match>;
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub root: PathBuf,
    pub cross_fs: bool,
    pub network_fs: bool,
    /// Globs matched against full paths; excluded paths are not counted.
    pub excludes: Vec<String>,
    /// Files at least this large are kept individually (for `big`, `dupes`).
    pub record_min: u64,
    /// Read `.fagia.toml` files found during the walk.
    pub project_config: bool,
    /// Lower-case extensions of files kept whatever their size (for
    /// analyses of small files, such as text duplicates).
    pub record_ext: Vec<String>,
}

impl ScanOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cross_fs: false,
            network_fs: false,
            excludes: Vec::new(),
            record_min: 1 << 20,
            project_config: true,
            record_ext: Vec::new(),
        }
    }
}

/// Live counters for a progress display; `cancel` stops the walk early.
#[derive(Debug, Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

pub const NONE: u32 = u32::MAX;
pub const FLAG_COLLAPSED: u8 = 1;
pub const FLAG_GIT: u8 = 2;
pub const FLAG_UNREADABLE: u8 = 4;

#[derive(Debug, Clone)]
pub struct Node {
    name: u32,
    pub parent: u32,
    first_child: u32,
    child_count: u32,
    pub real: u64,
    pub apparent: u64,
    pub files: u64,
    pub mtime: i64,
    /// Newest modification time of files directly inside (suspect contents
    /// excluded).
    pub newest_file: i64,
    pub finding: u32,
    pub flags: u8,
}

/// Directory tree in an arena. Children of a node are contiguous, and
/// names are interned, so a million-file scan stays small.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    root: PathBuf,
    nodes: Vec<Node>,
    names: Vec<Box<OsStr>>,
}

impl Tree {
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, id: u32) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn name(&self, id: u32) -> &OsStr {
        &self.names[self.nodes[id as usize].name as usize]
    }

    pub fn children(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        let n = &self.nodes[id as usize];
        n.first_child..n.first_child + n.child_count
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != 0 {
            parts.push(self.name(cur));
            cur = self.nodes[cur as usize].parent;
        }
        let mut p = self.root.clone();
        for part in parts.into_iter().rev() {
            p.push(part);
        }
        p
    }

    pub fn depth(&self, mut id: u32) -> usize {
        let mut d = 0;
        while id != 0 {
            id = self.nodes[id as usize].parent;
            d += 1;
        }
        d
    }

    /// True when `id` is `ancestor` or lies below it.
    pub fn is_within(&self, mut id: u32, ancestor: u32) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            if id == 0 {
                return false;
            }
            id = self.nodes[id as usize].parent;
        }
    }

    pub fn find(&self, path: &Path) -> Option<u32> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let mut cur = 0;
        for comp in rel.components() {
            cur = self
                .children(cur)
                .find(|&c| self.name(c) == comp.as_os_str())?;
        }
        Some(cur)
    }

    pub fn ids(&self) -> std::ops::Range<u32> {
        0..self.nodes.len() as u32
    }
}

#[derive(Debug, Clone)]
pub struct FileRec {
    pub dir: u32,
    pub name: Box<OsStr>,
    pub stat: FileStat,
    pub rule: Option<Box<Match>>,
    /// False for a hard link whose inode is attributed to another path.
    pub counted: bool,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub tree: Tree,
    pub files: Vec<FileRec>,
    pub findings: Vec<Finding>,
    pub errors: u64,
    pub error_samples: Vec<String>,
    pub warnings: Vec<String>,
    /// Protected paths contributed by project `.fagia.toml` files.
    pub project_protect: Vec<PathBuf>,
    /// Mount points not entered (other filesystem, network, virtual).
    pub skipped_mounts: Vec<PathBuf>,
    /// Caveats about the numbers (copy-on-write, sparse files).
    pub notes: Vec<String>,
    pub elapsed: Duration,
    pub cancelled: bool,
    pub fs_type: Option<String>,
}

impl Scan {
    pub fn file_path(&self, f: &FileRec) -> PathBuf {
        self.tree.path(f.dir).join(&*f.name)
    }

    pub fn total_real(&self) -> u64 {
        self.tree.nodes.first().map_or(0, |n| n.real)
    }

    pub fn total_apparent(&self) -> u64 {
        self.tree.nodes.first().map_or(0, |n| n.apparent)
    }
}

// ---- walk ----

struct RawFile {
    name: OsString,
    stat: FileStat,
    rule: Option<Match>,
    hardlinked: bool,
}

type InodeKey = (u64, u64);

/// One entry per hard-linked inode (not per link), so memory scales with
/// unique inodes even when a cache links the same file into many places.
struct InodeEntry {
    /// Lexicographically smallest path seen: the attribution winner.
    path: Box<Path>,
    /// Raw id of the directory node that owns the winning path.
    owner: u64,
    stat: FileStat,
}

const SHARDS: usize = 64;

#[derive(Default)]
struct RawDir {
    id: u64,
    name: OsString,
    real: u64,
    apparent: u64,
    files: u64,
    mtime: i64,
    newest_file: i64,
    flags: u8,
    children: Vec<RawDir>,
    files_recs: Vec<RawFile>,
    links: Vec<InodeKey>,
    rule: Option<Match>,
}

impl RawDir {
    fn new(id: u64, name: OsString, st: &FileStat) -> Self {
        Self {
            id,
            name,
            real: st.real,
            apparent: st.apparent,
            mtime: st.mtime,
            ..Self::default()
        }
    }

    fn absorb(&mut self, c: &RawDir) {
        self.real += c.real;
        self.apparent += c.apparent;
        self.files += c.files;
    }
}

#[derive(Clone)]
struct LocalExclude {
    dir: PathBuf,
    set: GlobSet,
}

struct Ctx<'a> {
    platform: &'a dyn Platform,
    classifier: Option<&'a dyn Classifier>,
    opts: &'a ScanOptions,
    progress: &'a Progress,
    excludes: GlobSet,
    errors: AtomicU64,
    samples: Mutex<Vec<String>>,
    warnings: Mutex<Vec<String>>,
    protect: Mutex<Vec<PathBuf>>,
    skipped: Mutex<Vec<PathBuf>>,
    sparse: AtomicU64,
    next_id: AtomicU64,
    inodes: Vec<Mutex<HashMap<InodeKey, InodeEntry>>>,
}

impl Ctx<'_> {
    fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Records one link of a hard-linked inode, keeping the smallest path.
    fn link(&self, path: &Path, st: &FileStat, owner: u64) -> InodeKey {
        let key = (st.dev, st.ino);
        let shard = &self.inodes[(st.ino as usize) % SHARDS];
        let mut map = shard.lock().unwrap_or_else(|p| p.into_inner());
        match map.get_mut(&key) {
            Some(e) if path < &*e.path => {
                e.path = path.into();
                e.owner = owner;
            }
            Some(_) => {}
            None => {
                map.insert(
                    key,
                    InodeEntry {
                        path: path.into(),
                        owner,
                        stat: *st,
                    },
                );
            }
        }
        key
    }

    fn error(&self, path: &Path, e: &std::io::Error) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        let mut s = self.samples.lock().unwrap_or_else(|p| p.into_inner());
        if s.len() < 20 {
            s.push(format!("{}: {e}", path.display()));
        }
    }

    fn cancelled(&self) -> bool {
        self.progress.cancel.load(Ordering::Relaxed)
    }

    fn excluded(&self, path: &Path, local: &[LocalExclude]) -> bool {
        self.excludes.is_match(path)
            || local.iter().any(|l| {
                path.strip_prefix(&l.dir)
                    .is_ok_and(|rel| l.set.is_match(rel))
            })
    }

    /// Whether to enter a directory on another device.
    fn may_cross(&self, path: &Path) -> bool {
        if self.platform.is_virtual_path(path) {
            return false;
        }
        let network = self
            .platform
            .mount_for(path)
            .is_some_and(|m| is_network_fs(&m.fs_type));
        let ok = if network {
            self.opts.network_fs
        } else {
            self.opts.cross_fs
        };
        if !ok {
            self.skipped
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(path.to_path_buf());
        }
        ok
    }

    fn count_file(&self, st: &FileStat) {
        self.progress.files.fetch_add(1, Ordering::Relaxed);
        self.progress.bytes.fetch_add(st.real, Ordering::Relaxed);
        if st.is_sparse() {
            self.sparse.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).map_err(|e| Error::Other(format!("exclude glob {p:?}: {e}")))?);
    }
    b.build().map_err(|e| Error::Other(e.to_string()))
}

fn read_entries(ctx: &Ctx, path: &Path) -> Option<Vec<(OsString, Metadata)>> {
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => {
            ctx.error(path, &e);
            return None;
        }
    };
    let mut out = Vec::new();
    for entry in rd {
        match entry.and_then(|e| Ok((e.file_name(), e.metadata()?))) {
            Ok(x) => out.push(x),
            Err(e) => ctx.error(path, &e),
        }
    }
    Some(out)
}

fn list_names(path: &Path) -> Option<Vec<OsString>> {
    std::fs::read_dir(path)
        .ok()
        .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
}

fn scan_dir(
    ctx: &Ctx,
    path: &Path,
    name: OsString,
    st: &FileStat,
    local: &[LocalExclude],
) -> RawDir {
    let mut raw = RawDir::new(ctx.new_id(), name, st);
    ctx.progress.dirs.fetch_add(1, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(st.real, Ordering::Relaxed);
    let Some(entries) = read_entries(ctx, path) else {
        raw.flags |= FLAG_UNREADABLE;
        return raw;
    };
    let names: Vec<OsString> = entries.iter().map(|(n, _)| n.clone()).collect();

    let mut local_owned: Vec<LocalExclude>;
    let mut local = local;
    if ctx.opts.project_config && names.iter().any(|n| n == PROJECT_FILE) {
        let file = path.join(PROJECT_FILE);
        if let Ok(text) = std::fs::read_to_string(&file) {
            let pc = ProjectConfig::parse(&text, path);
            ctx.warnings
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend(pc.warnings);
            ctx.protect
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend(pc.protect);
            match build_globset(&pc.excludes) {
                Ok(set) if !pc.excludes.is_empty() => {
                    local_owned = local.to_vec();
                    local_owned.push(LocalExclude {
                        dir: path.to_path_buf(),
                        set,
                    });
                    local = &local_owned;
                }
                Ok(_) => {}
                Err(e) => ctx
                    .warnings
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(format!("{}: {e}", file.display())),
            }
        }
    }

    let mut subdirs = Vec::new();
    for (fname, meta) in entries {
        let child = path.join(&fname);
        if meta.is_dir() {
            subdirs.push((fname, meta, child));
            continue;
        }
        if ctx.excluded(&child, local) {
            continue;
        }
        let fst = ctx.platform.file_stat(&meta);
        ctx.count_file(&fst);
        let is_file = meta.is_file();
        let rule = if is_file {
            ctx.classifier
                .and_then(|c| c.classify_file(&child, &fname, &names))
        } else {
            None
        };
        let keep = is_file
            && (fst.apparent >= ctx.opts.record_min
                || rule.is_some()
                || media::is_candidate(&fname)
                || (!ctx.opts.record_ext.is_empty()
                    && Path::new(&fname).extension().is_some_and(|e| {
                        ctx.opts
                            .record_ext
                            .iter()
                            .any(|x| e.eq_ignore_ascii_case(x.as_str()))
                    })));
        raw.newest_file = raw.newest_file.max(fst.mtime);
        let hardlinked = is_file && fst.nlink > 1;
        if hardlinked {
            let key = ctx.link(&child, &fst, raw.id);
            raw.links.push(key);
        } else {
            raw.real += fst.real;
            raw.apparent += fst.apparent;
            raw.files += 1;
        }
        if keep {
            raw.files_recs.push(RawFile {
                name: fname,
                stat: fst,
                rule,
                hardlinked,
            });
        }
    }

    let parent_dev = st.dev;
    let children: Vec<RawDir> = subdirs
        .into_par_iter()
        .filter_map(|(fname, meta, child)| {
            if ctx.cancelled() || ctx.excluded(&child, local) {
                return None;
            }
            let cst = ctx.platform.file_stat(&meta);
            if (cst.dev != parent_dev || ctx.platform.is_virtual_path(&child))
                && !ctx.may_cross(&child)
            {
                return None;
            }
            let rule = ctx
                .classifier
                .and_then(|c| c.classify_dir(&child, &fname, &names, &mut || list_names(&child)));
            let mut d = match rule {
                Some(m) => {
                    let mut d = tally_dir(ctx, &child, fname, &cst, local, None);
                    d.rule = Some(m);
                    d.flags |= FLAG_COLLAPSED;
                    d
                }
                None => scan_dir(ctx, &child, fname, &cst, local),
            };
            if d.name == ".git" {
                d.flags |= FLAG_GIT;
            }
            Some(d)
        })
        .collect();
    for c in &children {
        raw.absorb(c);
    }
    raw.children = children;
    raw
}

/// Sizes a claimed folder without keeping its children: the largest speed
/// and memory saving of the walk.
fn tally_dir(
    ctx: &Ctx,
    path: &Path,
    name: OsString,
    st: &FileStat,
    local: &[LocalExclude],
    owner: Option<u64>,
) -> RawDir {
    let mut raw = RawDir::new(ctx.new_id(), name, st);
    let owner = owner.unwrap_or(raw.id);
    ctx.progress.dirs.fetch_add(1, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(st.real, Ordering::Relaxed);
    let Some(entries) = read_entries(ctx, path) else {
        raw.flags |= FLAG_UNREADABLE;
        return raw;
    };
    let mut subdirs = Vec::new();
    for (fname, meta) in entries {
        let child = path.join(&fname);
        if ctx.excluded(&child, local) {
            continue;
        }
        let fst = ctx.platform.file_stat(&meta);
        if meta.is_dir() {
            if fst.dev == st.dev {
                subdirs.push((fname, fst, child));
            }
            continue;
        }
        ctx.count_file(&fst);
        raw.mtime = raw.mtime.max(fst.mtime);
        if meta.is_file() && fst.nlink > 1 {
            let key = ctx.link(&child, &fst, owner);
            raw.links.push(key);
        } else {
            raw.real += fst.real;
            raw.apparent += fst.apparent;
            raw.files += 1;
        }
    }
    let parts: Vec<RawDir> = subdirs
        .into_par_iter()
        .filter(|_| !ctx.cancelled())
        .map(|(fname, fst, child)| tally_dir(ctx, &child, fname, &fst, local, Some(owner)))
        .collect();
    for mut p in parts {
        raw.absorb(&p);
        raw.mtime = raw.mtime.max(p.mtime);
        raw.links.append(&mut p.links);
    }
    raw
}

/// Walks `opts.root` and returns the sized, classified tree.
pub fn scan(
    platform: &dyn Platform,
    classifier: Option<&dyn Classifier>,
    opts: &ScanOptions,
    progress: &Progress,
) -> Result<Scan> {
    let started = Instant::now();
    let root = std::fs::canonicalize(&opts.root).at(&opts.root)?;
    let meta = std::fs::symlink_metadata(&root).at(&root)?;
    if !meta.is_dir() {
        return Err(Error::Other(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let ctx = Ctx {
        platform,
        classifier,
        opts,
        progress,
        excludes: build_globset(&opts.excludes)?,
        errors: AtomicU64::new(0),
        samples: Mutex::new(Vec::new()),
        warnings: Mutex::new(Vec::new()),
        protect: Mutex::new(Vec::new()),
        skipped: Mutex::new(Vec::new()),
        sparse: AtomicU64::new(0),
        next_id: AtomicU64::new(0),
        inodes: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
    };
    let st = platform.file_stat(&meta);
    let raw = scan_dir(&ctx, &root, OsString::new(), &st, &[]);

    let mut scan = Scan {
        errors: ctx.errors.load(Ordering::Relaxed),
        error_samples: ctx.samples.into_inner().unwrap_or_default(),
        warnings: ctx.warnings.into_inner().unwrap_or_default(),
        project_protect: ctx.protect.into_inner().unwrap_or_default(),
        skipped_mounts: ctx.skipped.into_inner().unwrap_or_default(),
        cancelled: progress.cancel.load(Ordering::Relaxed),
        fs_type: platform.mount_for(&root).map(|m| m.fs_type),
        ..Scan::default()
    };
    scan.skipped_mounts.sort();
    let sparse = ctx.sparse.load(Ordering::Relaxed);
    let inodes: HashMap<InodeKey, InodeEntry> = ctx
        .inodes
        .into_iter()
        .flat_map(|m| m.into_inner().unwrap_or_default())
        .collect();
    let (ids, finding_links) = flatten(&mut scan, root, raw);
    resolve_hardlinks(&mut scan, &ids, &inodes);
    compute_reclaimable(&mut scan, &inodes, &ids, &finding_links);

    if let Some(fs) = scan.fs_type.as_deref().filter(|fs| is_cow_fs(fs)) {
        scan.notes.push(format!(
            "{fs} filesystem: shared extents and snapshots can make freed space lower than shown"
        ));
    }
    if sparse > 0 {
        scan.notes.push(format!(
            "{sparse} sparse file(s) (VM disks, swap files) are shown at real size, below their length"
        ));
    }
    scan.elapsed = started.elapsed();
    Ok(scan)
}

/// Raw directory id to node id.
type IdMap = HashMap<u64, u32>;

/// Hard links found inside claimed folders: (inode, finding node).
type FindingLinks = Vec<(InodeKey, u32)>;

fn flatten(scan: &mut Scan, root: PathBuf, raw: RawDir) -> (IdMap, FindingLinks) {
    let mut interned: HashMap<OsString, u32> = HashMap::new();
    let mut tree = Tree {
        root,
        nodes: Vec::new(),
        names: Vec::new(),
    };
    let mut ids = IdMap::new();
    let mut finding_links = Vec::new();
    let mut intern = |tree: &mut Tree, name: &OsStr| -> u32 {
        if let Some(&i) = interned.get(name) {
            return i;
        }
        let i = tree.names.len() as u32;
        tree.names.push(name.into());
        interned.insert(name.to_os_string(), i);
        i
    };
    let mut queue: VecDeque<(RawDir, u32)> = VecDeque::new();
    let root_name = intern(&mut tree, OsStr::new(""));
    tree.nodes.push(node_from(&raw, root_name, NONE));
    queue.push_back((raw, 0));
    while let Some((mut raw, id)) = queue.pop_front() {
        ids.insert(raw.id, id);
        let first = tree.nodes.len() as u32;
        let count = raw.children.len() as u32;
        tree.nodes[id as usize].first_child = first;
        tree.nodes[id as usize].child_count = count;
        for child in raw.children.drain(..) {
            let cid = tree.nodes.len() as u32;
            let n = intern(&mut tree, &child.name);
            tree.nodes.push(node_from(&child, n, id));
            queue.push_back((child, cid));
        }
        if let Some(m) = raw.rule.take() {
            let path = tree.path(id);
            let n = &tree.nodes[id as usize];
            let mut f = Finding::from_match(path, EntryKind::Dir, &m);
            f.real = n.real;
            f.apparent = n.apparent;
            f.files = n.files;
            f.mtime = n.mtime;
            tree.nodes[id as usize].finding = scan.findings.len() as u32;
            scan.findings.push(f);
            finding_links.extend(raw.links.iter().map(|k| (*k, id)));
        }
        for f in raw.files_recs.drain(..) {
            scan.files.push(FileRec {
                dir: id,
                name: f.name.into_boxed_os_str(),
                stat: f.stat,
                rule: f.rule.map(Box::new),
                counted: !f.hardlinked,
            });
        }
    }
    scan.tree = tree;
    (ids, finding_links)
}

fn node_from(raw: &RawDir, name: u32, parent: u32) -> Node {
    Node {
        name,
        parent,
        first_child: 0,
        child_count: 0,
        real: raw.real,
        apparent: raw.apparent,
        files: raw.files,
        mtime: raw.mtime,
        newest_file: raw.newest_file,
        finding: NONE,
        flags: raw.flags,
    }
}

/// Adds each hard-linked inode once, to the node owning its smallest path,
/// so totals are identical on every run of a parallel walk.
fn resolve_hardlinks(scan: &mut Scan, ids: &IdMap, inodes: &HashMap<InodeKey, InodeEntry>) {
    for e in inodes.values() {
        let Some(&node) = ids.get(&e.owner) else {
            continue;
        };
        let mut cur = node;
        loop {
            let n = &mut scan.tree.nodes[cur as usize];
            n.real += e.stat.real;
            n.apparent += e.stat.apparent;
            n.files += 1;
            if cur == 0 {
                break;
            }
            cur = n.parent;
        }
    }
    let tree = &scan.tree;
    for f in scan.findings.iter_mut() {
        if let Some(id) = tree.find(&f.path) {
            let n = &tree.nodes[id as usize];
            f.real = n.real;
            f.apparent = n.apparent;
            f.files = n.files;
        }
    }
    for f in scan.files.iter_mut().filter(|f| !f.counted) {
        if let Some(e) = inodes.get(&(f.stat.dev, f.stat.ino)) {
            f.counted = *e.path == *tree.path(f.dir).join(&*f.name);
        }
    }
}

fn compute_reclaimable(
    scan: &mut Scan,
    inodes: &HashMap<InodeKey, InodeEntry>,
    ids: &IdMap,
    finding_links: &FindingLinks,
) {
    // Links of each inode inside each claimed folder.
    let mut inside: HashMap<(InodeKey, u32), u64> = HashMap::new();
    for (key, node) in finding_links {
        *inside.entry((*key, *node)).or_default() += 1;
    }
    // Bytes a claimed folder shows but would not free: inodes it owns
    // that still have links elsewhere.
    let mut kept: HashMap<u32, u64> = HashMap::new();
    for ((key, node), count) in inside {
        if let Some(e) = inodes.get(&key)
            && ids.get(&e.owner) == Some(&node)
            && count < e.stat.nlink
        {
            *kept.entry(node).or_default() += e.stat.real;
        }
    }
    let tree = &scan.tree;
    for id in tree.ids() {
        let f = tree.nodes[id as usize].finding;
        if f != NONE {
            let fnd = &mut scan.findings[f as usize];
            fnd.reclaimable = fnd.real.saturating_sub(kept.get(&id).copied().unwrap_or(0));
        }
    }
    for fr in &scan.files {
        if let Some(m) = &fr.rule {
            let mut f = Finding::from_match(tree.path(fr.dir).join(&*fr.name), EntryKind::File, m);
            f.real = if fr.counted { fr.stat.real } else { 0 };
            f.apparent = fr.stat.apparent;
            f.files = 1;
            f.mtime = fr.stat.mtime;
            // Deleting one link of several frees nothing.
            f.reclaimable = if fr.stat.nlink > 1 { 0 } else { fr.stat.real };
            scan.findings.push(f);
        }
    }
}
