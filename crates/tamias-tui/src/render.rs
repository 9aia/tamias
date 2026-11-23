//! Drawing — tab bar on top, the active pane filling the frame, a
//! status bar at the bottom, and modal/prompt overlays. Chrome bars
//! yield to the pane on short terminals, same budget rule as
//! myco-tui: tabs need height ≥3, the status bar ≥5. `L`/`:log`
//! docks the op log as a right-hand sidebar (a centered overlay on
//! terminals too narrow for one) — there is no permanent log strip.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::app::{Detail, Modal, Mode, Model, Pane, PromptKind, SPINNER};

/// Accent — a bright saturated blue that stays legible on dark
/// terminals (the old `Indexed(69)` navy read as near-background);
/// also the active tab's background and the `›` cursor marker.
const ACCENT: Color = Color::Indexed(75);
/// Secondary text — mid gray: clearly above a dark terminal's
/// background where `Indexed(241)` read as bg-colored, still a
/// step dimmer than [`TEXT`].
const MUTED: Color = Color::Indexed(245);
/// Primary text — the explicit fg for spans that carry no style of
/// their own; terminals' default fg can be near-black on dark
/// themes, which is what made table cells and body copy vanish.
const TEXT: Color = Color::White;
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
    // `L` docks the op log as a right-hand sidebar — the pane chrome
    // lays out inside the shrunken main rect, so `tabs_area`/
    // `board_area`/`home_area` and the status bar all describe what
    // is actually on screen (the status bar shrinks with the pane,
    // it doesn't span under the sidebar). Below ~50 cols the sidebar
    // would starve the pane, so the centered overlay takes over.
    let split = if m.mode == Mode::Log && area.width >= 50 {
        let side_w = (area.width / 4)
            .clamp(24, 48)
            .min(area.width.saturating_sub(20));
        let c = Layout::horizontal([Constraint::Min(0), Constraint::Length(side_w)]).split(area);
        Some((c[0], c[1]))
    } else {
        None
    };
    let main = split.map_or(area, |(main, _)| main);
    let (show_tabs, show_status) = chrome(main.height);
    let mut constraints = Vec::with_capacity(3);
    if show_tabs {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    if show_status {
        constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::vertical(constraints).split(main);

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
    if show_status {
        draw_status(f, m, chunks[next]);
    }
    match &m.mode {
        Mode::Help => draw_help(f, area),
        Mode::Filter | Mode::Search | Mode::Command | Mode::Prompt(_) => draw_input(f, m, area),
        Mode::Normal | Mode::Log => {}
    }
    // The log view needs `&mut Model` (it clamps `log_scroll` and
    // reports `log_view_h`/`log_area`), so it can't ride the
    // `&m.mode` match.
    if let Some((_, side)) = split {
        draw_log_view(f, m, side);
    } else if m.mode == Mode::Log {
        draw_log_overlay(f, m, area);
    }
    if let Some(modal) = &m.modal {
        draw_modal(f, modal, area);
    }
}

/// Tab captions — full names when they fit, initials when they
/// don't (the `1`-`6`/`h`/`l` binds still work; PROJECT/PUBLISH get
/// two-char codes since both start with P).
fn tab_labels(width: usize) -> Vec<String> {
    let full: Vec<String> = Pane::ALL
        .iter()
        .map(|p| format!(" {} ", p.label()))
        .collect();
    let need: usize = full.iter().map(|s| s.chars().count()).sum::<usize>() + full.len() + 1;
    if need <= width {
        full
    } else {
        // Initials fallback — PROJECT and PUBLISH collide on 'P',
        // so those two widen to two chars ("PR"/"PU"); the digits
        // and h/l binds don't depend on the letters anyway.
        Pane::ALL
            .iter()
            .map(|p| match p {
                Pane::Manifest => "PR".to_string(),
                Pane::Author => "PU".to_string(),
                _ => p.label()[..1].to_string(),
            })
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

/// Unix seconds now — [`ago`]'s clock.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Relative-time label for an `installed_at` unix-seconds stamp —
/// "just now", "5m ago", "3h ago", "2d ago", then months/years.
/// `now` is a parameter so the buckets stay unit-testable; [`ago`]
/// is the clock-reading wrapper the Home pane uses.
fn ago_at(ts: u64, now: u64) -> String {
    let secs = now.saturating_sub(ts);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=2_592_000 => format!("{}d ago", secs / 86_400), // ≤30d
        2_592_001..=31_536_000 => format!("{}mo ago", secs / 2_592_000),
        _ => format!("{}y ago", secs / 31_536_000),
    }
}

/// [`ago_at`] against the wall clock.
fn ago(ts: u64) -> String {
    ago_at(ts, unix_now())
}

/// Trust-tier color — `signed`/`official` green, `bad` red,
/// `pinned`/`unpinned` warn yellow, anything else muted.
fn trust_style(tier: &str) -> Style {
    match tier {
        "official" | "signed" => Style::default().fg(Color::Green),
        "bad" => Style::default().fg(Color::Red),
        "pinned" | "unpinned" => Style::default().fg(WARN),
        _ => Style::default().fg(MUTED),
    }
}

/// The `[tier]` badge span for a store record — colored by
/// [`trust_style`]; `None` tier renders nothing (records that
/// predate trust tiers carry none).
fn trust_badge(tier: Option<&str>) -> Option<Span<'static>> {
    tier.map(|t| Span::styled(format!(" [{t}]"), trust_style(t)))
}

fn draw_pane(f: &mut Frame, m: &mut Model, area: Rect) {
    match m.pane {
        // The package page replaces whichever pane opened it — the
        // model only sets `detail` on Installed/Registry and clears
        // it on pane switches.
        Pane::Installed | Pane::Registry if m.detail.is_some() => draw_detail(f, m, area),
        Pane::Home => draw_home(f, m, area),
        Pane::Installed => draw_installed(f, m, area),
        Pane::Registry => draw_registry(f, m, area),
        Pane::Services => draw_services(f, m, area),
        Pane::Manifest => draw_manifest(f, m, area),
        Pane::Author => draw_author(f, m, area),
    }
}

/// The Home pane — the npm-home analog and the landing view:
/// wordmark + version, a muted stats strip (installed count, registry
/// target, store/cache roots), and the recently-installed list
/// (`installed_at` desc, top 8 — the same
/// [`Model::recent_installed`] ordering the `home_sel` cursor walks).
/// Key hints live only in the status bar — the body deliberately
/// carries none (the footer is the single hint surface).
/// The highlighted row gets the versions-focus treatment — accent
/// `›` marker + `Indexed(238)` row background — and `Enter` opens its
/// package page via the Installed pane.
///
/// The recents block is pinned to the bottom of the inner area and
/// rendered without wrapping, so each row is exactly one display
/// line — that's what lets `Model::home_area` map a mouse click to a
/// `recent_installed()` index. The wrapping header Paragraph takes
/// whatever height is left above it.
fn draw_home(f: &mut Frame, m: &mut Model, area: Rect) {
    m.home_area = Rect::default();
    let mut lines: Vec<Line> = Vec::new();
    // Wordmark — accent dot + bold white name; the crate version
    // rides beside it, the tagline on its own muted line.
    lines.push(Line::from(vec![
        Span::styled("● ", Style::default().fg(ACCENT)),
        Span::styled(
            "tamias",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(MUTED),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "package & content manager",
        Style::default().fg(MUTED),
    )));
    lines.push(Line::from(""));
    // Stats — one muted line that wraps naturally on narrow frames.
    lines.push(Line::from(Span::styled(
        format!(
            "{} installed · registry {} · store {} · cache {}",
            m.entries.len(),
            m.reg_target_label,
            m.ctx.store_root.display(),
            m.ctx.cache_root.display()
        ),
        Style::default().fg(MUTED),
    )));
    lines.push(Line::from(""));
    // The recents block — section header plus one line per row (or
    // the empty-state hint). Kept out of `lines` so it renders in its
    // own fixed-height chunk below.
    let mut tail: Vec<Line> = vec![Line::from(Span::styled(
        "recently installed",
        Style::default().fg(ACCENT),
    ))];
    if m.entries.is_empty() {
        tail.push(Line::from(Span::styled(
            "  nothing installed yet",
            Style::default().fg(MUTED),
        )));
    } else {
        for (pos, &i) in m.recent_installed().iter().enumerate() {
            let row = &m.entries[i];
            let rec = &row.entry.record;
            let cursor = pos == m.home_sel;
            let mut spans = vec![Span::styled(
                if cursor { "› " } else { "  " },
                if cursor {
                    Style::default().fg(ACCENT)
                } else {
                    Style::default()
                },
            )];
            spans.push(Span::raw(row.entry.label()));
            if let Some(badge) = trust_badge(rec.trust.as_deref()) {
                spans.push(badge);
            }
            if rec.installed_at > 0 {
                spans.push(Span::styled(
                    format!("  · {}", ago(rec.installed_at)),
                    Style::default().fg(MUTED),
                ));
            }
            let line = Line::from(spans);
            tail.push(if cursor {
                line.style(Style::default().bg(Color::Indexed(238)))
            } else {
                line
            });
        }
    }
    let block = Block::default().borders(Borders::ALL).title(" home ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    // Pin the tail to the bottom — `Min(0)` header above it. The tail
    // Paragraph gets no `wrap`, so each `Line` is exactly one display
    // row (long labels clip instead of wrapping onto the next row).
    let tail_h = (tail.len() as u16).min(inner.height);
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(tail_h)]).split(inner);
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: true }),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(tail).style(Style::default().fg(TEXT)),
        chunks[1],
    );
    // Data rows start one line below the "recently installed" header;
    // height is the rows actually rendered (clipped on short frames).
    let shown = chunks[1]
        .height
        .saturating_sub(1)
        .min(m.recent_installed().len() as u16);
    if shown > 0 {
        m.home_area = Rect {
            x: chunks[1].x,
            y: chunks[1].y.saturating_add(1),
            width: chunks[1].width,
            height: shown,
        };
    }
    m.board_area = Rect::default();
    m.viewport_top = 0;
}

/// The Services pane — every installed provider service with its
/// unit state. Rows come from [`Job::ServicesRefresh`]; the table
/// marks stale while a refresh is queued/running via the busy bar.
fn draw_services(f: &mut Frame, m: &mut Model, area: Rect) {
    if let Some(err) = &m.svc_err {
        if m.svc_rows.is_empty() {
            f.render_widget(
                Paragraph::new(format!(
                    "services: {err} — `R` retries · `:doctor` diagnoses"
                ))
                .style(Style::default().fg(Color::Red)),
                area,
            );
            m.board_area = Rect::default();
            m.viewport_top = 0;
            return;
        }
    }
    if m.svc_rows.is_empty() {
        let hint = if m.busy.is_some() {
            "loading services…"
        } else {
            "no provider services — `i` installs a package with a `provider:` block"
        };
        f.render_widget(Paragraph::new(hint).style(Style::default().fg(MUTED)), area);
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    let header = Row::new(vec!["PACKAGE", "SERVICE", "UNIT", "STATE", "EXEC"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = m
        .svc_rows
        .iter()
        .map(|r| {
            Row::new(vec![
                r.package.clone(),
                r.service.clone(),
                r.unit.clone(),
                r.state.clone().unwrap_or_else(|| "-".into()),
                r.exec.clone(),
            ])
        })
        .collect();
    let widths = [
        Constraint::Min(14),
        Constraint::Length(12),
        Constraint::Min(14),
        Constraint::Length(10),
        Constraint::Min(16),
    ];
    let title = if m.svc_stale || m.busy.is_some() {
        " services — refreshing… "
    } else {
        " services "
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .style(Style::default().fg(TEXT))
        .row_highlight_style(Style::default().bg(Color::Indexed(238)));
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(m.svc_sel));
    f.render_stateful_widget(table, area, &mut state);
    note_table_area(m, area, state.offset());
}

/// The Project pane — the project manifest's `deps:` as editable
/// rows; every other field renders read-only above the table. Writes
/// go through `Manifest::save` + `tamias.lock` sync on the worker.
fn draw_manifest(f: &mut Frame, m: &mut Model, area: Rect) {
    // Read-only header — path plus the manifest's scalar fields.
    let mut info: Vec<Line> = Vec::new();
    match (&m.project, &m.man_err) {
        (Some(manifest), _) => {
            info.push(Line::from(vec![
                Span::styled("manifest: ", Style::default().fg(ACCENT)),
                Span::raw(m.manifest_path.display().to_string()),
            ]));
            let mut scalars = Vec::new();
            if !manifest.name.is_empty() {
                scalars.push(format!("name {}", manifest.name));
            }
            if let Some(v) = &manifest.version {
                scalars.push(format!("v{v}"));
            }
            if let Some(d) = &manifest.description {
                scalars.push(d.clone());
            }
            if !scalars.is_empty() {
                info.push(Line::from(Span::styled(
                    scalars.join(" · "),
                    Style::default().fg(MUTED),
                )));
            }
            if !manifest.include.is_empty() {
                info.push(Line::from(vec![
                    Span::styled("include:  ", Style::default().fg(ACCENT)),
                    Span::raw(manifest.include.join(", ")),
                ]));
            }
            if manifest.provider.is_some() {
                info.push(Line::from(vec![
                    Span::styled("provider: ", Style::default().fg(ACCENT)),
                    Span::raw("declared (read-only here)"),
                ]));
            }
        }
        (None, Some(err)) => {
            info.push(Line::from(Span::styled(
                format!("{err} — `:manifest` retries once the file is fixed"),
                Style::default().fg(WARN),
            )));
        }
        (None, None) => {
            info.push(Line::from(Span::styled(
                "no project manifest — `a` adds a dep (writes tamias.yaml + tamias.lock)",
                Style::default().fg(MUTED),
            )));
        }
    }
    // Deps table gets the rest of the frame; the info strip yields on
    // short terminals.
    let info_h = if area.height > 14 {
        5.min(area.height / 3)
    } else {
        0
    };
    let (info_area, table_area) = if info_h > 0 {
        let chunks = Layout::vertical([Constraint::Length(info_h), Constraint::Min(0)]).split(area);
        (chunks[0], chunks[1])
    } else {
        (Rect::default(), area)
    };
    if info_h > 0 {
        f.render_widget(
            Paragraph::new(info).style(Style::default().fg(TEXT)),
            info_area,
        );
    }
    let deps: Vec<String> = m
        .project
        .as_ref()
        .map(|p| p.deps.clone())
        .unwrap_or_default();
    if deps.is_empty() {
        f.render_widget(
            Paragraph::new("no deps — `a` adds a ref").style(Style::default().fg(MUTED)),
            table_area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    let header =
        Row::new(vec!["#", "REF"]).style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = deps
        .iter()
        .enumerate()
        .map(|(i, d)| Row::new(vec![(i + 1).to_string(), d.clone()]))
        .collect();
    let table = Table::new(rows, [Constraint::Length(4), Constraint::Min(30)])
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" deps — {} ", m.manifest_path.display())),
        )
        .style(Style::default().fg(TEXT))
        .row_highlight_style(Style::default().bg(Color::Indexed(238)));
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(m.man_sel));
    f.render_stateful_widget(table, table_area, &mut state);
    note_table_area(m, table_area, state.offset());
}

/// The Publish pane — configured registries plus the pack/publish
/// entry points; results land in the op log like every other job.
/// The pane carries no cursor, so its body legitimately lists the
/// key shapes (with args) the terse status-bar hint abbreviates.
fn draw_author(f: &mut Frame, m: &mut Model, area: Rect) {
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(
            "publishing",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" — bundle pack/publish"),
    ])];
    lines.push(Line::from(""));
    if m.author_regs.is_empty() {
        lines.push(Line::from(Span::styled(
            "no registries configured — `registries:` in tamias.yml adds them · publish → default local root",
            Style::default().fg(MUTED),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "registries:",
            Style::default().fg(ACCENT),
        )));
        for (name, kind) in &m.author_regs {
            let label = if kind.is_empty() {
                name.clone()
            } else {
                format!("{name}  ({kind})")
            };
            lines.push(Line::from(Span::raw(format!("  {label}"))));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "p  pack <dir> [out.tgz]      P  publish <dir> [registry]",
        Style::default().fg(MUTED),
    )));
    lines.push(Line::from(Span::styled(
        "r  :registries report        R  reload registry list",
        Style::default().fg(MUTED),
    )));
    lines.push(Line::from(Span::styled(
        "publish asks for confirmation; progress streams to the log",
        Style::default().fg(MUTED),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(Block::default().borders(Borders::ALL).title(" publish ")),
        area,
    );
    m.board_area = Rect::default();
    m.viewport_top = 0;
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
            Paragraph::new(format!("store: {err} — `R` reloads · `:doctor` diagnoses"))
                .style(Style::default().fg(Color::Red)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    if m.entries.is_empty() {
        f.render_widget(
            Paragraph::new("no packages installed — `i` installs by ref")
                .style(Style::default().fg(MUTED)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    if vis.is_empty() {
        f.render_widget(
            Paragraph::new(format!(
                "no entries match /{} — `F` clears the filter",
                m.filter
            ))
            .style(Style::default().fg(MUTED)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }

    // RESOLVED and DIGEST collapse to one compact DIGEST column —
    // the fetch sha and full digests live on the package page's
    // provenance section; the table only needs the verify-relevant
    // short form.
    let header = Row::new(vec!["NAME", "TYPE", "SOURCE", "DIGEST", "SIZE"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = vis
        .iter()
        .map(|&i| {
            let r = &m.entries[i];
            let rec = &r.entry.record;
            // Name cell carries styled spans — the trust badge is
            // colored by tier, `[sealed]` stays muted.
            let mut name = vec![Span::raw(r.entry.label())];
            if let Some(badge) = trust_badge(rec.trust.as_deref()) {
                name.push(badge);
            }
            if r.sealed {
                name.push(Span::styled(" [sealed]", Style::default().fg(MUTED)));
            }
            Row::new(vec![
                Cell::from(Line::from(name)),
                Cell::from(rec.kind.clone().unwrap_or_else(|| "-".into())),
                Cell::from(rec.source_ref.clone()),
                Cell::from(short_sha(&rec.digest)),
                Cell::from(r.size.map_or("-".into(), human_size)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Min(18),
        Constraint::Length(12),
        Constraint::Min(14),
        Constraint::Length(13),
        Constraint::Length(9),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" installed "))
        .style(Style::default().fg(TEXT))
        .row_highlight_style(Style::default().bg(Color::Indexed(238)));
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(m.sel));
    f.render_stateful_widget(table, area, &mut state);
    note_table_area(m, area, state.offset());
}

/// A muted `[chip]` for the page header — trust tiers pass their own
/// [`trust_style`] color instead of this.
fn chip(text: &str) -> Span<'static> {
    Span::styled(format!(" [{text}]"), Style::default().fg(MUTED))
}

/// `── name ──` section rule inside the package page.
fn rule(name: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("── {name} ──"),
        Style::default().fg(ACCENT),
    ))
}

/// `key: value` line — the `key:` stays contiguous (tests and readers
/// grep for it) and pads to a fixed column so values line up down the
/// provenance/meta sections.
fn kv(key: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{:<11}", format!("{key}:")),
            Style::default().fg(ACCENT),
        ),
        Span::raw(value.into()),
    ])
}

/// Everything the package page renders, built from the [`Detail`]
/// variant — owned `'static` lines so the borrow on `m.detail` ends
/// before the frame's scroll offset is clamped. Layout follows the
/// npm package-page anatomy: bold label + chips, a ≤3-line wrapped
/// description (pinned strip), the `tamias install` command, then
/// `── section ──` rules ordered by user value — deps/services/keywords
/// + meta + versions — with provenance last.
///
/// `installed_ver` is the store's version label for the page's
/// `(name, scope)` when one is installed
/// ([`Model::installed_label_for`]) — Registry pages only; `None`
/// elsewhere.
///
/// The Registry page's `── versions ──` section is fed by the
/// background fetch [`Model::fetch_versions`] kicks off on open: a
/// muted `versions …` placeholder shows while it's in flight, the
/// section appears once the index read lands (index order, the page's
/// own entry marked `← current`), and a fetch failure degrades to a
/// muted line — never an error toast.
///
/// `versions_sel` is `Some(i)` while the versions-focus mode is
/// active ([`Model::versions_focused`]) — row `i` gets the table's
/// `Indexed(238)` row highlight plus an accent `›` marker, and the
/// footer hint switches to the focus keys.
///
/// The fifth return value is the body line index of the focused
/// version row (`None` unless focus is active and the list landed) —
/// [`draw_detail`] feeds it to [`scroll_to_show`] so the cursor row
/// stays on screen past one page of versions.
fn page_parts(
    detail: &Detail,
    registry_target: &str,
    installed_ver: Option<&str>,
    versions_sel: Option<usize>,
) -> (
    String,
    Line<'static>,
    Option<String>,
    Vec<Line<'static>>,
    Option<usize>,
) {
    let label = detail.label();
    // Header — bold label then muted/bracketed chips; the trust chip
    // is the one colored span (tier → [`trust_style`]).
    let mut head = vec![Span::styled(
        label.clone(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(kind) = detail.kind() {
        head.push(chip(kind));
    }
    if let Some(scope) = detail.scope() {
        head.push(chip(&format!("@{scope}")));
    }
    if let Some(license) = detail.license() {
        head.push(chip(license));
    }
    if let Some(tier) = detail.trust() {
        head.push(Span::styled(format!(" [{tier}]"), trust_style(&tier)));
    }
    if detail.sealed() {
        head.push(chip("sealed"));
    }
    let head = Line::from(head);
    let desc = detail.description().map(str::to_string);

    let mut body: Vec<Line> = Vec::new();
    // Body index of the versions-focus row — set by the Registry
    // arm below; `draw_detail` scrolls it into view.
    let mut versions_row: Option<usize> = None;
    // Install command — the ref an installed entry recorded, or the
    // registry entry's resolved install ref (error inline on failure).
    match detail {
        Detail::Installed { entry, .. } => body.push(Line::from(vec![
            Span::styled("install  ", Style::default().fg(ACCENT)),
            Span::styled("tamias install ", Style::default().fg(MUTED)),
            Span::raw(entry.record.source_ref.clone()),
        ])),
        Detail::Registry {
            install_ref,
            install_err,
            ..
        } => match (install_ref, install_err) {
            (Some(reference), _) => body.push(Line::from(vec![
                Span::styled("install  ", Style::default().fg(ACCENT)),
                Span::styled("tamias install ", Style::default().fg(MUTED)),
                Span::raw(reference.clone()),
            ])),
            (None, Some(err)) => body.push(Line::from(vec![
                Span::styled("install  ", Style::default().fg(ACCENT)),
                Span::styled("unavailable — ", Style::default().fg(WARN)),
                Span::raw(format!("{err} — `:registry <name>` picks another target")),
            ])),
            (None, None) => {}
        },
    }

    match detail {
        Detail::Installed {
            entry,
            manifest,
            manifest_err,
        } => {
            let rec = &entry.record;
            if let Some(m) = manifest {
                // deps — the manifest's direct dependency refs.
                if !m.deps.is_empty() {
                    body.push(Line::from(""));
                    body.push(rule("deps"));
                    for dep in &m.deps {
                        body.push(Line::from(Span::raw(format!("  {dep}"))));
                    }
                }
                // provider services — name + exec, the deploy surface.
                if let Some(provider) = &m.provider {
                    if !provider.services.is_empty() {
                        body.push(Line::from(""));
                        body.push(rule("services"));
                        for s in &provider.services {
                            body.push(Line::from(vec![
                                Span::raw(format!("  {}", s.name)),
                                Span::styled(format!("  {}", s.exec), Style::default().fg(MUTED)),
                            ]));
                        }
                    }
                }
                // meta — the manifest's display fields the chips
                // don't already carry.
                let mut meta: Vec<Line> = Vec::new();
                if !m.keywords.is_empty() {
                    meta.push(kv("keywords", m.keywords.join(", ")));
                }
                if let Some(homepage) = &m.homepage {
                    meta.push(kv("homepage", homepage.clone()));
                }
                if let Some(repository) = &m.repository {
                    meta.push(kv("repository", repository.clone()));
                }
                if !m.authors.is_empty() {
                    meta.push(kv("authors", m.authors.join(", ")));
                }
                if !m.include.is_empty() {
                    meta.push(kv("include", m.include.join(", ")));
                }
                if let Some(sealed) = &m.sealed {
                    meta.push(kv(
                        "sealed",
                        format!(
                            "{} — {} secret(s), ciphertext-only",
                            sealed.format,
                            sealed.files.len()
                        ),
                    ));
                }
                if !meta.is_empty() {
                    body.push(Line::from(""));
                    body.push(rule("meta"));
                    body.extend(meta);
                }
            } else if let Some(err) = manifest_err {
                body.push(Line::from(vec![
                    Span::styled("manifest ", Style::default().fg(WARN)),
                    Span::raw(format!("{err} — `x` removes it; reinstall repairs")),
                ]));
            }
            // provenance — last, per the package-page anatomy.
            body.push(Line::from(""));
            body.push(rule("provenance"));
            body.push(kv("source", rec.source_ref.clone()));
            body.push(kv("resolved", rec.resolved.clone()));
            body.push(kv("digest", rec.digest.clone()));
            // `installed_at` is unix seconds — render the same
            // relative label Home uses, not the raw epoch.
            body.push(kv(
                "installed",
                if rec.installed_at > 0 {
                    ago(rec.installed_at)
                } else {
                    "-".to_string()
                },
            ));
            body.push(kv("dir", entry.dir.display().to_string()));
            if let Some(signer) = &rec.signer {
                body.push(kv("signer", signer.clone()));
            }
            body.push(kv("trust", rec.trust.clone().unwrap_or_else(|| "-".into())));
        }
        Detail::Registry {
            entry,
            versions,
            versions_err,
            ..
        } => {
            // installed marker — the same (name, scope) is already in
            // the store, possibly at another version; sits right
            // under the install command.
            if let Some(ver) = installed_ver {
                let pkg = match &entry.scope {
                    Some(s) => format!("@{s}/{}", entry.name),
                    None => entry.name.clone(),
                };
                body.push(Line::from(vec![
                    Span::styled("installed  ", Style::default().fg(ACCENT)),
                    Span::styled(format!("{pkg}@{ver}"), Style::default().fg(MUTED)),
                ]));
            }
            // keywords — the index analogue of deps (search matches
            // them, so they double as discovery terms).
            if !entry.keywords.is_empty() {
                body.push(Line::from(vec![
                    Span::styled("keywords: ", Style::default().fg(ACCENT)),
                    Span::raw(entry.keywords.join(", ")),
                ]));
            }
            // meta — index-carried display fields (P-006).
            let mut meta: Vec<Line> = Vec::new();
            if let Some(homepage) = &entry.homepage {
                meta.push(kv("homepage", homepage.clone()));
            }
            if let Some(repository) = &entry.repository {
                meta.push(kv("repository", repository.clone()));
            }
            if !entry.authors.is_empty() {
                meta.push(kv("authors", entry.authors.join(", ")));
            }
            if !meta.is_empty() {
                body.push(Line::from(""));
                body.push(rule("meta"));
                body.extend(meta);
            }
            // versions — the background index fetch; pending shows a
            // subtle placeholder, the section renders once data lands
            // (a lone current version still lists — the header already
            // carries it, so no "no other versions" special-case).
            match (versions, versions_err) {
                (Some(list), _) => {
                    body.push(Line::from(""));
                    // Unfocused, the section advertises its `v` toggle
                    // inline — the footer hint alone is easy to miss.
                    if versions_sel.is_none() && !list.is_empty() {
                        body.push(Line::from(vec![
                            Span::styled("── versions ──", Style::default().fg(ACCENT)),
                            Span::styled("  · v picks a version", Style::default().fg(MUTED)),
                        ]));
                    } else {
                        body.push(rule("versions"));
                    }
                    if list.is_empty() {
                        body.push(Line::from(Span::styled(
                            "  no versions indexed",
                            Style::default().fg(MUTED),
                        )));
                    }
                    // The focused row's body index — `draw_detail`
                    // scrolls it into view while versions focus is on.
                    let rows_start = body.len();
                    versions_row = versions_sel
                        .filter(|&s| s < list.len())
                        .map(|s| rows_start + s);
                    for (i, v) in list.iter().enumerate() {
                        let ver = v.version.clone().unwrap_or_else(|| "-".into());
                        // `file` is the per-version blob path — the
                        // unique match even if version strings repeat.
                        let current = v.file == entry.file;
                        let line = if versions_sel == Some(i) {
                            let mut spans = vec![
                                Span::styled("› ", Style::default().fg(ACCENT)),
                                Span::raw(ver),
                            ];
                            if current {
                                spans
                                    .push(Span::styled("  ← current", Style::default().fg(ACCENT)));
                            }
                            Line::from(spans).style(Style::default().bg(Color::Indexed(238)))
                        } else {
                            let mut spans = vec![Span::raw(format!("  {ver}"))];
                            if current {
                                spans
                                    .push(Span::styled("  ← current", Style::default().fg(ACCENT)));
                            }
                            Line::from(spans)
                        };
                        body.push(line);
                    }
                }
                (None, Some(err)) => body.push(Line::from(vec![
                    Span::styled("versions ", Style::default().fg(ACCENT)),
                    Span::styled(
                        format!("— {err} — reopen the page to retry"),
                        Style::default().fg(WARN),
                    ),
                ])),
                (None, None) => body.push(Line::from(vec![
                    Span::styled("versions ", Style::default().fg(ACCENT)),
                    Span::styled("…", Style::default().fg(MUTED)),
                ])),
            }
            // provenance — index-side facts; the signature verifies
            // at install time, not here.
            body.push(Line::from(""));
            body.push(rule("provenance"));
            body.push(kv("registry", registry_target.to_string()));
            body.push(kv("file", entry.file.clone()));
            body.push(kv("digest", entry.digest.clone()));
            if let Some(publisher) = &entry.publisher {
                body.push(kv("publisher", publisher.clone()));
            }
            body.push(kv(
                "signature",
                if entry.signature.is_some() {
                    "signed".to_string()
                } else {
                    "unsigned".to_string()
                },
            ));
        }
    }
    // No trailing hint line — the status bar's `detail_hint` is the
    // single hint surface (a body-bottom copy would both duplicate
    // it and scroll away with the content).
    (label, head, desc, body, versions_row)
}

/// The package page's status-bar hint — `draw_status`'s right-side
/// hint while `detail` is open. `versions_focused` is the caller's
/// focus state ([`Model::versions_focused`]).
fn detail_hint(detail: &Detail, versions_focused: bool) -> &'static str {
    match detail {
        Detail::Installed { .. } => "esc back · j/k scroll · v/u/p/x/c/d act on the row",
        Detail::Registry { .. } if versions_focused => {
            "versions — j/k move · i install · v/esc back"
        }
        // Landed, non-empty sibling list — advertise the `v` toggle.
        Detail::Registry {
            versions: Some(list),
            ..
        } if !list.is_empty() => "esc back · j/k scroll · i install · v versions",
        Detail::Registry { .. } => "esc back · j/k scroll · i install",
    }
}

/// Scroll offset that keeps body line `row` inside a `view_h`-line
/// window — scrolls the minimum amount (up when the row is above the
/// window, down when below); the caller still clamps to `max_scroll`.
/// `view_h == 0` leaves the offset alone.
fn scroll_to_show(row: usize, scroll: usize, view_h: usize) -> usize {
    if view_h == 0 {
        return scroll;
    }
    if row < scroll {
        row
    } else if row >= scroll + view_h {
        row + 1 - view_h
    } else {
        scroll
    }
}

/// Greedy word-wrap line count for `text` at `width` columns,
/// capped at `max` — the package page's pinned strips measure
/// themselves with this (`Paragraph::line_count` is unstable in
/// ratatui 0.30). Overlong words count as filling their line; the
/// estimate only sizes a strip, never clips content.
fn wrapped_lines(text: &str, width: u16, max: usize) -> usize {
    let width = (width as usize).max(1);
    let mut lines = 1usize;
    let mut col = 0usize;
    for word in text.split_whitespace() {
        let ww = Span::raw(word).width();
        if col == 0 {
            col = ww;
        } else if col + 1 + ww <= width {
            col += 1 + ww;
        } else {
            lines += 1;
            col = ww;
        }
        if lines >= max {
            return max;
        }
    }
    lines.min(max)
}

/// The `Enter` package page — npm-inspired anatomy inside the
/// bordered block. The header and the (≤3-line, wrapped) description
/// stay pinned; the body — install command, sections, provenance —
/// scrolls with `j`/`k` via `Model::detail_scroll`.
fn draw_detail(f: &mut Frame, m: &mut Model, area: Rect) {
    let Some(detail) = m.detail.as_ref() else {
        return;
    };
    // Registry pages mark an existing install of the same
    // (name, scope) — any version counts.
    let installed_ver = match detail {
        Detail::Registry { entry, .. } => {
            m.installed_label_for(&entry.name, entry.scope.as_deref())
        }
        _ => None,
    };
    let (title, head, desc, body, versions_row) = page_parts(
        detail,
        &m.reg_target_label,
        installed_ver.as_deref(),
        m.versions_focused().then_some(m.versions_sel),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    // Pinned strips — the header wraps to at most 3 lines, the
    // description to at most 3 (overflow clips; the body below still
    // scrolls).
    let header_text: String = head.spans.iter().map(|s| s.content.as_ref()).collect();
    let header_h = wrapped_lines(&header_text, inner.width, 3).min(inner.height as usize) as u16;
    let desc_h = match &desc {
        Some(d) => wrapped_lines(d, inner.width, 3)
            .min(inner.height.saturating_sub(header_h) as usize) as u16,
        None => 0,
    };
    let body_h = inner.height.saturating_sub(header_h).saturating_sub(desc_h);
    let header_area = Rect {
        height: header_h,
        ..inner
    };
    let desc_area = Rect {
        y: inner.y + header_h,
        height: desc_h,
        ..inner
    };
    let body_area = Rect {
        y: inner.y + header_h + desc_h,
        height: body_h,
        ..inner
    };
    f.render_widget(
        Paragraph::new(head)
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: true }),
        header_area,
    );
    if let Some(d) = &desc {
        f.render_widget(
            Paragraph::new(d.as_str())
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(MUTED)),
            desc_area,
        );
    }
    // Versions focus keeps its cursor row on screen — the body is a
    // scrolling Paragraph, so aim `detail_scroll` at the row before
    // the usual max_scroll clamp (body-line approximation, same
    // posture as the clamp itself under wrapped lines).
    if let Some(row) = versions_row {
        m.detail_scroll = scroll_to_show(row, m.detail_scroll, body_area.height as usize);
    }
    // Clamp the scroll offset to the body, then render. The max is
    // recorded for `G` — the model's jump-to-bottom target.
    let max_scroll = body.len().saturating_sub(body_area.height as usize);
    m.detail_scroll = m.detail_scroll.min(max_scroll);
    m.detail_max_scroll = max_scroll;
    f.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: false })
            .scroll((m.detail_scroll as u16, 0)),
        body_area,
    );
    m.board_area = Rect::default();
    m.viewport_top = 0;
}

/// The Registry pane — search results table plus the target/query
/// context in the block title.
fn draw_registry(f: &mut Frame, m: &mut Model, area: Rect) {
    if let Some(err) = &m.reg_err {
        f.render_widget(
            Paragraph::new(format!(
                "registry: {err} — `:registry <name>` switches target · `:doctor` diagnoses"
            ))
            .style(Style::default().fg(Color::Red)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    if m.reg_query.trim().is_empty() {
        f.render_widget(
            Paragraph::new(
                "registry search — `/` types a query · `:registry <name>` picks the target",
            )
            .style(Style::default().fg(MUTED)),
            area,
        );
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    if m.reg_results.is_empty() {
        let text = if m.busy.is_some() {
            format!("searching… — /{}", m.reg_query)
        } else {
            format!(
                "no results for /{} — `/` edits the query · `:registry <name>` switches target",
                m.reg_query
            )
        };
        f.render_widget(Paragraph::new(text).style(Style::default().fg(MUTED)), area);
        m.board_area = Rect::default();
        m.viewport_top = 0;
        return;
    }
    // NAME's label already carries `@scope/name@version` — SCOPE and
    // DIGEST columns were pure noise (scope prefixes the label where
    // truncation can't reach it; the digest lives on the package
    // page's provenance). VERSION stays a column so the version
    // survives even when a long name pushes the label's `@ver` tail
    // off-screen.
    //
    // LICENSE drops out below ~72 cols: the five Min/Length minimums
    // (62 + column spacing) would squeeze DESCRIPTION to nothing, and
    // the license still shows on the package page.
    let license_col = area.width >= 72;
    let mut head = vec!["NAME", "TYPE", "VERSION"];
    if license_col {
        head.push("LICENSE");
    }
    head.push("DESCRIPTION");
    let header = Row::new(head).style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = m
        .reg_results
        .iter()
        .enumerate()
        .map(|(i, e)| {
            // Name cell carries styled spans — `[signed]` green for
            // signed index entries, `· installed <ver>` muted when
            // the store already has this (name, scope) at any version,
            // `· N versions` muted when the row collapsed siblings.
            let mut name = vec![Span::raw(e.label())];
            if e.signature.is_some() {
                name.push(Span::styled(" [signed]", trust_style("signed")));
            }
            if let Some(ver) = m.installed_label_for(&e.name, e.scope.as_deref()) {
                name.push(Span::styled(
                    format!(" · installed {ver}"),
                    Style::default().fg(MUTED),
                ));
            }
            let versions = m.reg_version_counts.get(i).copied().unwrap_or(1);
            if versions > 1 {
                name.push(Span::styled(
                    format!(" · {versions} versions"),
                    Style::default().fg(MUTED),
                ));
            }
            let mut cells = vec![
                Cell::from(Line::from(name)),
                Cell::from(e.kind.clone().unwrap_or_else(|| "-".into())),
                Cell::from(e.version.clone().unwrap_or_else(|| "-".into())),
            ];
            if license_col {
                cells.push(Cell::from(e.license.clone().unwrap_or_else(|| "-".into())));
            }
            cells.push(Cell::from(
                e.description.clone().unwrap_or_else(|| "-".into()),
            ));
            Row::new(cells)
        })
        .collect();
    // DESCRIPTION is the flexible column — it absorbs leftover width
    // and clips on narrow terminals.
    let mut widths = vec![
        Constraint::Min(18),
        Constraint::Length(10),
        Constraint::Length(9),
    ];
    if license_col {
        widths.push(Constraint::Min(11));
    }
    widths.push(Constraint::Min(14));
    let title = format!(" registry — {} · /{} ", m.reg_target_label, m.reg_query);
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .style(Style::default().fg(TEXT))
        .row_highlight_style(Style::default().bg(Color::Indexed(238)));
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(m.reg_sel));
    f.render_stateful_widget(table, area, &mut state);
    note_table_area(m, area, state.offset());
}

/// Style for one op-log line — errors red, warnings amber, the rest
/// muted.
fn log_style(line: &str) -> Style {
    if line.starts_with("error") || line.starts_with("!!") {
        Style::default().fg(Color::Red)
    } else if line.starts_with("warning") {
        Style::default().fg(WARN)
    } else {
        Style::default().fg(MUTED)
    }
}

/// The scrollable op-log body — the full retained history (up to
/// `LOG_CAP` lines), newest at the bottom. Reports the text height
/// into `log_view_h` (so `PgUp`/`PgDn` page by what's on screen and
/// `open_log` aims the tail) and the block rect into `log_area` (for
/// mouse hit-tests). Shared by the right-docked sidebar and the
/// narrow-terminal centered overlay.
fn draw_log_view(f: &mut Frame, m: &mut Model, area: Rect) {
    m.log_area = area;
    m.log_view_h = area.height.saturating_sub(2) as usize;
    let max_scroll = m.log.len().saturating_sub(m.log_view_h);
    m.log_scroll = m.log_scroll.min(max_scroll);
    let lines: Vec<Line> = if m.log.is_empty() {
        vec![Line::from(Span::styled(
            " no operations yet — installs, `:` commands, and jobs log here",
            Style::default().fg(MUTED),
        ))]
    } else {
        m.log
            .iter()
            .map(|l| Line::from(Span::styled(format!(" {l}"), log_style(l))))
            .collect()
    };
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" log — {} lines ", m.log.len()))
                    .title_bottom(
                        Line::from(Span::styled(
                            " j/k scroll · PgUp/PgDn page · g/G ends · q/esc/L close ",
                            Style::default().fg(MUTED),
                        ))
                        .centered(),
                    ),
            )
            .style(Style::default().fg(Color::White))
            .scroll((m.log_scroll as u16, 0)),
        area,
    );
}

/// The narrow-terminal fallback for `L`/`:log` — the same scrollable
/// body centered over a cleared frame, used when the frame is too
/// narrow (<50 cols) for a right sidebar to leave the pane usable.
/// Key handling lives in `Model::key_log`.
fn draw_log_overlay(f: &mut Frame, m: &mut Model, area: Rect) {
    let w = (area.width * 4 / 5).clamp(30, area.width.max(30));
    let h = (area.height * 4 / 5).clamp(5, area.height.max(5));
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect::new(x, y, w, h);
    f.render_widget(Clear, rect);
    draw_log_view(f, m, rect);
}

fn draw_status(f: &mut Frame, m: &Model, area: Rect) {
    let left = if let Some(busy) = &m.busy {
        let frame = SPINNER[(m.tick_n as usize) % SPINNER.len()];
        // Busy doesn't swallow the status toast — a "busy — still
        // running" refusal or a finished job's summary stays visible
        // next to the spinner (`fit_width` truncates the tail).
        if m.status.is_empty() {
            format!("{frame} {busy}")
        } else {
            format!("{frame} {busy} · {}", m.status)
        }
    } else if !m.status.is_empty() {
        m.status.clone()
    } else {
        match m.pane {
            Pane::Home => format!("home — {} installed", m.entries.len()),
            Pane::Installed => {
                if m.detail.is_some() {
                    "package — esc back".to_string()
                } else if m.filter.is_empty() {
                    format!("{} installed", m.entries.len())
                } else {
                    format!("{}/{} · /{}", m.visible().len(), m.entries.len(), m.filter)
                }
            }
            Pane::Registry => {
                if m.detail.is_some() {
                    "package — esc back".to_string()
                } else if m.reg_query.is_empty() {
                    "registry — / search · :registry <name> · i install".to_string()
                } else {
                    // `sel+1/len` — the conventional current/total.
                    format!(
                        "{}/{} · /{} · {}",
                        m.reg_sel.saturating_add(1),
                        m.reg_results.len(),
                        m.reg_query,
                        m.reg_target_label
                    )
                }
            }
            Pane::Services => {
                if m.svc_rows.is_empty() {
                    "services — s/S start/stop · ! restart · e/E enable/disable".to_string()
                } else {
                    format!(
                        "{} service row(s){}",
                        m.svc_rows.len(),
                        if m.svc_stale || m.busy.is_some() {
                            " · refreshing"
                        } else {
                            ""
                        }
                    )
                }
            }
            Pane::Manifest => match &m.project {
                Some(p) => format!(
                    "project {} — {} dep(s)",
                    m.manifest_path.display(),
                    p.deps.len()
                ),
                None => format!("project {} — no manifest", m.manifest_path.display()),
            },
            Pane::Author => "publish — p pack · P publish · r registries".to_string(),
        }
    };
    let style = if m.status_err {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(MUTED)
    };
    // The package page's hint wins over the pane's while `detail` is
    // open; either way the hint yields to the left status text —
    // truncated with `…` to whatever width is left.
    let hint = if let Some(d) = &m.detail {
        detail_hint(d, m.versions_focused())
    } else {
        match m.pane {
            Pane::Home => {
                if m.entries.is_empty() {
                    "/ search registry · i install · : cmds · q quit"
                } else {
                    "j/k recent · Enter open · / search · i install · : cmds · q quit"
                }
            }
            Pane::Installed => {
                "j/k · Enter page · v verify · u update · p pin · x remove · i install · c clean · d doctor · / filter · : cmds · q quit"
            }
            Pane::Registry => "j/k · Enter page · i install · / query · : cmds · q quit",
            Pane::Services => {
                if m.stream_child.is_some() {
                    "streaming — x kill · q quit"
                } else {
                    "j/k · s/S start/stop · ! restart · e/E enable/disable · d deploy · x uninstall · t/T logs · n run · R refresh · : cmds · q quit"
                }
            }
            Pane::Manifest => {
                "j/k · a add dep · d/x remove · e edit ref · s sync · S sync-lock · R reload · : cmds · q quit"
            }
            Pane::Author => "p pack · P publish · r registries · R reload · : cmds · q quit",
        }
    };
    // `L log` rides every pane's hint — the overlay works from any
    // mode, so the affordance is global (and yields to `fit_width`
    // like the rest of the hint).
    let hint = format!("{hint} · L log");
    let left_w = Line::raw(left.as_str()).width();
    let hint = fit_width(&hint, (area.width as usize).saturating_sub(left_w + 2));
    let spans = vec![
        Span::styled(left, style),
        Span::raw("  "),
        Span::styled(hint, Style::default().fg(MUTED)),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `s` clipped to `max` display columns with a trailing `…` — the
/// status bar's right-side hint yields to the left status text.
fn fit_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if Line::raw(s).width() <= max {
        return s.to_string();
    }
    let budget = max - 1; // room for the ellipsis
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = Span::raw(ch.to_string()).width();
        if w + cw > budget {
            break;
        }
        w += cw;
        out.push(ch);
    }
    out.push('…');
    out
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
                PromptKind::AddDep => "dep+ > ",
                PromptKind::SetDep { .. } => "dep→ > ",
                PromptKind::DeployConfig { .. } => "deploy cfg > ",
                PromptKind::PackSpec => "pack dir [out] > ",
                PromptKind::PublishSpec => "publish dir [registry] > ",
            };
            (l, m.prompt_buf.as_str())
        }
        _ => return,
    };
    let y = area.bottom().saturating_sub(2).max(area.y);
    // `:` mode gets a candidates strip above the input line — the
    // Tab-selected entry highlighted, the rest dimmed.
    if matches!(m.mode, Mode::Command) && !m.cmd_completions.is_empty() {
        let cy = y.saturating_sub(1);
        if cy >= area.y {
            let sel = (m.cmd_sel != usize::MAX).then(|| m.cmd_sel.min(m.cmd_completions.len() - 1));
            let mut spans: Vec<Span> = Vec::new();
            for (i, c) in m.cmd_completions.iter().take(14).enumerate() {
                if i > 0 {
                    spans.push(Span::raw("  "));
                }
                let shown = c.insert.trim_end();
                let style = if sel == Some(i) {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                };
                spans.push(Span::styled(shown.to_string(), style));
                // The hint rides dimmed after the insert — capped so
                // one verbose candidate can't eat the strip (the
                // paragraph clips at the pane edge regardless).
                let hint = c.hint.trim();
                if !hint.is_empty() {
                    let hint: String = hint.chars().take(48).collect();
                    spans.push(Span::styled(
                        format!(" — {hint}"),
                        Style::default().fg(MUTED),
                    ));
                }
            }
            if m.cmd_completions.len() > 14 {
                spans.push(Span::styled(
                    format!("  +{} more", m.cmd_completions.len() - 14),
                    Style::default().fg(MUTED),
                ));
            }
            let strip = Rect {
                x: area.x,
                y: cy,
                width: area.width,
                height: 1,
            };
            f.render_widget(Clear, strip);
            f.render_widget(Paragraph::new(Line::from(spans)), strip);
        }
    }
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    };
    // Clear first — the line overlays the pane's last row and stale
    // cell contents would bleed through where the text is shorter.
    f.render_widget(Clear, rect);
    let spans = vec![
        Span::styled(label.to_string(), Style::default().fg(ACCENT)),
        Span::raw(text.to_string()),
        Span::styled("█", Style::default().fg(ACCENT)),
    ];
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().fg(TEXT)),
        rect,
    );
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
    // `min(area.width)` — the 30-col floor must yield on terminals
    // narrower than the modal's minimum or the rect overflows.
    let w = ((area.width * 3 / 5).clamp(30, 72)).min(area.width);
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
    // Static text — keep it honest: pane names match `Pane::label`,
    // and `q`/`esc` close an open package page before they quit.
    let text = "\
keys — installed pane

  j/k, ↑/↓      move selection        g/G    top/bottom
  Enter         package page          esc/←   back / close page
  v             verify (digest)       u       update selected
  U             update all            p       pin → new ref
  x             remove (confirm)      c/C     cache clean >30d / all
  d             doctor summary        i       install by ref
  l/Tab, 1-6    switch pane — closes an open page too
  h/←           back — closes the page first, then prev pane
  /             filter installed      F       clear filter
  :             command line          Tab    complete · esc leave
  L/:log       op-log sidebar         j/k    scroll · g/G ends · q close
  R             reload store          ?       this help
  q             quit — closes an open page first · C-c quits anywhere

home pane:    landing view — wordmark · stats · recently installed ·
              j/k + g/G walk the recents · Enter opens the package
              page · / jumps to registry search · i install by ref

package page:  npm-style page — header chips · description · install
               command · deps/services/meta · provenance last ·
               j/k scroll · g/G jump ends · wheel scrolls ·
               esc/q/h/← close (back to where Enter was pressed) ·
               l/Tab/1-6 switch panes and close · registry pages: v
               focuses versions — j/k/g/G pick · i installs that
               version · `installed <pkg>@<ver>` line when already
               in the store

registry pane:  / search-as-you-type (debounced) · Enter package page ·
                i install result · :registry <name> target · :search <q>
                results mark [signed] · installed <ver> · N versions

services pane:  provider services + unit state · s/S start/stop ·
                ! restart · e/E enable/disable · d deploy · x uninstall
                t/T journal tail/follow · n foreground run (x kills)
                Enter opens the package's page · R refresh

project pane:   project manifest deps: — a add · d/x remove · e new ref
                (writes tamias.yaml + tamias.lock like `tamias add`)
                s / :sync reconciles deps: against the store + lock
                S / :sync --from-lock installs tamias.lock verbatim
                R reloads the manifest

publish pane:   p bundle pack · P publish (confirm) · r registries ·
                R reloads the registry list

commands:  :q  :r/:reload  :help  :log  :install <ref>  :remove <name>
           :uninstall <name> (provider teardown)  :verify <name>
           :update [name]  :pin <name> <ref>
           :search <q>  :registry <name>  :clean [all]  :doctor
           :services  :service <verb> <unit>  :run <pkg> [svc]
           :logs <pkg|unit> [-f]  :deploy <name>  :manifest
           :pack <dir> [out]  :publish <dir> [registry]  :registries
           :sync [--from-lock]
           (: line editing — arrows · Home/End · Ctrl-A/E/U/W ·
            Tab completes · Up/Down history)
mouse:     wheel scrolls (the package page when open) · click row
           selects · click tab switches (version rows aren't
           click-selectable — the page is one scrolling block)";
    let width = area.width.min(72) as usize;
    let w = (width as u16 + 2).min(area.width);
    // Height counts *wrapped* lines — at narrow widths the static
    // text wraps past a raw `lines().count()` estimate and the box
    // clipped its own bottom border off-screen.
    let inner_w = w.saturating_sub(2).max(1);
    let h = (text
        .lines()
        .map(|l| wrapped_lines(l, inner_w, usize::MAX))
        .sum::<usize>()
        + 2)
    .min(area.height as usize) as u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect::new(x, y, w, h);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(" help "))
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::{Path, PathBuf};
    use tamias_core::{Manifest, Provider, ProviderService, Sealed, StoreEntry, StoreRecord};
    use tamias_provider::ServiceRow;
    use tamias_registry::RegistryEntry;

    /// A registry index entry — `w@<ver>` unscoped.
    fn reg_entry(version: &str) -> RegistryEntry {
        RegistryEntry {
            name: "w".into(),
            scope: None,
            version: Some(version.into()),
            kind: None,
            description: None,
            keywords: Vec::new(),
            homepage: None,
            repository: None,
            license: None,
            authors: Vec::new(),
            digest: format!("sha256:{version}"),
            signature: None,
            publisher: None,
            file: format!("w/{version}.tgz"),
        }
    }

    /// Flatten a `page_parts` body to plain text for assertions.
    fn body_text(detail: &Detail) -> String {
        body_text_installed(detail, None)
    }

    /// `body_text` with an installed-version marker for the page.
    fn body_text_installed(detail: &Detail, installed_ver: Option<&str>) -> String {
        body_text_full(detail, installed_ver, None)
    }

    /// `body_text` with the versions-focus cursor (`Some(i)` marks
    /// row `i` and switches the footer hint).
    fn body_text_full(
        detail: &Detail,
        installed_ver: Option<&str>,
        versions_sel: Option<usize>,
    ) -> String {
        let (_, _, _, body, _) = page_parts(detail, "default", installed_ver, versions_sel);
        body.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The versions section's three states: pending placeholder, the
    /// rendered list with the page's own entry marked, and the
    /// degraded error line.
    #[test]
    fn registry_page_versions_states() {
        let page = reg_entry("1.0.0");
        let detail = |versions, versions_err| Detail::Registry {
            entry: page.clone(),
            install_ref: None,
            install_err: None,
            versions,
            versions_err,
        };
        // Pending — the subtle placeholder, no section rule yet.
        let pending = body_text(&detail(None, None));
        assert!(pending.contains("versions …"));
        assert!(!pending.contains("── versions ──"));
        // Landed — the rule, every version in index order, `← current`
        // on the page's own entry (matched by blob `file`), and the
        // inline `v` affordance while unfocused. Key hints live in the
        // status bar (`detail_hint`), not the scrolling body.
        let ready = body_text(&detail(Some(vec![reg_entry("0.9.0"), page.clone()]), None));
        assert!(ready.contains("── versions ──"));
        assert!(ready.contains("· v picks a version"));
        assert!(!ready.contains("esc back · j/k scroll"));
        assert!(ready.contains("  0.9.0"));
        assert!(ready.contains("  1.0.0  ← current"));
        // Failed — the error line, still no rule.
        let failed = body_text(&detail(None, Some("index gone".into())));
        assert!(failed.contains("— index gone"));
        assert!(!failed.contains("── versions ──"));
        // Empty index result still renders the section's empty note.
        let empty = body_text(&detail(Some(Vec::new()), None));
        assert!(empty.contains("── versions ──"));
        assert!(empty.contains("no versions indexed"));
    }

    // --- frame-level coverage: every pane, mode, modal, and the
    //     chrome budget branches, rendered into a TestBackend ---

    /// Roots under `tmp` — nothing needs to exist for render tests.
    fn tctx(tmp: &Path) -> crate::Ctx {
        crate::Ctx::new(
            tmp.join("store"),
            tmp.join("cache"),
            tmp.join("registry"),
            Some(tmp.join("cfg.yml")),
        )
    }

    fn model(tmp: &Path) -> Model {
        Model::open(&tctx(tmp)).unwrap()
    }

    /// A store record — `name@1.0.0`, `sha256:`-prefixed digests.
    fn rec(name: &str) -> StoreRecord {
        StoreRecord {
            name: name.into(),
            scope: None,
            version: Some("1.0.0".into()),
            kind: Some("files".into()),
            description: Some(format!("the {name} package")),
            keywords: vec!["demo".into()],
            homepage: None,
            repository: None,
            license: Some("MIT".into()),
            authors: Vec::new(),
            source_ref: format!("./{name}"),
            resolved: "sha256:0123456789abcdef".into(),
            digest: "sha256:fedcba9876543210".into(),
            signature: None,
            signer: None,
            trust: Some("signed".into()),
            installed_at: 1_700_000_000,
        }
    }

    /// A store entry without touching disk — `dir` is display-only.
    fn entry(name: &str) -> StoreEntry {
        StoreEntry {
            dir: PathBuf::from("/store").join(name),
            record: rec(name),
        }
    }

    /// An installed-pane row.
    fn row(name: &str, sealed: bool) -> crate::app::Row {
        crate::app::Row {
            entry: entry(name),
            size: Some(2048),
            sealed,
        }
    }

    /// Draw one frame and flatten the buffer to text (trailing spaces
    /// trimmed per line so assertions read naturally).
    fn screen(m: &mut Model, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, m)).unwrap();
        let buf = term.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            let mut line = String::new();
            for x in 0..area.width {
                line.push_str(buf[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// A rich payload manifest for the Installed detail page — deps,
    /// provider services, and every meta field.
    fn full_manifest() -> Manifest {
        Manifest {
            name: "w".into(),
            description: Some("a widget".into()),
            keywords: vec!["ui".into(), "demo".into()],
            homepage: Some("https://example.com".into()),
            repository: Some("git@example.com:w.git".into()),
            authors: vec!["ann".into()],
            kind: Some("demo-type".into()),
            include: vec!["base-pkg".into()],
            deps: vec!["./dep-a".into(), "other@2.0.0".into()],
            provider: Some(Provider {
                services: vec![ProviderService {
                    name: "web".into(),
                    exec: "/bin/serve".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A sealed-pack manifest for the meta `sealed` kv line.
    fn sealed_manifest() -> Manifest {
        Manifest {
            name: "s".into(),
            sealed: Some(Sealed {
                format: "age/v1".into(),
                recipients: "recipients.txt".into(),
                files: [("db".to_string(), "db.age".to_string())]
                    .into_iter()
                    .collect(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn pure_helpers() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(human_size(2 * 1024u64.pow(4)), "2.0 TiB");
        assert_eq!(short_sha("sha256:0123456789abcdef"), "0123456789ab");
        assert_eq!(short_sha("deadbeef"), "deadbeef");
        // every trust tier maps somewhere; unknown falls to muted
        for t in ["official", "signed", "bad", "pinned", "unpinned", "weird"] {
            let _ = trust_style(t);
        }
        assert!(trust_badge(None).is_none());
        assert!(trust_badge(Some("signed")).is_some());
        // chrome budget: tabs ≥3 rows, status ≥5
        assert_eq!(chrome(2), (false, false));
        assert_eq!(chrome(3), (true, false));
        assert_eq!(chrome(5), (true, true));
        // fit_width: zero budget, fits, and ellipsis truncation
        assert_eq!(fit_width("abc", 0), "");
        assert_eq!(fit_width("abc", 10), "abc");
        let cut = fit_width("abcdefghijklmnopqrstuvwxyz", 10);
        assert!(cut.ends_with('…') && cut.chars().count() == 10);
        // wrapped_lines: single line, wrap, max cap, degenerate width
        assert_eq!(wrapped_lines("", 10, 3), 1);
        assert_eq!(wrapped_lines("a b c d e f", 5, 10), 2);
        let long = "x ".repeat(50);
        assert_eq!(wrapped_lines(&long, 10, 3), 3);
        assert_eq!(wrapped_lines("one two", 0, 2), 2);
    }

    #[test]
    fn tab_bar_hit_testing() {
        // wide enough for full labels
        let wide = tab_labels(200);
        assert_eq!(wide.len(), 6);
        assert!(wide[1].contains("INSTALLED"));
        assert_eq!(tab_hit(200, 0), None); // leading space
        assert_eq!(tab_hit(200, 1), Some(0));
        // narrow → initials (PROJECT/PUBLISH widen to PR/PU),
        // hit-test still resolves
        let narrow = tab_labels(20);
        assert!(narrow.iter().all(|l| l.chars().count() <= 2));
        assert!(narrow.contains(&"PR".to_string()));
        assert!(narrow.contains(&"PU".to_string()));
        assert!(tab_hit(20, 30).is_none());
        let _ = tab_hit(20, 2);
    }

    #[test]
    fn empty_pane_states() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        // Installed — no packages (default pane is Home)
        m.pane = Pane::Installed;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("no packages installed"), "{s}");
        assert!(s.contains("INSTALLED"), "{s}");
        // Registry — no query yet
        m.pane = Pane::Registry;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("registry search"), "{s}");
        // Services — no rows, not busy
        m.pane = Pane::Services;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("no provider services"), "{s}");
        // Manifest — no project (open() discovers ./tamias.yml from
        // the repo root, so clear it explicitly for the empty state)
        m.pane = Pane::Manifest;
        m.project = None;
        m.man_err = None;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("no project manifest"), "{s}");
        // Author (PUBLISH) — no registries
        m.pane = Pane::Author;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("publishing"), "{s}");
        assert!(s.contains("no registries configured"), "{s}");
    }

    #[test]
    fn installed_pane_rows_states() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        m.pane = Pane::Installed;
        m.entries = vec![row("alpha", false), row("beta", true)];
        m.entries[1].entry.record.trust = Some("bad".into());
        m.entries[1].entry.record.kind = None;
        let s = screen(&mut m, 100, 24);
        assert!(s.contains("alpha@1.0.0"), "{s}");
        // the name cell clips `[sealed]` on narrow tables — the badge
        // prefix is enough proof it rendered
        assert!(s.contains("[seal"), "{s}");
        assert!(s.contains("[bad]"), "{s}");
        assert!(s.contains("2.0 KiB"), "{s}");
        // filter with no matches
        m.filter = "zzz".into();
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("no entries match"), "{s}");
        // load error wins over everything
        m.load_err = Some("store gone".into());
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("store: store gone"), "{s}");
    }

    #[test]
    fn registry_pane_states() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        m.pane = Pane::Registry;
        m.reg_err = Some("index 404".into());
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("registry: index 404"), "{s}");
        m.reg_err = None;
        m.reg_query = "w".into();
        // empty results, not busy vs busy
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("no results"), "{s}");
        m.busy = Some("searching".into());
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("searching…"), "{s}");
        // populated table — plus a sparse entry exercising the "-"
        // fallback cells (no scope/kind/version/license/description)
        m.busy = None;
        let mut sparse = reg_entry("0.5.0");
        sparse.version = None;
        m.reg_results = vec![reg_entry("1.0.0"), sparse];
        let s = screen(&mut m, 100, 24);
        assert!(s.contains("w@1.0.0"), "{s}");
        assert!(s.contains("registry — default · /w"), "{s}");
    }

    #[test]
    fn services_pane_states() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        m.pane = Pane::Services;
        // error with no rows → red error paragraph
        m.svc_err = Some("systemctl missing".into());
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("services: systemctl missing"), "{s}");
        // rows present — error tolerated, table renders
        m.svc_rows = vec![ServiceRow {
            package: "pkg@1.0.0".into(),
            service: "web".into(),
            unit: "pkg-web".into(),
            state: Some("active".into()),
            exec: "/bin/serve --port 9".into(),
        }];
        m.svc_err = None;
        let s = screen(&mut m, 100, 24);
        assert!(s.contains("pkg-web"), "{s}");
        assert!(s.contains("active"), "{s}");
        // busy → refreshing title + the empty-rows loading hint
        m.svc_rows.clear();
        m.busy = Some("services refresh".into());
        let s = screen(&mut m, 100, 24);
        assert!(s.contains("loading services…"), "{s}");
        // rows back + busy → the refreshing title variant
        m.svc_rows = vec![ServiceRow {
            package: "pkg@1.0.0".into(),
            service: "web".into(),
            unit: "pkg-web".into(),
            state: Some("active".into()),
            exec: "/bin/serve --port 9".into(),
        }];
        let s = screen(&mut m, 100, 24);
        assert!(s.contains("refreshing"), "{s}");
        m.busy = None;
        m.svc_stale = true;
        let s = screen(&mut m, 100, 24);
        assert!(s.contains("refreshing"), "{s}");
    }

    #[test]
    fn manifest_pane_states() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        m.pane = Pane::Manifest;
        m.manifest_path = PathBuf::from("/proj/tamias.yaml");
        // error state — project must be None for the err arm to win
        // (open() discovered the repo's own tamias.yml)
        m.project = None;
        m.man_err = Some("bad yaml".into());
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("bad yaml"), "{s}");
        // manifest with deps → table; version/description/include/
        // provider all land in the read-only info strip
        let mut man = full_manifest();
        man.name = "proj".into();
        man.version = Some("0.2.0".into());
        m.project = Some(man);
        m.man_err = None;
        let s = screen(&mut m, 100, 30);
        assert!(s.contains("manifest:"), "{s}");
        assert!(s.contains("./dep-a"), "{s}");
        // short terminal → info strip yields, deps-only hint
        let s = screen(&mut m, 80, 10);
        assert!(s.contains("deps"), "{s}");
        // project with zero deps → empty-deps hint; then no project
        // at all → the create hint
        m.project = Some(Manifest::default());
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("no deps"), "{s}");
        m.project = None;
        m.man_err = None;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("no project manifest"), "{s}");
    }

    #[test]
    fn author_pane_registries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        m.pane = Pane::Author;
        m.author_regs = vec![
            ("local".into(), String::new()),
            ("cf".into(), "cf-worker".into()),
        ];
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("local"), "{s}");
        assert!(s.contains("cf  (cf-worker)"), "{s}");
    }

    #[test]
    fn detail_pages_render() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        // Installed detail — full manifest: deps/services/meta/
        // provenance all land; description pins under the header.
        m.pane = Pane::Installed;
        m.detail = Some(Detail::Installed {
            entry: entry("w"),
            manifest: Some(Box::new(full_manifest())),
            manifest_err: None,
        });
        let s = screen(&mut m, 100, 30);
        assert!(s.contains("── deps ──"), "{s}");
        assert!(s.contains("── services ──"), "{s}");
        assert!(s.contains("── meta ──"), "{s}");
        assert!(s.contains("── provenance ──"), "{s}");
        assert!(s.contains("tamias install ./w"), "{s}");
        assert!(s.contains("homepage:"), "{s}");
        assert!(s.contains("include:"), "{s}");
        // sealed meta line
        m.detail = Some(Detail::Installed {
            entry: entry("s"),
            manifest: Some(Box::new(sealed_manifest())),
            manifest_err: None,
        });
        let s = screen(&mut m, 100, 30);
        assert!(s.contains("sealed"), "{s}");
        assert!(s.contains("ciphertext-only"), "{s}");
        // manifest error line
        m.detail = Some(Detail::Installed {
            entry: entry("w"),
            manifest: None,
            manifest_err: Some("no tamias.yaml".into()),
        });
        let s = screen(&mut m, 100, 30);
        assert!(s.contains("manifest no tamias.yaml"), "{s}");
        // no manifest, no error → provenance only; signer Some covers
        // the extra kv line
        let mut signed = entry("w");
        signed.record.signer = Some("key-1".into());
        m.detail = Some(Detail::Installed {
            entry: signed,
            manifest: None,
            manifest_err: None,
        });
        let s = screen(&mut m, 100, 30);
        assert!(s.contains("── provenance ──"), "{s}");
        assert!(s.contains("signer"), "{s}");
        // Registry detail — install ref + versions pending
        m.pane = Pane::Registry;
        let page = reg_entry("1.0.0");
        m.detail = Some(Detail::Registry {
            entry: page.clone(),
            install_ref: Some("./w@1.0.0".into()),
            install_err: None,
            versions: None,
            versions_err: None,
        });
        let s = screen(&mut m, 100, 30);
        assert!(s.contains("tamias install ./w@1.0.0"), "{s}");
        assert!(s.contains("versions …"), "{s}");
        assert!(s.contains("esc back · j/k scroll · i install"), "{s}");
        // install resolution error + versions landed
        m.detail = Some(Detail::Registry {
            entry: page.clone(),
            install_ref: None,
            install_err: Some("no registry".into()),
            versions: Some(vec![reg_entry("0.9.0"), page.clone()]),
            versions_err: None,
        });
        let s = screen(&mut m, 100, 30);
        assert!(s.contains("unavailable — no registry"), "{s}");
        assert!(s.contains("← current"), "{s}");
        // keywords/meta/provenance on the registry page
        let mut rich = page.clone();
        rich.keywords = vec!["k1".into()];
        rich.homepage = Some("https://h".into());
        rich.authors = vec!["a".into()];
        rich.publisher = Some("pub".into());
        rich.signature = Some("ssh-sig".into());
        m.detail = Some(Detail::Registry {
            entry: rich,
            install_ref: None,
            install_err: None,
            versions: None,
            versions_err: Some("index gone".into()),
        });
        let s = screen(&mut m, 100, 30);
        assert!(s.contains("keywords:"), "{s}");
        assert!(s.contains("── meta ──"), "{s}");
        assert!(s.contains("publisher"), "{s}");
        assert!(s.contains("signature"), "{s}");
        assert!(s.contains("index gone"), "{s}");
        // 4x4 — inner collapses to 2x2, scroll offset clamps; 2x2 —
        // inner is empty, the early return fires. Neither may panic.
        m.detail_scroll = 99;
        let s = screen(&mut m, 4, 4);
        assert!(s.contains("w@"), "{s}");
        let _ = screen(&mut m, 2, 2);
    }

    #[test]
    fn input_modes_and_completions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        // filter + search share the "/" label
        m.mode = Mode::Filter;
        m.input = "abc".into();
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("/abc"), "{s}");
        m.mode = Mode::Search;
        m.pane = Pane::Registry;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("/abc"), "{s}");
        // command line with a completion strip
        m.mode = Mode::Command;
        m.cmdline = "ver".into();
        m.cmd_completions = vec![
            crate::complete::Candidate {
                insert: "verify ".into(),
                hint: "check digests".into(),
            },
            crate::complete::Candidate {
                insert: "version".into(),
                hint: String::new(),
            },
        ];
        m.cmd_sel = 0;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains(":ver"), "{s}");
        assert!(s.contains("verify"), "{s}");
        // >14 candidates → "+N more" tail
        m.cmd_completions = (0..20)
            .map(|i| crate::complete::Candidate {
                insert: format!("c{i} "),
                hint: String::new(),
            })
            .collect();
        let s = screen(&mut m, 120, 24);
        assert!(s.contains("+6 more"), "{s}");
        // every prompt kind label
        for (kind, label) in [
            (PromptKind::Pin { name: "w".into() }, "pin > "),
            (PromptKind::InstallRef, "install > "),
            (PromptKind::RegistryTarget, "registry > "),
            (PromptKind::AddDep, "dep+ > "),
            (PromptKind::SetDep { old: "x".into() }, "dep→ > "),
            (
                PromptKind::DeployConfig { name: "w".into() },
                "deploy cfg > ",
            ),
            (PromptKind::PackSpec, "pack dir [out] > "),
            (PromptKind::PublishSpec, "publish dir [registry] > "),
        ] {
            m.mode = Mode::Prompt(kind);
            m.prompt_buf = "arg".into();
            let s = screen(&mut m, 80, 24);
            assert!(s.contains(label), "{label} missing: {s}");
            assert!(s.contains("arg"), "{s}");
        }
        m.mode = Mode::Normal;
    }

    #[test]
    fn modals_help_and_overlays() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        // Confirm modal
        m.modal = Some(Modal::Confirm {
            title: "remove w@1.0.0?".into(),
            body: vec!["warning: tears down provider units".into()],
            job: crate::ops::Job::Doctor,
        });
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("remove w@1.0.0?"), "{s}");
        assert!(s.contains("tears down provider units"), "{s}");
        assert!(s.contains("y = yes"), "{s}");
        // Consent modal
        m.modal = Some(Modal::Consent {
            job: crate::ops::Job::Doctor,
            reason: "ships drivers/ — consent required".into(),
        });
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("bundled drivers — consent?"), "{s}");
        assert!(s.contains("consent required"), "{s}");
        m.modal = None;
        // Help overlay — drawn over whatever pane is active
        m.mode = Mode::Help;
        let s = screen(&mut m, 80, 40);
        assert!(s.contains("keys — installed pane"), "{s}");
        assert!(s.contains("package page:"), "{s}");
        // tiny terminal — modal/help clamp to the frame
        m.mode = Mode::Normal;
        m.modal = Some(Modal::Confirm {
            title: "t".into(),
            body: vec!["b".into()],
            job: crate::ops::Job::Doctor,
        });
        let s = screen(&mut m, 30, 6);
        assert!(s.contains('t'), "{s}");
    }

    #[test]
    fn status_log_and_chrome_budgets() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = model(tmp.path());
        // busy spinner + status error styling paths
        m.busy = Some("installing w".into());
        m.tick_n = 3;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("installing w"), "{s}");
        m.busy = None;
        m.status = "removed w".into();
        m.status_err = true;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("removed w"), "{s}");
        // no permanent log strip — history only shows in the `L`
        // sidebar (covered by log_mode_* tests), never in Normal mode
        m.log.push_back("phase fetch".into());
        m.log.push_back("warning: drift".into());
        m.log.push_back("error: boom".into());
        m.log.push_back("done".into());
        m.log.push_back("extra1".into());
        let s = screen(&mut m, 80, 16);
        assert!(!s.contains("error: boom"), "{s}");
        // 4 rows: tabs stay (≥3), status bar yields (<5), no log (<12)
        let s = screen(&mut m, 80, 4);
        assert!(s.contains("INSTALLED"), "{s}");
        assert!(!s.contains("removed w"), "{s}");
        // 2 rows: all chrome gone
        let s = screen(&mut m, 80, 2);
        assert!(!s.contains("INSTALLED"), "{s}");
        // detail-open status hints per pane
        m.log.clear();
        m.status.clear();
        m.status_err = false;
        m.pane = Pane::Installed;
        m.entries = vec![row("w", false)];
        m.detail = Some(Detail::Installed {
            entry: entry("w"),
            manifest: None,
            manifest_err: None,
        });
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("esc back · j/k scroll · v/u/p/x/c/d"), "{s}");
        // services status rows + stream hint
        m.detail = None;
        m.pane = Pane::Services;
        m.svc_rows = vec![ServiceRow {
            package: "p".into(),
            service: "s".into(),
            unit: "u".into(),
            state: None,
            exec: "e".into(),
        }];
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("1 service row(s)"), "{s}");
        // narrow width → hint truncates with ellipsis, no panic
        let s = screen(&mut m, 30, 24);
        assert!(s.contains('…'), "{s}");
        // registry status with query
        m.pane = Pane::Registry;
        m.reg_query = "w".into();
        m.reg_results = vec![reg_entry("1.0.0")];
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("1/1 · /w"), "{s}");
        // manifest + author status lines
        m.pane = Pane::Manifest;
        m.project = Some(Manifest {
            deps: vec!["a".into()],
            ..Default::default()
        });
        m.manifest_path = PathBuf::from("tamias.yaml");
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("1 dep(s)"), "{s}");
        m.pane = Pane::Author;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("publish — p pack"), "{s}");
    }

    /// Versions focus marks the cursor row with `›` and switches the
    /// footer hint; unfocused rows keep the plain `  <ver>` shape.
    #[test]
    fn registry_page_versions_focus_marks_cursor() {
        let page = reg_entry("1.0.0");
        let detail = Detail::Registry {
            entry: page.clone(),
            install_ref: None,
            install_err: None,
            versions: Some(vec![reg_entry("0.9.0"), page.clone()]),
            versions_err: None,
        };
        let focused = body_text_full(&detail, None, Some(0));
        assert!(focused.contains("› 0.9.0"));
        assert!(focused.contains("  1.0.0  ← current"));
        // The status-bar hint is what versions focus switches — the
        // body itself carries no hint line.
        assert_eq!(
            detail_hint(&detail, true),
            "versions — j/k move · i install · v/esc back"
        );
        assert_eq!(
            detail_hint(&detail, false),
            "esc back · j/k scroll · i install · v versions"
        );
        assert!(!focused.contains("esc back · j/k scroll"));
        // Focused — the inline `v` affordance steps aside.
        assert!(!focused.contains("v picks a version"));
        // Cursor on the current entry keeps its `← current` marker.
        let on_current = body_text_full(&detail, None, Some(1));
        assert!(on_current.contains("› 1.0.0  ← current"));
        // Unfocused stays byte-identical to the plain render.
        assert_eq!(body_text(&detail), body_text_full(&detail, None, None));
    }

    /// `page_parts` reports the focused version row's body index so
    /// `draw_detail` can scroll it into view — `None` when unfocused,
    /// unfetched, or the selection is out of range.
    #[test]
    fn versions_focus_row_index_for_scroll() {
        let page = reg_entry("1.0.0");
        let detail = Detail::Registry {
            entry: page.clone(),
            install_ref: None,
            install_err: None,
            versions: Some(vec![reg_entry("0.9.0"), page.clone(), reg_entry("1.1.0")]),
            versions_err: None,
        };
        let (_, _, _, body, row) = page_parts(&detail, "default", None, Some(2));
        let row = row.expect("focused row index");
        assert!(
            body[row].spans.iter().any(|s| s.content.contains("1.1.0")),
            "row {row} should be the focused 1.1.0 line"
        );
        assert!(page_parts(&detail, "default", None, None).4.is_none());
        assert!(page_parts(&detail, "default", None, Some(9)).4.is_none());
        // Pending fetch — no rows, no target.
        let pending = Detail::Registry {
            entry: page.clone(),
            install_ref: None,
            install_err: None,
            versions: None,
            versions_err: None,
        };
        assert!(page_parts(&pending, "default", None, Some(0)).4.is_none());
    }

    /// `scroll_to_show` scrolls the minimum to keep `row` inside the
    /// `view_h` window — up when above, down when below, untouched
    /// when already visible or the view is empty.
    #[test]
    fn scroll_to_show_minimal_adjustment() {
        assert_eq!(scroll_to_show(5, 0, 10), 0); // already visible
        assert_eq!(scroll_to_show(3, 5, 10), 3); // above → scroll up
        assert_eq!(scroll_to_show(20, 0, 10), 11); // below → scroll down
        assert_eq!(scroll_to_show(9, 0, 10), 0); // last visible row
        assert_eq!(scroll_to_show(10, 0, 10), 1); // first hidden row
        assert_eq!(scroll_to_show(4, 7, 0), 7); // empty view: untouched
    }

    /// The `installed <pkg>@<ver>` line appears on a Registry page
    /// only when an installed version label is supplied.
    #[test]
    fn registry_page_shows_installed_marker() {
        let page = reg_entry("1.0.0"); // unscoped w@1.0.0
        let detail = Detail::Registry {
            entry: page,
            install_ref: None,
            install_err: None,
            versions: None,
            versions_err: None,
        };
        let plain = body_text(&detail);
        assert!(!plain.contains("installed  w@"));
        let marked = body_text_installed(&detail, Some("0.2.0"));
        assert!(marked.contains("installed  w@0.2.0"));
        // A scoped entry renders `@scope/name@ver`.
        let mut scoped = reg_entry("1.0.0");
        scoped.scope = Some("s".into());
        let scoped_detail = Detail::Registry {
            entry: scoped,
            install_ref: None,
            install_err: None,
            versions: None,
            versions_err: None,
        };
        let scoped_text = body_text_installed(&scoped_detail, Some("0.2.0"));
        assert!(scoped_text.contains("installed  @s/w@0.2.0"));
        // Installed pages never take the marker (their provenance
        // already carries the version).
        let installed_page = Detail::Installed {
            entry: tamias_core::StoreEntry {
                dir: std::path::PathBuf::from("/x"),
                record: tamias_core::StoreRecord {
                    name: "w".into(),
                    scope: Some("s".into()),
                    version: Some("0.2.0".into()),
                    kind: None,
                    description: None,
                    keywords: Vec::new(),
                    homepage: None,
                    repository: None,
                    license: None,
                    authors: Vec::new(),
                    source_ref: "./x".into(),
                    resolved: "sha256:0".into(),
                    digest: "sha256:0".into(),
                    signature: None,
                    signer: None,
                    trust: None,
                    installed_at: 0,
                },
            },
            manifest: None,
            manifest_err: None,
        };
        let text = body_text_installed(&installed_page, Some("9.9.9"));
        assert!(!text.contains("installed  @s/w@9.9.9"));
    }

    /// `ago_at` bucket boundaries — just now / minutes / hours /
    /// days / months / years.
    #[test]
    fn ago_buckets() {
        let now = 1_800_000_000u64;
        assert_eq!(ago_at(now, now), "just now");
        assert_eq!(ago_at(now - 59, now), "just now");
        assert_eq!(ago_at(now - 60, now), "1m ago");
        assert_eq!(ago_at(now - 5 * 60, now), "5m ago");
        assert_eq!(ago_at(now - 3599, now), "59m ago");
        assert_eq!(ago_at(now - 3600, now), "1h ago");
        assert_eq!(ago_at(now - 3 * 3600, now), "3h ago");
        assert_eq!(ago_at(now - 86_399, now), "23h ago");
        assert_eq!(ago_at(now - 86_400, now), "1d ago");
        assert_eq!(ago_at(now - 2 * 86_400, now), "2d ago");
        assert_eq!(ago_at(now - 30 * 86_400, now), "30d ago");
        assert_eq!(ago_at(now - 31 * 86_400, now), "1mo ago");
        assert_eq!(ago_at(now - 365 * 86_400, now), "12mo ago");
        assert_eq!(ago_at(now - 400 * 86_400, now), "1y ago");
        // A future stamp (clock skew) reads as "just now", not a
        // negative duration.
        assert_eq!(ago_at(now + 60, now), "just now");
    }

    // --- frame-level layout tests (TestBackend) ---

    /// A model with `n` op-log lines, ready to draw.
    fn log_model(n: usize) -> Model {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            None,
        );
        let mut m = Model::open(&ctx).unwrap();
        for i in 0..n {
            m.log.push_back(format!("op line {i}"));
        }
        m
    }

    /// Render one frame of `m` at `w`×`h` and flatten the buffer to
    /// text lines for assertions.
    fn frame_text(m: &mut Model, w: u16, h: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, m)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// There is no permanent log strip: with op-log lines present
    /// and `Mode::Normal`, no ` log ` chrome renders and the pane
    /// owns the row the strip used to take.
    #[test]
    fn no_permanent_log_strip() {
        let mut m = log_model(10);
        let screen = frame_text(&mut m, 80, 24);
        // The strip's ` log ` title appears nowhere above the status
        // bar (the bar's own `· L log` hint is exempt); the log
        // history only exists behind `L`.
        assert!(
            !screen[..22].iter().any(|l| l.contains(" log ")),
            "permanent log strip still drawn:\n{}",
            screen.join("\n")
        );
        // The freed row belongs to the pane — the Home block's bottom
        // border now sits on the last row above the status bar (the
        // strip used to eat four rows there).
        assert!(screen[22].contains('─'), "{}", screen[22]);
        assert!(screen[23].contains("home"), "{}", screen[23]);
        // And the log view was never drawn, so no hit rect was set.
        assert_eq!(m.log_area, Rect::default());
    }

    /// The `:` candidate strip renders `insert — hint`: the hint is
    /// no longer dead data.
    #[test]
    fn completion_strip_shows_hints() {
        let mut m = log_model(0);
        m.mode = Mode::Command;
        m.cmdline = "ins".into();
        m.cmd_completions = vec![
            crate::complete::Candidate {
                insert: "install ".into(),
                hint: "install a package by ref".into(),
            },
            crate::complete::Candidate {
                insert: "i ".into(),
                hint: String::new(), // hintless candidates render bare
            },
        ];
        let screen = frame_text(&mut m, 80, 24);
        assert!(
            screen
                .iter()
                .any(|l| l.contains("install — install a package by ref")),
            "{}",
            screen.join("\n")
        );
        // The `i` alias shows without a dangling separator.
        assert!(screen.iter().any(|l| l.contains(" i ")));
    }

    /// `L` at a normal width docks the log as a right sidebar: the
    /// main chrome shrinks to the left column, the bordered log panel
    /// takes the right ~quarter, and the pane stays on screen.
    #[test]
    fn log_mode_docks_right_sidebar() {
        let mut m = log_model(50);
        m.mode = Mode::Log;
        m.log_view_h = 20;
        m.log_scroll = m.log.len() - 20; // tail, as `open_log` aims
        let screen = frame_text(&mut m, 80, 24);
        // 80/4 = 20 → clamped to 24 cols; the main column keeps 56.
        assert_eq!(m.log_area.width, 24);
        assert_eq!(m.log_area.x, 56);
        assert_eq!(m.log_view_h, 22); // block height minus borders
        assert_eq!(m.tabs_area.map(|r| r.width), Some(56));
        // The sidebar shows history; the pane (Home) is still drawn
        // in the left column.
        assert!(screen.iter().any(|l| l.contains("log — 50 lines")));
        assert!(screen.iter().any(|l| l.contains("op line 49")));
        assert!(screen.iter().any(|l| l.contains("home")));
    }

    /// Under ~50 cols the sidebar would starve the pane — the log
    /// falls back to the centered overlay instead.
    #[test]
    fn log_mode_narrow_falls_back_to_overlay() {
        let mut m = log_model(10);
        m.mode = Mode::Log;
        let screen = frame_text(&mut m, 40, 20);
        // Centered box: narrower than the frame, inset on both sides.
        assert!(m.log_area.width < 40);
        assert!(m.log_area.x > 0);
        assert!(m.log_area.x + m.log_area.width < 40);
        assert!(screen.iter().any(|l| l.contains("log — 10 lines")));
    }
}
