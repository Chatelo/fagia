//! Suspects, stale, media, dupes, diff, rules and config.

mod common;

use common::{Env, projects, rel};
use predicates::prelude::*;
use std::fs::{self, File, FileTimes};
use std::path::Path;
use std::time::{Duration, SystemTime};

fn age(p: &Path, days: u64) {
    let t = SystemTime::now() - Duration::from_secs(days * 86_400);
    File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_times(FileTimes::new().set_modified(t))
        .unwrap();
}

#[test]
fn suspects_groups_categories_and_skips_decoys() {
    let env = Env::new();
    projects(&env);
    env.write("code/py/.venv/pyvenv.cfg", 10);
    env.write("code/py/.venv/lib/site.py", 1 << 20);
    env.write("code/api/debug.log", 2 << 20);
    let v = env.json(&["suspects", "code", "--min-size", "0"]);
    let cats: Vec<&str> = v["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["category"].as_str().unwrap())
        .collect();
    assert_eq!(cats, ["Rust build", "Node deps", "Python venv", "Logs"]);
    let home = env.home();
    let paths: Vec<String> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| rel(&home, f["path"].as_str().unwrap()))
        .collect();
    assert!(!paths.iter().any(|p| p.contains("decoy")), "{paths:?}");
    let venv = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["rule_id"] == "python-venv")
        .unwrap();
    assert_eq!(venv["evidence"], "pyvenv.cfg inside");
    assert_eq!(
        venv["auto_select"], false,
        "medium risk is never preselected"
    );
    let target = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["rule_id"] == "rust-target")
        .unwrap();
    assert_eq!(target["auto_select"], true);
    assert!(rel(&home, target["project"]["path"].as_str().unwrap()).ends_with("/code/api"));
}

#[test]
fn suspects_text_layout() {
    let env = Env::new();
    projects(&env);
    let out = env.cmd().args(["suspects", "code"]).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("CATEGORY"), "{text}");
    assert!(text.contains("STALE >90d"));
    assert!(text.contains("TOTAL"));
}

#[test]
fn stale_ranks_by_project_age() {
    let env = Env::new();
    for (name, days) in [("old", 400), ("mid", 120), ("new", 1)] {
        let toml = env.write(&format!("code/{name}/Cargo.toml"), 10);
        let src = env.write(&format!("code/{name}/src/lib.rs"), 10);
        env.write(&format!("code/{name}/target/out.bin"), 2 << 20);
        age(&toml, days);
        age(&src, days);
    }
    let v = env.json(&["stale", "code"]);
    let home = env.home();
    let got: Vec<String> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| rel(&home, f["path"].as_str().unwrap()))
        .collect();
    assert_eq!(got, ["/code/old/target", "/code/mid/target"]);
    let v = env.json(&["stale", "code", "--older", "200d"]);
    assert_eq!(v["findings"].as_array().unwrap().len(), 1);
    assert_eq!(v["stale_days"], 200);
}

#[test]
fn media_groups_by_type_and_sniffs_renamed_files() {
    let env = Env::new();
    env.write("Videos/a.mp4", 3 << 20);
    env.write("Music/b.flac", 1 << 20);
    // A PNG with no extension, large enough to be sniffed.
    let p = env.home().join("Downloads/scan");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut png = vec![
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
    ];
    png.resize(2 << 20, 0);
    fs::write(&p, png).unwrap();
    let v = env.json(&["media"]);
    let kinds: Vec<&str> = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["video", "image", "audio"]);
    let image = &v["groups"][1]["largest"][0];
    assert_eq!(image["sniffed"], true);
    assert!(
        v["groups"][0]["folders"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("/Videos")
    );
}

#[test]
fn dupes_reports_wasted_space() {
    let env = Env::new();
    let data: Vec<u8> = (0..(3 << 20)).map(|i: u32| (i % 253) as u8).collect();
    for p in [
        "a/report.bin",
        "b/report (1).bin",
        "c/Report - Copy.bin",
        "d/unrelated.bin",
    ] {
        let path = env.home().join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &data).unwrap();
    }
    // Same name (ignoring copy markers), size and content.
    let v = env.json(&["dupes"]);
    assert_eq!(v["match_names"], true);
    assert_eq!(v["sets"].as_array().unwrap().len(), 1);
    assert_eq!(v["sets"][0]["copies"].as_array().unwrap().len(), 3);
    assert!(v["total_wasted"].as_u64().unwrap() >= 2 * (3 << 20));
    // Content alone also pulls in the differently named file.
    let v = env.json(&["dupes", "--any-name"]);
    assert_eq!(v["sets"][0]["copies"].as_array().unwrap().len(), 4);
}

#[test]
fn diff_reports_growth_between_scans() {
    let env = Env::new();
    env.write("proj/data/a.bin", 1 << 20);
    env.write("proj/old/b.bin", 2 << 20);
    let first = env.json(&["diff", "proj"]);
    assert_eq!(first["snapshots"], 1);
    env.write("proj/data/grown.bin", 4 << 20);
    // Moved out of the scanned tree: the test must not delete (clippy.toml).
    fs::rename(
        env.home().join("proj/old"),
        env.tmp.path().join("old-moved"),
    )
    .unwrap();
    env.write("proj/new/c.bin", 2 << 20);
    let v = env.json(&["diff", "proj"]);
    assert_eq!(v["snapshots"], 2);
    let entries: Vec<(String, String)> = v["diff"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                rel(&env.home(), e["path"].as_str().unwrap()),
                e["change"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            ("/proj/data".to_string(), "grew".to_string()),
            ("/proj/new".to_string(), "appeared".to_string()),
            ("/proj/old".to_string(), "vanished".to_string())
        ]
    );
}

#[test]
fn no_save_keeps_history_empty() {
    let env = Env::new();
    env.write("p/a.bin", 10);
    env.cmd().args(["top", "p", "--no-save"]).assert().success();
    let v = env.json(&["diff", "p", "--no-save"]);
    assert_eq!(v["snapshots"], 0);
}

#[test]
fn rules_list_test_and_validate() {
    let env = Env::new();
    projects(&env);
    let v = env.json(&["rules", "list"]);
    assert!(
        v["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "rust-target")
    );
    let v = env.json(&["rules", "test", "code/decoy/target"]);
    assert!(v["winner"].is_null());
    assert_eq!(v["checks"][0]["reason"], "no Cargo.toml in parent");
    env.cmd()
        .args(["rules", "test", "code/api/target"])
        .assert()
        .success()
        .stdout(predicate::str::contains("matches rule rust-target"));
    env.cmd().args(["rules", "validate"]).assert().success();
}

#[test]
fn broken_config_is_reported_and_fixable() {
    let env = Env::new();
    let cfg = env.home().join(".config/fagia/config.toml");
    fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    fs::write(&cfg, "[general]\nmin_sise = \"1M\"\n").unwrap();
    env.cmd()
        .arg("top")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("min_sise"));
    env.cmd().args(["rules", "validate"]).assert().code(1);
    env.cmd()
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config.toml"));
    fs::write(
        &cfg,
        "[[rule]]\nid = \"everything\"\nkind = \"path\"\npaths = [\"{home}\"]\n",
    )
    .unwrap();
    env.cmd()
        .args(["rules", "validate"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("protected"));
}

#[test]
fn config_init_writes_template_once() {
    let env = Env::new();
    env.cmd().args(["config", "init"]).assert().success();
    env.cmd().args(["config", "init"]).assert().code(1);
    env.cmd()
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"stale_days\": 90"));
}

#[test]
fn hostile_project_file_only_warns() {
    let env = Env::new();
    env.write("repo/src/main.rs", 2 << 20);
    fs::write(
        env.home().join("repo/.fagia.toml"),
        "[[rule]]\nid = \"src\"\nkind = \"dir\"\nnames = [\"src\"]\nregenerable = true\n",
    )
    .unwrap();
    let v = env.json(&["suspects", "repo"]);
    assert!(v["findings"].as_array().unwrap().is_empty());
    assert!(
        v["scan"]["warnings"][0]
            .as_str()
            .unwrap()
            .contains("may only set exclude and protect")
    );
}
