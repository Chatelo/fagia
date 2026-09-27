//! Walker throughput on a generated tree. Compare against `du` with
//! hyperfine for end-to-end numbers (see docs/performance.md).

use criterion::{Criterion, criterion_group, criterion_main};
use fagia_core::disk::{Progress, ScanOptions, scan};
use fagia_core::platform::linux::LinuxPlatform;
use fagia_core::rules::RuleSet;
use std::fs;

fn tree(files: usize) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..files {
        let dir = tmp.path().join(format!("p{}/src/m{}", i % 50, i % 17));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("f{i}.rs")), b"fn main() {}").unwrap();
    }
    tmp
}

fn bench(c: &mut Criterion) {
    let tmp = tree(20_000);
    let plat = LinuxPlatform::new();
    let rules = RuleSet::builtin(fagia_core::platform::Platform::dirs(&plat)).unwrap();
    let opts = ScanOptions::new(tmp.path());
    c.bench_function("scan 20k files, no rules", |b| {
        b.iter(|| scan(&plat, None, &opts, &Progress::default()).unwrap())
    });
    c.bench_function("scan 20k files, built-in rules", |b| {
        b.iter(|| scan(&plat, Some(&rules), &opts, &Progress::default()).unwrap())
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
