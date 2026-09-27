//! Summary, top and big; exit codes and output rules.

mod common;

use common::{Env, projects, rel};
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn top_json_is_versioned_and_sorted() {
    let env = Env::new();
    projects(&env);
    let v = env.json(&["top", "code", "--min-size", "0"]);
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "top");
    let entries = v["entries"].as_array().unwrap();
    let sizes: Vec<u64> = entries
        .iter()
        .map(|e| e["real"].as_u64().unwrap())
        .collect();
    let mut sorted = sizes.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(sizes, sorted);
    let first = entries[0]["path"].as_str().unwrap();
    assert!(first.ends_with("/code/api"), "{first}");
}

#[test]
fn top_text_snapshot() {
    let env = Env::new();
    env.write("data/a/one.bin", 1 << 20);
    env.write("data/b/two.bin", 2 << 20);
    env.write("data/loose.txt", 4096);
    let out = env
        .cmd()
        .args(["top", "~/data", "--min-size", "0"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    // Sizes depend on the filesystem's block size; keep paths and order.
    let paths: Vec<&str> = text
        .lines()
        .map(|l| l.split("  ").last().unwrap().trim())
        .collect();
    insta::assert_debug_snapshot!(paths);
}

#[test]
fn big_lists_largest_files_with_category() {
    let env = Env::new();
    projects(&env);
    env.write("Downloads/image.iso", 5 << 20);
    let v = env.json(&["big", "--limit", "2"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert!(files[0]["path"].as_str().unwrap().ends_with("image.iso"));
    assert_eq!(files[0]["category"], "Archives");
}

#[test]
fn summary_shows_suspects_not_decoys() {
    let env = Env::new();
    projects(&env);
    let v = env.json(&[]);
    assert_eq!(v["command"], "summary");
    let home = env.home();
    let mut paths: Vec<String> = v["top_suspects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| rel(&home, f["path"].as_str().unwrap()))
        .collect();
    paths.sort();
    assert_eq!(paths, ["/code/api/target", "/code/web/node_modules"]);
    assert!(v["disk"]["stats"]["total"].as_u64().unwrap() > 0);
}

#[test]
fn permission_errors_exit_3() {
    let env = Env::new();
    env.write("x/locked/secret", 10);
    let locked = env.home().join("x/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let is_root = fagia_core::platform::current().effective_uid() == 0;
    let assert = env.cmd().args(["top", "x"]).assert();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if !is_root {
        assert
            .code(3)
            .stderr(predicate::str::contains("could not be read"));
    }
}

#[test]
fn bad_input_exit_codes() {
    let env = Env::new();
    env.cmd().args(["top", "--depth", "x"]).assert().code(2);
    env.cmd()
        .args(["top", "--min-size", "ten"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("invalid size"));
    env.cmd().args(["top", "/no/such/dir"]).assert().code(1);
}

#[test]
fn control_characters_are_escaped() {
    let env = Env::new();
    env.write("evil/\x1b[2Jname.bin", 2 << 20);
    let out = env.cmd().args(["big", "evil"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains('\x1b'),
        "raw escape reached the terminal: {text:?}"
    );
    assert!(text.contains("\\u{1b}[2Jname.bin"));
}

#[test]
fn non_utf8_paths_carry_bytes_in_json() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let env = Env::new();
    let dir = env.home().join("bytes");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(OsStr::from_bytes(b"\xffbad.bin")),
        vec![0u8; 2 << 20],
    )
    .unwrap();
    let v = env.json(&["big", "bytes"]);
    let f = &v["files"][0];
    assert!(f["path"].as_str().unwrap().contains('\u{fffd}'));
    assert!(f["path_bytes"].is_string());
}

#[test]
fn csv_output() {
    let env = Env::new();
    env.write("c/a.bin", 2 << 20);
    env.cmd()
        .args(["big", "c", "--csv"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("SIZE,AGE,PATH\n"));
}

#[test]
fn completions_and_man_page() {
    let env = Env::new();
    env.cmd()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_fagia"));
    env.cmd()
        .arg("man")
        .assert()
        .success()
        .stdout(predicate::str::contains(".TH fagia"));
}
