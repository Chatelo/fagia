//! Rolling processes up into apps. Browsers, Electron apps and build tools
//! spawn many processes; users think in apps.
//!
//! 1. A process's group root is its topmost ancestor below a boundary:
//!    init, system processes, terminals and shells. So `npm run dev` typed
//!    in a terminal is its own app, not part of "Terminal".
//! 2. Processes in a systemd app scope (`app-*.scope`) that holds no
//!    terminal or shell are grouped by scope, which also catches helpers
//!    that re-parented to the user's systemd.
//! 3. Groups with the same name, owner (and working directory, for rules
//!    with `split_by_cwd`) are merged.

use crate::platform::{ProcInfo, ProcMem};
use crate::rules::MemRule;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Member {
    pub info: ProcInfo,
    pub mem: ProcMem,
}

impl Member {
    /// Fair share when measurable, resident otherwise.
    pub fn fair(&self) -> u64 {
        self.mem.pss.unwrap_or(self.mem.rss)
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    /// The rule's group label when the name is the app itself.
    pub category: Option<String>,
    pub rule_id: Option<String>,
    pub uid: u32,
    pub cwd: Option<PathBuf>,
    pub members: Vec<Member>,
    pub dev_suspect: bool,
    pub system: bool,
    pub unsaved_work: bool,
    pub reason: Option<String>,
}

impl Group {
    pub fn key(&self) -> String {
        match &self.cwd {
            Some(c) => format!("{}\u{0}{}\u{0}{}", self.uid, self.name, c.display()),
            None => format!("{}\u{0}{}", self.uid, self.name),
        }
    }

    pub fn display_name(&self, home: Option<&std::path::Path>) -> String {
        match &self.cwd {
            Some(c) => format!("{} ({})", self.name, crate::paths::display_path(c, home)),
            None => self.name.clone(),
        }
    }

    pub fn fair(&self) -> u64 {
        self.members.iter().map(Member::fair).sum()
    }
    pub fn uss(&self) -> u64 {
        self.members.iter().map(|m| m.mem.uss.unwrap_or(0)).sum()
    }
    pub fn rss(&self) -> u64 {
        self.members.iter().map(|m| m.mem.rss).sum()
    }
    pub fn swap(&self) -> u64 {
        self.members.iter().map(|m| m.mem.swap.unwrap_or(0)).sum()
    }
    /// All members measured with fair share (not the resident fallback).
    pub fn fully_measured(&self) -> bool {
        self.members.iter().all(|m| m.mem.pss.is_some())
    }
    pub fn started_at(&self) -> i64 {
        self.members
            .iter()
            .map(|m| m.info.started_at)
            .min()
            .unwrap_or(0)
    }
    /// The member that started first (usually the parent of the others).
    pub fn leader(&self) -> &Member {
        self.members
            .iter()
            .min_by_key(|m| (m.info.started_at, m.info.key.pid))
            .expect("groups are never empty")
    }
}

fn rule_for<'a>(rules: &'a [MemRule], p: &ProcInfo) -> Option<&'a MemRule> {
    rules.iter().find(|r| r.matches(p))
}

/// App name from a systemd scope such as `app-gnome-firefox-4242.scope`
/// or `app-flatpak-org.mozilla.firefox-1234.scope`.
pub fn scope_app(scope: &str) -> Option<String> {
    let s = scope.strip_prefix("app-")?.strip_suffix(".scope")?;
    let s = s.replace("\\x2d", "-");
    let s = s.split('@').next().unwrap_or(&s).to_string();
    let s = match s.rsplit_once('-') {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_hexdigit()) => head.to_string(),
        _ => s,
    };
    let s = ["gnome-", "kde-", "flatpak-", "snap-"]
        .iter()
        .fold(s, |acc, p| {
            acc.strip_prefix(p).map(str::to_string).unwrap_or(acc)
        });
    let s = s.rsplit('.').next().unwrap_or(&s).to_string();
    (!s.is_empty()).then_some(s)
}

pub fn group(procs: Vec<(ProcInfo, ProcMem)>, rules: &[MemRule]) -> Vec<Group> {
    let by_pid: HashMap<u32, &ProcInfo> = procs.iter().map(|(p, _)| (p.key.pid, p)).collect();
    let is_barrier = |p: &ProcInfo| {
        p.key.pid <= 1
            || p.kernel_thread
            || rule_for(rules, p).is_some_and(|r| r.boundary || r.system)
    };
    // Scopes that contain a terminal or shell are not one app.
    let mut mixed_scopes: HashSet<&str> = HashSet::new();
    for (p, _) in &procs {
        if let Some(scope) = p.cgroup_leaf.as_deref()
            && rule_for(rules, p).is_some_and(|r| r.boundary)
        {
            mixed_scopes.insert(scope);
        }
    }

    // Walk up to the topmost ancestor below a barrier.
    let root_of = |p: &'_ ProcInfo| -> ProcInfo {
        let mut root = p;
        if !is_barrier(p) {
            let mut seen = HashSet::new();
            while let Some(parent) = by_pid.get(&root.ppid) {
                if is_barrier(parent) || parent.uid != p.uid || !seen.insert(parent.key.pid) {
                    break;
                }
                root = parent;
            }
        }
        root.clone()
    };
    let app_scope = |p: &ProcInfo| -> Option<String> {
        p.cgroup_leaf.clone().filter(|s| {
            s.starts_with("app-") && s.ends_with(".scope") && !mixed_scopes.contains(s.as_str())
        })
    };
    // Each app scope is named after its first-started process that a rule
    // recognises (helpers such as crash handlers often start first), else
    // its first-started process.
    let mut scope_leader: HashMap<String, ProcInfo> = HashMap::new();
    for (p, _) in &procs {
        if let Some(s) = app_scope(p) {
            let rank = |x: &ProcInfo| (rule_for(rules, x).is_none(), x.started_at, x.key.pid);
            let e = scope_leader.entry(s).or_insert_with(|| p.clone());
            if rank(p) < rank(e) {
                *e = p.clone();
            }
        }
    }

    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for (p, mem) in &procs {
        if p.kernel_thread {
            continue;
        }
        let scope = app_scope(p);
        let root = match scope.as_ref().and_then(|s| scope_leader.get(s)) {
            Some(leader) => root_of(leader),
            None => root_of(p),
        };
        let root = &root;
        let rule = rule_for(rules, root).or_else(|| rule_for(rules, p));
        let app = || match &scope {
            Some(s) if rule_for(rules, root).is_none() => {
                scope_app(s).unwrap_or_else(|| root.exe_name())
            }
            _ => root.exe_name(),
        };
        let (name, category) = match rule {
            Some(r) if r.per_app => (app(), Some(r.group.clone())),
            Some(r) => (r.group.clone(), None),
            None => (app(), None),
        };
        let cwd = rule
            .filter(|r| r.split_by_cwd)
            .and_then(|_| root.cwd.clone());
        let mut g = Group {
            name,
            category,
            rule_id: rule.map(|r| r.id.clone()),
            uid: p.uid,
            cwd,
            members: Vec::new(),
            dev_suspect: rule.is_some_and(|r| r.dev_suspect),
            system: rule.is_some_and(|r| r.system) || p.key.pid == 1,
            unsaved_work: rule.is_some_and(|r| r.unsaved_work),
            reason: rule.and_then(|r| r.reason.clone()),
        };
        let entry = groups.entry(g.key()).or_insert_with(|| {
            let mut empty = g.clone();
            empty.members.clear();
            empty
        });
        g.members.push(Member {
            info: p.clone(),
            mem: *mem,
        });
        entry.system |= g.system;
        entry.members.append(&mut g.members);
    }
    let mut out: Vec<Group> = groups.into_values().collect();
    out.sort_by(|a, b| b.fair().cmp(&a.fair()).then(a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ProcKey;
    use crate::rules::RuleSet;

    fn proc(pid: u32, ppid: u32, exe: &str, cwd: &str) -> ProcInfo {
        ProcInfo {
            key: ProcKey {
                pid,
                start_ticks: u64::from(pid),
            },
            ppid,
            uid: 1000,
            comm: exe.chars().take(15).collect(),
            exe: Some(PathBuf::from(format!("/usr/bin/{exe}"))),
            cmdline: vec![exe.to_string()],
            started_at: i64::from(pid),
            tty: None,
            kernel_thread: false,
            state: 'S',
            cgroup_leaf: None,
            cwd: Some(PathBuf::from(cwd)),
            rss: 0,
        }
    }

    fn mem(kb: u64) -> ProcMem {
        ProcMem {
            rss: kb * 2048,
            pss: Some(kb * 1024),
            uss: Some(kb * 512),
            swap: Some(0),
        }
    }

    fn rules() -> Vec<MemRule> {
        let dirs = crate::platform::linux::dirs_from_env();
        RuleSet::builtin(&dirs).unwrap().mem_rules().to_vec()
    }

    #[test]
    fn terminals_and_shells_split_apps() {
        let mut systemd = proc(1, 0, "systemd", "/");
        systemd.uid = 0;
        let procs = vec![
            (systemd, mem(10)),
            (proc(100, 1, "gnome-terminal-server", "/home/u"), mem(50)),
            (proc(101, 100, "zsh", "/home/u/web"), mem(5)),
            (proc(102, 101, "npm", "/home/u/web"), mem(40)),
            (proc(103, 102, "node", "/home/u/web"), mem(400)),
            (proc(104, 100, "zsh", "/home/u/api"), mem(5)),
            (proc(105, 104, "cargo", "/home/u/api"), mem(100)),
            (proc(106, 105, "rustc", "/home/u/api"), mem(900)),
            (proc(200, 1, "firefox", "/home/u"), mem(700)),
            (proc(201, 200, "Isolated Web Co", "/home/u"), mem(300)),
            (proc(202, 200, "Isolated Web Co", "/home/u"), mem(200)),
        ];
        let g = group(procs, &rules());
        let names: Vec<String> = g.iter().map(|g| g.display_name(None)).collect();
        assert_eq!(names[0], "firefox", "{names:?}");
        assert_eq!(g[0].category.as_deref(), Some("Browser"));
        assert_eq!(g[0].members.len(), 3);
        assert_eq!(g[0].fair(), 1200 * 1024);
        assert!(names.contains(&"cargo / rustc".to_string()));
        assert!(
            names.contains(&"Node (/home/u/web)".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"Shell".to_string()));
        assert!(g.iter().find(|g| g.name == "System").unwrap().system);
        let node = g.iter().find(|g| g.name == "Node").unwrap();
        assert_eq!(node.members.len(), 2, "npm and node together");
        assert!(node.dev_suspect);
    }

    #[test]
    fn scopes_group_reparented_helpers() {
        let mut a = proc(300, 1, "slack", "/");
        a.cgroup_leaf = Some("app-gnome-slack-300.scope".into());
        let mut helper = proc(301, 1, "chrome_crashpad", "/");
        helper.cgroup_leaf = Some("app-gnome-slack-300.scope".into());
        let g = group(vec![(a, mem(100)), (helper, mem(10))], &rules());
        assert_eq!(
            g.len(),
            1,
            "{:?}",
            g.iter().map(|g| &g.name).collect::<Vec<_>>()
        );
        assert_eq!(g[0].name, "slack");
        assert_eq!(g[0].category.as_deref(), Some("Electron app"));
    }

    #[test]
    fn helper_scope_joins_its_app() {
        // Chrome: the main process in one scope; a crash handler (started
        // first, no rule) and the renderers in another.
        let mut main = proc(6260, 1, "chrome", "/");
        main.cgroup_leaf = Some("app-com.google.Chrome-6260.scope".into());
        let mut crash = proc(6270, 1, "chrome_crashpad_handler", "/");
        crash.cgroup_leaf = Some("app-gnome-google-chrome-6260.scope".into());
        let mut zygote = proc(6283, 6260, "chrome", "/");
        zygote.cgroup_leaf = crash.cgroup_leaf.clone();
        let mut renderer = proc(7000, 6283, "chrome", "/");
        renderer.cgroup_leaf = crash.cgroup_leaf.clone();
        let g = group(
            vec![
                (main, mem(10)),
                (crash, mem(1)),
                (zygote, mem(5)),
                (renderer, mem(50)),
            ],
            &rules(),
        );
        let names: Vec<&str> = g.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["chrome"], "{names:?}");
        assert_eq!(g[0].members.len(), 4);
    }

    #[test]
    fn scope_names() {
        assert_eq!(
            scope_app("app-gnome-firefox-4242.scope").as_deref(),
            Some("firefox")
        );
        assert_eq!(
            scope_app("app-flatpak-org.mozilla.firefox-1234.scope").as_deref(),
            Some("firefox")
        );
        assert_eq!(
            scope_app("app-org.kde.konsole-5a3f.scope").as_deref(),
            Some("konsole")
        );
        assert_eq!(scope_app("session-2.scope"), None);
    }
}
