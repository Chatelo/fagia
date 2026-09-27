//! Duplicate folders, loose names and near-duplicates through the CLI.

mod common;

use common::Env;
use predicates::prelude::*;
use std::fs;

fn book(seed: u32) -> Vec<u8> {
    (0..(2u32 << 20))
        .map(|i| ((i ^ seed) % 251) as u8)
        .collect()
}

fn put(env: &Env, rel: &str, data: &[u8]) {
    let p = env.home().join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

#[test]
fn identical_folders_are_one_finding() {
    let env = Env::new();
    for base in ["Documents/books", "backup/Documents/books"] {
        put(&env, &format!("{base}/a.pdf"), &book(1));
        put(&env, &format!("{base}/b.pdf"), &book(2));
    }
    put(&env, "Documents/other.txt", b"only here");
    let v = env.json(&["dupes"]);
    assert_eq!(v["folders"].as_array().unwrap().len(), 1);
    assert!(
        v["folders"][0]["copies"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("Documents/books")
    );
    assert_eq!(
        v["sets"].as_array().unwrap().len(),
        0,
        "files inside the folders are not listed twice"
    );
    assert!(v["total_wasted"].as_u64().unwrap() >= 4 << 20);
    let v = env.json(&["dupes", "--no-folders"]);
    assert_eq!(v["sets"].as_array().unwrap().len(), 2);
}

#[test]
fn clean_trashes_the_backup_folder_copy() {
    let env = Env::new();
    for base in ["Documents/books", "backup/Documents/books"] {
        put(&env, &format!("{base}/a.pdf"), &book(1));
        put(&env, &format!("{base}/b.pdf"), &book(2));
    }
    env.cmd()
        .args(["dupes", "--clean", "-y"])
        .assert()
        .success()
        .stdout(predicate::str::contains("copies of a folder"));
    assert!(env.home().join("Documents/books/a.pdf").exists());
    assert!(!env.home().join("backup/Documents").exists());
    env.cmd().args(["undo", "-y"]).assert().success();
    assert!(env.home().join("backup/Documents/books/b.pdf").exists());
}

#[test]
fn loose_names_catch_site_tags() {
    let env = Env::new();
    put(
        &env,
        "Documents/What to Eat During Cancer T_ (z-library.sk, 1lib.sk).pdf",
        &book(3),
    );
    put(&env, "projs/pdfs/What to Eat During Cancer T.pdf", &book(3));
    assert_eq!(env.json(&["dupes"])["sets"].as_array().unwrap().len(), 0);
    let v = env.json(&["dupes", "--match", "loose"]);
    assert_eq!(v["match_by"], "loose-name-and-content");
    assert_eq!(v["sets"].as_array().unwrap().len(), 1);
    assert_eq!(
        env.json(&["dupes", "--match", "content"])["sets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn similar_text_is_listed_never_removed() {
    let env = Env::new();
    let unix = "line one\nline two\n".repeat(400);
    put(&env, "notes/todo.md", unix.as_bytes());
    put(&env, "old/todo.md", unix.replace('\n', "  \r\n").as_bytes());
    let v = env.json(&["dupes", "--similar", "text"]);
    let sim = v["similar"].as_array().unwrap();
    assert_eq!(sim.len(), 1);
    assert_eq!(sim[0]["kind"], "text");
    assert_eq!(sim[0]["files"].as_array().unwrap().len(), 2);
    env.cmd()
        .args(["dupes", "--similar", "--clean", "-y"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "near-duplicates are never removed",
        ));
    assert!(env.home().join("old/todo.md").exists() && env.home().join("notes/todo.md").exists());
}

#[test]
fn edited_documents_are_similar() {
    let env = Env::new();
    let words: Vec<String> = (0..4000)
        .map(|i| format!("word{}", (i * 7919) % 2203))
        .collect();
    let mut edited = words.clone();
    edited.insert(2000, "a new paragraph in the middle of the proposal".into());
    let unrelated: Vec<String> = (0..4000)
        .map(|i| format!("other{}", (i * 104_729) % 3001))
        .collect();
    put(&env, "work/proposal.md", words.join(" ").as_bytes());
    put(&env, "work/proposal-v2.md", edited.join(" ").as_bytes());
    put(&env, "work/budget.md", unrelated.join(" ").as_bytes());
    let v = env.json(&["dupes", "--similar", "content"]);
    let sim = v["similar"].as_array().unwrap();
    assert_eq!(sim.len(), 1, "{v}");
    assert_eq!(sim[0]["files"].as_array().unwrap().len(), 2);
    assert!(sim[0]["similarity"].as_f64().unwrap() >= 0.8);
    env.cmd()
        .args(["dupes", "--similar", "content"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Similar documents"))
        .stdout(predicate::str::contains("never removed automatically"));
}
