//! `fagia ui`: full-screen interface over fagia-core. Scanning runs in a
//! background thread; every clean or kill goes through the same core plan
//! and gate as the CLI, with a confirmation screen.

pub mod app;
pub mod keys;
pub mod ui;

use anyhow::Result;
use fagia_core::session::{ScanFlags, Session};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Redraw rate while something animates (spinner, scan counters).
const BUSY_FRAME: Duration = Duration::from_millis(50);
/// Wake-up rate when idle, to pick up worker results (memory samples).
const IDLE_FRAME: Duration = Duration::from_millis(250);

pub fn run(session: Session, root: PathBuf, flags: ScanFlags, as_root: bool) -> Result<()> {
    let mut app = app::App::new(Arc::new(session), root, flags, as_root);
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        while !app.quit {
            app.tick();
            terminal.draw(|f| ui::draw(f, &app))?;
            let wait = if app.busy() { BUSY_FRAME } else { IDLE_FRAME };
            if event::poll(wait)? {
                // Handle every pending event before the next frame, so held
                // keys and fast typing never queue up behind redraws.
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => app.handle_key(k),
                        _ => {}
                    }
                    if app.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}
