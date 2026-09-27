//! `fagia` command-line front end. All work happens in fagia-core; this
//! crate parses flags and formats output.

mod cli;
mod cmd;
mod output;
mod progress;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match cmd::run(cli) {
        Ok(outcome) => outcome.code(),
        Err(e) => {
            let msg = fagia_core::paths::escape_control(&format!("{e:#}"));
            let color = std::io::IsTerminal::is_terminal(&std::io::stderr())
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
            if color {
                eprintln!("\x1b[1;31m✖ error:\x1b[0m {msg}");
            } else {
                eprintln!("fagia: error: {msg}");
            }
            ExitCode::from(1)
        }
    }
}
