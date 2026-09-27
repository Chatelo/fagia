//! Disk engine: walking, sizing, classification and the analyses built on
//! a scan (media, duplicates, staleness, history, hidden space).

pub mod dupe_dirs;
pub mod dupes;
pub mod media;
pub mod scope;
pub mod similar;
pub mod stale;
pub mod unseen;
pub mod walk;

pub use walk::{Classifier, FileRec, Progress, Scan, ScanOptions, Tree, scan};
