//! Drives the TUI state machine and renders to an in-memory terminal.

use fagia_core::platform::Dirs;
use fagia_core::platform::linux::LinuxPlatform;
use fagia_core::session::{ScanFlags, Session};
use fagia_tui::app::{App, DiskState, Modal, Tab};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap().join("home");
    let w = |rel: &str, len: usize| {
        let p = home.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; len]).unwrap();
    };
    w("code/api/Cargo.toml", 10);
    w("code/api/target/debug/app", 2 << 20);
    w("code/web/package.json", 10);
    w("code/web/node_modules/x/index.js", 1 << 20);
    w("Videos/trip.mp4", 3 << 20);
    Fx { _tmp: tmp, home }
}

fn app(fx: &Fx) -> App {
    let h = &fx.home;
    let dirs = Dirs {
        cache: h.join(".cache"),
        data: h.join(".local/share"),
        config: h.join(".config"),
        state: h.join(".local/state"),
        cargo_home: h.join(".cargo"),
        home: h.clone(),
    };
    let plat = Arc::new(LinuxPlatform::with_roots("/proc", "/sys", "/dev", dirs));
    let session = Session::with_platform(plat, Some(&h.join(".config/fagia/config.toml"))).unwrap();
    App::new(Arc::new(session), h.clone(), ScanFlags::default(), false)
}

fn wait_ready(app: &mut App) {
    let start = Instant::now();
    while !matches!(app.disk, DiskState::Ready(_)) {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "scan never finished"
        );
        app.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn screen(app: &App) -> String {
    let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
    t.draw(|f| fagia_tui::ui::draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut s = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.push('\n');
    }
    s
}

fn key(app: &mut App, c: KeyCode) {
    app.handle_key(KeyEvent::from(c));
}

/// Ticks until `done` holds; workers report back asynchronously.
fn until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
    let start = Instant::now();
    while !done(app) {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        app.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn renders_while_scanning_then_shows_categories() {
    let fx = fixture();
    let started = Instant::now();
    let mut a = app(&fx);
    // Construction does not wait for the scan.
    assert!(started.elapsed() < Duration::from_secs(2));
    let first = screen(&a);
    assert!(first.contains("Disk") && first.contains("RAM") && first.contains("History"));
    wait_ready(&mut a);
    let s = screen(&a);
    for want in [
        "Rust build",
        "Node deps",
        "Media",
        "Big folders",
        "3.0 MiB selected",
        "Cargo.toml in parent",
    ] {
        assert!(s.contains(want), "missing {want:?} in\n{s}");
    }
}

#[test]
fn clean_goes_through_confirmation_and_gate() {
    let fx = fixture();
    let mut a = app(&fx);
    wait_ready(&mut a);
    // Both regenerable, low-risk suspects are preselected, as in the CLI.
    assert!(a.selected_bytes() >= 3 << 20);
    key(&mut a, KeyCode::Char('c'));
    // Planning runs on a worker; the UI shows progress meanwhile.
    assert!(matches!(a.modal, Some(Modal::Busy { .. })));
    assert!(screen(&a).contains("Checking selected items"));
    until(&mut a, "the plan", |a| {
        matches!(a.modal, Some(Modal::ConfirmClean(_)))
    });
    let Some(Modal::ConfirmClean(plan)) = &a.modal else {
        panic!("no confirmation screen");
    };
    assert_eq!(plan.items.iter().filter(|i| i.selected).count(), 2);
    assert!(screen(&a).contains("Confirm clean"));
    // Nothing happens before confirming.
    assert!(fx.home.join("code/api/target").exists());
    key(&mut a, KeyCode::Char('y'));
    until(&mut a, "the clean", |a| {
        matches!(a.modal, Some(Modal::Info(..)))
    });
    assert!(!fx.home.join("code/api/target").exists());
    assert!(fx.home.join(".local/share/Trash/files/target").exists());
    let log = fs::read_to_string(fx.home.join(".local/state/fagia/actions.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2);
}

#[test]
fn cancel_leaves_everything() {
    let fx = fixture();
    let mut a = app(&fx);
    wait_ready(&mut a);
    key(&mut a, KeyCode::Char('c'));
    until(&mut a, "the plan", |a| {
        matches!(a.modal, Some(Modal::ConfirmClean(_)))
    });
    key(&mut a, KeyCode::Char('n'));
    assert!(a.modal.is_none());
    assert!(fx.home.join("code/api/target").exists());
}

#[test]
fn deselect_filter_and_drill_down() {
    let fx = fixture();
    let mut a = app(&fx);
    wait_ready(&mut a);
    // Items pane of the first category; space toggles the selection off.
    key(&mut a, KeyCode::Right);
    let before = a.selected_bytes();
    key(&mut a, KeyCode::Char(' '));
    assert!(a.selected_bytes() < before);

    // Move to "Big folders" (the last category).
    key(&mut a, KeyCode::Left);
    for _ in 0..5 {
        key(&mut a, KeyCode::Down);
    }
    // Filter narrows items.
    key(&mut a, KeyCode::Char('/'));
    for c in "code".chars() {
        key(&mut a, KeyCode::Char(c));
    }
    key(&mut a, KeyCode::Enter);
    let items = a.items();
    assert!(!items.is_empty());
    assert!(
        items
            .iter()
            .all(|i| i.path.to_string_lossy().contains("code"))
    );
    // Clear the filter, then drill into "code" and back up.
    key(&mut a, KeyCode::Char('/'));
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Right);
    let code_idx = a
        .items()
        .iter()
        .position(|i| i.path.ends_with("code"))
        .unwrap();
    for _ in 0..code_idx {
        key(&mut a, KeyCode::Down);
    }
    key(&mut a, KeyCode::Enter);
    let inside: Vec<PathBuf> = a.items().iter().map(|i| i.path.clone()).collect();
    assert!(inside.iter().any(|p| p.ends_with("code/api")), "{inside:?}");
    key(&mut a, KeyCode::Backspace);
    assert!(
        a.items()
            .iter()
            .any(|i| i.path == Path::new(&fx.home).join("code"))
    );
}

#[test]
fn ram_and_history_tabs_render() {
    let fx = fixture();
    let mut a = app(&fx);
    wait_ready(&mut a);
    key(&mut a, KeyCode::Char('2'));
    assert_eq!(a.tab, Tab::Ram);
    assert!(screen(&a).contains("Reading memory"));
    until(&mut a, "a memory sample", |a| {
        a.mem.as_ref().is_some_and(|m| !m.groups.is_empty())
    });
    let s = screen(&a);
    assert!(s.contains("by fair share"), "{s}");
    assert!(s.contains("available of"));
    key(&mut a, KeyCode::Char('3'));
    assert!(screen(&a).contains("saved scan"));
    key(&mut a, KeyCode::Char('?'));
    assert!(screen(&a).contains("Keys"));
    key(&mut a, KeyCode::Char('z'));
    assert!(a.modal.is_none());
    key(&mut a, KeyCode::Char('q'));
    assert!(a.quit);
}

#[test]
fn empty_trash_needs_typed_word() {
    let fx = fixture();
    let files = fx.home.join(".local/share/Trash/files");
    fs::create_dir_all(&files).unwrap();
    fs::create_dir_all(fx.home.join(".local/share/Trash/info")).unwrap();
    fs::write(files.join("old.bin"), vec![0u8; 1 << 20]).unwrap();
    fs::write(
        fx.home.join(".local/share/Trash/info/old.bin.trashinfo"),
        "[Trash Info]\nPath=/somewhere/old.bin\nDeletionDate=2026-01-01T00:00:00\n",
    )
    .unwrap();
    let mut a = app(&fx);
    wait_ready(&mut a);
    let idx = match &a.disk {
        DiskState::Ready(v) => v
            .cats
            .iter()
            .position(|c| c.label == "Trash")
            .expect("Trash category"),
        _ => unreachable!(),
    };
    for _ in 0..idx {
        key(&mut a, KeyCode::Down);
    }
    assert!(screen(&a).contains("empty trash"));
    key(&mut a, KeyCode::Char('c'));
    until(&mut a, "the trash listing", |a| {
        matches!(a.modal, Some(Modal::ConfirmEmpty { .. }))
    });
    assert!(screen(&a).contains("cannot be undone"));
    // "y" is just a letter here; a wrong word keeps the dialog open.
    for c in "yes".chars() {
        key(&mut a, KeyCode::Char(c));
    }
    key(&mut a, KeyCode::Enter);
    assert!(matches!(a.modal, Some(Modal::ConfirmEmpty { .. })));
    assert!(files.join("old.bin").exists());
    for _ in 0..3 {
        key(&mut a, KeyCode::Backspace);
    }
    for c in "empty".chars() {
        key(&mut a, KeyCode::Char(c));
    }
    key(&mut a, KeyCode::Enter);
    until(&mut a, "emptying", |a| {
        matches!(a.modal, Some(Modal::Info(..)))
    });
    assert!(!files.join("old.bin").exists());
}
