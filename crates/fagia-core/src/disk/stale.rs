//! Staleness: how long since a project was last touched. For a suspect
//! inside a project this is the newest source change or git commit in the
//! project, not the suspect folder's own time (build output is rewritten
//! by every build, even of an abandoned project).

use crate::disk::Scan;
use crate::disk::walk::{FLAG_COLLAPSED, FLAG_GIT};
use crate::model::now_epoch;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Newest file time in each node's subtree, ignoring claimed folders and
/// `.git`. Children always have larger ids than their parent, so one
/// reverse pass suffices.
pub fn subtree_newest(scan: &Scan) -> Vec<i64> {
    let t = &scan.tree;
    let mut newest: Vec<i64> = t.ids().map(|id| t.node(id).newest_file).collect();
    for id in t.ids().rev() {
        let n = t.node(id);
        if id == 0 || n.flags & (FLAG_COLLAPSED | FLAG_GIT) != 0 {
            continue;
        }
        let p = n.parent as usize;
        newest[p] = newest[p].max(newest[id as usize]);
    }
    newest
}

/// Unix time of the last commit touching `dir`, if it is in a git repo.
pub fn last_commit(dir: &Path) -> Option<i64> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "-1", "--format=%ct", "--", "."])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Fills `stale_days` on every finding. `use_git` also consults commit
/// times (one `git log` per project).
pub fn annotate(scan: &mut Scan, use_git: bool) {
    let now = now_epoch();
    let newest = subtree_newest(scan);
    let projects: Vec<PathBuf> = {
        let mut p: Vec<PathBuf> = scan
            .findings
            .iter()
            .filter_map(|f| f.project_dir.clone())
            .collect();
        p.sort();
        p.dedup();
        p
    };
    let commits: HashMap<PathBuf, i64> = if use_git {
        projects
            .par_iter()
            .filter(|p| in_git_repo(p))
            .filter_map(|p| last_commit(p).map(|t| (p.clone(), t)))
            .collect()
    } else {
        HashMap::new()
    };
    let tree = &scan.tree;
    for f in scan.findings.iter_mut() {
        let touched = match &f.project_dir {
            Some(proj) => {
                let files = tree.find(proj).map_or(0, |id| newest[id as usize]);
                let commit = commits.get(proj).copied().unwrap_or(0);
                files.max(commit)
            }
            None => 0,
        };
        let touched = if touched > 0 { touched } else { f.mtime };
        f.stale_days = Some((now.saturating_sub(touched).max(0) / 86_400) as u64);
    }
}

fn in_git_repo(dir: &Path) -> bool {
    dir.ancestors().any(|a| a.join(".git").exists())
}

#[cfg(test)]
mod tests {
    use crate::disk::{Progress, ScanOptions, scan};
    use crate::platform::Platform;
    use crate::platform::linux::LinuxPlatform;
    use crate::rules::RuleSet;
    use std::fs::{self, File, FileTimes};
    use std::time::{Duration, SystemTime};

    fn set_age(p: &std::path::Path, days: u64) {
        let t = SystemTime::now() - Duration::from_secs(days * 86_400);
        File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_times(FileTimes::new().set_modified(t).set_accessed(t))
            .unwrap();
    }

    #[test]
    fn staleness_follows_project_sources_not_build_output() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        for (proj, age) in [("old", 200), ("fresh", 2)] {
            fs::create_dir_all(r.join(proj).join("src")).unwrap();
            fs::create_dir_all(r.join(proj).join("target")).unwrap();
            fs::write(r.join(proj).join("Cargo.toml"), "x").unwrap();
            fs::write(r.join(proj).join("src/main.rs"), "x").unwrap();
            fs::write(r.join(proj).join("target/out.bin"), "x").unwrap();
            set_age(&r.join(proj).join("Cargo.toml"), age);
            set_age(&r.join(proj).join("src/main.rs"), age);
            // Build output is always recent.
            set_age(&r.join(proj).join("target/out.bin"), 0);
        }
        let plat = LinuxPlatform::new();
        let rules = RuleSet::builtin(plat.dirs()).unwrap();
        let mut s = scan(
            &plat,
            Some(&rules),
            &ScanOptions::new(r),
            &Progress::default(),
        )
        .unwrap();
        super::annotate(&mut s, false);
        let days = |name: &str| {
            s.findings
                .iter()
                .find(|f| f.path.ends_with(format!("{name}/target")))
                .unwrap()
                .stale_days
                .unwrap()
        };
        assert!((199..=201).contains(&days("old")), "{}", days("old"));
        assert!(days("fresh") <= 3);
    }
}
