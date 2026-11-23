//! Drawing — tab bar on top, the active pane filling the frame, an
//! optional op-log strip, status bar at the bottom, and modal/prompt
//! overlays. Chrome bars yield to the pane on short terminals, same
//! budget rule as myco-tui: tabs need height ≥3, the status bar ≥5,
//! the log strip ≥12 (and only while the log has lines).

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::app::{Detail, Modal, Mode, Model, Pane, PromptKind, SPINNER};

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
/// target, store/cache roots), the key hints in two columns, and the
/// recently-installed list (`installed_at` desc, top 8 — the same
/// [`Model::recent_installed`] ordering the `home_sel` cursor walks).
/// The highlighted row gets the versions-focus treatment — accent
/// `›` marker + `Indexed(238)` row background — and `Enter` opens its
/// package page via the Installed pane. Clicks don't hit-test here
/// (`board_area` stays empty); the wheel still scrolls the cursor.
fn draw_home(f: &mut Frame, m: &mut Model, area: Rect) {
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
    // Hints — two columns like the Author pane's; the recent-list
    // binds only appear when there's a list to walk.
    let mut hint_rows = vec![
        ("/  search registry", "i  install by ref"),
        ("1-6  jump to a pane", ":  command line · ? help"),
    ];
    if !m.entries.is_empty() {
        hint_rows.insert(1, ("j/k · Enter  open recent", "g/G  first/last"));
    }
    for (left, right) in hint_rows {
        lines.push(Line::from(vec![
            Span::styled(format!("{left:<22}"), Style::default().fg(MUTED)),
            Span::styled(right, Style::default().fg(MUTED)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "recently installed",
        Style::default().fg(ACCENT),
    )));
    if m.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  nothing installed yet — `i` install by ref",
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
            lines.push(if cursor {
                line.style(Style::default().bg(Color::Indexed(238)))
            } else {
                line
            });
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" home ")),
        area,
    );
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
                Paragraph::new(format!("services: {err}")).style(Style::default().fg(Color::Red)),
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
            "no provider services — install a package with a `provider:` block"
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
        .row_highlight_style(Style::default().bg(Color::Indexed(238)));
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(m.svc_sel));
    f.render_stateful_widget(table, area, &mut state);
    note_table_area(m, area, state.offset());
}

/// The Manifest pane — the project manifest's `deps:` as editable
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
                format!("{err} — `:manifest` retries, `a` creates tamias.yaml on save"),
                Style::default().fg(WARN),
            )));
        }
        (None, None) => {
            info.push(Line::from(Span::styled(
                "no project manifest — `a` writes tamias.yaml + tamias.lock",
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
        f.render_widget(Paragraph::new(info), info_area);
    }
    let deps: Vec<String> = m
        .project
        .as_ref()
        .map(|p| p.deps.clone())
        .unwrap_or_default();
    if deps.is_empty() {
        f.render_widget(
            Paragraph::new("no deps — `a` add ref · rows edit via `e`/`x`")
                .style(Style::default().fg(MUTED)),
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
        .row_highlight_style(Style::default().bg(Color::Indexed(238)));
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(m.man_sel));
    f.render_stateful_widget(table, table_area, &mut state);
    note_table_area(m, table_area, state.offset());
}

/// The Author pane — configured registries plus the pack/publish
/// entry points; results land in the op log like every other job.
fn draw_author(f: &mut Frame, m: &mut Model, area: Rect) {
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(
            "authoring",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" — bundle pack/publish"),
    ])];
    lines.push(Line::from(""));
    if m.author_regs.is_empty() {
        lines.push(Line::from(Span::styled(
            "no registries configured — publish routes to the default local root",
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
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" author ")),
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
                Cell::from(short_sha(&rec.resolved)),
                Cell::from(short_sha(&rec.digest)),
                Cell::from(r.size.map_or("-".into(), human_size)),
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
fn page_parts(
    detail: &Detail,
    registry_target: &str,
    installed_ver: Option<&str>,
    versions_sel: Option<usize>,
) -> (String, Line<'static>, Option<String>, Vec<Line<'static>>) {
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
                Span::raw(err.clone()),
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
                    Span::raw(err.clone()),
                ]));
            }
            // provenance — last, per the package-page anatomy.
            body.push(Line::from(""));
            body.push(rule("provenance"));
            body.push(kv("source", rec.source_ref.clone()));
            body.push(kv("resolved", rec.resolved.clone()));
            body.push(kv("digest", rec.digest.clone()));
            body.push(kv("installed", rec.installed_at.to_string()));
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
                    body.push(rule("versions"));
                    if list.is_empty() {
                        body.push(Line::from(Span::styled(
                            "  no versions indexed",
                            Style::default().fg(MUTED),
                        )));
                    }
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
                    Span::styled(format!("— {err}"), Style::default().fg(WARN)),
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
    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        match detail {
            Detail::Installed { .. } => "esc back · j/k scroll · v/u/p/r/c/d act on the row",
            Detail::Registry { .. } if versions_sel.is_some() => {
                "versions — j/k move · i install · v/esc back"
            }
            Detail::Registry { .. } => "esc back · j/k scroll · i install",
        },
        Style::default().fg(MUTED),
    )));
    (label, head, desc, body)
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
    let (title, head, desc, body) = page_parts(
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
    f.render_widget(Paragraph::new(head).wrap(Wrap { trim: true }), header_area);
    if let Some(d) = &desc {
        f.render_widget(
            Paragraph::new(d.as_str())
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(MUTED)),
            desc_area,
        );
    }
    // Clamp the scroll offset to the body, then render.
    let max_scroll = body.len().saturating_sub(body_area.height as usize);
    m.detail_scroll = m.detail_scroll.min(max_scroll);
    f.render_widget(
        Paragraph::new(body)
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
                 `:registry <name>` picks the target, Enter opens the package \
                 page, `i` installs a result",
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
    let header = Row::new(vec![
        "NAME",
        "SCOPE",
        "TYPE",
        "VERSION",
        "LICENSE",
        "DIGEST",
        "DESCRIPTION",
    ])
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = m
        .reg_results
        .iter()
        .map(|e| {
            // Name cell carries styled spans — `[signed]` green for
            // signed index entries, `· installed <ver>` muted when
            // the store already has this (name, scope) at any version.
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
            Row::new(vec![
                Cell::from(Line::from(name)),
                Cell::from(e.scope.clone().unwrap_or_else(|| "-".into())),
                Cell::from(e.kind.clone().unwrap_or_else(|| "-".into())),
                Cell::from(e.version.clone().unwrap_or_else(|| "-".into())),
                Cell::from(e.license.clone().unwrap_or_else(|| "-".into())),
                Cell::from(short_sha(&e.digest)),
                Cell::from(e.description.clone().unwrap_or_else(|| "-".into())),
            ])
        })
        .collect();
    // DESCRIPTION is the flexible column — it absorbs leftover width
    // and clips on narrow terminals; LICENSE stays a fixed narrow
    // column so it survives degradation.
    let widths = [
        Constraint::Min(14),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Min(12),
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
                    format!(
                        "{}/{} · /{} · {}",
                        m.reg_results.len(),
                        m.reg_results.len(),
                        m.reg_query,
                        m.reg_target_label
                    )
                }
            }
            Pane::Services => {
                if m.svc_rows.is_empty() {
                    "services — s/S start/stop · r restart · e/E enable/disable".to_string()
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
                Some(p) => format!("{} — {} dep(s)", m.manifest_path.display(), p.deps.len()),
                None => format!("{} — no manifest", m.manifest_path.display()),
            },
            Pane::Author => "author — p pack · P publish · r registries".to_string(),
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
    let hint = if m.detail.is_some() {
        match &m.detail {
            Some(Detail::Registry { .. }) if m.versions_focused() => {
                "versions — j/k move · i install · v/esc back"
            }
            Some(Detail::Registry { .. }) => "esc back · j/k scroll · i install",
            _ => "esc back · j/k scroll · v/u/p/r/c/d act on the row",
        }
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
                "j/k · Enter page · v verify · u update · p pin · r remove · i install · c clean · d doctor · / filter · : cmds · q quit"
            }
            Pane::Registry => "j/k · Enter page · i install · / query · : cmds · q quit",
            Pane::Services => {
                if m.stream_child.is_some() {
                    "streaming — x kill · q quit"
                } else {
                    "j/k · s/S start/stop · r restart · e/E enable/disable · d deploy · x uninstall · t/T logs · n run · R refresh · : cmds · q quit"
                }
            }
            Pane::Manifest => {
                "j/k · a add dep · d/x remove · e edit ref · s sync · S sync-lock · R reload · : cmds · q quit"
            }
            Pane::Author => "p pack · P publish · r registries · R reload · : cmds · q quit",
        }
    };
    let left_w = Line::raw(left.as_str()).width();
    let hint = fit_width(hint, (area.width as usize).saturating_sub(left_w + 2));
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
            }
            if m.cmd_completions.len() > 14 {
                spans.push(Span::styled(
                    format!("  +{} more", m.cmd_completions.len() - 14),
                    Style::default().fg(MUTED),
                ));
            }
            f.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect {
                    x: area.x,
                    y: cy,
                    width: area.width,
                    height: 1,
                },
            );
        }
    }
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
  Enter         package page          esc/←   back / close page
  v             verify (digest check) u       update selected
  U             update all            p       pin → new ref
  r             remove (confirm)      c/C     cache clean >30d / all
  d             doctor summary        i       install by ref
  Tab, h/l      cycle panes           1-6    jump to pane
  /             filter installed      Enter  keep filter
  :             command line          Tab   complete · esc leave mode
  R             reload store          F      clear filter
  ?             this help             q/C-c  quit

home pane:    landing view — wordmark · stats · recently installed ·
              / jumps to registry search · i install by ref · no cursor

package page:  npm-style page — header chips · description · install
               command · deps/services/meta · provenance last ·
               j/k scroll · Enter/esc close · registry: i installs ·
               v focuses versions — j/k pick · i installs that version ·
               `installed <pkg>@<ver>` line when already in the store

registry pane:  / search-as-you-type (debounced) · Enter package page ·
                i install result · :registry <name> target · :search <q>
                results mark [signed] and · installed <ver>

commands:  :q  :r/:reload  :help  :install <ref>  :remove <name>
           :verify <name>  :update [name]  :pin <name> <ref>
           :search <q>  :registry <name>  :clean [all]  :doctor
           :services  :service <verb> <unit>  :run <pkg> [svc]
           :logs <pkg|unit> [-f]  :deploy <name>  :manifest
           :pack <dir> [out]  :publish <dir> [registry]  :registries
           :sync [--from-lock]
mouse:     wheel scrolls · click row selects · click tab switches

services pane:  provider services + unit state · s/S start/stop ·
                r restart · e/E enable/disable · d deploy · x uninstall
                t/T journal tail/follow · n foreground run (x kills)
manifest pane:  deps rows editable — a add · d/x remove · e new ref
                (writes tamias.yaml + tamias.lock like `tamias add`)
                s / :sync reconciles deps: against the store + lock
                S / :sync --from-lock installs tamias.lock entries verbatim
author pane:    p bundle pack · P publish (confirm) · r registries";
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
        let (_, _, _, body) = page_parts(detail, "default", installed_ver, versions_sel);
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
        // on the page's own entry (matched by blob `file`).
        let ready = body_text(&detail(Some(vec![reg_entry("0.9.0"), page.clone()]), None));
        assert!(ready.contains("── versions ──"));
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
        // narrow → initials, hit-test still resolves
        let narrow = tab_labels(20);
        assert!(narrow.iter().all(|l| l.chars().count() == 1));
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
        // Author — no registries
        m.pane = Pane::Author;
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("authoring"), "{s}");
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
        // log strip: ≥12 rows and non-empty; error/warning styling
        // five lines → tail-4 keeps warning/error/done/extra1 inside
        // the strip's three visible rows
        m.log.push_back("phase fetch".into());
        m.log.push_back("warning: drift".into());
        m.log.push_back("error: boom".into());
        m.log.push_back("done".into());
        m.log.push_back("extra1".into());
        let s = screen(&mut m, 80, 16);
        assert!(s.contains(" log "), "{s}");
        assert!(s.contains("warning: drift"), "{s}");
        assert!(s.contains("error: boom"), "{s}");
        assert!(!s.contains("phase fetch"), "{s}"); // tail-4 window
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
        m.entries = vec![row("w", false)];
        m.detail = Some(Detail::Installed {
            entry: entry("w"),
            manifest: None,
            manifest_err: None,
        });
        let s = screen(&mut m, 80, 24);
        assert!(s.contains("esc back · j/k scroll · v/u/p/r/c/d"), "{s}");
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
        assert!(s.contains("author — p pack"), "{s}");
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
        assert!(focused.contains("versions — j/k move · i install · v/esc back"));
        assert!(!focused.contains("esc back · j/k scroll · i install"));
        // Cursor on the current entry keeps its `← current` marker.
        let on_current = body_text_full(&detail, None, Some(1));
        assert!(on_current.contains("› 1.0.0  ← current"));
        // Unfocused stays byte-identical to the plain render.
        assert_eq!(body_text(&detail), body_text_full(&detail, None, None));
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
}
