use super::*;
use crate::disk::walk::{Classifier, Progress, ScanOptions, scan};
use crate::platform::linux::LinuxPlatform;
use std::fs;
use tempfile::TempDir;

fn dirs_at(home: &Path) -> Dirs {
    Dirs {
        home: home.to_path_buf(),
        cache: home.join(".cache"),
        data: home.join(".local/share"),
        config: home.join(".config"),
        state: home.join(".local/state"),
        cargo_home: home.join(".cargo"),
    }
}

fn touch(p: &Path) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b"x").unwrap();
}

/// A concrete marker file name for a glob pattern.
fn marker_for(pattern: &str) -> String {
    pattern.replace('*', "js")
}

fn classify(rs: &RuleSet, dir: &Path) -> Option<Match> {
    let siblings = list_names(dir.parent().unwrap());
    rs.classify_dir(dir, dir.file_name().unwrap(), &siblings, &mut || {
        Some(list_names(dir))
    })
}

#[test]
fn builtin_rules_load_and_validate() {
    let tmp = TempDir::new().unwrap();
    let rs = RuleSet::builtin(&dirs_at(tmp.path())).unwrap();
    assert!(rs.rules().len() >= 20);
    assert!(rs.mem_rules().iter().any(|m| m.system));
    let protected = vec![
        PathBuf::from("/"),
        PathBuf::from("/usr"),
        tmp.path().to_path_buf(),
    ];
    assert_eq!(rs.validate(&protected), Vec::<String>::new());
}

/// Every built-in dir rule: with its evidence it matches; the same name
/// without evidence (the decoy) is never a finding.
#[test]
fn every_dir_rule_needs_its_evidence() {
    let tmp = TempDir::new().unwrap();
    let rs = RuleSet::builtin(&dirs_at(tmp.path())).unwrap();
    for (i, rule) in rs
        .rules()
        .iter()
        .filter(|r| r.kind == RuleKind::Dir)
        .enumerate()
    {
        for name in &rule.names {
            let base = tmp.path().join(format!("real{i}-{name}"));
            let target = base.join(name);
            fs::create_dir_all(&target).unwrap();
            if let Some(p) = rule.require_sibling.patterns.first() {
                touch(&base.join(marker_for(p)));
            }
            if let Some(p) = rule.require_inside.patterns.first() {
                touch(&target.join(marker_for(p)));
            }
            let m = classify(&rs, &target)
                .unwrap_or_else(|| panic!("{} did not match {name}", rule.id));
            assert!(m.regenerable == rule.regenerable, "{}", rule.id);

            let decoy = tmp.path().join(format!("decoy{i}-{name}")).join(name);
            fs::create_dir_all(&decoy).unwrap();
            touch(&decoy.join("unrelated.txt"));
            let has_evidence = !rule.require_sibling.is_empty() || !rule.require_inside.is_empty();
            if has_evidence {
                assert_eq!(
                    classify(&rs, &decoy),
                    None,
                    "decoy for {} ({name}) matched",
                    rule.id
                );
            }
        }
    }
}

#[test]
fn overlapping_rules_pick_highest_priority() {
    let tmp = TempDir::new().unwrap();
    let rs = RuleSet::builtin(&dirs_at(tmp.path())).unwrap();
    let proj = tmp.path().join("app");
    touch(&proj.join("build.gradle.kts"));
    touch(&proj.join("package.json"));
    fs::create_dir_all(proj.join("build")).unwrap();
    let m = classify(&rs, &proj.join("build")).unwrap();
    assert_eq!(&*m.rule_id, "gradle-build");
    assert_eq!(m.evidence, "build.gradle.kts in parent");
    assert_eq!(m.project_dir.as_deref(), Some(proj.as_path()));

    let checks = rs.explain(&proj.join("build"));
    assert_eq!(checks[0].rule_id, "gradle-build");
    assert!(
        checks
            .iter()
            .any(|c| c.rule_id == "generic-build" && c.matched)
    );
    assert!(
        checks
            .iter()
            .any(|c| c.rule_id == "flutter-build" && !c.matched)
    );
}

#[test]
fn path_rules_resolve_variables() {
    let tmp = TempDir::new().unwrap();
    let d = dirs_at(tmp.path());
    let rs = RuleSet::builtin(&d).unwrap();
    let pip = d.cache.join("pip");
    fs::create_dir_all(&pip).unwrap();
    let m = classify(&rs, &pip).unwrap();
    assert_eq!(&*m.rule_id, "pip-cache");
    assert!(m.evidence.starts_with("known path"));
}

#[test]
fn file_rules_match_compound_extensions() {
    let tmp = TempDir::new().unwrap();
    let rs = RuleSet::builtin(&dirs_at(tmp.path())).unwrap();
    let p = tmp.path().join("backup.TAR.GZ");
    let m = rs.classify_file(&p, p.file_name().unwrap(), &[]).unwrap();
    assert_eq!(&*m.rule_id, "archives");
    assert_eq!(m.evidence, "extension .tar.gz");
    let model = tmp.path().join("llama.gguf");
    assert_eq!(
        &*rs.classify_file(&model, model.file_name().unwrap(), &[])
            .unwrap()
            .rule_id,
        "ai-model-files"
    );
    let plain = tmp.path().join("notes.txt");
    assert!(
        rs.classify_file(&plain, plain.file_name().unwrap(), &[])
            .is_none()
    );
}

#[test]
fn user_rules_override_and_add() {
    let tmp = TempDir::new().unwrap();
    let cfg = Config::parse(
        r#"
[[rule]]
id = "generic-build"
enabled = false
[[rule]]
id = "elixir-build"
category = "Elixir build"
kind = "dir"
names = ["_build", "deps"]
require_sibling = ["mix.exs"]
regenerable = true
risk = "low"
"#,
    )
    .unwrap();
    let rs = RuleSet::load(&cfg, &dirs_at(tmp.path())).unwrap();
    assert!(!rs.get("generic-build").unwrap().enabled);
    assert_eq!(rs.get("generic-build").unwrap().origin, Origin::Overridden);
    let proj = tmp.path().join("ex");
    touch(&proj.join("mix.exs"));
    touch(&proj.join("package.json"));
    fs::create_dir_all(proj.join("_build")).unwrap();
    fs::create_dir_all(proj.join("dist")).unwrap();
    assert_eq!(
        &*classify(&rs, &proj.join("_build")).unwrap().rule_id,
        "elixir-build"
    );
    assert_eq!(
        classify(&rs, &proj.join("dist")),
        None,
        "disabled rule still matched"
    );
}

#[test]
fn invalid_rules_are_rejected() {
    let tmp = TempDir::new().unwrap();
    let d = dirs_at(tmp.path());
    for bad in [
        "[[rule]]\nid = \"x\"\nkind = \"dir\"\n",
        "[[rule]]\nid = \"x\"\nkind = \"dir\"\nnames = [\"a/b\"]\n",
        "[[rule]]\nid = \"x\"\nkind = \"path\"\npaths = [\"{nope}/x\"]\n",
        "[[rule]]\nid = \"x\"\nnames = [\"a\"]\n",
    ] {
        let cfg = Config::parse(bad).unwrap();
        assert!(RuleSet::load(&cfg, &d).is_err(), "accepted: {bad}");
    }
    let cfg =
        Config::parse("[[rule]]\nid = \"home\"\nkind = \"path\"\npaths = [\"{home}\"]\n").unwrap();
    let rs = RuleSet::load(&cfg, &d).unwrap();
    assert_eq!(rs.validate(std::slice::from_ref(&d.home)).len(), 1);
}

#[test]
fn scan_collapses_suspects_and_skips_decoys() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("code");
    touch(&root.join("api/Cargo.toml"));
    touch(&root.join("api/target/debug/app"));
    touch(&root.join("api/target/debug/deps/x.rlib"));
    touch(&root.join("web/package.json"));
    touch(&root.join("web/node_modules/left-pad/index.js"));
    touch(&root.join("decoy/target/keep.txt"));
    touch(&root.join("decoy/build/keep.txt"));
    let plat = LinuxPlatform::new();
    let rs = RuleSet::builtin(&dirs_at(tmp.path())).unwrap();
    let s = scan(
        &plat,
        Some(&rs),
        &ScanOptions::new(&root),
        &Progress::default(),
    )
    .unwrap();
    let mut ids: Vec<_> = s.findings.iter().map(|f| f.rule_id.to_string()).collect();
    ids.sort();
    assert_eq!(ids, ["node-modules", "rust-target"]);
    let target = s
        .tree
        .find(&root.canonicalize().unwrap().join("api/target"))
        .unwrap();
    assert_eq!(
        s.tree.children(target).count(),
        0,
        "suspect children were listed"
    );
    assert_eq!(s.tree.node(target).files, 2);
    assert!(
        s.tree
            .find(&root.canonicalize().unwrap().join("decoy/target"))
            .is_some()
    );
}
