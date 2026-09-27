//! fagia-core: disk and RAM analysis, rules, and the action gate. Front
//! ends call into this crate and never touch the OS directly.

pub mod actions;
pub mod config;
pub mod disk;
pub mod error;
pub mod model;
pub mod paths;
pub mod platform;
pub mod providers;
pub mod ram;
pub mod report;
pub mod rules;
pub mod session;
pub mod size;
pub mod store;

pub use error::{Error, Result};
