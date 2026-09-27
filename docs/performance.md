# Performance

Measured 2026-09-27 on the development machine: 4 cores, SATA SSD, ext4,
release build. Home folder: 1.37 M files, 153 k folders, 520 k hard-linked
files (uv and pnpm caches), 168 GB.

| Target                       | Goal       | Measured                  |
| ---------------------------- | ---------- | ------------------------- |
| Home folder scan, warm cache | < 15 s     | 3.2–5.0 s                 |
| Tool's own RAM, 1.4 M files  | < 300 MiB  | 196 MiB peak RSS          |
| Total vs `du -s -B1 ~`       | within 1%  | 12 KiB difference on 168 GB |
| `fagia mem` snapshot         | < 1 s      | 0.35 s (290 processes)    |
| Live view CPU                | < 3%       | 2.2% (2 s refresh, 90 s run) |
| TUI frame (any tab)          | —          | 0.6–4 ms; no key handler above 1.2 ms |

Reproduce:

```sh
cargo build --release
/usr/bin/time -f '%e s, %M KiB' target/release/fagia top ~ --json > /dev/null
du -s -B1 ~
cargo bench -p fagia-core            # walker throughput on a 20 k-file tree
/usr/bin/time -f '%U+%S s CPU over %e s' timeout -s INT 90 target/release/fagia mem --live > /dev/null
hyperfine 'target/release/fagia top ~' 'du -sh ~'   # if hyperfine is installed
```
