//! Drawing. A pure function of the app state; it only reads data the
//! workers have already computed, so a frame costs a few milliseconds.

use crate::app::{App, CatKind, DiskState, Focus, Modal, Tab};
use crate::keys::Action;
use fagia_core::actions::Mode;
use fagia_core::actions::kill::SignalKind;
use fagia_core::paths::display_path;
use fagia_core::size::{format_age, format_delta, format_size};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Gauge, List, ListItem, ListState, Padding, Paragraph, Row,
    Scrollbar, ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
};
use std::path::Path;
use std::sync::atomic::Ordering;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const SELECT_BG: Color = Color::Indexed(237);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner(app: &App) -> &'static str {
    SPINNER[(app.frame as usize / 2) % SPINNER.len()]
}

/// Sizes coloured by magnitude, so big items stand out at a glance.
pub fn size_style(bytes: u64) -> Style {
    match bytes {
        b if b >= 10 << 30 => Style::default().fg(Color::Magenta).bold(),
        b if b >= 1 << 30 => Style::default().fg(Color::Red).bold(),
        b if b >= 100 << 20 => Style::default().fg(Color::Yellow),
        b if b >= 1 << 20 => Style::default().fg(Color::Green),
        _ => Style::default().fg(MUTED),
    }
}

fn size_span(bytes: u64) -> Span<'static> {
    Span::styled(format_size(bytes), size_style(bytes))
}

/// A horizontal bar with eighth-block precision.
pub fn bar(fraction: f64, width: usize) -> String {
    const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let f = fraction.clamp(0.0, 1.0) * width as f64;
    let full = f as usize;
    let mut s = "█".repeat(full);
    if full < width {
        s.push(EIGHTHS[((f - full as f64) * 8.0) as usize]);
        s.push_str(&" ".repeat(width - full - 1));
    }
    s
}

fn stale_style(days: u64) -> Style {
    match days {
        d if d >= 365 => Style::default().fg(Color::Red),
        d if d >= 90 => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(MUTED),
    }
}

/// A path with its folder part dimmed and its name bright.
fn path_spans(app: &App, p: &Path, dir_suffix: bool) -> Vec<Span<'static>> {
    let s = display_path(p, Some(&app.session.platform.dirs().home));
    let (head, tail) = match s.rfind('/') {
        Some(i) if i + 1 < s.len() => (s[..=i].to_string(), s[i + 1..].to_string()),
        _ => (String::new(), s),
    };
    let mut v = vec![
        Span::styled(head, Style::default().fg(MUTED)),
        Span::raw(tail).bold(),
    ];
    if dir_suffix {
        v.push(Span::styled("/", Style::default().fg(MUTED)));
    }
    v
}

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
        .title(title)
}

pub fn draw(f: &mut Frame, app: &App) {
    let [top, body, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .areas(f.area());
    draw_header(f, app, top);
    match app.tab {
        Tab::Disk => draw_disk(f, app, body, bottom),
        Tab::Ram => draw_ram(f, app, body, bottom),
        Tab::History => draw_history(f, app, body, bottom),
    }
    if let Some(m) = &app.modal {
        draw_modal(f, app, m);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled(
            " fagia ",
            Style::default().fg(Color::Black).bg(ACCENT).bold(),
        ),
        Span::raw(" "),
    ];
    for (tab, key, label) in [
        (Tab::Disk, "1", "Disk"),
        (Tab::Ram, "2", "RAM"),
        (Tab::History, "3", "History"),
    ] {
        let active = app.tab == tab;
        spans.push(Span::styled(format!(" {key} "), Style::default().fg(MUTED)));
        spans.push(if active {
            Span::styled(
                format!("{label} "),
                Style::default().fg(ACCENT).bold().underlined(),
            )
        } else {
            Span::raw(format!("{label} "))
        });
    }
    let [left, right] =
        Layout::horizontal([Constraint::Min(30), Constraint::Length(44)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(spans)), left);
    if let (Some(free), Some(total)) = (app.disk_free, app.disk_total) {
        let used = 1.0 - free as f64 / total.max(1) as f64;
        let color = if used > 0.9 {
            Color::Red
        } else if used > 0.75 {
            Color::Yellow
        } else {
            Color::Green
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("disk ", Style::default().fg(MUTED)),
                Span::styled(
                    bar(used, 12),
                    Style::default().fg(color).bg(Color::Indexed(236)),
                ),
                Span::styled(
                    format!(" {:.0}% ", used * 100.0),
                    Style::default().fg(color).bold(),
                ),
                Span::raw(format!("{} free ", format_size(free))),
            ]))
            .alignment(Alignment::Right),
            right,
        );
    }
}

/// Key hints: key in accent, label muted; status message on the left.
fn footer(f: &mut Frame, app: &App, area: Rect, status: Line<'static>, pairs: &[(Action, &str)]) {
    let mut spans = Vec::new();
    for (a, label) in pairs {
        spans.push(Span::styled(
            format!(" {} ", app.keys.key_for(*a)),
            Style::default().fg(Color::Black).bg(Color::Indexed(244)),
        ));
        spans.push(Span::styled(
            format!(" {label}  "),
            Style::default().fg(MUTED),
        ));
    }
    let [s, k] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    f.render_widget(Paragraph::new(status), s);
    f.render_widget(Paragraph::new(Line::from(spans)), k);
}

fn status_line(app: &App, fallback: Line<'static>) -> Line<'static> {
    if app.status.is_empty() {
        fallback
    } else {
        Line::from(vec![
            Span::styled("● ", Style::default().fg(ACCENT)),
            Span::raw(app.status.clone()),
        ])
    }
}

fn draw_disk(f: &mut Frame, app: &App, body: Rect, bottom: Rect) {
    let v = match &app.disk {
        DiskState::Scanning(p, started) => {
            let secs = started.elapsed().as_secs_f64().max(0.1);
            let files = p.files.load(Ordering::Relaxed);
            let bytes = p.bytes.load(Ordering::Relaxed);
            let lines = vec![
                Line::from(vec![
                    Span::styled(
                        format!("{} ", spinner(app)),
                        Style::default().fg(ACCENT).bold(),
                    ),
                    Span::raw("Scanning "),
                    Span::raw(display_path(
                        &app.root,
                        Some(&app.session.platform.dirs().home),
                    ))
                    .bold(),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(format!("{files:>12}"), Style::default().fg(ACCENT).bold()),
                    Span::styled(" files   ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("{:>8}", p.dirs.load(Ordering::Relaxed)),
                        Style::default().fg(ACCENT).bold(),
                    ),
                    Span::styled(" folders   ", Style::default().fg(MUTED)),
                    size_span(bytes),
                ]),
                Line::from(Span::styled(
                    format!("{:>12.0} files/s   {:.1} s", files as f64 / secs, secs),
                    Style::default().fg(MUTED),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "The interface stays usable: press 2 for RAM, q to quit.",
                    Style::default().fg(MUTED).italic(),
                )),
            ];
            let area = centered(body, 70, 8);
            f.render_widget(
                Paragraph::new(lines).block(panel(" Disk ", true).padding(Padding::horizontal(2))),
                area,
            );
            footer(
                f,
                app,
                bottom,
                status_line(app, Line::from("")),
                &[(Action::NextTab, "next tab"), (Action::Quit, "quit")],
            );
            return;
        }
        DiskState::Failed(e) => {
            f.render_widget(
                Paragraph::new(format!("Scan failed: {e}")).block(panel(" Disk ", true)),
                body,
            );
            footer(
                f,
                app,
                bottom,
                status_line(app, Line::from("")),
                &[(Action::Refresh, "rescan"), (Action::Quit, "quit")],
            );
            return;
        }
        DiskState::Ready(v) => v,
    };
    let [left, right] =
        Layout::horizontal([Constraint::Length(38), Constraint::Min(40)]).areas(body);

    // Categories: name, size and a share bar against the largest.
    let max_cat = v
        .cats
        .iter()
        .filter(|c| c.kind != CatKind::BigFolders)
        .map(|c| c.size)
        .max()
        .unwrap_or(1)
        .max(1);
    let cats: Vec<ListItem> = v
        .cats
        .iter()
        .map(|c| {
            let (icon, color) = match c.kind {
                CatKind::Findings(_) if c.regenerable => ("◆", Color::Green),
                CatKind::Findings(_) => ("◇", Color::Yellow),
                CatKind::Media => ("▶", Color::Magenta),
                CatKind::BigFolders => ("▤", ACCENT),
            };
            let share = if c.kind == CatKind::BigFolders {
                1.0
            } else {
                c.size as f64 / max_cat as f64
            };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("{icon} "), Style::default().fg(color)),
                    Span::raw(format!("{:<16.16}", c.label)),
                    Span::styled(format!("{:>10}", format_size(c.size)), size_style(c.size)),
                ]),
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        bar(share, 14),
                        Style::default().fg(color).add_modifier(Modifier::DIM),
                    ),
                    Span::styled(
                        format!(
                            " {:>5} item{}",
                            c.count,
                            if c.count == 1 { "" } else { "s" }
                        ),
                        Style::default().fg(MUTED),
                    ),
                ]),
            ])
        })
        .collect();
    let mut cs = ListState::default().with_selected(Some(v.cat));
    f.render_stateful_widget(
        List::new(cats)
            .block(panel(" Categories ", v.focus == Focus::Categories))
            .highlight_style(Style::default().bg(SELECT_BG))
            .highlight_symbol("▌"),
        left,
        &mut cs,
    );

    // Items.
    let items = app.items();
    let cat = v.cats.get(v.cat);
    let browsing = cat.is_some_and(|c| c.kind == CatKind::BigFolders);
    let max_item = items.iter().map(|i| i.size).max().unwrap_or(1).max(1);
    let rows: Vec<Row> = items
        .iter()
        .map(|i| {
            let mark = match i.finding {
                Some(fi) if v.selected.contains(&fi) => {
                    Span::styled("✔", Style::default().fg(Color::Green).bold())
                }
                Some(_) => Span::styled("○", Style::default().fg(MUTED)),
                None => Span::raw(" "),
            };
            Row::new(vec![
                Cell::from(mark),
                Cell::from(Line::from(path_spans(app, &i.path, browsing && i.is_dir))),
                Cell::from(Line::from(size_span(i.size)).alignment(Alignment::Right)),
                Cell::from(Span::styled(
                    bar(i.size as f64 / max_item as f64, 10),
                    size_style(i.size).add_modifier(Modifier::DIM),
                )),
                Cell::from(
                    Line::from(
                        i.stale_days
                            .map(|d| Span::styled(format!("{d}d"), stale_style(d)))
                            .unwrap_or_default(),
                    )
                    .alignment(Alignment::Right),
                ),
            ])
        })
        .collect();
    let mut title = vec![Span::raw(" ")];
    if browsing {
        title.push(
            Span::raw(display_path(
                &v.scan.tree.path(v.browse),
                Some(&app.session.platform.dirs().home),
            ))
            .bold(),
        );
    } else {
        title.push(Span::raw(cat.map(|c| c.label.clone()).unwrap_or_default()).bold());
    }
    title.push(Span::styled(
        format!("  {} · by {} ", items.len(), app.sort.label()),
        Style::default().fg(MUTED),
    ));
    if !app.filter.is_empty() || app.filtering {
        title.push(Span::styled(
            format!(" /{}{} ", app.filter, if app.filtering { "▏" } else { "" }),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
    }
    let mut ts =
        TableState::default().with_selected(if items.is_empty() { None } else { Some(v.item) });
    f.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Min(20),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(5),
            ],
        )
        .block(panel(Line::from(title), v.focus == Focus::Items))
        .row_highlight_style(Style::default().bg(SELECT_BG))
        .highlight_symbol("▌"),
        right,
        &mut ts,
    );
    if items.len() > right.height.saturating_sub(2) as usize {
        let mut sb = ScrollbarState::new(items.len()).position(v.item);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(ACCENT))
                .track_style(Style::default().fg(MUTED)),
            right.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb,
        );
    }

    let evidence = items
        .get(v.item)
        .and_then(|i| i.finding)
        .map(|fi| {
            let f = &v.findings[fi];
            let mut spans = vec![
                Span::styled("✓ ", Style::default().fg(Color::Green)),
                Span::raw(f.evidence.clone()),
            ];
            if let Some(r) = &f.regenerate {
                spans.push(Span::styled("  ↻ ", Style::default().fg(MUTED)));
                spans.push(Span::styled(
                    r.to_string(),
                    Style::default().fg(MUTED).italic(),
                ));
            }
            if let Some(n) = &f.note {
                spans.push(Span::styled(format!("  · {n}"), Style::default().fg(MUTED)));
            }
            Line::from(spans)
        })
        .unwrap_or_else(|| {
            Line::from(Span::styled(
                "Space selects suspects; only they can be cleaned.",
                Style::default().fg(MUTED),
            ))
        });
    let mut status = status_line(app, evidence);
    status.spans.insert(
        0,
        Span::styled(
            format!(" {} selected ", format_size(app.selected_bytes())),
            Style::default().fg(Color::Black).bg(Color::Green).bold(),
        ),
    );
    status.spans.insert(1, Span::raw(" "));
    footer(
        f,
        app,
        bottom,
        status,
        &[
            (Action::Select, "select"),
            (
                Action::Clean,
                if cat.is_some_and(|c| c.label == "Trash") {
                    "empty trash"
                } else {
                    "clean"
                },
            ),
            (Action::Open, "open"),
            (Action::Filter, "filter"),
            (Action::Sort, "sort"),
            (Action::Help, "help"),
            (Action::Quit, "quit"),
        ],
    );
}

fn sparkline(values: &[u64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.len() < 2 {
        return String::new();
    }
    let tail = &values[values.len().saturating_sub(16)..];
    let (lo, hi) = (
        *tail.iter().min().unwrap_or(&0),
        *tail.iter().max().unwrap_or(&0),
    );
    tail.iter()
        .map(|v| {
            BARS[if hi == lo {
                0
            } else {
                ((v - lo) * 7 / (hi - lo)) as usize
            }]
        })
        .collect()
}

fn draw_ram(f: &mut Frame, app: &App, body: Rect, bottom: Rect) {
    let Some(m) = &app.mem else {
        let area = centered(body, 40, 3);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{} ", spinner(app)), Style::default().fg(ACCENT)),
                Span::raw("Reading memory…"),
            ]))
            .block(panel(" RAM ", true)),
            area,
        );
        footer(
            f,
            app,
            bottom,
            status_line(app, Line::from("")),
            &[(Action::Quit, "quit")],
        );
        return;
    };
    let [bar_area, info, table] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(body);
    let s = &m.system;
    let used = 1.0 - s.available as f64 / s.total.max(1) as f64;
    let color = if used > 0.9 {
        Color::Red
    } else if used > 0.75 {
        Color::Yellow
    } else {
        Color::Green
    };
    f.render_widget(
        Gauge::default()
            .block(panel(
                Line::from(vec![
                    Span::raw(" Memory "),
                    Span::styled("(in use = total − available) ", Style::default().fg(MUTED)),
                ]),
                false,
            ))
            .gauge_style(Style::default().fg(color).bg(Color::Indexed(236)))
            .use_unicode(true)
            .ratio(used.clamp(0.0, 1.0))
            .label(Span::styled(
                format!(
                    "{} available of {}",
                    format_size(s.available),
                    format_size(s.total)
                ),
                Style::default().fg(Color::White).bold(),
            )),
        bar_area,
    );
    let mut parts = vec![
        Span::styled(" programs ", Style::default().fg(MUTED)),
        size_span(s.used_by_programs),
        Span::styled("  file cache ", Style::default().fg(MUTED)),
        Span::raw(format_size(s.file_cache)),
        Span::styled(" (released on demand)", Style::default().fg(MUTED)),
    ];
    if s.swap_total > 0 {
        let heavy = s.swap_in_rate + s.swap_out_rate > 100.0;
        parts.push(Span::styled("  swap ", Style::default().fg(MUTED)));
        parts.push(Span::raw(format!(
            "{} / {}",
            format_size(s.swap_used),
            format_size(s.swap_total)
        )));
        parts.push(Span::styled(
            format!("  {:.0}↓ {:.0}↑ pages/s", s.swap_in_rate, s.swap_out_rate),
            if heavy {
                Style::default().fg(Color::Red).bold()
            } else {
                Style::default().fg(MUTED)
            },
        ));
    }
    f.render_widget(Paragraph::new(Line::from(parts)), info);

    let max = m.groups.iter().map(|g| g.fair).max().unwrap_or(1).max(1);
    let rows: Vec<Row> = m
        .groups
        .iter()
        .map(|g| {
            let mut name = vec![Span::raw(g.display.clone()).bold()];
            if let Some(c) = &g.category {
                name.push(Span::styled(format!("  {c}"), Style::default().fg(MUTED)));
            }
            let mut tags: Vec<Span> = Vec::new();
            for flag in &g.flags {
                let color = if flag == "leaking" {
                    Color::Red
                } else {
                    Color::Yellow
                };
                tags.push(Span::styled(
                    format!(" {flag} "),
                    Style::default().fg(Color::Black).bg(color),
                ));
                tags.push(Span::raw(" "));
            }
            if g.system {
                tags.push(Span::styled("system ", Style::default().fg(MUTED)));
            }
            if !g.fully_measured {
                tags.push(Span::styled(
                    "resident",
                    Style::default().fg(MUTED).italic(),
                ));
            }
            Row::new(vec![
                Cell::from(Line::from(size_span(g.fair)).alignment(Alignment::Right)),
                Cell::from(Span::styled(
                    bar(g.fair as f64 / max as f64, 8),
                    size_style(g.fair).add_modifier(Modifier::DIM),
                )),
                Cell::from(
                    Line::from(Span::styled(
                        format_size(g.unique),
                        Style::default().fg(MUTED),
                    ))
                    .alignment(Alignment::Right),
                ),
                Cell::from(
                    Line::from(Span::styled(
                        g.processes.to_string(),
                        Style::default().fg(MUTED),
                    ))
                    .alignment(Alignment::Right),
                ),
                Cell::from(
                    Line::from(Span::styled(
                        format_age(g.age_secs),
                        Style::default().fg(MUTED),
                    ))
                    .alignment(Alignment::Right),
                ),
                Cell::from(Line::from(name)),
                Cell::from(Line::from(tags)),
                Cell::from(Span::styled(
                    sparkline(&g.trend),
                    Style::default().fg(ACCENT),
                )),
            ])
        })
        .collect();
    let mut ts = TableState::default().with_selected(Some(app.ram_idx));
    let header = Row::new(vec![
        Cell::from(Line::from("FAIR").alignment(Alignment::Right)),
        Cell::from(""),
        Cell::from(Line::from("UNIQUE").alignment(Alignment::Right)),
        Cell::from(Line::from("PROCS").alignment(Alignment::Right)),
        Cell::from(Line::from("AGE").alignment(Alignment::Right)),
        Cell::from("APP"),
        Cell::from("FLAGS"),
        Cell::from("TREND"),
    ])
    .style(Style::default().fg(MUTED).bold());
    f.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Min(24),
                Constraint::Length(20),
                Constraint::Length(16),
            ],
        )
        .header(header)
        .block(panel(
            Line::from(vec![
                Span::raw(" Apps "),
                Span::styled(
                    format!("by fair share · {} processes ", m.processes),
                    Style::default().fg(MUTED),
                ),
            ]),
            true,
        ))
        .row_highlight_style(Style::default().bg(SELECT_BG))
        .highlight_symbol("▌"),
        table,
        &mut ts,
    );
    let detail = m
        .groups
        .get(app.ram_idx)
        .map(|g| {
            let mut spans = Vec::new();
            if let Some(fg) = &g.forgotten {
                spans.push(Span::styled("⚠ ", Style::default().fg(Color::Yellow)));
                spans.push(Span::raw(fg.clone()));
            } else if let Some(r) = &g.reason {
                spans.push(Span::styled(r.clone(), Style::default().fg(MUTED)));
            }
            if g.unsaved_work {
                spans.push(Span::styled(
                    "  · may hold unsaved work",
                    Style::default().fg(Color::Yellow),
                ));
            }
            Line::from(spans)
        })
        .unwrap_or_default();
    footer(
        f,
        app,
        bottom,
        status_line(app, detail),
        &[
            (Action::Kill, "quit app"),
            (Action::Pause, "pause"),
            (Action::Resume, "resume"),
            (Action::Help, "help"),
            (Action::Quit, "quit"),
        ],
    );
}

fn draw_history(f: &mut Frame, app: &App, body: Rect, bottom: Rect) {
    let home = app.session.platform.dirs().home.clone();
    if app.history.len() < 2 {
        let area = centered(body, 76, 5);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "{} saved scan(s) of {}.",
                    app.history.len(),
                    display_path(&app.root, Some(&home))
                )),
                Line::from(Span::styled(
                    format!(
                        "Growth appears here after the next scan ({} rescans).",
                        app.keys.key_for(Action::Refresh)
                    ),
                    Style::default().fg(MUTED),
                )),
            ])
            .alignment(Alignment::Center)
            .block(panel(" History ", true).padding(Padding::vertical(1))),
            area,
        );
    } else {
        // Oldest to newest, at most six columns.
        let snaps: Vec<_> = app.history.iter().take(6).rev().collect();
        let mut cats: Vec<&String> = snaps.iter().flat_map(|s| s.categories.keys()).collect();
        cats.sort();
        cats.dedup();
        let now = fagia_core::model::now_epoch();
        let mut header = vec![Cell::from("CATEGORY")];
        header.extend(snaps.iter().map(|s| {
            Cell::from(
                Line::from(format!(
                    "{} ago",
                    format_age((now - s.taken_at).max(0) as u64)
                ))
                .alignment(Alignment::Right),
            )
        }));
        header.push(Cell::from(Line::from("CHANGE").alignment(Alignment::Right)));
        let delta_cell = |d: i64| {
            let style = match d {
                d if d > 0 => Style::default().fg(Color::Yellow).bold(),
                d if d < 0 => Style::default().fg(Color::Green).bold(),
                _ => Style::default().fg(MUTED),
            };
            Cell::from(
                Line::from(Span::styled(
                    if d == 0 {
                        "–".into()
                    } else {
                        format_delta(d)
                    },
                    style,
                ))
                .alignment(Alignment::Right),
            )
        };
        let mut rows: Vec<Row> = cats
            .iter()
            .map(|c| {
                let vals: Vec<u64> = snaps
                    .iter()
                    .map(|s| s.categories.get(*c).copied().unwrap_or(0))
                    .collect();
                let delta = *vals.last().unwrap_or(&0) as i64 - *vals.first().unwrap_or(&0) as i64;
                let mut cells = vec![Cell::from((*c).clone())];
                cells
                    .extend(vals.iter().map(|v| {
                        Cell::from(Line::from(size_span(*v)).alignment(Alignment::Right))
                    }));
                cells.push(delta_cell(delta));
                Row::new(cells)
            })
            .collect();
        let totals: Vec<u64> = snaps.iter().map(|s| s.total_real).collect();
        let mut cells = vec![Cell::from(Span::raw("TOTAL").bold())];
        cells.extend(totals.iter().map(|t| {
            Cell::from(Line::from(Span::raw(format_size(*t)).bold()).alignment(Alignment::Right))
        }));
        cells.push(delta_cell(
            *totals.last().unwrap_or(&0) as i64 - *totals.first().unwrap_or(&0) as i64,
        ));
        rows.push(Row::new(cells).style(Style::default().bg(Color::Indexed(236))));
        let mut widths = vec![Constraint::Min(18)];
        widths.extend(std::iter::repeat_n(Constraint::Length(11), snaps.len() + 1));
        f.render_widget(
            Table::new(rows, widths)
                .header(Row::new(header).style(Style::default().fg(MUTED).bold()))
                .block(panel(
                    Line::from(vec![
                        Span::raw(" Growth per category in "),
                        Span::raw(display_path(&app.root, Some(&home))).bold(),
                        Span::raw(" "),
                    ]),
                    true,
                )),
            body,
        );
    }
    footer(
        f,
        app,
        bottom,
        status_line(app, Line::from("")),
        &[
            (Action::Refresh, "rescan"),
            (Action::NextTab, "next tab"),
            (Action::Quit, "quit"),
        ],
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn keycap(k: &str) -> Span<'static> {
    Span::styled(
        format!(" {k} "),
        Style::default().fg(Color::Black).bg(Color::Indexed(244)),
    )
}

fn draw_modal(f: &mut Frame, app: &App, m: &Modal) {
    let home = app.session.platform.dirs().home.clone();
    let (title, lines, color): (String, Vec<Line>, Color) = match m {
        Modal::Help => (
            " Keys ".into(),
            Action::ALL
                .iter()
                .map(|(a, label)| {
                    Line::from(vec![
                        Span::styled(
                            format!("{:>12}  ", app.keys.key_for(*a)),
                            Style::default().fg(ACCENT).bold(),
                        ),
                        Span::raw(*label),
                    ])
                })
                .chain([
                    Line::from(vec![
                        Span::styled(
                            format!("{:>12}  ", "1 2 3"),
                            Style::default().fg(ACCENT).bold(),
                        ),
                        Span::raw("switch tab"),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Any key closes this help.",
                        Style::default().fg(MUTED),
                    )),
                ])
                .collect(),
            ACCENT,
        ),
        Modal::Busy {
            title,
            detail,
            started,
        } => (
            format!(" {title} "),
            vec![
                Line::from(vec![
                    Span::styled(
                        format!("{} ", spinner(app)),
                        Style::default().fg(ACCENT).bold(),
                    ),
                    Span::raw(detail.clone()),
                ]),
                Line::from(Span::styled(
                    format!("{:.1} s", started.elapsed().as_secs_f64()),
                    Style::default().fg(MUTED),
                )),
            ],
            ACCENT,
        ),
        Modal::ConfirmClean(plan) => {
            let mut l: Vec<Line> = vec![
                Line::from(match plan.mode {
                    Mode::Trash => "Move these to the trash (restore with `fagia undo`):",
                    Mode::Permanent => "PERMANENTLY delete these:",
                })
                .bold(),
                Line::from(""),
            ];
            for i in &plan.items {
                let p = display_path(&i.finding.path, Some(&home));
                l.push(match &i.refusal {
                    Some(why) => Line::from(vec![
                        Span::styled("  ⊘ ", Style::default().fg(Color::Yellow)),
                        Span::styled(p, Style::default().fg(MUTED)),
                        Span::styled(format!("  {why}"), Style::default().fg(Color::Yellow)),
                    ]),
                    None => Line::from(vec![
                        Span::styled("  ✔ ", Style::default().fg(Color::Green)),
                        Span::styled(
                            format!("{:>10}  ", format_size(i.finding.reclaimable)),
                            size_style(i.finding.reclaimable),
                        ),
                        Span::raw(p),
                        Span::styled(
                            format!("  {}", i.finding.evidence),
                            Style::default().fg(MUTED),
                        ),
                    ]),
                });
            }
            l.push(Line::from(""));
            l.push(Line::from(vec![
                Span::raw("Total "),
                size_span(plan.selected_bytes()),
                Span::raw("     "),
                keycap("y"),
                Span::raw(" confirm   "),
                keycap("n"),
                Span::raw(" cancel"),
            ]));
            (" Confirm clean ".into(), l, Color::Green)
        }
        Modal::ConfirmKill(plan, kind) => {
            let verb = match kind {
                SignalKind::Quit | SignalKind::ForceKill => "Quit (SIGTERM)",
                SignalKind::Pause => "Pause (SIGSTOP)",
                SignalKind::Resume => "Resume (SIGCONT)",
            };
            let mut l = vec![
                Line::from(vec![
                    Span::raw(format!("{verb} ")),
                    Span::raw(plan.group.clone()).bold(),
                ]),
                Line::from(""),
            ];
            for t in &plan.targets {
                l.push(match &t.refusal {
                    Some(why) => Line::from(Span::styled(
                        format!("  ⊘ {:>7}  {why}", t.key.pid),
                        Style::default().fg(Color::Yellow),
                    )),
                    None => Line::from(vec![
                        Span::styled(format!("  {:>7}  ", t.key.pid), Style::default().fg(MUTED)),
                        Span::styled(format!("{:>10}  ", format_size(t.fair)), size_style(t.fair)),
                        Span::raw(t.name.clone()),
                    ]),
                });
            }
            if plan.unsaved_work && *kind == SignalKind::Quit {
                l.push(Line::from(""));
                l.push(Line::from(Span::styled(
                    "⚠ This app may hold unsaved work.",
                    Style::default().fg(Color::Yellow),
                )));
            }
            l.push(Line::from(""));
            l.push(Line::from(vec![
                keycap("y"),
                Span::raw(" confirm   "),
                keycap("n"),
                Span::raw(" cancel"),
            ]));
            (" Confirm ".into(), l, Color::Yellow)
        }
        Modal::ConfirmForce(plan) => (
            " Still running ".into(),
            vec![
                Line::from(vec![
                    Span::raw(plan.group.clone()).bold(),
                    Span::raw(" did not quit within the grace period."),
                ]),
                Line::from(Span::styled(
                    "Force kill (SIGKILL)? Unsaved work is lost.",
                    Style::default().fg(Color::Red),
                )),
                Line::from(""),
                Line::from(vec![
                    keycap("y"),
                    Span::raw(" force kill   "),
                    keycap("n"),
                    Span::raw(" leave it running"),
                ]),
            ],
            Color::Red,
        ),
        Modal::ConfirmEmpty { entries, typed } => {
            let total: u64 = entries.iter().map(|e| e.size).sum();
            let mut l = vec![
                Line::from(vec![
                    Span::raw("Permanently delete "),
                    Span::raw(format!("{} item(s)", entries.len())).bold(),
                    Span::raw(", "),
                    size_span(total),
                    Span::raw(" from the trash."),
                ]),
                Line::from(Span::styled(
                    "This cannot be undone.",
                    Style::default().fg(Color::Red).bold(),
                )),
                Line::from(""),
            ];
            for e in entries.iter().take(8) {
                let what = e.original.as_deref().unwrap_or(&e.path);
                l.push(Line::from(vec![
                    Span::styled(
                        format!("  {:>10}  ", format_size(e.size)),
                        size_style(e.size),
                    ),
                    Span::raw(display_path(what, Some(&home))),
                ]));
            }
            if entries.len() > 8 {
                l.push(Line::from(Span::styled(
                    format!("  …and {} more", entries.len() - 8),
                    Style::default().fg(MUTED),
                )));
            }
            l.push(Line::from(""));
            l.push(Line::from(vec![
                Span::raw("Type "),
                Span::styled("empty", Style::default().fg(Color::Red).bold()),
                Span::raw(" and press Enter: "),
                Span::styled(
                    format!("{typed}▏"),
                    Style::default().fg(Color::White).bold(),
                ),
                Span::styled("   esc cancels", Style::default().fg(MUTED)),
            ]));
            (" Empty the trash ".into(), l, Color::Red)
        }
        Modal::Info(title, lines) => (
            format!(" {title} "),
            lines
                .iter()
                .map(|s| Line::from(s.clone()))
                .chain([
                    Line::from(""),
                    Line::from(Span::styled("Any key closes.", Style::default().fg(MUTED))),
                ])
                .collect(),
            ACCENT,
        ),
    };
    let h = (lines.len() as u16 + 4).min(f.area().height);
    let w = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(40)
        .clamp(40, 100) as u16
        + 6;
    let area = centered(f.area(), w, h);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(color))
                    .title(Span::styled(title, Style::default().fg(color).bold()))
                    .padding(Padding::new(2, 2, 1, 0)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
