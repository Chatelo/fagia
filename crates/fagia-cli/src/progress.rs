//! Scan progress on stderr, only when stderr is a terminal.

use fagia_core::disk::Progress;
use fagia_core::size::format_size;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct Spinner {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub fn start(progress: Arc<Progress>, label: &str) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        if !std::io::stderr().is_terminal() {
            return Self { done, handle: None };
        }
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg} {elapsed:.dim}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"]),
        );
        bar.enable_steady_tick(Duration::from_millis(80));
        let label = label.to_string();
        let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        let flag = done.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                let (files, dirs, bytes) = (
                    progress.files.load(Ordering::Relaxed),
                    progress.dirs.load(Ordering::Relaxed),
                    format_size(progress.bytes.load(Ordering::Relaxed)),
                );
                bar.set_message(if color {
                    format!(
                        "{label}  \x1b[36;1m{files}\x1b[0m\x1b[2m files ·\x1b[0m \x1b[36;1m{dirs}\x1b[0m\x1b[2m folders ·\x1b[0m \x1b[1m{bytes}\x1b[0m"
                    )
                } else {
                    format!("{label}  {files} files · {dirs} folders · {bytes}")
                });
                std::thread::sleep(Duration::from_millis(120));
            }
            bar.finish_and_clear();
        });
        Self {
            done,
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Spinner {
    /// A spinner counting items for work after the scan, such as comparing
    /// files for similarity.
    pub fn counter(count: Arc<std::sync::atomic::AtomicU64>, label: &str) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        if !std::io::stderr().is_terminal() {
            return Self { done, handle: None };
        }
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg} {elapsed:.dim}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"]),
        );
        bar.enable_steady_tick(Duration::from_millis(80));
        let label = label.to_string();
        let flag = done.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                bar.set_message(format!("{label}: {} files", count.load(Ordering::Relaxed)));
                std::thread::sleep(Duration::from_millis(120));
            }
            bar.finish_and_clear();
        });
        Self {
            done,
            handle: Some(handle),
        }
    }
}
