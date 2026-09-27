//! Walker accuracy: totals against `du`, hard links, symlinks, errors.

use fagia_core::disk::{Progress, ScanOptions, scan};
use fagia_core::platform::Platform;
use fagia_core::platform::linux::LinuxPlatform;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn write(p: &Path, len: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![7u8; len]).unwrap();
}

fn du_bytes(p: &Path) -> u64 {
    let out = Command::new("du")
        .arg("-s")
        .arg("-B1")
        .arg(p)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    for i in 0..40 {
        write(&r.join(format!("src/mod{}/file{i}.rs", i % 7)), 100 * i + 1);
    }
    write(&r.join("big/blob.bin"), 3 << 20);
    write(&r.join("big/mid.bin"), 200_000);
    fs::create_dir_all(r.join("empty/deeper/still")).unwrap();
    // Sparse file: 64 MiB length, one written block.
    let f = fs::File::create(r.join("sparse.img")).unwrap();
    f.set_len(64 << 20).unwrap();
    tmp
}

fn run(root: &Path) -> fagia_core::disk::Scan {
    scan(
        &LinuxPlatform::new(),
        None,
        &ScanOptions::new(root),
        &Progress::default(),
    )
    .unwrap()
}

#[test]
fn totals_match_du_within_one_percent() {
    let tmp = fixture();
    let s = run(tmp.path());
    let du = du_bytes(tmp.path());
    let ours = s.total_real();
    let diff = ours.abs_diff(du) as f64 / du as f64;
    assert!(diff <= 0.01, "ours {ours} vs du {du}");
    assert!(
        s.total_apparent() > 64 << 20,
        "apparent should include the sparse length"
    );
    assert!(
        s.notes.iter().any(|n| n.contains("sparse")),
        "{:?}",
        s.notes
    );
}

#[test]
fn hard_links_count_once_and_deterministically() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    write(&r.join("store/pkg/data.bin"), 1 << 20);
    for p in ["b/link.bin", "a/link.bin", "c/deep/link.bin"] {
        fs::create_dir_all(r.join(p).parent().unwrap()).unwrap();
        fs::hard_link(r.join("store/pkg/data.bin"), r.join(p)).unwrap();
    }
    let du = du_bytes(r);
    let first = run(r);
    assert!(first.total_real().abs_diff(du) as f64 / du as f64 <= 0.01);
    let root = r.canonicalize().unwrap();
    let a = first.tree.find(&root.join("a")).unwrap();
    assert!(
        first.tree.node(a).real >= 1 << 20,
        "smallest path wins attribution"
    );
    for _ in 0..5 {
        let again = run(r);
        let a2 = again.tree.find(&root.join("a")).unwrap();
        assert_eq!(again.tree.node(a2).real, first.tree.node(a).real);
        assert_eq!(again.total_real(), first.total_real());
    }
    let counted: Vec<_> = first.files.iter().filter(|f| f.counted).collect();
    assert_eq!(counted.len(), 1, "one record owns the inode");
}

#[test]
fn symlinks_are_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(&outside.path().join("huge.bin"), 4 << 20);
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
    write(&tmp.path().join("small.txt"), 10);
    let s = run(tmp.path());
    assert!(s.total_real() < 1 << 20);
}

#[test]
fn permission_errors_are_counted_not_fatal() {
    if LinuxPlatform::new().effective_uid() == 0 {
        return; // root reads everything
    }
    let tmp = tempfile::tempdir().unwrap();
    let locked = tmp.path().join("locked");
    write(&locked.join("secret.txt"), 10);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let s = run(tmp.path());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(s.errors, 1);
    assert!(s.error_samples[0].contains("locked"));
}

#[test]
fn excludes_and_project_files_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    write(&r.join("proj/vendor/lib.bin"), 2 << 20);
    write(&r.join("proj/src/main.rs"), 10);
    write(&r.join("other/cache.bin"), 2 << 20);
    fs::write(
        r.join("proj/.fagia.toml"),
        "[exclude]\nglobs = [\"vendor/**\", \"vendor\"]\n[protect]\npaths = [\"src\"]\n[[rule]]\nid = \"x\"\n",
    )
    .unwrap();
    let mut opts = ScanOptions::new(r);
    opts.excludes = vec![format!("{}/other", r.canonicalize().unwrap().display())];
    let s = scan(&LinuxPlatform::new(), None, &opts, &Progress::default()).unwrap();
    assert!(
        s.total_real() < 1 << 20,
        "excluded bytes were counted: {}",
        s.total_real()
    );
    assert_eq!(
        s.project_protect,
        vec![r.canonicalize().unwrap().join("proj/src")]
    );
    assert_eq!(s.warnings.len(), 1, "{:?}", s.warnings);
}

#[test]
fn big_files_are_recorded() {
    let tmp = fixture();
    let s = run(tmp.path());
    let names: Vec<_> = s
        .files
        .iter()
        .map(|f| f.name.to_string_lossy().to_string())
        .collect();
    assert!(names.contains(&"blob.bin".to_string()));
    assert!(!names.contains(&"mid.bin".to_string()));
}

#[test]
fn reclaimable_excludes_links_that_survive_elsewhere() {
    use fagia_core::rules::RuleSet;
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    write(&r.join("proj/package.json"), 10);
    write(&r.join("store/shared.bin"), 1 << 20);
    write(&r.join("proj/node_modules/own.bin"), 1 << 20);
    fs::hard_link(
        r.join("store/shared.bin"),
        r.join("proj/node_modules/shared.bin"),
    )
    .unwrap();
    // Both links of this inode are inside node_modules: deleting frees it.
    write(&r.join("proj/node_modules/a/inner.bin"), 1 << 20);
    fs::hard_link(
        r.join("proj/node_modules/a/inner.bin"),
        r.join("proj/node_modules/b.bin"),
    )
    .unwrap();

    let plat = LinuxPlatform::new();
    let rules = RuleSet::builtin(plat.dirs()).unwrap();
    let s = scan(
        &plat,
        Some(&rules),
        &ScanOptions::new(r),
        &Progress::default(),
    )
    .unwrap();
    let f = s
        .findings
        .iter()
        .find(|f| &*f.rule_id == "node-modules")
        .unwrap();
    // "proj/..." sorts before "store/...", so node_modules owns the shared inode.
    assert!(f.real >= 3 << 20, "real {}", f.real);
    assert!(
        f.reclaimable >= 2 << 20 && f.reclaimable < 3 << 20,
        "reclaimable {}",
        f.reclaimable
    );
    let du = du_bytes(r);
    assert!(s.total_real().abs_diff(du) as f64 / du as f64 <= 0.01);
}
