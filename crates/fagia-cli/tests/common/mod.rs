#![allow(dead_code)]

use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// An isolated environment: HOME, XDG dirs and config all inside a temp
/// folder, so tests never read or write the real user's files.
pub struct Env {
    pub tmp: TempDir,
}

impl Env {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("home")).unwrap();
        Self { tmp }
    }

    pub fn home(&self) -> PathBuf {
        self.tmp.path().join("home").canonicalize().unwrap()
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("fagia").unwrap();
        let home = self.home();
        c.current_dir(&home)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("NO_COLOR", "1");
        c
    }

    pub fn write(&self, rel: &str, len: usize) -> PathBuf {
        let p = self.home().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![b'x'; len]).unwrap();
        p
    }

    pub fn json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.cmd().args(args).arg("--json").output().unwrap();
        assert!(
            out.status.code() == Some(0) || out.status.code() == Some(3),
            "status {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

pub fn projects(env: &Env) {
    env.write("code/api/Cargo.toml", 100);
    env.write("code/api/src/main.rs", 100);
    env.write("code/api/target/debug/app", 3 << 20);
    env.write("code/web/package.json", 100);
    env.write("code/web/node_modules/left-pad/index.js", 2 << 20);
    env.write("code/decoy/target/keep.bin", 1 << 20);
    env.write("code/decoy/build/keep.bin", 1 << 20);
    env.write("Videos/holiday.mp4", 2 << 20);
}

pub fn rel(home: &Path, p: &str) -> String {
    p.strip_prefix(home.to_str().unwrap())
        .unwrap_or(p)
        .to_string()
}
