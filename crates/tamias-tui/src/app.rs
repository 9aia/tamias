//! The TUI model — panes, cursor, modes, and the store reads every
//! key maps to. Phase 1 is read-only: the Installed pane lists
//! `tamias-store-local` entries with a client-side `/` filter; the
//! other four panes are placeholders until their phases land.
//!
//! Reloads: an OS filesystem watcher (`notify`) on the store root
//! sets a dirty flag that `tick` turns into a reload, so installs
//! from other `tamias` invocations show up without a keypress; a
//! slow periodic rescan covers events a watcher can miss. `R` and
//! `:reload` force one.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tamias_core::StoreEntry;
use tamias_store_local::Store;

use crate::Ctx;

/// How often `tick` re-lists the store even without a watcher event —
/// covers watcher queue overflow and mounts added after launch.
const RESCAN_TICKS: u32 = 40; // ~10s at the 250ms poll

/// How long a non-error status message holds the bar.
const STATUS_TTL: Duration = Duration::from_secs(4);

/// One installed package row — the store entry plus its payload
/// size, measured once at load (the walk is too slow per frame).
#[derive(Debug, Clone)]
pub struct Row {
    /// The store entry (record = provenance).
    pub entry: StoreEntry,
    /// Payload bytes (`None` when the walk failed).
    pub size: Option<u64>,
}

/// Which pane the tab bar shows. `ALL` fixes the `1`-`5` jump order
/// and tab labels; only [`Pane::Installed`] has content in phase 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Store entries — name/type/source/resolved/digest/size.
    Installed,
    /// Registry search-as-you-type (phase 3).
    Registry,
    /// Provider lifecycle (phase 4).
    Services,
    /// `tamias.yaml` deps form (phase 5).
    Manifest,
    /// `bundle pack`/`publish` authoring (phase 6).
    Author,
}

impl Pane {
    /// Tab label.
    pub fn label(self) -> &'static str {
        match self {
            Pane::Installed => "INSTALLED",
            Pane::Registry => "REGISTRY",
            Pane::Services => "SERVICES",
            Pane::Manifest => "MANIFEST",
            Pane::Author => "AUTHOR",
        }
    }

    /// Tab order for `h`/`l`, `Tab`/`Shift-Tab`, and `1`-`5`.
    pub const ALL: [Pane; 5] = [
        Pane::Installed,
        Pane::Registry,
        Pane::Services,
        Pane::Manifest,
        Pane::Author,
    ];

    /// One-line placeholder for the not-yet-built panes.
    pub fn placeholder(self) -> &'static str {
        match self {
            Pane::Installed => "",
            Pane::Registry => "registry search lands in phase 3 — `tamias search <q>` works today",
            Pane::Services => "provider lifecycle lands in phase 4 — `tamias provider list` works today",
            Pane::Manifest => "the tamias.yaml deps form lands in phase 5 — `tamias add` works today",
            Pane::Author => "bundle pack / publish land in phase 6 — `tamias bundle pack` / `tamias publish` work today",
        }
    }
}

/// UI mode — the active pane handles keys differently per mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Browsing.
    Normal,
    /// `/` — live filter over the installed entries; `Enter` keeps
    /// the filter, `esc` clears it.
    Filter,
    /// `:` — ex-style command line (`:q` `:r` `:help`); `Enter` runs
    /// it, `esc` drops the line.
    Command,
    /// `?` — keybind overlay; `esc`/`?`/`q` closes.
    Help,
}

/// The whole TUI state.
pub struct Model {
    /// Store root the entries were listed from (watcher target).
    store_root: PathBuf,
    /// Every installed entry, sorted by label (as `Store::entries`).
    pub entries: Vec<Row>,
    /// Last `entries()` failure — the pane shows it instead of rows.
    pub load_err: Option<String>,
    /// Active pane.
    pub pane: Pane,
    /// Index into the *filtered* row list (clamped on rebuild).
    pub sel: usize,
    /// Label of the selected row before the last rebuild — keeps the
    /// cursor on the same package across reloads.
    sel_label: Option<String>,
    /// `/` live filter — case-insensitive substring over label,
    /// source ref, kind, and resolved sha.
    pub filter: String,
    /// Current mode.
    pub mode: Mode,
    /// `/` input buffer (the filter text while typing).
    pub input: String,
    /// `:` command-line buffer — separate from `input` so a filter
    /// draft isn't clobbered.
    pub cmdline: String,
    /// Status-bar message (last action / error) — expires after
    /// [`STATUS_TTL`] unless it's an error (errors are sticky).
    pub status: String,
    /// When `status` was last set.
    status_at: Instant,
    /// `status` is an error — holds the bar until the next input.
    pub status_err: bool,
    /// OS filesystem watcher on the store root — `tick` drains its
    /// events into `dirty`. `None` when the platform/backend can't
    /// watch or the root doesn't exist yet; the rescan fallback
    /// still applies.
    watcher: Option<notify::RecommendedWatcher>,
    /// Event channel paired with `watcher`.
    watch_rx: Option<std::sync::mpsc::Receiver<notify::Result<notify::Event>>>,
    /// Set by watcher events; `tick` reloads and clears it.
    dirty: bool,
    /// `tick` counter — drives the [`RESCAN_TICKS`] fallback.
    tick_n: u32,
    /// List rect from the last frame — `render::draw` writes it so
    /// mouse hits map to rows.
    pub board_area: Rect,
    /// First row index visible in `board_area` — the scroll offset
    /// the `List` widget chose to keep `sel` on screen.
    pub viewport_top: usize,
    /// Tab-bar rect when the frame is tall enough to draw one.
    pub tabs_area: Option<Rect>,
}

impl Model {
    /// First load against `ctx.store_root`.
    pub fn open(ctx: &Ctx) -> Result<Self, String> {
        let store_root = ctx.store_root.clone();
        let mut m = Model {
            store_root,
            entries: Vec::new(),
            load_err: None,
            pane: Pane::Installed,
            sel: 0,
            sel_label: None,
            filter: String::new(),
            mode: Mode::Normal,
            input: String::new(),
            cmdline: String::new(),
            status: String::new(),
            status_at: Instant::now(),
            status_err: false,
            watcher: None,
            watch_rx: None,
            dirty: false,
            tick_n: 0,
            board_area: Rect::default(),
            viewport_top: 0,
            tabs_area: None,
        };
        m.reload();
        Ok(m)
    }

    /// Re-list the store from disk, keeping the cursor on the same
    /// label when it survives. `R`, `:reload`, watcher events, and
    /// the rescan fallback all land here.
    pub fn reload(&mut self) {
        let at = self.sel;
        let keep = self.sel_label.clone();
        match Store::new(&self.store_root).entries() {
            Ok(entries) => {
                self.entries = entries
                    .into_iter()
                    .map(|entry| {
                        let size = entry.size().ok();
                        Row { entry, size }
                    })
                    .collect();
                self.load_err = None;
            }
            Err(e) => {
                self.load_err = Some(e.to_string());
            }
        }
        self.rebuild_sel(keep.as_deref(), at);
        self.arm_watch();
    }

    /// Event-loop heartbeat — expire the status toast, drain watcher
    /// events into `dirty`, and reload when dirty or on the
    /// [`RESCAN_TICKS`] fallback.
    pub fn tick(&mut self) {
        self.tick_n += 1;
        if !self.status.is_empty() && !self.status_err && self.status_at.elapsed() >= STATUS_TTL {
            self.status.clear();
        }
        self.dirty |= self.drain_watch();
        let scan_due = self.tick_n.is_multiple_of(RESCAN_TICKS);
        if self.dirty || scan_due {
            self.dirty = false;
            self.reload();
        }
    }

    /// Drain pending watcher events — any event on the store root
    /// counts (installs/removes are the only writers we care about;
    /// a `verify`/`info` read doesn't write). Channel errors count
    /// as dirty: a dead watcher means we can no longer trust
    /// silence.
    fn drain_watch(&mut self) -> bool {
        let Some(rx) = &self.watch_rx else {
            return false;
        };
        rx.try_iter().any(|ev| match ev {
            Ok(_) => true,
            Err(_) => true,
        })
    }

    /// (Re)arm the filesystem watcher on the store root. Called
    /// after open and every reload — a fresh channel drops events
    /// describing the just-loaded state. Failure (or a missing root,
    /// or a platform without a backend) leaves the watcher `None`;
    /// the rescan fallback keeps working.
    fn arm_watch(&mut self) {
        self.watcher = None;
        self.watch_rx = None;
        if !self.store_root.is_dir() {
            return;
        }
        use notify::Watcher as _;
        let (tx, rx) = std::sync::mpsc::channel();
        let Ok(mut w) = notify::recommended_watcher(tx) else {
            return;
        };
        if w.watch(&self.store_root, notify::RecursiveMode::Recursive)
            .is_ok()
        {
            self.watcher = Some(w);
            self.watch_rx = Some(rx);
        }
    }

    /// Indices of the rows passing the `/` filter, in store order.
    /// The filter is a case-insensitive substring over the label,
    /// source ref, kind, and resolved sha — everything the row shows.
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                if needle.is_empty() {
                    return true;
                }
                let rec = &r.entry.record;
                let hay = format!(
                    "{} {} {} {} {}",
                    r.entry.label(),
                    rec.kind.as_deref().unwrap_or(""),
                    rec.source_ref,
                    rec.resolved,
                    rec.digest
                )
                .to_lowercase();
                hay.contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The row under the cursor, if the filtered list is non-empty.
    pub fn selected(&self) -> Option<&Row> {
        let vis = self.visible();
        vis.get(self.sel).and_then(|&i| self.entries.get(i))
    }

    /// Keep the cursor on `keep`'s label when it survives the reload,
    /// else clamp `at` into the filtered range.
    fn rebuild_sel(&mut self, keep: Option<&str>, at: usize) {
        let vis = self.visible();
        if let Some(label) = keep {
            if let Some(pos) = vis.iter().position(|&i| {
                self.entries
                    .get(i)
                    .is_some_and(|r| r.entry.label() == label)
            }) {
                self.sel = pos;
                return;
            }
        }
        self.sel = at.min(vis.len().saturating_sub(1));
    }

    fn move_sel(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            self.sel = 0;
            return;
        }
        self.sel = (self.sel as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    fn switch_pane(&mut self, pane: Pane) {
        self.pane = pane;
        self.sel_label = self.selected().map(|r| r.entry.label());
    }

    fn step_pane(&mut self, delta: isize) {
        let n = Pane::ALL.len() as isize;
        let cur = Pane::ALL.iter().position(|p| *p == self.pane).unwrap_or(0) as isize;
        self.switch_pane(Pane::ALL[(cur + delta).rem_euclid(n) as usize]);
    }

    /// Record the selected row's label — call after moves too, so a
    /// `j` then `R` lands back on the same package.
    fn sync_sel_label(&mut self) {
        self.sel_label = self.selected().map(|r| r.entry.label());
    }

    /// Status-bar toast.
    fn say(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_at = Instant::now();
        self.status_err = false;
    }

    /// Sticky error toast — holds the bar until the next keypress.
    fn say_err(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_at = Instant::now();
        self.status_err = true;
    }

    /// An error toast clears on the next input; a normal one on TTL.
    fn dismiss_error(&mut self) {
        if self.status_err {
            self.status.clear();
            self.status_err = false;
        }
    }

    /// `:` command line — `:q` quits, `:r`/`:reload` re-lists the
    /// store, `:help` opens the overlay; anything else echoes back
    /// as an error (the palette grows with the phases).
    fn run_command(&mut self) {
        let cmd = self.cmdline.trim().trim_start_matches(':').to_string();
        match cmd.as_str() {
            "q" | "quit" | "q!" => {
                // handled by the caller's return — see key_command
            }
            "r" | "reload" => {
                self.reload();
                self.say("reloaded");
            }
            "help" | "?" => self.mode = Mode::Help,
            "" => {}
            other => self.say_err(format!("unknown command ':{other}'")),
        }
    }

    /// `:` mode keys — `Enter` runs, `esc` drops the line.
    fn key_command(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                let quit = matches!(self.cmdline.trim(), "q" | "quit" | "q!");
                if !quit {
                    self.run_command();
                }
                quit
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.cmdline.clear();
                false
            }
            KeyCode::Backspace => {
                self.cmdline.pop();
                false
            }
            KeyCode::Char(c) => {
                self.cmdline.push(c);
                false
            }
            _ => false,
        }
    }

    /// `/` mode keys — live filter; `Enter` keeps it, `esc` clears.
    fn key_filter(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.input.clear();
                false
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.filter.clear();
                self.rebuild_sel(None, self.sel);
                false
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.filter = self.input.clone();
                self.rebuild_sel(None, self.sel);
                false
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.filter = self.input.clone();
                self.rebuild_sel(None, self.sel);
                false
            }
            _ => false,
        }
    }

    /// `?` overlay — any of `esc`/`?`/`q` closes.
    fn key_help(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                false
            }
            _ => false,
        }
    }

    /// One key event → `true` when the app should quit. Ctrl-C is
    /// the force-quit and works from every mode.
    pub fn key(&mut self, k: KeyEvent) -> bool {
        self.dismiss_error();
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        match self.mode {
            Mode::Filter => return self.key_filter(k),
            Mode::Command => return self.key_command(k),
            Mode::Help => return self.key_help(k),
            Mode::Normal => {}
        }
        match k.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('1') => self.switch_pane(Pane::Installed),
            KeyCode::Char('2') => self.switch_pane(Pane::Registry),
            KeyCode::Char('3') => self.switch_pane(Pane::Services),
            KeyCode::Char('4') => self.switch_pane(Pane::Manifest),
            KeyCode::Char('5') => self.switch_pane(Pane::Author),
            KeyCode::Char('h') | KeyCode::BackTab => self.step_pane(-1),
            KeyCode::Tab | KeyCode::Char('l') => self.step_pane(1),
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_sel(1);
                self.sync_sel_label();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_sel(-1);
                self.sync_sel_label();
            }
            KeyCode::Char('g') => self.sel = 0,
            KeyCode::Char('G') => {
                self.sel = self.visible().len().saturating_sub(1);
                self.sync_sel_label();
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.input = self.filter.clone();
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.cmdline.clear();
            }
            KeyCode::Char('F') => {
                self.filter.clear();
                self.rebuild_sel(None, self.sel);
            }
            KeyCode::Char('R') => {
                self.reload();
                self.say("reloaded");
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
        false
    }

    /// Mouse — wheel scrolls the selection, a click on the tab bar
    /// switches panes, a click on a row selects it. (No double-click
    /// open yet — the detail pane is phase 2.)
    pub fn mouse(&mut self, ev: MouseEvent) {
        match ev.kind {
            MouseEventKind::ScrollUp => {
                self.move_sel(-3);
                self.sync_sel_label();
            }
            MouseEventKind::ScrollDown => {
                self.move_sel(3);
                self.sync_sel_label();
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(tabs) = self.tabs_area {
                    if tabs.contains((ev.column, ev.row).into()) {
                        if let Some(i) = crate::render::tab_hit(tabs.width, ev.column) {
                            if let Some(pane) = Pane::ALL.get(i) {
                                self.switch_pane(*pane);
                            }
                        }
                        return;
                    }
                }
                let area = self.board_area;
                if area.height > 0 && area.contains((ev.column, ev.row).into()) {
                    let row_in_view = (ev.row - area.y) as usize;
                    // display row 0 inside the bordered table is the
                    // header — data rows start one line below it
                    let Some(data_row) = row_in_view.checked_sub(1) else {
                        return;
                    };
                    let idx = self.viewport_top + data_row;
                    let vis = self.visible();
                    if idx < vis.len() {
                        self.sel = idx;
                        self.sync_sel_label();
                    }
                }
            }
            _ => {}
        }
    }
}
