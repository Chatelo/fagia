//! Clean and undo through the CLI.

mod common;

use common::Env;
use predicates::prelude::*;
use std::fs;

fn fixture(env: &Env) {
    env.write("code/api/Cargo.toml", 10);
    env.write("code/api/target/debug/app", 2 << 20);
    env.write("code/web/package.json", 10);
    env.write("code/web/node_modules/x/index.js", 1 << 20);
    env.write("code/py/.venv/pyvenv.cfg", 10);
    env.write("code/py/.venv/lib/x.py", 1 << 20);
}

#[test]
fn dry_run_changes_nothing() {
    let env = Env::new();
    fixture(&env);
    env.cmd()
        .args(["clean", "code", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run"))
        .stdout(predicate::str::contains("[x]"))
        .stdout(predicate::str::contains("[ ]"));
    assert!(env.home().join("code/api/target").exists());
}

#[test]
fn clean_without_terminal_needs_yes() {
    let env = Env::new();
    fixture(&env);
    env.cmd()
        .args(["clean", "code"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("pass -y"))
        // The dry-run list is still shown.
        .stdout(predicate::str::contains("Dry run"));
    assert!(env.home().join("code/api/target").exists());
}

#[test]
fn yes_trashes_preselected_then_undo_restores() {
    let env = Env::new();
    fixture(&env);
    env.cmd()
        .args(["clean", "code", "-y"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run"))
        .stdout(predicate::str::contains("Trashed 2 of 2"));
    let h = env.home();
    assert!(!h.join("code/api/target").exists());
    assert!(!h.join("code/web/node_modules").exists());
    assert!(
        h.join("code/py/.venv").exists(),
        "medium risk is never auto-selected"
    );
    assert!(h.join(".local/share/Trash/files/target").exists());
    let log = fs::read_to_string(h.join(".local/state/fagia/actions.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2);

    env.cmd()
        .args(["undo", "--list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("RESTORABLE"));
    env.cmd()
        .args(["undo", "-y"])
        .assert()
        .success()
        .stdout(predicate::str::contains("restored"));
    assert!(h.join("code/api/target/debug/app").exists());
    assert!(h.join("code/web/node_modules/x/index.js").exists());
    env.cmd()
        .args(["undo", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("nothing to undo"));
}

#[test]
fn json_clean_reports_result() {
    let env = Env::new();
    fixture(&env);
    let out = env
        .cmd()
        .args(["clean", "code", "--json", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "clean");
    assert_eq!(v["result"]["results"].as_array().unwrap().len(), 2);
    assert_eq!(v["result"]["mode"], "trash");
    env.cmd().args(["clean", "code", "--json"]).assert().code(1);
}

#[test]
fn permanent_needs_typed_confirmation() {
    let env = Env::new();
    fixture(&env);
    env.cmd()
        .args(["clean", "code", "--permanent", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("typed confirmation"));
    assert!(env.home().join("code/api/target").exists());
}

#[test]
fn config_protect_is_honoured() {
    let env = Env::new();
    fixture(&env);
    let cfg = env.home().join(".config/fagia/config.toml");
    fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    fs::write(&cfg, "[protect]\npaths = [\"~/code/api\"]\n").unwrap();
    env.cmd()
        .args(["clean", "code", "-y"])
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped: inside protected"));
    assert!(env.home().join("code/api/target").exists());
    assert!(!env.home().join("code/web/node_modules").exists());
}

#[test]
fn project_protect_is_honoured() {
    let env = Env::new();
    fixture(&env);
    fs::write(
        env.home().join("code/web/.fagia.toml"),
        "[protect]\npaths = [\"node_modules\"]\n",
    )
    .unwrap();
    env.cmd().args(["clean", "code", "-y"]).assert().success();
    assert!(env.home().join("code/web/node_modules").exists());
}

#[test]
fn category_filter_limits_clean() {
    let env = Env::new();
    fixture(&env);
    env.cmd()
        .args(["clean", "code", "-y", "--category", "rust"])
        .assert()
        .success();
    assert!(!env.home().join("code/api/target").exists());
    assert!(env.home().join("code/web/node_modules").exists());
}
