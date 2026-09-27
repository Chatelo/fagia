//! `fagia dupes --clean/--link` and `fagia trash`.

mod common;

use common::Env;
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::MetadataExt;

fn book() -> Vec<u8> {
    (0..(2u32 << 20)).map(|i| (i % 251) as u8).collect()
}

fn put(env: &Env, rel: &str, data: &[u8]) -> std::path::PathBuf {
    let p = env.home().join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, data).unwrap();
    p
}

fn fixture(env: &Env) {
    let b = book();
    put(env, "Documents/book.pdf", &b);
    put(env, "Downloads/book (1).pdf", &b);
    put(env, "backup/book.pdf", &b);
    // Same content inside a project and inside hidden app data: never touched.
    put(env, "projs/app/package.json", b"{}");
    put(env, "projs/app/assets/book.pdf", &b);
    let blob: Vec<u8> = b.iter().rev().copied().collect();
    put(env, ".cache/model/blob", &blob);
    put(env, "other/.cache/blob", &blob);
}

#[test]
fn dry_run_lists_and_changes_nothing() {
    let env = Env::new();
    fixture(&env);
    env.cmd()
        .args(["dupes", "--clean", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("keep     ~/Documents/book.pdf"))
        .stdout(predicate::str::contains("trash    ~/backup/book.pdf"))
        .stdout(predicate::str::contains("inside project app"))
        .stdout(predicate::str::contains("Will trash 2 copies"));
    assert!(env.home().join("backup/book.pdf").exists());
}

#[test]
fn clean_trashes_extras_and_undo_restores() {
    let env = Env::new();
    fixture(&env);
    env.cmd()
        .args(["dupes", "--clean"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("pass -y"));
    env.cmd()
        .args(["dupes", "--clean", "-y"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Trashed 2 of 2"));
    let h = env.home();
    assert!(h.join("Documents/book.pdf").exists());
    assert!(!h.join("backup/book.pdf").exists() && !h.join("Downloads/book (1).pdf").exists());
    assert!(
        h.join("projs/app/assets/book.pdf").exists(),
        "project copy untouched"
    );
    assert!(
        h.join(".cache/model/blob").exists() && h.join("other/.cache/blob").exists(),
        "app data untouched"
    );
    env.cmd().args(["undo", "-y"]).assert().success();
    assert!(h.join("backup/book.pdf").exists());
}

#[test]
fn link_mode_frees_space_and_keeps_paths() {
    let env = Env::new();
    fixture(&env);
    let out = env
        .cmd()
        .args(["dupes", "--link", "--json", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["mode"], "link");
    assert_eq!(v["result"]["results"].as_array().unwrap().len(), 2);
    let h = env.home();
    let ino = |p: &str| fs::metadata(h.join(p)).unwrap().ino();
    assert_eq!(ino("Documents/book.pdf"), ino("backup/book.pdf"));
    assert_eq!(ino("Documents/book.pdf"), ino("Downloads/book (1).pdf"));
    assert_eq!(fs::read(h.join("backup/book.pdf")).unwrap(), book());
}

#[test]
fn trash_lists_and_empty_needs_typed_confirmation() {
    let env = Env::new();
    fixture(&env);
    env.cmd()
        .args(["dupes", "--clean", "-y"])
        .assert()
        .success();
    let v = env.json(&["trash"]);
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    assert!(
        v["items"][0]["original"]["path"]
            .as_str()
            .unwrap()
            .ends_with(".pdf")
    );
    env.cmd()
        .arg("trash")
        .assert()
        .success()
        .stdout(predicate::str::contains("fagia trash --empty"));
    // No terminal: refused even with -y; nothing deleted.
    env.cmd()
        .args(["trash", "--empty", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("typed confirmation"));
    env.cmd()
        .args(["trash", "--empty", "--json"])
        .assert()
        .code(1);
    assert_eq!(env.json(&["trash"])["items"].as_array().unwrap().len(), 2);
    // Just trashed: nothing is 30 days old.
    env.cmd()
        .args(["trash", "--older", "30d"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing in the trash is that old"));
}

#[test]
fn clean_points_trash_contents_to_trash_command() {
    let env = Env::new();
    put(
        &env,
        ".local/share/Trash/files/old.bin",
        &vec![1u8; 2 << 20],
    );
    env.cmd()
        .args(["clean", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fagia trash --empty"));
}
