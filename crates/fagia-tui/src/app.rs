//! TUI state and key handling, independent of the terminal so it can be
//! driven in tests.
//!
//! Nothing slow runs on the UI thread. Scanning, memory sampling, clean
//! planning and cleaning run on worker threads and report back through one
//! channel that [`App::tick`] drains; key handling and drawing only touch
//! data that is already computed.

use crate::keys::{Action, Keys};
use fagia_core::actions::kill::{self, SignalKind, SignalPlan};
use fagia_core::actions::log::ActionLog;
use fagia_core::actions::trash::TrashEntry;
use fagia_core::actions::{CleanResult, Gate, ItemOutcome, ItemResult, Mode, Plan};
use fagia_core::disk::media::kind_of_file;
use fagia_core::disk::walk::NONE;
use fagia_core::disk::{Progress, Scan, stale};
use fagia_core::model::{EntryKind, Finding};
use fagia_core::ram::group::Group;
use fagia_core::ram::{self, MemCache, ReportOptions, Sampler};
use fagia_core::report::MemReport;
use fagia_core::rules::Rule;
use fagia_core::session::{ScanFlags, Session};
use fagia_core::store::Snapshot;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// How often the memory view refreshes.
const MEM_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Disk,
    Ram,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Categories,
    Items,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Size,
    Stale,
    Name,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Stale => "staleness",
            Self::Name => "name",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatKind {
    Findings(String),
    BigFolders,
    Media,
}

#[derive(Debug, Clone)]
pub struct Cat {
    pub kind: CatKind,
    pub label: String,
    pub size: u64,
    pub count: usize,
    /// Every finding in it is regenerable (drives the colour).
    pub regenerable: bool,
}

/// One row of the items pane.
#[derive(Debug, Clone)]
pub struct Item {
    pub path: PathBuf,
    pub size: u64,
    pub stale_days: Option<u64>,
    /// Index into the findings: only findings can be selected and cleaned.
    pub finding: Option<usize>,
    /// Tree node, for drilling into folders.
    pub node: Option<u32>,
    pub is_dir: bool,
}

pub struct DiskView {
    pub scan: Scan,
    pub findings: Vec<Finding>,
    /// Media files (indices into `scan.files`), largest first; found on the
    /// scan thread because some need their header read.
    pub media: Vec<usize>,
    pub cats: Vec<Cat>,
    pub cat: usize,
    pub item: usize,
    pub focus: Focus,
    pub selected: HashSet<usize>,
    /// Folder shown under "Big folders" (drill-down position).
    pub browse: u32,
    /// Rows of the items pane for the current category, filter and sort.
    items: Vec<Item>,
}

pub enum DiskState {
    Scanning(Arc<Progress>, Instant),
    Ready(Box<DiskView>),
    Failed(String),
}

pub enum Modal {
    Help,
    /// Work running on a worker thread; `detail` updates as it goes.
    Busy {
        title: String,
        detail: String,
        started: Instant,
    },
    ConfirmClean(Plan),
    ConfirmKill(SignalPlan, SignalKind),
    ConfirmForce(SignalPlan),
    /// Emptying the trash is permanent: the user types `empty`.
    ConfirmEmpty {
        entries: Vec<TrashEntry>,
        typed: String,
    },
    Info(String, Vec<String>),
}

type ScanDone = (Scan, Vec<Finding>, Vec<usize>);

/// Everything workers send back to the UI thread.
enum Msg {
    Scan(Box<Result<ScanDone, String>>),
    Mem(Box<(MemReport, Vec<Group>)>),
    Planned(Result<Plan, String>),
    CleanStep(usize, usize, String),
    Cleaned(Result<CleanResult, String>),
    TrashListed(Vec<TrashEntry>),
    Emptied(Vec<ItemResult>),
}

/// Stops the memory worker when the app goes away.
struct MemWorker {
    active: Arc<AtomicBool>,
    now: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Drop for MemWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub struct App {
    pub session: Arc<Session>,
    pub root: PathBuf,
    pub flags: ScanFlags,
    pub keys: Keys,
    pub tab: Tab,
    pub disk: DiskState,
    pub sort: Sort,
    pub filter: String,
    pub filtering: bool,
    pub mem: Option<MemReport>,
    pub ram_idx: usize,
    pub history: Vec<Snapshot>,
    pub modal: Option<Modal>,
    pub status: String,
    pub quit: bool,
    pub as_root: bool,
    pub disk_free: Option<u64>,
    pub disk_total: Option<u64>,
    /// Frame counter for spinners.
    pub frame: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    scan_gen: u64,
    groups: Vec<Group>,
    mem_worker: Option<MemWorker>,
    pending_quit: Option<(SignalPlan, Instant)>,
}

impl App {
    pub fn new(session: Arc<Session>, root: PathBuf, flags: ScanFlags, as_root: bool) -> Self {
        let (keys, problems) = Keys::new(&session.config.keys);
        let stats = session.platform.fs_stats(&root).ok();
        let (tx, rx) = channel();
        let mut app = Self {
            session,
            root,
            flags,
            keys,
            tab: Tab::Disk,
            disk: DiskState::Failed(String::new()),
            sort: Sort::Size,
            filter: String::new(),
            filtering: false,
            mem: None,
            ram_idx: 0,
            history: Vec::new(),
            modal: None,
            status: problems.join("; "),
            quit: false,
            as_root,
            disk_free: stats.map(|s| s.available),
            disk_total: stats.map(|s| s.total),
            frame: 0,
            tx,
            rx,
            scan_gen: 0,
            groups: Vec::new(),
            mem_worker: None,
            pending_quit: None,
        };
        app.start_scan();
        app
    }

    /// True while something is animating (scan, busy work, live memory),
    /// so the loop redraws on a timer instead of only on keys.
    pub fn busy(&self) -> bool {
        matches!(self.disk, DiskState::Scanning(..))
            || matches!(self.modal, Some(Modal::Busy { .. }))
            || self.pending_quit.is_some()
    }

    /// Scans in a background thread; the UI keeps drawing meanwhile.
    pub fn start_scan(&mut self) {
        let progress = Arc::new(Progress::default());
        let session = self.session.clone();
        let opts = session.scan_options(self.root.clone(), &self.flags);
        let p = progress.clone();
        let tx = self.tx.clone();
        self.scan_gen += 1;
        std::thread::spawn(move || {
            let res = session
                .scan(&opts, &p)
                .map(|mut scan| {
                    stale::annotate(&mut scan, true);
                    let _ = session.save_snapshot(&scan);
                    let findings = scan.findings.clone();
                    let mut media: Vec<usize> = scan
                        .files
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| {
                            f.counted
                                && kind_of_file(&scan.file_path(f), &f.name, f.stat.placeholder)
                                    .is_some()
                        })
                        .map(|(i, _)| i)
                        .collect();
                    media.sort_by_key(|&i| std::cmp::Reverse(scan.files[i].stat.real));
                    (scan, findings, media)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Scan(Box::new(res)));
        });
        self.disk = DiskState::Scanning(progress, Instant::now());
    }

    /// Samples memory every two seconds on its own thread while the RAM
    /// tab is showing.
    fn ensure_mem_worker(&mut self) {
        if let Some(w) = &self.mem_worker {
            w.active.store(true, Ordering::Relaxed);
            w.now.store(true, Ordering::Relaxed);
            return;
        }
        let w = MemWorker {
            active: Arc::new(AtomicBool::new(true)),
            now: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let (active, now, stop) = (w.active.clone(), w.now.clone(), w.stop.clone());
        let session = self.session.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut cache = MemCache::default();
            let mut sampler = Sampler::new(60);
            let mut last: Option<Instant> = None;
            while !stop.load(Ordering::Relaxed) {
                let due = now.swap(false, Ordering::Relaxed)
                    || last.is_none_or(|t| t.elapsed() >= MEM_EVERY);
                if active.load(Ordering::Relaxed) && due {
                    let p = session.platform.as_ref();
                    if let Ok(snap) =
                        ram::snapshot_with(p, session.rules.mem_rules(), Some(&mut cache))
                    {
                        sampler.record(&snap.groups);
                        let report = ram::report(
                            &snap,
                            &ReportOptions {
                                platform: p,
                                cfg: &session.config.ram,
                                sampler: Some(&sampler),
                                leaks: &HashMap::new(),
                                expand: false,
                                with_forgotten: true,
                            },
                        );
                        if tx.send(Msg::Mem(Box::new((report, snap.groups)))).is_err() {
                            return;
                        }
                    }
                    last = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        self.mem_worker = Some(w);
    }

    /// Asks the memory worker for a fresh sample now.
    pub fn refresh_mem(&mut self) {
        self.ensure_mem_worker();
    }

    fn load_history(&mut self) {
        let host = self.session.platform.hostname();
        self.history = self
            .session
            .store()
            .and_then(|s| s.list(&host, &self.root.to_string_lossy(), 12))
            .unwrap_or_default();
    }

    /// Takes in worker results and follows up on quit requests. Cheap: it
    /// never waits.
    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        while let Ok(msg) = self.rx.try_recv() {
            self.on_msg(msg);
        }
        if let Some((plan, deadline)) = &self.pending_quit
            && Instant::now() >= *deadline
        {
            let alive = kill::wait_for_exit(self.session.platform.as_ref(), plan, Duration::ZERO);
            let plan = kill::narrow(plan, &alive);
            self.pending_quit = None;
            if alive.is_empty() {
                self.status = format!("{} quit.", plan.group);
            } else if self.modal.is_none() {
                self.modal = Some(Modal::ConfirmForce(plan));
            }
            self.refresh_mem();
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Scan(res) => match *res {
                Ok((scan, findings, media)) => {
                    let mut v = DiskView::new(scan, findings, media);
                    // Keep the user's place across a rescan.
                    if let DiskState::Ready(old) = &self.disk {
                        v.cat = v
                            .cats
                            .iter()
                            .position(|c| old.cats.get(old.cat).is_some_and(|o| o.kind == c.kind))
                            .unwrap_or(0);
                        v.focus = old.focus;
                    }
                    self.disk = DiskState::Ready(Box::new(v));
                    self.rebuild_items();
                    let stats = self.session.platform.fs_stats(&self.root).ok();
                    self.disk_free = stats.map(|s| s.available);
                    self.disk_total = stats.map(|s| s.total);
                    self.load_history();
                }
                Err(e) => self.disk = DiskState::Failed(e),
            },
            Msg::Mem(m) => {
                let (report, groups) = *m;
                self.ram_idx = self.ram_idx.min(report.groups.len().saturating_sub(1));
                self.mem = Some(report);
                self.groups = groups;
            }
            Msg::Planned(Ok(plan)) => {
                if matches!(self.modal, Some(Modal::Busy { .. })) {
                    self.modal = Some(Modal::ConfirmClean(plan));
                }
            }
            Msg::Planned(Err(e)) | Msg::Cleaned(Err(e)) => {
                self.modal = None;
                self.status = e;
            }
            Msg::CleanStep(done, total, path) => {
                if let Some(Modal::Busy { detail, .. }) = &mut self.modal {
                    *detail = format!("{done} of {total}: {path}");
                }
            }
            Msg::TrashListed(entries) => {
                if !matches!(self.modal, Some(Modal::Busy { .. })) {
                    return;
                }
                self.modal = if entries.is_empty() {
                    Some(Modal::Info(
                        "Trash".into(),
                        vec!["The trash is empty.".into()],
                    ))
                } else {
                    Some(Modal::ConfirmEmpty {
                        entries,
                        typed: String::new(),
                    })
                };
            }
            Msg::Emptied(results) => {
                let done = results
                    .iter()
                    .filter(|r| r.outcome == ItemOutcome::Done)
                    .count();
                let mut lines = vec![format!(
                    "Deleted {done} of {} item(s) for good ({}).",
                    results.len(),
                    fagia_core::size::format_size(results.iter().map(|r| r.bytes).sum())
                )];
                for r in results
                    .iter()
                    .filter(|r| r.outcome != ItemOutcome::Done)
                    .take(10)
                {
                    lines.push(format!(
                        "skipped {}: {}",
                        r.path.path,
                        r.detail.as_deref().unwrap_or("")
                    ));
                }
                self.modal = Some(Modal::Info("Trash emptied".into(), lines));
                self.start_scan();
            }
            Msg::Cleaned(Ok(res)) => {
                let done = res
                    .results
                    .iter()
                    .filter(|r| r.outcome == ItemOutcome::Done)
                    .count();
                let mut lines = vec![format!(
                    "Moved {done} of {} item(s) to the trash ({}).",
                    res.results.len(),
                    fagia_core::size::format_size(res.estimate)
                )];
                for r in res
                    .results
                    .iter()
                    .filter(|r| r.outcome != ItemOutcome::Done)
                {
                    lines.push(format!(
                        "skipped {}: {}",
                        r.path.path,
                        r.detail.as_deref().unwrap_or("")
                    ));
                }
                lines.push(format!("Restore with `fagia undo` (run {}).", res.run));
                lines.push("Space is freed when the trash is emptied.".into());
                self.modal = Some(Modal::Info("Clean finished".into(), lines));
                self.start_scan();
            }
        }
    }

    pub fn disk_view(&self) -> Option<&DiskView> {
        match &self.disk {
            DiskState::Ready(v) => Some(v),
            _ => None,
        }
    }

    /// Rows of the items pane (cached; rebuilt when the view changes).
    pub fn items(&self) -> &[Item] {
        self.disk_view().map_or(&[], |v| v.items.as_slice())
    }

    /// Recomputes the items pane after a change of category, folder,
    /// filter or sort. Only here, never while drawing.
    fn rebuild_items(&mut self) {
        let filter = self.filter.to_lowercase();
        let sort = self.sort;
        let DiskState::Ready(v) = &mut self.disk else {
            return;
        };
        let Some(cat) = v.cats.get(v.cat) else {
            v.items.clear();
            return;
        };
        let t = &v.scan.tree;
        let mut items: Vec<Item> = match &cat.kind {
            CatKind::Findings(c) => v
                .findings
                .iter()
                .enumerate()
                .filter(|(_, f)| *f.category == **c)
                .map(|(i, f)| Item {
                    path: f.path.clone(),
                    size: f.reclaimable,
                    stale_days: f.stale_days,
                    finding: Some(i),
                    node: None,
                    is_dir: f.kind == EntryKind::Dir,
                })
                .collect(),
            CatKind::BigFolders => {
                let by_path: HashMap<&std::path::Path, usize> = v
                    .findings
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (f.path.as_path(), i))
                    .collect();
                t.children(v.browse)
                    .map(|id| {
                        let n = t.node(id);
                        let path = t.path(id);
                        let finding = (n.finding != NONE)
                            .then(|| by_path.get(path.as_path()).copied())
                            .flatten();
                        Item {
                            path,
                            size: n.real,
                            stale_days: None,
                            finding,
                            node: Some(id),
                            is_dir: true,
                        }
                    })
                    .collect()
            }
            CatKind::Media => v
                .media
                .iter()
                .take(if filter.is_empty() { 500 } else { usize::MAX })
                .map(|&i| {
                    let f = &v.scan.files[i];
                    Item {
                        path: v.scan.file_path(f),
                        size: f.stat.real,
                        stale_days: None,
                        finding: None,
                        node: None,
                        is_dir: false,
                    }
                })
                .collect(),
        };
        if !filter.is_empty() {
            items.retain(|i| i.path.to_string_lossy().to_lowercase().contains(&filter));
            items.truncate(500);
        }
        match sort {
            Sort::Size => items.sort_by(|a, b| b.size.cmp(&a.size).then(a.path.cmp(&b.path))),
            Sort::Stale => {
                items.sort_by(|a, b| b.stale_days.cmp(&a.stale_days).then(b.size.cmp(&a.size)))
            }
            Sort::Name => items.sort_by(|a, b| a.path.cmp(&b.path)),
        }
        v.item = v.item.min(items.len().saturating_sub(1));
        v.items = items;
    }

    pub fn selected_bytes(&self) -> u64 {
        self.disk_view().map_or(0, |v| {
            v.selected.iter().map(|&i| v.findings[i].reclaimable).sum()
        })
    }

    pub fn handle_key(&mut self, ev: KeyEvent) {
        if self.modal.is_some() {
            self.handle_modal(ev);
            return;
        }
        if self.filtering {
            match ev.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.rebuild_items();
            return;
        }
        match ev.code {
            KeyCode::Char('1') => return self.set_tab(Tab::Disk),
            KeyCode::Char('2') => return self.set_tab(Tab::Ram),
            KeyCode::Char('3') => return self.set_tab(Tab::History),
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                return self.rebuild_items();
            }
            _ => {}
        }
        let Some(action) = self.keys.action(&ev) else {
            return;
        };
        match action {
            Action::Quit => self.quit = true,
            Action::Help => self.modal = Some(Modal::Help),
            Action::NextTab => {
                let next = match self.tab {
                    Tab::Disk => Tab::Ram,
                    Tab::Ram => Tab::History,
                    Tab::History => Tab::Disk,
                };
                self.set_tab(next);
            }
            Action::Filter if self.tab == Tab::Disk => {
                self.filtering = true;
                self.filter.clear();
                self.rebuild_items();
            }
            Action::Sort => {
                self.sort = match self.sort {
                    Sort::Size => Sort::Stale,
                    Sort::Stale => Sort::Name,
                    Sort::Name => Sort::Size,
                };
                self.rebuild_items();
            }
            Action::Refresh => match self.tab {
                Tab::Disk | Tab::History => {
                    if !matches!(self.disk, DiskState::Scanning(..)) {
                        self.start_scan();
                    }
                }
                Tab::Ram => self.refresh_mem(),
            },
            _ => match self.tab {
                Tab::Disk => self.disk_action(action),
                Tab::Ram => self.ram_action(action),
                Tab::History => {}
            },
        }
    }

    fn set_tab(&mut self, t: Tab) {
        self.tab = t;
        if let Some(w) = &self.mem_worker {
            w.active.store(t == Tab::Ram, Ordering::Relaxed);
        }
        if t == Tab::Ram {
            self.ensure_mem_worker();
        }
        if t == Tab::History {
            self.load_history();
        }
    }

    fn disk_action(&mut self, a: Action) {
        let DiskState::Ready(v) = &mut self.disk else {
            return;
        };
        let n = v.items.len();
        let mut rebuild = false;
        match (a, v.focus) {
            (Action::Up, Focus::Categories) if v.cat > 0 => {
                v.cat -= 1;
                v.item = 0;
                v.browse = 0;
                rebuild = true;
            }
            (Action::Down, Focus::Categories) if v.cat + 1 < v.cats.len() => {
                v.cat += 1;
                v.item = 0;
                v.browse = 0;
                rebuild = true;
            }
            (Action::Up, Focus::Items) => v.item = v.item.saturating_sub(1),
            (Action::Down, Focus::Items) => v.item = (v.item + 1).min(n.saturating_sub(1)),
            (Action::Right, _) => v.focus = Focus::Items,
            (Action::Left, _) => v.focus = Focus::Categories,
            (Action::Open, Focus::Categories) => v.focus = Focus::Items,
            (Action::Open, Focus::Items) => {
                // Drill into a folder like a file browser.
                if v.cats
                    .get(v.cat)
                    .is_some_and(|c| c.kind == CatKind::BigFolders)
                    && let Some(node) = v.items.get(v.item).and_then(|i| i.node)
                    && v.scan.tree.children(node).count() > 0
                {
                    v.browse = node;
                    v.item = 0;
                    rebuild = true;
                }
            }
            (Action::Back, _) if v.browse != 0 => {
                let from = v.browse;
                v.browse = v.scan.tree.node(v.browse).parent;
                self.rebuild_items();
                // Land on the folder we came from.
                if let DiskState::Ready(v) = &mut self.disk {
                    v.item = v
                        .items
                        .iter()
                        .position(|i| i.node == Some(from))
                        .unwrap_or(0);
                }
                return;
            }
            (Action::Select, Focus::Items) => {
                if let Some(i) = v.items.get(v.item).and_then(|i| i.finding)
                    && !v.selected.remove(&i)
                {
                    v.selected.insert(i);
                }
                v.item = (v.item + 1).min(n.saturating_sub(1));
            }
            (Action::Clean, _) if v.cats.get(v.cat).is_some_and(|c| c.label == "Trash") => {
                self.list_trash()
            }
            (Action::Clean, _) => self.plan_clean(),
            _ => {}
        }
        if rebuild {
            self.rebuild_items();
        }
    }

    /// Builds the same plan the CLI shows, on a worker thread (it measures
    /// every item), then asks for confirmation.
    fn plan_clean(&mut self) {
        let Some(v) = self.disk_view() else {
            return;
        };
        if v.selected.is_empty() {
            self.status = "Nothing selected: space selects items.".into();
            return;
        }
        let chosen: Vec<Finding> = v.selected.iter().map(|&i| v.findings[i].clone()).collect();
        let project = v.scan.project_protect.clone();
        let (session, root, as_root, tx) = (
            self.session.clone(),
            self.root.clone(),
            self.as_root,
            self.tx.clone(),
        );
        self.modal = Some(Modal::Busy {
            title: "Checking selected items".into(),
            detail: format!(
                "{} item(s): paths, git state, open files, sizes",
                chosen.len()
            ),
            started: Instant::now(),
        });
        std::thread::spawn(move || {
            let res = Gate::with_project_protect(&session, &root, &project, as_root)
                .map(|gate| {
                    let mut plan = gate.plan(chosen, Mode::Trash);
                    // Everything the user picked, not just the preselected kinds.
                    let all: Vec<usize> = (0..plan.items.len()).collect();
                    plan.select(&all);
                    plan
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Planned(res));
        });
    }

    fn start_clean(&mut self, plan: Plan) {
        let project = self
            .disk_view()
            .map(|v| v.scan.project_protect.clone())
            .unwrap_or_default();
        let (session, root, as_root, tx) = (
            self.session.clone(),
            self.root.clone(),
            self.as_root,
            self.tx.clone(),
        );
        let total = plan.items.iter().filter(|i| i.selected).count();
        self.modal = Some(Modal::Busy {
            title: "Moving to the trash".into(),
            detail: format!("0 of {total}"),
            started: Instant::now(),
        });
        let home = self.session.platform.dirs().home.clone();
        std::thread::spawn(move || {
            let res = Gate::with_project_protect(&session, &root, &project, as_root)
                .map(|gate| {
                    let mut done = 0;
                    let step = tx.clone();
                    gate.execute(&plan, &AtomicBool::new(false), &mut |r: &ItemResult| {
                        done += 1;
                        let p = fagia_core::paths::display_path(
                            std::path::Path::new(&r.path.path),
                            Some(&home),
                        );
                        let _ = step.send(Msg::CleanStep(done, total, p));
                    })
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Cleaned(res));
        });
    }

    /// Reads the trash (sizes can take a moment) on a worker thread.
    fn list_trash(&mut self) {
        let (session, tx) = (self.session.clone(), self.tx.clone());
        self.modal = Some(Modal::Busy {
            title: "Reading the trash".into(),
            detail: "measuring trashed items".into(),
            started: Instant::now(),
        });
        std::thread::spawn(move || {
            let _ = tx.send(Msg::TrashListed(fagia_core::actions::trash::list(
                session.platform.as_ref(),
            )));
        });
    }

    fn start_empty(&mut self, entries: Vec<TrashEntry>) {
        let total = entries.len();
        let (log, tx) = (self.log(), self.tx.clone());
        self.modal = Some(Modal::Busy {
            title: "Emptying the trash".into(),
            detail: format!("0 of {total}"),
            started: Instant::now(),
        });
        std::thread::spawn(move || {
            let mut done = 0;
            let step = tx.clone();
            let results = fagia_core::actions::empty_trash(
                &log,
                &entries,
                &AtomicBool::new(false),
                &mut |r| {
                    done += 1;
                    let _ = step.send(Msg::CleanStep(done, total, r.path.path.clone()));
                },
            );
            let _ = tx.send(Msg::Emptied(results));
        });
    }

    fn ram_action(&mut self, a: Action) {
        let n = self.mem.as_ref().map_or(0, |m| m.groups.len());
        match a {
            Action::Up => self.ram_idx = self.ram_idx.saturating_sub(1),
            Action::Down => self.ram_idx = (self.ram_idx + 1).min(n.saturating_sub(1)),
            Action::Kill | Action::Pause | Action::Resume => {
                let kind = match a {
                    Action::Kill => SignalKind::Quit,
                    Action::Pause => SignalKind::Pause,
                    _ => SignalKind::Resume,
                };
                // The report and the groups come from the same sample, in
                // the same order. Stale PIDs are caught at signal time by
                // the pidfd start-time check.
                let Some(group) = self.groups.get(self.ram_idx) else {
                    return;
                };
                let p = self.session.platform.as_ref();
                let home = p.dirs().home.clone();
                let plan = kill::plan(p, group, Some(&home), false);
                if plan.allowed().count() == 0 {
                    self.status = format!("{}: nothing here may be signalled", plan.group);
                    return;
                }
                self.modal = Some(Modal::ConfirmKill(plan, kind));
            }
            _ => {}
        }
    }

    fn log(&self) -> ActionLog {
        ActionLog::new(ActionLog::default_path(&self.session.platform.dirs().state))
    }

    fn handle_modal(&mut self, ev: KeyEvent) {
        let yes = matches!(
            ev.code,
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
        );
        let no = matches!(
            ev.code,
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q')
        );
        let Some(modal) = self.modal.take() else {
            return;
        };
        // Typed confirmation: letters go into the field, not to y/n.
        if let Modal::ConfirmEmpty { entries, mut typed } = modal {
            match ev.code {
                KeyCode::Esc => self.status = "Cancelled.".into(),
                KeyCode::Enter if typed.trim() == "empty" => self.start_empty(entries),
                KeyCode::Enter => {
                    self.status = "Type `empty` to confirm.".into();
                    self.modal = Some(Modal::ConfirmEmpty { entries, typed });
                }
                KeyCode::Backspace => {
                    typed.pop();
                    self.modal = Some(Modal::ConfirmEmpty { entries, typed });
                }
                KeyCode::Char(c) => {
                    typed.push(c);
                    self.modal = Some(Modal::ConfirmEmpty { entries, typed });
                }
                _ => self.modal = Some(Modal::ConfirmEmpty { entries, typed }),
            }
            return;
        }
        match modal {
            Modal::Help | Modal::Info(..) => {}
            // Workers finish what they started; the result replaces this.
            busy @ Modal::Busy { .. } => self.modal = Some(busy),
            Modal::ConfirmClean(plan) if yes => self.start_clean(plan),
            Modal::ConfirmKill(plan, kind) if yes => {
                let p = self.session.platform.as_ref();
                let res = kill::send(p, &self.log(), &plan, kind);
                let failed = res.iter().filter(|r| !r.ok).count();
                let verb = match kind {
                    SignalKind::Quit | SignalKind::ForceKill => "Asked to quit",
                    SignalKind::Pause => "Paused",
                    SignalKind::Resume => "Resumed",
                };
                self.status = format!(
                    "{verb}: {} ({} process(es){})",
                    plan.group,
                    res.len() - failed,
                    if failed > 0 {
                        format!(", {failed} failed")
                    } else {
                        String::new()
                    }
                );
                if kind == SignalKind::Quit {
                    let grace = Duration::from_secs(self.session.config.ram.grace_seconds);
                    self.pending_quit = Some((plan, Instant::now() + grace));
                }
                self.refresh_mem();
            }
            Modal::ConfirmForce(plan) if yes => {
                let res = kill::send(
                    self.session.platform.as_ref(),
                    &self.log(),
                    &plan,
                    SignalKind::ForceKill,
                );
                self.status = format!(
                    "Force-killed {} process(es) of {}",
                    res.iter().filter(|r| r.ok).count(),
                    plan.group
                );
                self.refresh_mem();
            }
            other if !no => self.modal = Some(other),
            _ => self.status = "Cancelled.".into(),
        }
    }
}

impl DiskView {
    pub fn new(scan: Scan, findings: Vec<Finding>, media: Vec<usize>) -> Self {
        let mut cats: Vec<Cat> = Vec::new();
        for f in &findings {
            let size = f.reclaimable.max(f.real);
            match cats.iter_mut().find(|c| c.label == *f.category) {
                Some(c) => {
                    c.size += size;
                    c.count += 1;
                    c.regenerable &= f.regenerable;
                }
                None => cats.push(Cat {
                    kind: CatKind::Findings(f.category.to_string()),
                    label: f.category.to_string(),
                    size,
                    count: 1,
                    regenerable: f.regenerable,
                }),
            }
        }
        // Regenerable categories first, then by size.
        cats.sort_by(|a, b| b.regenerable.cmp(&a.regenerable).then(b.size.cmp(&a.size)));
        if !media.is_empty() {
            cats.push(Cat {
                kind: CatKind::Media,
                label: "Media".into(),
                size: media.iter().map(|&i| scan.files[i].stat.real).sum(),
                count: media.len(),
                regenerable: false,
            });
        }
        cats.push(Cat {
            kind: CatKind::BigFolders,
            label: "Big folders".into(),
            size: scan.total_real(),
            count: scan.tree.children(0).count(),
            regenerable: false,
        });
        // Preselect what the CLI preselects: regenerable, low risk.
        let selected = findings
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                Rule::auto_select(f.regenerable, f.risk) && f.kind != EntryKind::Provider
            })
            .map(|(i, _)| i)
            .collect();
        Self {
            scan,
            findings,
            media,
            cats,
            cat: 0,
            item: 0,
            focus: Focus::Categories,
            selected,
            browse: 0,
            items: Vec::new(),
        }
    }
}
