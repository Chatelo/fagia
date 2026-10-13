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

#[test]
fn home_local_toolchain_caches_are_medium_risk() {
    let tmp = TempDir::new().unwrap();
    let rs = RuleSet::builtin(&dirs_at(tmp.path())).unwrap();
    for id in [
        "cargo-cache",
        "npm-cache",
        "yarn-cache",
        "pnpm-store",
        "pip-cache",
        "go-build-cache",
        "gradle-cache",
        "maven-repo",
        "huggingface-cache",
    ] {
        let rule = rs
            .rules()
            .iter()
            .find(|rule| &*rule.id == id)
            .unwrap_or_else(|| panic!("missing cache rule {id}"));
        assert!(rule.report_only, "{id}");
    }
    for id in ["cargo-cache", "npm-cache", "gradle-cache"] {
        let rule = rs
            .rules()
            .iter()
            .find(|rule| &*rule.id == id)
            .unwrap_or_else(|| panic!("missing built-in rule {id}"));
        assert_eq!(rule.risk, crate::model::Risk::Medium, "{id}");
        assert!(rule.report_only, "{id}");
    }
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
            assert_eq!(m.report_only, rule.report_only, "{}", rule.id);

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

/// Paths a real clean trashed, breaking npm, pnpm, global CLIs and editor
/// extensions: each is a `node_modules` beside a `package.json`, but
/// inside installed software.
#[test]
fn installed_software_is_never_a_finding() {
    let tmp = TempDir::new().unwrap();
    let d = dirs_at(tmp.path());
    let rs = RuleSet::builtin(&d).unwrap();
    for rel in [
        ".nvm/versions/node/v24.14.1/lib/node_modules/npm",
        ".nvm/versions/node/v24.14.1/lib/node_modules/eas-cli",
        ".local/share/pnpm/.tools/pnpm/10.33.2_tmp_17794_0",
        ".local/share/pnpm/global/v11/d59c-1a0d7a4a57a-2a1ab43d7c716a03",
        ".vscode/extensions/ritwickdey.liveserver-5.7.10",
        ".config/Code/agent-host/sdk-cache/claude/0.3.220/linux-x64/node_modules/type-is",
        ".jdks/openjdk-21/lib/js-tool",
        "Android/Sdk/emulator/lib/js-tool",
        // Not under a known tool root: caught by the ancestor folder name.
        "somewhere/lib/node_modules/some-cli",
        "tools/bin/js-tool",
    ] {
        let pkg = tmp.path().join(rel);
        touch(&pkg.join("package.json"));
        let nm = pkg.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        let m = classify(&rs, &nm).unwrap_or_else(|| panic!("{rel} did not match"));
        assert!(m.report_only, "{rel} was not report-only");
    }

    // A project's own dependency tree is measured but not a suspect.
    let proj = tmp.path().join("projs/app");
    touch(&proj.join("package.json"));
    fs::create_dir_all(proj.join("node_modules")).unwrap();
    let m = classify(&rs, &proj.join("node_modules")).unwrap();
    assert!(m.report_only);

    // Fixed-path rules still apply inside a tool root.
    let store = d.data.join("pnpm/store");
    fs::create_dir_all(&store).unwrap();
    let m = classify(&rs, &store).unwrap();
    assert_eq!(&*m.rule_id, "pnpm-store");
    assert!(m.report_only);

    let installed_project = tmp.path().join(".nvm/versions/node/v24/native-addon");
    touch(&installed_project.join("Cargo.toml"));
    fs::create_dir_all(installed_project.join("target")).unwrap();
    assert!(
        classify(&rs, &installed_project.join("target")).is_none(),
        "a Rust target inside an installed toolchain must stay protected"
    );
}

/// Files named like junk that belong to a program: a database journal
/// named `*.log`, an AppImage (a program, not an archive), anything in
/// app data.
#[test]
fn program_files_are_never_findings() {
    let tmp = TempDir::new().unwrap();
    let rs = RuleSet::builtin(&dirs_at(tmp.path())).unwrap();
    let file = |rel: &str| {
        let p = tmp.path().join(rel);
        touch(&p);
        let siblings = list_names(p.parent().unwrap());
        rs.classify_file(&p, p.file_name().unwrap(), &siblings)
    };
    touch(&tmp.path().join("data/db/CURRENT"));
    assert!(file("data/db/000123.log").is_none(), "LevelDB journal");
    assert!(file(".config/google-chrome/Default/x/000900.log").is_none());
    assert!(file("Downloads/audacity.AppImage").is_none());
    assert!(file("bin/old.zip").is_none());
    assert_eq!(&*file("work/build.log").unwrap().rule_id, "logs");
    assert_eq!(&*file("Downloads/old.zip").unwrap().rule_id, "archives");
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
    fs::create_dir_all(root.join("web/node_modules/left-pad")).unwrap();
    fs::write(
        root.join("web/node_modules/left-pad/index.js"),
        vec![b'x'; 1 << 20],
    )
    .unwrap();
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
    assert_eq!(ids, ["rust-target"]);
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
    let node_modules = s
        .tree
        .find(&root.canonicalize().unwrap().join("web/node_modules"))
        .unwrap();
    assert_eq!(s.tree.children(node_modules).count(), 0);
    assert!(s.tree.node(node_modules).real >= 1 << 20);
    assert!(
        s.findings.iter().all(|f| !f.path.ends_with("node_modules")),
        "node_modules appeared as a cleanable finding"
    );
    let target_finding = s
        .findings
        .iter()
        .find(|f| f.path.ends_with("api/target"))
        .unwrap();
    assert_eq!(&*target_finding.rule_id, "rust-target");
    assert_eq!(target_finding.real, s.tree.node(target).real);
    assert!(
        s.tree
            .find(&root.canonicalize().unwrap().join("decoy/target"))
            .is_some()
    );
}

#[test]
fn report_only_cache_is_sized_but_not_a_finding() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    touch(&root.join(".npm/_cacache/content/cache-entry"));
    let platform = LinuxPlatform::new();
    let rules = RuleSet::builtin(&dirs_at(root)).unwrap();
    let scan_result = scan(
        &platform,
        Some(&rules),
        &ScanOptions::new(root),
        &Progress::default(),
    )
    .unwrap();
    assert!(
        scan_result
            .findings
            .iter()
            .all(|f| f.path != root.join(".npm")),
        "the npm cache appeared as a cleanable finding"
    );
    let cache = scan_result.tree.find(&root.join(".npm")).unwrap();
    assert!(scan_result.tree.node(cache).real > 0);
    assert_eq!(scan_result.tree.children(cache).count(), 0);
}
