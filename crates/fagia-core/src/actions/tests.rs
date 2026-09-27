//! Safety tests: every disk safety rule must refuse.

use super::*;
use crate::disk::{Progress, ScanOptions, scan};
use crate::platform::Dirs;
use crate::platform::linux::LinuxPlatform;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

struct Fx {
    tmp: TempDir,
    plat: LinuxPlatform,
    rules: RuleSet,
}

impl Fx {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap().join("home");
        fs::create_dir_all(&home).unwrap();
        let dirs = Dirs {
            cache: home.join(".cache"),
            data: home.join(".local/share"),
            config: home.join(".config"),
            state: home.join(".local/state"),
            cargo_home: home.join(".cargo"),
            home,
        };
        let plat = LinuxPlatform::with_roots("/proc", "/sys", "/dev", dirs.clone());
        let rules = RuleSet::builtin(&dirs).unwrap();
        Self { tmp, plat, rules }
    }

    fn home(&self) -> PathBuf {
        self.plat.dirs().home.clone()
    }

    fn file(&self, rel: &str, len: usize) -> PathBuf {
        let p = self.home().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![b'x'; len]).unwrap();
        p
    }

    fn project(&self, name: &str) -> PathBuf {
        self.file(&format!("code/{name}/Cargo.toml"), 10);
        self.file(&format!("code/{name}/target/debug/app"), 64 * 1024);
        self.home().join(format!("code/{name}/target"))
    }

    fn gate(&self, protected: Vec<PathBuf>) -> Gate<'_> {
        Gate::with_parts(
            &self.plat,
            &self.rules,
            &self.home(),
            vec![PathBuf::from("/"), self.home()],
            protected,
            ActionLog::new(self.home().join(".local/state/fagia/actions.jsonl")),
            false,
        )
        .unwrap()
    }

    fn findings(&self) -> Vec<Finding> {
        scan(
            &self.plat,
            Some(&self.rules),
            &ScanOptions::new(self.home()),
            &Progress::default(),
        )
        .unwrap()
        .findings
    }

    fn plan(&self, gate: &Gate, mode: Mode) -> Plan {
        gate.plan(self.findings(), mode)
    }
}

fn item<'a>(plan: &'a Plan, suffix: &str) -> &'a PlanItem {
    plan.items
        .iter()
        .find(|i| i.finding.path.ends_with(suffix))
        .unwrap_or_else(|| panic!("no plan item {suffix}"))
}

fn run(gate: &Gate, plan: &Plan) -> CleanResult {
    gate.execute(plan, &AtomicBool::new(false), &mut |_| {})
}

#[test]
fn trash_then_undo_round_trip() {
    let fx = Fx::new();
    let target = fx.project("api");
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    assert!(item(&plan, "api/target").selected);
    let res = run(&gate, &plan);
    assert!(res.all_done(), "{:?}", res.results);
    assert!(!target.exists());
    let trashed = fx.home().join(".local/share/Trash/files/target");
    assert!(trashed.join("debug/app").exists());
    assert!(
        fx.home()
            .join(".local/share/Trash/info/target.trashinfo")
            .exists()
    );

    let entries = gate.log().read();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, "trash");
    assert_eq!(runs(gate.log())[0].restorable, 1);

    let (_, restored) = undo(gate.log(), None).unwrap();
    assert_eq!(restored[0].outcome, ItemOutcome::Done);
    assert!(target.join("debug/app").exists());
    assert!(undo(gate.log(), None).is_err(), "nothing left to undo");
}

#[test]
fn permanent_delete_removes() {
    let fx = Fx::new();
    let target = fx.project("api");
    let gate = fx.gate(vec![]);
    let res = run(&gate, &fx.plan(&gate, Mode::Permanent));
    assert!(res.all_done());
    assert!(!target.exists());
    assert!(!fx.home().join(".local/share/Trash/files/target").exists());
    assert_eq!(gate.log().read()[0].action, "delete");
}

#[test]
fn symlink_escape_refused() {
    let fx = Fx::new();
    let outside = TempDir::new().unwrap();
    fs::create_dir_all(outside.path().join("target")).unwrap();
    fs::write(outside.path().join("Cargo.toml"), "x").unwrap();
    std::os::unix::fs::symlink(outside.path(), fx.home().join("linked")).unwrap();
    let gate = fx.gate(vec![]);
    let mut f = fx.findings().into_iter().next().unwrap_or_else(|| {
        Finding::from_match(
            PathBuf::new(),
            EntryKind::Dir,
            &crate::model::Match {
                rule_id: "rust-target".into(),
                category: "Rust build".into(),
                evidence: String::new(),
                regenerable: true,
                regenerate: None,
                risk: crate::model::Risk::Low,
                project_dir: None,
            },
        )
    });
    // A finding forged to point through the symlink.
    f.path = fx.home().join("linked/target");
    let plan = gate.plan(vec![f], Mode::Trash);
    let why = plan.items[0].refusal.clone().unwrap();
    assert!(why.contains("symlink"), "{why}");
    assert!(outside.path().join("target").exists());
}

#[test]
fn outside_root_refused() {
    let fx = Fx::new();
    let target = fx.project("api");
    let gate = Gate::with_parts(
        &fx.plat,
        &fx.rules,
        &fx.home().join("code/other"),
        vec![],
        vec![],
        ActionLog::new(fx.home().join("log.jsonl")),
        false,
    );
    // Root does not exist: no gate at all.
    assert!(gate.is_err());
    fs::create_dir_all(fx.home().join("code/other")).unwrap();
    let gate = Gate::with_parts(
        &fx.plat,
        &fx.rules,
        &fx.home().join("code/other"),
        vec![],
        vec![],
        ActionLog::new(fx.home().join("log.jsonl")),
        false,
    )
    .unwrap();
    let plan = gate.plan(fx.findings(), Mode::Trash);
    assert!(
        item(&plan, "api/target")
            .refusal
            .as_ref()
            .unwrap()
            .contains("outside")
    );
    assert!(target.exists());
}

#[test]
fn protected_paths_refused() {
    let fx = Fx::new();
    fx.project("keep");
    let gate = fx.gate(vec![fx.home().join("code/keep")]);
    let plan = fx.plan(&gate, Mode::Trash);
    let it = item(&plan, "keep/target");
    assert!(it.refusal.as_ref().unwrap().contains("protected"));
    assert!(!it.selected);
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn git_tracked_and_unignored_refused() {
    let fx = Fx::new();
    fx.project("tracked");
    fx.project("unignored");
    fx.project("ignored");
    for (name, ignore, add) in [
        ("tracked", false, true),
        ("unignored", false, false),
        ("ignored", true, false),
    ] {
        let dir = fx.home().join("code").join(name);
        if !git(&dir, &["init", "-q"]) {
            return; // git not installed
        }
        if ignore {
            fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        }
        if add {
            assert!(git(&dir, &["add", "-f", "target"]));
            assert!(git(&dir, &["commit", "-qm", "x"]));
        }
    }
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    assert!(
        item(&plan, "tracked/target")
            .refusal
            .as_ref()
            .unwrap()
            .contains("tracked")
    );
    assert!(
        item(&plan, "unignored/target")
            .refusal
            .as_ref()
            .unwrap()
            .contains("not git-ignored")
    );
    assert!(item(&plan, "ignored/target").refusal.is_none());
}

#[test]
fn in_use_folder_refused() {
    let fx = Fx::new();
    let target = fx.project("busy");
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(&target)
        .spawn()
        .unwrap();
    // Give /proc a moment to show the new process's cwd.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    let why = item(&plan, "busy/target").refusal.clone();
    let _ = child.kill();
    let _ = child.wait();
    assert!(why.unwrap().contains("in use by process"));
}

#[test]
fn growth_since_dry_run_is_skipped() {
    let fx = Fx::new();
    let target = fx.project("grow");
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    fx.file("code/grow/target/new.bin", 4 << 20);
    let res = run(&gate, &plan);
    assert_eq!(res.results[0].outcome, ItemOutcome::Skipped);
    assert!(res.results[0].detail.as_ref().unwrap().contains("grew"));
    assert!(target.exists());
    assert_eq!(gate.log().read()[0].outcome, "skipped");
}

#[test]
fn vanished_evidence_is_skipped() {
    let fx = Fx::new();
    let target = fx.project("gone");
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    fs::rename(
        fx.home().join("code/gone/Cargo.toml"),
        fx.tmp.path().join("moved.toml"),
    )
    .unwrap();
    let res = run(&gate, &plan);
    assert!(res.results[0].detail.as_ref().unwrap().contains("evidence"));
    assert!(target.exists());
}

#[test]
fn swapped_for_symlink_after_plan_is_skipped() {
    let fx = Fx::new();
    let target = fx.project("swap");
    let outside = TempDir::new().unwrap();
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    fs::rename(&target, fx.tmp.path().join("real-target")).unwrap();
    std::os::unix::fs::symlink(outside.path(), &target).unwrap();
    let res = run(&gate, &plan);
    assert_eq!(res.results[0].outcome, ItemOutcome::Skipped);
    assert!(outside.path().exists());
}

#[test]
fn cancel_stops_before_next_item() {
    let fx = Fx::new();
    let a = fx.project("a");
    let b = fx.project("b");
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    let cancel = AtomicBool::new(false);
    let mut seen = 0;
    let res = gate.execute(&plan, &cancel, &mut |_| {
        seen += 1;
        cancel.store(true, Ordering::SeqCst);
    });
    assert!(res.interrupted);
    assert_eq!(seen, 2);
    assert_eq!(
        res.results
            .iter()
            .filter(|r| r.outcome == ItemOutcome::Done)
            .count(),
        1
    );
    assert!(a.exists() != b.exists(), "exactly one item handled");
}

#[test]
fn running_as_root_refused_without_flag() {
    let fx = Fx::new();
    let plat = LinuxPlatform::with_roots("/proc", "/sys", "/dev", fx.plat.dirs().clone())
        .with_effective_uid(0);
    let log = ActionLog::new(fx.home().join("log.jsonl"));
    let err = Gate::with_parts(
        &plat,
        &fx.rules,
        &fx.home(),
        vec![],
        vec![],
        log.clone(),
        false,
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("--as-root"));
    assert!(Gate::with_parts(&plat, &fx.rules, &fx.home(), vec![], vec![], log, true).is_ok());
}

#[test]
fn trash_contents_and_providers_refused() {
    let fx = Fx::new();
    fx.file(".local/share/Trash/files/old.bin", 10);
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    assert!(
        item(&plan, "share/Trash")
            .refusal
            .as_ref()
            .unwrap()
            .contains("fagia trash --empty")
    );
    let docker = crate::providers::docker::parse(
        r#"{"Reclaimable":"1GB (50%)","Size":"2GB","TotalCount":"3","Type":"Images"}"#,
    );
    let plan = gate.plan(docker, Mode::Permanent);
    assert!(
        plan.items[0]
            .refusal
            .as_ref()
            .unwrap()
            .contains("docker image prune")
    );
}

#[test]
fn medium_risk_is_listed_not_selected() {
    let fx = Fx::new();
    fx.file("code/py/.venv/pyvenv.cfg", 10);
    fx.file("code/py/.venv/lib/x.py", 10);
    let gate = fx.gate(vec![]);
    let plan = fx.plan(&gate, Mode::Trash);
    let it = item(&plan, "py/.venv");
    assert!(it.refusal.is_none());
    assert!(!it.selected);
    let res = run(&gate, &plan);
    assert!(res.results.is_empty(), "nothing selected, nothing done");
    assert!(fx.home().join("code/py/.venv").exists());
}
