//! Which files belong to the person rather than to a program: the scope
//! for duplicate removal and near-duplicate detection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Files that mark a folder as a code project.
pub const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "requirements.txt",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "Gemfile",
    "mix.exs",
    "pubspec.yaml",
    "CMakeLists.txt",
];

/// Folder names whose contents belong to programs, not people.
pub const PROGRAM_FOLDERS: &[&str] = &[
    "node_modules",
    "site-packages",
    "vendor",
    "target",
    "__pycache__",
];

/// Answers "is this path the user's own?" for many paths, remembering which
/// folders are code projects so each folder is checked once.
pub struct Scope {
    root: PathBuf,
    any_type: bool,
    projects: HashMap<PathBuf, bool>,
}

impl Scope {
    pub fn new(root: &Path, any_type: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            any_type,
            projects: HashMap::new(),
        }
    }

    /// True if `dir` or a folder between it and the root is a code project.
    fn in_project(&mut self, dir: &Path) -> bool {
        if dir == self.root || !dir.starts_with(&self.root) {
            return false;
        }
        if let Some(&v) = self.projects.get(dir) {
            return v;
        }
        let v = PROJECT_MARKERS.iter().any(|m| dir.join(m).exists())
            || dir.parent().is_some_and(|p| self.in_project(p));
        self.projects.insert(dir.to_path_buf(), v);
        v
    }

    /// Why `path` (a file) is out of scope, if it is. `.git` is always out.
    pub fn exclusion(&mut self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let dirs: Vec<String> = rel
            .parent()
            .into_iter()
            .flat_map(|p| p.components())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if dirs.iter().any(|c| c == ".git") {
            return Some("inside git's own storage".into());
        }
        if self.any_type {
            return None;
        }
        if let Some(c) = dirs.iter().find(|c| c.starts_with('.')) {
            return Some(format!(
                "inside hidden folder {c} (app data; --any-type to include)"
            ));
        }
        if let Some(c) = dirs.iter().find(|c| PROGRAM_FOLDERS.contains(&c.as_str())) {
            return Some(format!("inside {c} (program files; --any-type to include)"));
        }
        let parent = path.parent()?.to_path_buf();
        if self.in_project(&parent) {
            let project = parent
                .ancestors()
                .take_while(|a| a.starts_with(&self.root) && *a != self.root)
                .filter(|a| PROJECT_MARKERS.iter().any(|m| a.join(m).exists()))
                .last()
                .unwrap_or(&parent)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            return Some(format!(
                "inside project {project} (its code may use it; --any-type to include)"
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn projects_hidden_and_program_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("code/app/src/deep")).unwrap();
        fs::write(r.join("code/app/package.json"), "{}").unwrap();
        fs::create_dir_all(r.join("Documents")).unwrap();
        let mut s = Scope::new(r, false);
        assert!(s.exclusion(&r.join("Documents/a.pdf")).is_none());
        assert!(
            s.exclusion(&r.join("code/app/src/deep/a.pdf"))
                .unwrap()
                .contains("inside project app")
        );
        assert!(
            s.exclusion(&r.join(".cache/a.pdf"))
                .unwrap()
                .contains("hidden")
        );
        assert!(
            s.exclusion(&r.join("x/node_modules/a.pdf"))
                .unwrap()
                .contains("node_modules")
        );
        let mut any = Scope::new(r, true);
        assert!(any.exclusion(&r.join("code/app/src/deep/a.pdf")).is_none());
        assert!(
            any.exclusion(&r.join("repo/.git/objects/x"))
                .unwrap()
                .contains("git")
        );
    }
}
