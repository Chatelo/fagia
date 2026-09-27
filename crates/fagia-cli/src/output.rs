//! Terminal and machine output: aligned tables, colour only on a terminal
//! (respecting NO_COLOR), CSV, and versioned JSON.
//!
//! Two switches: `fancy` (stdout is a terminal: bars and symbols) and
//! `color` (fancy and NO_COLOR unset). Piped output is plain text, stable
//! for scripts.

use fagia_core::paths::{display_path, escape_control};
use fagia_core::report::Envelope;
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Out {
    pub json: bool,
    pub csv: bool,
    pub color: bool,
    pub fancy: bool,
    pub verbose: bool,
    pub home: PathBuf,
}

impl Out {
    pub fn new(json: bool, csv: bool, verbose: bool, home: PathBuf) -> Self {
        let fancy = std::io::stdout().is_terminal() && !json && !csv;
        let color = fancy && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        Self {
            json,
            csv,
            color,
            fancy,
            verbose,
            home,
        }
    }

    pub fn path(&self, p: &Path) -> String {
        display_path(p, Some(&self.home))
    }

    /// A path with its folder part dimmed and its name emphasised.
    pub fn path_styled(&self, p: &Path) -> String {
        let s = self.path(p);
        if !self.color {
            return s;
        }
        match s.rfind('/') {
            Some(i) if i + 1 < s.len() => {
                format!("{}{}", self.dim(&s[..=i]), self.bold(&s[i + 1..]))
            }
            _ => self.bold(&s),
        }
    }

    pub fn print_json<T: Serialize>(&self, command: &str, data: T) -> anyhow::Result<()> {
        println!(
            "{}",
            serde_json::to_string_pretty(&Envelope::new(command, data))?
        );
        Ok(())
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn italic(&self, s: &str) -> String {
        self.paint("2;3", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn accent(&self, s: &str) -> String {
        self.paint("36", s)
    }

    /// A size coloured by magnitude, so big items stand out.
    pub fn size(&self, bytes: u64) -> String {
        let s = fagia_core::size::format_size(bytes);
        let code = match bytes {
            b if b >= 10 << 30 => "1;35",
            b if b >= 1 << 30 => "1;31",
            b if b >= 100 << 20 => "33",
            b if b >= 1 << 20 => "32",
            _ => "2",
        };
        self.paint(code, &s)
    }

    /// [`Out::size`] right-aligned to `width` columns (padding before the
    /// colour codes, which take no space).
    pub fn size_pad(&self, bytes: u64, width: usize) -> String {
        let plain = fagia_core::size::format_size(bytes);
        format!(
            "{}{}",
            " ".repeat(width.saturating_sub(plain.chars().count())),
            self.size(bytes)
        )
    }

    /// Days since last touched, warmer as it ages.
    pub fn age_days(&self, days: u64) -> String {
        self.aged(format!("{days}d"), days)
    }

    /// A compact age (`5h`, `210d`) coloured like [`Out::age_days`].
    pub fn age(&self, secs: u64) -> String {
        self.aged(fagia_core::size::format_age(secs), secs / 86_400)
    }

    fn aged(&self, s: String, days: u64) -> String {
        match days {
            d if d >= 365 => self.red(&s),
            d if d >= 90 => self.yellow(&s),
            _ => self.dim(&s),
        }
    }

    /// A share bar with eighth-block precision; empty when not on a terminal.
    pub fn bar(&self, fraction: f64, width: usize) -> String {
        if !self.fancy {
            return String::new();
        }
        const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
        let f = fraction.clamp(0.0, 1.0) * width as f64;
        let full = f as usize;
        let mut s = "█".repeat(full);
        if full < width {
            s.push(EIGHTHS[((f - full as f64) * 8.0) as usize]);
            s.push_str(&" ".repeat(width - full - 1));
        }
        self.accent(&s)
    }

    /// A usage bar that turns yellow, then red, as it fills.
    pub fn usage_bar(&self, fraction: f64, width: usize) -> String {
        if !self.fancy {
            return String::new();
        }
        let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
        let code = if fraction > 0.9 {
            "31"
        } else if fraction > 0.75 {
            "33"
        } else {
            "32"
        };
        format!(
            "{}{}",
            self.paint(code, &"━".repeat(filled)),
            self.dim(&"━".repeat(width - filled))
        )
    }

    /// A section heading.
    pub fn title(&self, s: &str) -> String {
        if self.fancy {
            format!("{} {}", self.accent("▌"), self.bold(s))
        } else {
            s.to_string()
        }
    }

    /// A small label such as a category or flag.
    pub fn badge(&self, s: &str, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m {s} \x1b[0m")
        } else {
            format!("[{s}]")
        }
    }

    pub fn ok_mark(&self) -> String {
        if self.fancy {
            self.green("✔")
        } else {
            "done".into()
        }
    }
    pub fn skip_mark(&self) -> String {
        if self.fancy {
            self.yellow("⊘")
        } else {
            "skip".into()
        }
    }
    pub fn fail_mark(&self) -> String {
        if self.fancy {
            self.red("✖")
        } else {
            "fail".into()
        }
    }

    /// A line on stderr for warnings and notes, so JSON on stdout stays clean.
    pub fn note(&self, s: &str) {
        let s = escape_control(s);
        if self.color && std::io::stderr().is_terminal() {
            let (label, rest) = s.split_once(": ").unwrap_or(("", &s));
            match label {
                "warning" => eprintln!("\x1b[33m⚠ warning\x1b[0m {rest}"),
                "note" => eprintln!("\x1b[2m• {rest}\x1b[0m"),
                _ => eprintln!("\x1b[2m{s}\x1b[0m"),
            }
        } else {
            eprintln!("{s}");
        }
    }
}

/// Width as displayed: ANSI colour sequences take no space.
pub fn visible_width(s: &str) -> usize {
    strip_ansi(s).chars().count()
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

pub struct Table {
    headers: Vec<(String, Align)>,
    rows: Vec<Vec<String>>,
    /// Rows drawn bold (totals).
    emphasis: Vec<bool>,
}

impl Table {
    pub fn new(headers: &[(&str, Align)]) -> Self {
        Self {
            headers: headers.iter().map(|(h, a)| (h.to_string(), *a)).collect(),
            rows: Vec::new(),
            emphasis: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
        self.emphasis.push(false);
    }

    pub fn total(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
        self.emphasis.push(true);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn print(&self, out: &Out) {
        print!("{}", self.render(out));
    }

    /// The table as text, one line per row, ending in a newline.
    pub fn render(&self, out: &Out) -> String {
        use std::fmt::Write;
        if out.csv {
            return self.to_csv();
        }
        let mut buf = String::new();
        let mut widths: Vec<usize> = self
            .headers
            .iter()
            .map(|(h, _)| h.chars().count())
            .collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(visible_width(c));
            }
        }
        let fmt = |cells: &[String]| -> String {
            let last = cells.len().saturating_sub(1);
            cells
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let pad = widths[i].saturating_sub(visible_width(c));
                    match self.headers[i].1 {
                        Align::Right => format!("{}{c}", " ".repeat(pad)),
                        Align::Left if i == last => c.clone(),
                        Align::Left => format!("{c}{}", " ".repeat(pad)),
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        };
        let header: Vec<String> = self.headers.iter().map(|(h, _)| h.clone()).collect();
        let rule_width = widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
        if out.fancy {
            let _ = writeln!(buf, "{}", out.paint("1;2", &fmt(&header)));
            let _ = writeln!(buf, "{}", out.dim(&"─".repeat(rule_width.min(120))));
        } else {
            let _ = writeln!(buf, "{}", out.bold(&fmt(&header)));
        }
        for (r, bold) in self.rows.iter().zip(&self.emphasis) {
            let line = fmt(r);
            if *bold {
                if out.fancy {
                    let _ = writeln!(buf, "{}", out.dim(&"─".repeat(rule_width.min(120))));
                }
                let _ = writeln!(buf, "{}", out.bold(&line));
            } else {
                let _ = writeln!(buf, "{line}");
            }
        }
        buf
    }

    pub fn to_csv(&self) -> String {
        let esc = |s: &str| {
            let s = strip_ansi(s);
            if s.contains([',', '"', '\n']) {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s
            }
        };
        let mut s = String::new();
        let header: Vec<String> = self.headers.iter().map(|(h, _)| esc(h)).collect();
        s.push_str(&header.join(","));
        s.push('\n');
        for r in &self.rows {
            s.push_str(&r.iter().map(|c| esc(c)).collect::<Vec<_>>().join(","));
            s.push('\n');
        }
        s
    }
}

pub fn pct(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_when_needed() {
        let mut t = Table::new(&[("A", Align::Left), ("B", Align::Right)]);
        t.row(vec!["x,y".into(), "say \"hi\"".into()]);
        assert_eq!(t.to_csv(), "A,B\n\"x,y\",\"say \"\"hi\"\"\"\n");
    }

    #[test]
    fn ansi_codes_take_no_width() {
        assert_eq!(visible_width("\x1b[1;31m12.0 GiB\x1b[0m"), 8);
        assert_eq!(strip_ansi("a\x1b[2mb\x1b[0mc"), "abc");
        assert_eq!(visible_width("█▌ ✔"), 4);
    }

    #[test]
    fn plain_output_has_no_escapes() {
        let out = Out {
            json: false,
            csv: false,
            color: false,
            fancy: false,
            verbose: false,
            home: PathBuf::from("/h"),
        };
        assert_eq!(out.size(5 << 30), "5.0 GiB");
        assert_eq!(out.bar(0.5, 10), "");
        assert_eq!(out.badge("Rust build", "30;42"), "[Rust build]");
        assert_eq!(out.path_styled(Path::new("/h/a/b")), "~/a/b");
    }
}
