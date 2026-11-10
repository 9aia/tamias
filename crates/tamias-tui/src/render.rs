//! Drawing — tab bar on top, the active pane filling the frame, an
//! optional op-log strip, status bar at the bottom, and modal/prompt
//! overlays. Chrome bars yield to the pane on short terminals, same
//! budget rule as myco-tui: tabs need height ≥3, the status bar ≥5,
//! the log strip ≥12 (and only while the log has lines).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::app::{Modal, Mode, Model, Pane, PromptKind, SPINNER};

const ACCENT: Color = Color::Indexed(69); // same nav blue as myco-tui
const MUTED: Color = Color::Indexed(241);
const WARN: Color = Color::Indexed(214);

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
    let show_log = area.height >= 12 && !m.log.is_empty();
    let mut constraints = Vec::with_capacity(4);
    if show_tabs {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    if show_log {
        constraints.push(Constraint::Length(4));
    }
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
    next += 1;
    if show_log {
        draw_log(f, m, chunks[next]);
        next += 1;
    }
    if show_status {
        draw_status(f, m, chunks[next]);
    }
    match &m.mode {
        Mode::Help => draw_help(f, area),
        Mode::Filter | Mode::Search | Mode::Command | Mode::Prompt(_) => draw_input(f, m, area),
        Mode::Normal => {}
    }
    if let Some(modal) = &m.modal {
        draw_modal(f, modal, area);
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
        Pane::Installed => {
            if m.detail.is_some() {
                draw_detail(f, m, area);
            } else {
                draw_installed(f, m, area);
            }
        }
        Pane::Registry => draw_registry(f, m, area),
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

/// Record where a bordered table's data rows landed — the border
/// eats one row/column on each side; `state.offset()` is the scroll.
fn note_table_area(m: &mut Model, area: Rect, offset: usize) {
    m.board_area = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    m.viewport_top = offset;
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
            Paragraph::new("no packages installed — `i` install-by-ref, `:install <ref>`, or `tamias install <ref>`")
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
    note_table_area(m, area, state.offset());
}

/// The `Enter` detail view — record provenance plus the payload
/// manifest's fields (provider/services/deps when present).
fn draw_detail(f: &mut Frame, m: &mut Model, area: Rect) {
    let Some(detail) = &m.detail else {
        return;
    };
    let rec = &detail.entry.record;
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("name:      ", Style::default().fg(ACCENT)),
            Span::raw(rec.name.clone()),
        ]),
        Line::from(vec![
            Span::styled("scope:     ", Style::default().fg(ACCENT)),
            Span::raw(rec.scope.clone().unwrap_or_else(|| "-".into())),
        ]),
        Line::from(vec![
            Span::styled("version:   ", Style::default().fg(ACCENT)),
            Span::raw(rec.version.clone().unwrap_or_else(|| "-".into())),
        ]),
        Line::from(vec![
            Span::styled("type:      ", Style::default().fg(ACCENT)),
            Span::raw(rec.kind.clone().unwrap_or_else(|| "-".into())),
        ]),
        Line::from(vec![
            Span::styled("source:    ", Style::default().fg(ACCENT)),
            Span::raw(rec.source_ref.clone()),
        ]),
        Line::from(vec![
            Span::styled("resolved:  ", Style::default().fg(ACCENT)),
            Span::raw(rec.resolved.clone()),
        ]),
        Line::from(vec![
            Span::styled("digest:    ", Style::default().fg(ACCENT)),
            Span::raw(rec.digest.clone()),
        ]),
        Line::from(vec![
            Span::styled("installed: ", Style::default().fg(ACCENT)),
            Span::raw(format!(
                "{} (unix {})",
                detail.entry.dir.display(),
                rec.installed_at
            )),
        ]),
    ];
    match &detail.manifest {
        Some(manifest) => {
            if let Some(desc) = &manifest.description {
                lines.push(Line::from(vec![
                    Span::styled("desc:      ", Style::default().fg(ACCENT)),
                    Span::raw(desc.clone()),
                ]));
            }
            if !manifest.include.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("include:   ", Style::default().fg(ACCENT)),
                    Span::raw(manifest.include.join(", ")),
                ]));
            }
            if !manifest.deps.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("deps:      ", Style::default().fg(ACCENT)),
                    Span::raw(manifest.deps.join(", ")),
                ]));
            }
            if let Some(provider) = &manifest.provider {
                let services = provider
                    .services
                    .iter()
                    .map(|s| s.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(Line::from(vec![
                    Span::styled("provider:  ", Style::default().fg(ACCENT)),
                    Span::raw(format!(
                        "{} service(s){}",
                        provider.services.len(),
                        if services.is_empty() {
                            String::new()
                        } else {
                            format!(" — {services}")
                        }
                    )),
                ]));
            }
        }
        None => lines.push(Line::from(vec![
            Span::styled("manifest:  ", Style::default().fg(WARN)),
            Span::raw(detail.manifest_err.clone().unwrap_or_else(|| "none".into())),
        ])),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "esc/← back — v verify · u update · p pin · r remove · c cache clean · d doctor",
        Style::default().fg(MUTED),
    )));
    let title = format!(" {} ", detail.entry.label());
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
    m.board_area = Rect::default();
    m.viewport_top = 0;
}

/// The Registry pane — search results table plus the target/query
/// context in the block title.
fn draw_registry(f: &mut Frame, m: &mut Model, area: Rect) {
    if let Some(err) = &m.reg_err {
        f.render_widget(
            Paragraph::new(format!("registry: {err}")).style(Style::default().fg(Color::Red)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    if m.reg_query.trim().is_empty() {
        f.render_widget(
            Paragraph::new(
                "registry search — `/` to type a query (name/scope substring), \
                 `:registry <name>` picks the target, `i` installs a result",
            )
            .style(Style::default().fg(MUTED)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    if m.reg_results.is_empty() {
        let hint = if m.busy.is_some() {
            "searching…"
        } else {
            "no results"
        };
        f.render_widget(
            Paragraph::new(format!("{hint} — /{query}", query = m.reg_query))
                .style(Style::default().fg(MUTED)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    let header = Row::new(vec!["NAME", "SCOPE", "TYPE", "VERSION", "DIGEST"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = m
        .reg_results
        .iter()
        .map(|e| {
            Row::new(vec![
                e.label(),
                e.scope.clone().unwrap_or_else(|| "-".into()),
                e.kind.clone().unwrap_or_else(|| "-".into()),
                e.version.clone().unwrap_or_else(|| "-".into()),
                short_sha(&e.digest),
            ])
        })
        .collect();
    let widths = [
        Constraint::Min(16),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(14),
    ];
    let title = format!(" registry — {} · /{} ", m.reg_target_label, m.reg_query);
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(Style::default().bg(Color::Indexed(238)));
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(m.reg_sel));
    f.render_stateful_widget(table, area, &mut state);
    note_table_area(m, area, state.offset());
}

/// The op log — last few phase/result lines, tail-scroll.
fn draw_log(f: &mut Frame, m: &Model, area: Rect) {
    let skip = m.log.len().saturating_sub(4);
    let text: Vec<Line> = m
        .log
        .iter()
        .skip(skip)
        .map(|l| {
            let style = if l.starts_with("error") || l.starts_with("!!") {
                Style::default().fg(Color::Red)
            } else if l.starts_with("warning") {
                Style::default().fg(WARN)
            } else {
                Style::default().fg(MUTED)
            };
            Line::from(Span::styled(format!(" {l}"), style))
        })
        .collect();
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::TOP).title(" log ")),
        area,
    );
}

fn draw_status(f: &mut Frame, m: &Model, area: Rect) {
    let left = if let Some(busy) = &m.busy {
        let frame = SPINNER[(m.tick_n as usize) % SPINNER.len()];
        format!("{frame} {busy}")
    } else if !m.status.is_empty() {
        m.status.clone()
    } else {
        match m.pane {
            Pane::Installed => {
                if m.detail.is_some() {
                    "detail — esc back".to_string()
                } else if m.filter.is_empty() {
                    format!("{} installed", m.entries.len())
                } else {
                    format!("{}/{} · /{}", m.visible().len(), m.entries.len(), m.filter)
                }
            }
            Pane::Registry => {
                if m.reg_query.is_empty() {
                    "registry — / search · :registry <name> · i install".to_string()
                } else {
                    format!(
                        "{}/{} · /{} · {}",
                        m.reg_results.len(),
                        m.reg_results.len(),
                        m.reg_query,
                        m.reg_target_label
                    )
                }
            }
            _ => format!("{} pane — placeholder", m.pane.label()),
        }
    };
    let style = if m.status_err {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(MUTED)
    };
    let hint = match m.pane {
        Pane::Installed => {
            if m.detail.is_some() {
                "esc back · v/u/p/r/c/d act on the row"
            } else {
                "j/k · Enter detail · v verify · u update · p pin · r remove · i install · c clean · d doctor · / filter · : cmds · q quit"
            }
        }
        Pane::Registry => "j/k · i install · / query · : cmds · q quit",
        _ => "Tab panes · / filter · : cmds · ? help · q quit",
    };
    let spans = vec![
        Span::styled(left, style),
        Span::raw("  "),
        Span::styled(hint, Style::default().fg(MUTED)),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The `/`/`:`/prompt input line — drawn over the bottom of the pane
/// while a text mode is active.
fn draw_input(f: &mut Frame, m: &Model, area: Rect) {
    let (label, text) = match &m.mode {
        Mode::Filter => ("/", m.input.as_str()),
        Mode::Search => ("/", m.input.as_str()),
        Mode::Command => (":", m.cmdline.as_str()),
        Mode::Prompt(kind) => {
            let l = match kind {
                PromptKind::Pin { .. } => "pin > ",
                PromptKind::InstallRef => "install > ",
                PromptKind::RegistryTarget => "registry > ",
            };
            (l, m.prompt_buf.as_str())
        }
        _ => return,
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
        Span::raw(text.to_string()),
        Span::styled("█", Style::default().fg(ACCENT)),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// A centered confirm/consent dialog — `y`/`n`, default `n`.
fn draw_modal(f: &mut Frame, modal: &Modal, area: Rect) {
    let (title, body): (String, Vec<String>) = match modal {
        Modal::Confirm { title, body, .. } => (title.clone(), body.clone()),
        Modal::Consent { reason, .. } => (
            "bundled drivers — consent? [y/n]".to_string(),
            vec![reason.clone()],
        ),
    };
    let mut lines: Vec<Line> = body
        .iter()
        .map(|b| Line::from(Span::styled(b.clone(), Style::default().fg(WARN))))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "y = yes · n/esc = no (default n)",
        Style::default().fg(MUTED),
    )));
    let w = (area.width * 3 / 5).clamp(30, 72);
    let h = (lines.len() as u16 + 2).min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect::new(x, y, w, h);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let text = "\
keys — installed pane

  j/k, ↑/↓      move selection        g/G    top/bottom
  Enter         detail view           esc/←   back
  v             verify (digest check) u       update selected
  U             update all            p       pin → new ref
  r             remove (confirm)      c/C     cache clean >30d / all
  d             doctor summary        i       install by ref
  Tab, h/l      cycle panes           1-5    jump to pane
  /             filter installed      Enter  keep filter
  :             command line          esc    leave mode
  R             reload store          F      clear filter
  ?             this help             q/C-c  quit

registry pane:  / search-as-you-type (debounced) · i install result ·
                :registry <name> target · :search <q>

commands:  :q  :r/:reload  :help  :install <ref>  :remove <name>
           :verify <name>  :update [name]  :pin <name> <ref>
           :search <q>  :registry <name>  :clean [all]  :doctor
mouse:     wheel scrolls · click row selects · click tab switches

later phases: provider run/logs · manifest deps form · bundle pack/publish";
    let width = area.width.min(72) as usize;
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
