//! Drawing — env tab bar on top, ref list left, detail pane right,
//! help-hint footer at the bottom, `?` as a centered overlay.
//! Everything degrades per frame: the detail pane folds under
//! ~72 columns, chrome yields on short terminals, and a vault that
//! won't open renders as an error line, never a panic.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{Listing, Model};

const ACCENT: Color = Color::Indexed(69); // nav blue
const MUTED: Color = Color::Indexed(241);
const WARN: Color = Color::Indexed(214);

/// One full frame.
pub fn draw(f: &mut Frame, m: &mut Model) {
    let area = f.area();
    let show_tabs = area.height >= 3;
    let show_status = area.height >= 5;
    let mut constraints = Vec::with_capacity(3);
    if show_tabs {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    if show_status {
        constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::vertical(constraints).split(area);

    let mut next = 0;
    if show_tabs {
        draw_tabs(f, m, chunks[next]);
        next += 1;
    }
    draw_main(f, m, chunks[next]);
    if show_status {
        draw_footer(f, m, chunks[next + 1]);
    }
    if m.help {
        draw_help(f, area);
    }
}

/// ` dev  staging  prod ` — active tab inverted, digit binds
/// implied by position.
fn draw_tabs(f: &mut Frame, m: &Model, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    for (i, t) in m.tabs.iter().enumerate() {
        let label = format!(" {} ", t.env.as_str());
        let style = if i == m.active {
            Style::default().fg(Color::White).bg(ACCENT)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        "— browse only; values stay sealed",
        Style::default().fg(MUTED),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// List + detail side by side; under ~72 columns the list keeps
/// the screen and the detail folds to a hint.
fn draw_main(f: &mut Frame, m: &mut Model, area: Rect) {
    if area.width >= 72 {
        let halves = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        draw_list(f, m, halves[0]);
        draw_detail(f, m, halves[1]);
    } else {
        draw_list(f, m, area);
    }
}

fn draw_list(f: &mut Frame, m: &mut Model, area: Rect) {
    let title = match &m.listing {
        Listing::Ready(entries) => {
            let tab = &m.tabs[m.active];
            format!(
                " {} → {} ({}) — {} secret{} ",
                tab.env.as_str(),
                tab.vault,
                tab.kind,
                entries.len(),
                if entries.len() == 1 { "" } else { "s" },
            )
        }
        Listing::Failed(_) => format!(" {} ", m.tabs[m.active].env.as_str()),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    match &m.listing {
        Listing::Failed(e) => {
            let p = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "vault not readable",
                    Style::default().fg(WARN),
                )),
                Line::from(""),
                Line::from(e.as_str()),
                Line::from(""),
                Line::from(Span::styled(
                    "fix the vault, then R to retry",
                    Style::default().fg(MUTED),
                )),
            ])
            .block(block)
            .wrap(Wrap { trim: false });
            f.render_widget(p, area);
        }
        Listing::Ready(entries) => {
            if entries.is_empty() {
                let p = Paragraph::new(vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "no secrets stored in this env",
                        Style::default().fg(MUTED),
                    )),
                    Line::from(Span::styled(
                        "printf … | tamias put tamias://ns/name",
                        Style::default().fg(MUTED),
                    )),
                ])
                .block(block);
                f.render_widget(p, area);
                return;
            }
            let needle = m.filter.to_lowercase();
            let items: Vec<ListItem> = entries
                .iter()
                .filter(|e| needle.is_empty() || e.rref.path().to_lowercase().contains(&needle))
                .map(|e| ListItem::new(format!("tamias://{}", e.rref.path())))
                .collect();
            if items.is_empty() {
                let p = Paragraph::new(Line::from(Span::styled(
                    format!("nothing matches /{}", m.filter),
                    Style::default().fg(MUTED),
                )))
                .block(block);
                f.render_widget(p, area);
                return;
            }
            let mut state = ListState::default();
            state.select(Some(m.selected.min(items.len() - 1)));
            let list = List::new(items)
                .block(block)
                .highlight_style(Style::default().bg(ACCENT).fg(Color::White))
                .highlight_symbol("▸ ");
            f.render_stateful_widget(list, area, &mut state);
        }
    }
}

/// Metadata only: version, timestamps, env, backing vault. A
/// revealed value shows here — the only place a value can ever
/// render, and only after the audited `r`→`r`.
fn draw_detail(f: &mut Frame, m: &Model, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" detail ");
    let Some(e) = m.current() else {
        f.render_widget(Paragraph::new("").block(block), area);
        return;
    };
    let tab = &m.tabs[m.active];
    let kv = |k: &'static str, v: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{k:<9}"), Style::default().fg(MUTED)),
            Span::raw(v),
        ])
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("tamias://{}", e.rref.path()),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        kv("env", tab.env.as_str().to_string()),
        kv("vault", format!("{} ({})", tab.vault, tab.kind)),
        kv(
            "via",
            tab.location
                .clone()
                .unwrap_or_else(|| "default paths".into()),
        ),
        kv("version", e.version.to_string()),
        kv("created", e.created_at.clone()),
        kv(
            "rotated",
            e.rotated_at.clone().unwrap_or_else(|| "never".into()),
        ),
        Line::from(""),
    ];
    match &m.revealed {
        Some((path, value)) if *path == e.rref.path() => lines.extend([
            Line::from(Span::styled(
                "value (revealed — audited get):",
                Style::default().fg(WARN),
            )),
            Line::from(value.expose().to_string()),
            Line::from(""),
            Line::from(Span::styled(
                "Esc or move to hide",
                Style::default().fg(MUTED),
            )),
        ]),
        _ => lines.push(Line::from(Span::styled(
            "value: sealed — r then r to reveal",
            Style::default().fg(MUTED),
        ))),
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Footer: filter line while editing, status when set, else the
/// key hints.
fn draw_footer(f: &mut Frame, m: &Model, area: Rect) {
    let line = if m.filtering {
        Line::from(vec![
            Span::styled("/", Style::default().fg(ACCENT)),
            Span::raw(m.filter.clone()),
            Span::styled("  (Enter keep · Esc clear)", Style::default().fg(MUTED)),
        ])
    } else if let Some(s) = &m.status {
        Line::from(Span::styled(s.clone(), Style::default().fg(WARN)))
    } else {
        Line::from(vec![
            Span::styled(" j/k", Style::default().fg(ACCENT)),
            Span::raw(" move  "),
            Span::styled("tab/1-9", Style::default().fg(ACCENT)),
            Span::raw(" env  "),
            Span::styled("/", Style::default().fg(ACCENT)),
            Span::raw(" filter  "),
            Span::styled("r", Style::default().fg(ACCENT)),
            Span::raw(" reveal  "),
            Span::styled("R", Style::default().fg(ACCENT)),
            Span::raw(" reload  "),
            Span::styled("?", Style::default().fg(ACCENT)),
            Span::raw(" help  "),
            Span::styled("q", Style::default().fg(ACCENT)),
            Span::raw(" quit"),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
}

/// `?` — centered key reference.
fn draw_help(f: &mut Frame, area: Rect) {
    let w = 54u16.min(area.width.saturating_sub(4));
    let h = 16u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let rows = [
        ("j / k, ↓ / ↑", "move"),
        ("g / G", "first / last"),
        ("tab / shift-tab", "cycle envs"),
        ("1 … 9", "jump to env tab"),
        ("/", "filter refs"),
        ("r then r", "reveal a value (audited get)"),
        ("R", "reload the active env"),
        ("Esc", "clear filter / hide value"),
        ("?", "this help"),
        ("q, ctrl-c", "quit"),
    ];
    let mut lines = vec![Line::from("")];
    for (k, v) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<16}"), Style::default().fg(ACCENT)),
            Span::raw(v),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  read-only — values stay sealed unless revealed",
        Style::default().fg(MUTED),
    )));
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" tamias ui — keys "),
        ),
        popup,
    );
}
