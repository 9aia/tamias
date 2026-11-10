//! Drawing — tab bar on top, the active pane filling the frame,
//! status bar at the bottom. Phase 1 renders only the Installed
//! pane's table; the other panes draw their phase placeholder.
//! Chrome bars yield to the pane on short terminals, same budget
//! rule as myco-tui: tabs need height ≥3, the status bar ≥5.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::app::{Mode, Model, Pane};

const ACCENT: Color = Color::Indexed(69); // same nav blue as myco-tui
const MUTED: Color = Color::Indexed(241);

/// Chrome budget — `(tabs, status)`: tabs cost a row at height ≥3,
/// the status bar only at height ≥5.
fn chrome(height: u16) -> (bool, bool) {
    (height >= 3, height >= 5)
}

/// One full frame. Also records the tab/pane geometry on `m` so the
/// next mouse event can hit-test against where rows landed.
pub fn draw(f: &mut Frame, m: &mut Model) {
    let area = f.area();
    let (show_tabs, show_status) = chrome(area.height);
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
        m.tabs_area = Some(chunks[next]);
        next += 1;
    } else {
        m.tabs_area = None;
    }
    draw_pane(f, m, chunks[next]);
    if show_status {
        draw_status(f, m, chunks[next + 1]);
    }
    match m.mode {
        Mode::Help => draw_help(f, area),
        Mode::Filter | Mode::Command => draw_input(f, m, area),
        Mode::Normal => {}
    }
}

/// Tab captions — full names when they fit, initials when they
/// don't (the `1`-`5`/`h`/`l` binds still work).
fn tab_labels(width: usize) -> Vec<String> {
    let full: Vec<String> = Pane::ALL
        .iter()
        .map(|p| format!(" {} ", p.label()))
        .collect();
    let need: usize = full.iter().map(|s| s.chars().count()).sum::<usize>() + full.len() + 1;
    if need <= width {
        full
    } else {
        Pane::ALL
            .iter()
            .map(|p| p.label()[..1].to_string())
            .collect()
    }
}

fn draw_tabs(f: &mut Frame, m: &Model, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    for (p, label) in Pane::ALL.iter().zip(tab_labels(area.width as usize)) {
        let style = if *p == m.pane {
            Style::default().fg(Color::White).bg(ACCENT)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Click hit-test for the tab row: which tab does column `x` land
/// on in a `width`-wide bar? Mirrors the span layout `draw_tabs`
/// emits — leading space + one separator per tab.
pub fn tab_hit(width: u16, x: u16) -> Option<usize> {
    let mut col = 1u16; // leading space
    for (i, label) in tab_labels(width as usize).iter().enumerate() {
        let w = label.chars().count() as u16;
        if (col..col + w).contains(&x) {
            return Some(i);
        }
        col += w + 1; // the separator after each tab
    }
    None
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn short_sha(s: &str) -> String {
    let bare = s.strip_prefix("sha256:").unwrap_or(s);
    bare.chars().take(12).collect()
}

fn draw_pane(f: &mut Frame, m: &mut Model, area: Rect) {
    match m.pane {
        Pane::Installed => draw_installed(f, m, area),
        other => {
            let text = format!(
                "{}\n\n(the {} pane is a phase-1 placeholder)",
                other.placeholder(),
                other.label().to_lowercase()
            );
            f.render_widget(Paragraph::new(text).style(Style::default().fg(MUTED)), area);
            m.board_area = Rect::default();
            m.viewport_top = 0;
        }
    }
}

fn draw_installed(f: &mut Frame, m: &mut Model, area: Rect) {
    let vis = m.visible();
    if let Some(err) = &m.load_err {
        f.render_widget(
            Paragraph::new(format!("store: {err}")).style(Style::default().fg(Color::Red)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    if m.entries.is_empty() {
        f.render_widget(
            Paragraph::new("no packages installed — `tamias install <ref>` or `tamias add <ref>`")
                .style(Style::default().fg(MUTED)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    if vis.is_empty() {
        f.render_widget(
            Paragraph::new(format!("no entries match /{}", m.filter))
                .style(Style::default().fg(MUTED)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }

    let header = Row::new(vec!["NAME", "TYPE", "SOURCE", "RESOLVED", "DIGEST", "SIZE"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = vis
        .iter()
        .map(|&i| {
            let r = &m.entries[i];
            let rec = &r.entry.record;
            Row::new(vec![
                r.entry.label(),
                rec.kind.clone().unwrap_or_else(|| "-".into()),
                rec.source_ref.clone(),
                short_sha(&rec.resolved),
                short_sha(&rec.digest),
                r.size.map_or("-".into(), human_size),
            ])
        })
        .collect();
    let widths = [
        Constraint::Min(16),
        Constraint::Length(12),
        Constraint::Min(14),
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(9),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" installed "))
        .row_highlight_style(Style::default().bg(Color::Indexed(238)));
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(m.sel));
    f.render_stateful_widget(table, area, &mut state);
    // remember where the rows landed for mouse hit-testing — the
    // block border eats one row/column on each side
    m.board_area = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    m.viewport_top = state.offset();
}

fn draw_status(f: &mut Frame, m: &Model, area: Rect) {
    let left = if m.status.is_empty() {
        match m.pane {
            Pane::Installed => {
                if m.filter.is_empty() {
                    format!("{} installed", m.entries.len())
                } else {
                    format!("{}/{} · /{}", m.visible().len(), m.entries.len(), m.filter)
                }
            }
            _ => format!("{} pane — placeholder", m.pane.label()),
        }
    } else {
        m.status.clone()
    };
    let style = if m.status_err {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(MUTED)
    };
    let hint = "j/k move · Tab panes · / filter · : cmds · ? help · q quit";
    let spans = vec![
        Span::styled(left, style),
        Span::raw("  "),
        Span::styled(hint, Style::default().fg(MUTED)),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The `/`/`:` input line — drawn over the bottom of the pane while
/// a text mode is active.
fn draw_input(f: &mut Frame, m: &Model, area: Rect) {
    let (label, text) = match m.mode {
        Mode::Filter => ("/", &m.input),
        Mode::Command => (":", &m.cmdline),
        _ => unreachable!(),
    };
    let y = area.bottom().saturating_sub(2).max(area.y);
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    };
    let spans = vec![
        Span::styled(label.to_string(), Style::default().fg(ACCENT)),
        Span::raw(text.clone()),
        Span::styled("█", Style::default().fg(ACCENT)),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), rect);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let text = "\
keys — phase 1 (read-only)

  j/k, ↑/↓      move selection        g/G    top/bottom
  Tab, h/l      cycle panes           1-5    jump to pane
  /             filter installed      Enter  keep filter
  :             command line          esc    leave mode
  R             reload store          F      clear filter
  ?             this help             q/C-c  quit

commands:  :q  :r/:reload  :help
mouse:     wheel scrolls · click row selects · click tab switches

later phases: v verify · u update · p pin · r remove (confirm) ·
i install · d doctor · registry search · provider run/logs ·
manifest deps form · bundle pack/publish";
    let width = area.width.min(64) as usize;
    let h = text.lines().count() as u16 + 2;
    let w = (width as u16 + 2).min(area.width);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect::new(x, y, w, h.min(area.height));
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(" help "))
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false }),
        rect,
    );
}
