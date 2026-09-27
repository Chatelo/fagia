//! The checks behind every destructive action: containment, protected
//! paths, git state and processes using a folder.

use crate::platform::{OpenPaths, Platform};
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Why `target` is protected, if it is. `anchors` (`/` and home) protect
/// only themselves and their ancestors; every other protected path also
/// protects everything inside it.
pub fn protection(target: &Path, anchors: &[PathBuf], protected: &[PathBuf]) -> Option<String> {
    for a in anchors {
        if a.starts_with(target) {
            return Some(format!("{} is protected", a.display()));
        }
    }
    for p in protected {
        if target.starts_with(p) {
            return Some(format!("inside protected {}", p.display()));
        }
        if p.starts_with(target) {
            return Some(format!("contains protected {}", p.display()));
        }
    }
    None
}

/// Resolves `path` and checks it is the same path (no symlink anywhere in
/// it) and inside `root`.
pub fn contained(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("cannot read: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("is a symlink".into());
    }
    let canon = std::fs::canonicalize(path).map_err(|e| format!("cannot resolve: {e}"))?;
    if canon != path {
        return Err(format!("resolves through a symlink to {}", canon.display()));
    }
    if canon == root || !canon.starts_with(root) {
        return Err(format!("outside the scanned root {}", root.display()));
    }
    Ok(canon)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitState {
    NotInRepo,
    /// Ignored and holds no tracked files: safe for a suspect.
    Ignored,
    Tracked,
    UntrackedNotIgnored,
    Unknown(String),
}

impl GitState {
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::NotInRepo | Self::Ignored => None,
            Self::Tracked => Some("contains files tracked by git".into()),
            Self::UntrackedNotIgnored => {
                Some("in a git repository but not git-ignored (may hold real work)".into())
            }
            Self::Unknown(why) => Some(format!("git state unknown ({why})")),
        }
    }
}

fn repo_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

pub fn git_state(path: &Path) -> GitState {
    let Some(repo) = repo_root(path) else {
        return GitState::NotInRepo;
    };
    let rel = path.strip_prefix(&repo).unwrap_or(path);
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .arg("--")
            .arg(rel)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
    };
    let tracked = match run(&["ls-files"]) {
        Ok(o) if o.status.success() => !o.stdout.is_empty(),
        Ok(o) => return GitState::Unknown(format!("git ls-files exited {}", o.status)),
        Err(e) => return GitState::Unknown(format!("git not runnable: {e}")),
    };
    if tracked {
        return GitState::Tracked;
    }
    match run(&["check-ignore", "-q"]) {
        Ok(o) if o.status.code() == Some(0) => GitState::Ignored,
        Ok(o) if o.status.code() == Some(1) => GitState::UntrackedNotIgnored,
        Ok(o) => GitState::Unknown(format!("git check-ignore exited {}", o.status)),
        Err(e) => GitState::Unknown(format!("git not runnable: {e}")),
    }
}

/// Paths processes are using right now: working directories and open files.
pub struct InUse {
    paths: Vec<(u32, PathBuf)>,
}

impl InUse {
    pub fn snapshot(platform: &dyn Platform) -> Self {
        let OpenPaths { cwds, files, .. } = platform.open_paths();
        let me = platform.current_pid();
        Self {
            paths: cwds
                .into_iter()
                .chain(files)
                .filter(|(pid, _)| *pid != me)
                .collect(),
        }
    }

    #[cfg(test)]
    pub fn from_paths(paths: Vec<(u32, PathBuf)>) -> Self {
        Self { paths }
    }

    /// The first process using `target` or anything inside it.
    pub fn user_of(&self, target: &Path) -> Option<u32> {
        self.paths
            .iter()
            .find(|(_, p)| p.starts_with(target))
            .map(|(pid, _)| *pid)
    }
}

/// Real size of `path`, each inode once, without following symlinks or
/// leaving the filesystem. Used to detect change between plan and action.
pub fn measure(path: &Path) -> u64 {
    fn walk(p: &Path, dev: u64, seen: &mut HashSet<(u64, u64)>) -> u64 {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return 0;
        };
        if m.dev() != dev || (m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino()))) {
            return 0;
        }
        let mut total = m.blocks() * 512;
        if m.is_dir()
            && let Ok(rd) = std::fs::read_dir(p)
        {
            for e in rd.flatten() {
                total += walk(&e.path(), dev, seen);
            }
        }
        total
    }
    let dev = std::fs::symlink_metadata(path)
        .map(|m| m.dev())
        .unwrap_or(0);
    walk(path, dev, &mut HashSet::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;

    #[test]
    fn protection_rules() {
        let anchors = vec![PathBuf::from("/"), PathBuf::from("/home/u")];
        let prot = vec![PathBuf::from("/usr"), PathBuf::from("/home/u/Documents")];
        let p = |s: &str| protection(Path::new(s), &anchors, &prot);
        assert!(p("/").is_some());
        assert!(p("/home").is_some(), "ancestor of home");
        assert!(p("/home/u").is_some());
        assert!(p("/home/u/code/target").is_none());
        assert!(p("/usr/lib/node_modules").is_some());
        assert!(p("/home/u/Documents/x").is_some());
        assert!(
            p("/home/u/Doc").is_none(),
            "prefix of a name is not containment"
        );
    }

    #[test]
    fn symlink_escape_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root_c = root.path().canonicalize().unwrap();
        fs::create_dir(outside.path().join("victim")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let err = contained(&root_c.join("link/victim"), &root_c).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        std::os::unix::fs::symlink(outside.path().join("victim"), root.path().join("direct"))
            .unwrap();
        assert_eq!(
            contained(&root_c.join("direct"), &root_c).unwrap_err(),
            "is a symlink"
        );
        fs::create_dir(root.path().join("real")).unwrap();
        assert!(contained(&root_c.join("real"), &root_c).is_ok());
        assert!(
            contained(&root_c, &root_c).is_err(),
            "the root itself is never a target"
        );
    }

    #[test]
    fn in_use_matches_inside_only() {
        let u = InUse::from_paths(vec![(7, PathBuf::from("/p/target/debug/app"))]);
        assert_eq!(u.user_of(Path::new("/p/target")), Some(7));
        assert_eq!(u.user_of(Path::new("/p/targ")), None);
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
    fn git_states() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path().canonicalize().unwrap();
        if !git(&r, &["init", "-q"]) {
            return; // git not installed
        }
        fs::write(r.join(".gitignore"), "target/\n").unwrap();
        for d in ["target", "build", "vendored"] {
            fs::create_dir(r.join(d)).unwrap();
            fs::write(r.join(d).join("f"), "x").unwrap();
        }
        assert!(git(&r, &["add", ".gitignore", "vendored"]));
        assert!(git(&r, &["commit", "-qm", "init"]));
        assert_eq!(git_state(&r.join("target")), GitState::Ignored);
        assert_eq!(git_state(&r.join("build")), GitState::UntrackedNotIgnored);
        assert_eq!(git_state(&r.join("vendored")), GitState::Tracked);
        // Force-added file inside an ignored folder.
        assert!(git(&r, &["add", "-f", "target/f"]));
        assert_eq!(git_state(&r.join("target")), GitState::Tracked);
        assert_eq!(git_state(tmp.path().parent().unwrap()), GitState::NotInRepo);
    }

    proptest! {
        /// Whatever the components, a lexically joined path that contains
        /// `..` never passes containment.
        #[test]
        fn dotdot_never_contained(parts in proptest::collection::vec("[a-z]{1,3}|\\.\\.", 1..6)) {
            let root = tempfile::tempdir().unwrap();
            let root_c = root.path().canonicalize().unwrap();
            let mut p = root_c.clone();
            for part in &parts {
                p.push(part);
            }
            if let Ok(c) = contained(&p, &root_c) {
                prop_assert!(c.starts_with(&root_c) && c != root_c);
                prop_assert!(!parts.iter().any(|x| x == ".."));
            }
        }
    }
}
