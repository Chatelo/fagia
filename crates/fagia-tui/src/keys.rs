//! Key bindings: sensible defaults, overridable in `[keys]`.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Up,
    Down,
    Left,
    Right,
    Open,
    Back,
    Select,
    Clean,
    Quit,
    Filter,
    Sort,
    Help,
    NextTab,
    Kill,
    Pause,
    Resume,
    Refresh,
}

impl Action {
    fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "open" => Self::Open,
            "back" => Self::Back,
            "select" => Self::Select,
            "clean" => Self::Clean,
            "quit" => Self::Quit,
            "filter" => Self::Filter,
            "sort" => Self::Sort,
            "help" => Self::Help,
            "next_tab" => Self::NextTab,
            "kill" => Self::Kill,
            "pause" => Self::Pause,
            "resume" => Self::Resume,
            "refresh" => Self::Refresh,
            _ => return None,
        })
    }

    pub const ALL: [(Action, &'static str); 17] = [
        (Self::Up, "move up"),
        (Self::Down, "move down"),
        (Self::Left, "categories pane"),
        (Self::Right, "items pane"),
        (Self::Open, "open folder / items"),
        (Self::Back, "parent folder"),
        (Self::Select, "select item"),
        (Self::Clean, "clean selected (to trash)"),
        (Self::Filter, "filter"),
        (Self::Sort, "change sort"),
        (Self::NextTab, "next tab"),
        (Self::Kill, "quit app (RAM tab)"),
        (Self::Pause, "pause app (RAM tab)"),
        (Self::Resume, "resume app (RAM tab)"),
        (Self::Refresh, "rescan / refresh"),
        (Self::Help, "help"),
        (Self::Quit, "quit fagia"),
    ];
}

fn parse_key(s: &str) -> Option<KeyCode> {
    Some(match s.to_ascii_lowercase().as_str() {
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        _ => {
            let mut c = s.chars();
            let ch = c.next()?;
            if c.next().is_some() {
                return None;
            }
            KeyCode::Char(ch)
        }
    })
}

pub fn key_name(k: KeyCode) -> String {
    match k {
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    }
}

#[derive(Debug, Clone)]
pub struct Keys {
    map: HashMap<KeyCode, Action>,
}

impl Keys {
    /// Defaults plus overrides; returns problems with the overrides.
    pub fn new(overrides: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        use Action::*;
        let mut map: HashMap<KeyCode, Action> = [
            (KeyCode::Up, Up),
            (KeyCode::Char('k'), Up),
            (KeyCode::Down, Down),
            (KeyCode::Char('j'), Down),
            (KeyCode::Left, Left),
            (KeyCode::Char('h'), Left),
            (KeyCode::Right, Right),
            (KeyCode::Char('l'), Right),
            (KeyCode::Enter, Open),
            (KeyCode::Backspace, Back),
            (KeyCode::Char(' '), Select),
            (KeyCode::Char('c'), Clean),
            (KeyCode::Char('q'), Quit),
            (KeyCode::Char('/'), Filter),
            (KeyCode::Char('s'), Sort),
            (KeyCode::Char('?'), Help),
            (KeyCode::Tab, NextTab),
            (KeyCode::Char('x'), Kill),
            (KeyCode::Char('p'), Pause),
            (KeyCode::Char('P'), Resume),
            (KeyCode::Char('r'), Refresh),
        ]
        .into_iter()
        .collect();
        let mut problems = Vec::new();
        for (name, key) in overrides {
            match (Action::from_name(name), parse_key(key)) {
                (Some(a), Some(k)) => {
                    map.retain(|_, v| *v != a);
                    map.insert(k, a);
                }
                (None, _) => problems.push(format!("[keys] unknown action {name:?}")),
                (_, None) => problems.push(format!("[keys] {name}: cannot parse key {key:?}")),
            }
        }
        (Self { map }, problems)
    }

    pub fn action(&self, ev: &KeyEvent) -> Option<Action> {
        if ev.modifiers.contains(KeyModifiers::CONTROL) && ev.code == KeyCode::Char('c') {
            return Some(Action::Quit);
        }
        self.map.get(&ev.code).copied()
    }

    /// First key bound to `a`, for help text.
    pub fn key_for(&self, a: Action) -> String {
        let mut keys: Vec<String> = self
            .map
            .iter()
            .filter(|(_, v)| **v == a)
            .map(|(k, _)| key_name(*k))
            .collect();
        keys.sort_by_key(|k| (k.chars().count() != 1, k.clone()));
        keys.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    #[test]
    fn overrides_replace_defaults() {
        let mut o = BTreeMap::new();
        o.insert("clean".to_string(), "d".to_string());
        o.insert("nope".to_string(), "z".to_string());
        let (k, problems) = Keys::new(&o);
        assert_eq!(problems.len(), 1);
        assert_eq!(
            k.action(&KeyEvent::from(KeyCode::Char('d'))),
            Some(Action::Clean)
        );
        assert_eq!(k.action(&KeyEvent::from(KeyCode::Char('c'))), None);
        assert_eq!(k.key_for(Action::Up), "k/↑");
    }
}
