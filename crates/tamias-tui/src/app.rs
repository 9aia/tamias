//! The TUI model — panes, cursor, modes, modals, and the store reads
//! every key maps to. The Installed pane lists `tamias-store-local`
//! entries with a client-side `/` filter; the Registry pane searches
//! a registry as-you-type; Services/Manifest/Author stay placeholders
//! until their phases land.
//!
//! Mutations never run on the event loop: [`Model::spawn`] hands a
//! [`Job`] to a worker thread that calls the same library functions
//! `tamias-serve`/the CLI use (see [`crate::ops`]) and reports back
//! over an mpsc channel — phase lines to the log, the final
//! [`Outcome`] or error to the status bar. A bundled-driver consent
//! refusal comes back as a modal instead of an error.
//!
//! Reloads: an OS filesystem watcher (`notify`) on the store root
//! sets a dirty flag that `tick` turns into a reload, so installs
//! from other `tamias` invocations show up without a keypress; a
//! slow periodic rescan covers events a watcher can miss. `R` and
//! `:reload` force one.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tamias_core::{Manifest, StoreEntry};
use tamias_registry::RegistryEntry;
use tamias_store_local::Store;

use crate::ops::{self, Job, Outcome};
use crate::Ctx;

/// How often `tick` re-lists the store even without a watcher event —
/// covers watcher queue overflow and mounts added after launch.
const RESCAN_TICKS: u32 = 40; // ~10s at the 250ms poll

/// How long a non-error status message holds the bar.
const STATUS_TTL: Duration = Duration::from_secs(4);

/// Log ring capacity — old phase lines drop off the top.
const LOG_CAP: usize = 300;

/// Registry search debounce — a pause this long after the last
/// keystroke fires the query (Enter fires immediately).
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Spinner frames for the busy indicator.
pub(crate) const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
/// and tab labels; Services/Manifest/Author are still placeholders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Store entries — name/type/source/resolved/digest/size.
    Installed,
    /// Registry search-as-you-type.
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
            Pane::Registry => "",
            Pane::Services => "provider lifecycle lands in phase 4 — `tamias provider list` works today",
            Pane::Manifest => "the tamias.yaml deps form lands in phase 5 — `tamias add` works today",
            Pane::Author => "bundle pack / publish land in phase 6 — `tamias bundle pack` / `tamias publish` work today",
        }
    }
}

/// What a `:` prompt is collecting — `Enter` turns the buffer into a
/// job/modal/target change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    /// `p` — new source ref for the selected entry (`:pin`-equivalent).
    Pin {
        /// Entry label to retarget.
        name: String,
    },
    /// `i`/`:install` — an installable source ref.
    InstallRef,
    /// `:registry <name>` — the search/install target override.
    RegistryTarget,
}

/// UI mode — the active pane handles keys differently per mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Browsing.
    Normal,
    /// `/` on Installed — live filter over the entries; `Enter` keeps
    /// the filter, `esc` clears it.
    Filter,
    /// `/` on Registry — the search query; edits `reg_query` live and
    /// debounces the actual search.
    Search,
    /// `:` — ex-style command line; `Enter` runs it, `esc` drops it.
    Command,
    /// `p`/`i`/`:registry` — single-line prompt (`prompt_buf`).
    Prompt(PromptKind),
    /// `?` — keybind overlay; `esc`/`?`/`q` closes.
    Help,
}

/// A modal dialog over the panes — `y`/`n` (default `n`), `esc` = `n`.
#[derive(Debug)]
pub enum Modal {
    /// Mutating-op confirmation — `y` spawns `job`.
    Confirm {
        /// Dialog title (`remove widget@1.0.0?`).
        title: String,
        /// Detail lines (the non-`files` teardown warning lives here).
        body: Vec<String>,
        /// The job `y` runs.
        job: Job,
    },
    /// Bundled-driver consent — the install was refused because the
    /// package ships `drivers/`; `y` re-runs `job` with consent.
    Consent {
        /// The refused job (re-spawned via `with_drivers_allowed`).
        job: Job,
        /// The pipeline's refusal message (shown for context).
        reason: String,
    },
}

/// A worker-thread message — phase lines, the final outcome, or a
/// consent refusal handed back for the modal.
pub enum Msg {
    /// One log line (progress phase, driver action, warning).
    Log(String),
    /// The install was refused for bundled-driver consent — the UI
    /// shows the modal; `y` re-runs `job` with consent.
    Consent(Job, String),
    /// The job finished — summary text or typed search results.
    Done(Result<Outcome, String>),
}

/// The detail view — the selected entry plus its payload manifest,
/// loaded once on `Enter` (a local file read, cheap enough to stay on
/// the event loop).
#[derive(Debug)]
pub struct Detail {
    /// The entry the detail was opened on (a clone — selection may
    /// move underneath without disturbing the view).
    pub entry: StoreEntry,
    /// `entry.manifest()` when the payload carries one.
    pub manifest: Option<Manifest>,
    /// Why the manifest is absent (`None` manifest + `Some` err).
    pub manifest_err: Option<String>,
}

/// A parsed `:` command — separated from execution so the grammar is
/// unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cmd {
    Quit,
    Reload,
    Help,
    /// `:install <ref>`.
    Install(String),
    /// `:remove <name>`.
    Remove(String),
    /// `:verify <name>`.
    Verify(String),
    /// `:update [name]` — bare updates everything (`--all`).
    Update(Option<String>),
    /// `:pin <name> <ref>`.
    Pin(String, String),
    /// `:search <q>` — jump to the Registry pane and query.
    Search(String),
    /// `:registry [name]` — set the search/install target; bare opens
    /// the prompt.
    Registry(Option<String>),
    /// `:clean [all]` — cache clean (>30d, or everything with `all`).
    Clean(bool),
    /// `:doctor`.
    Doctor,
}

/// Parse a `:` command line (the leading `:` already stripped).
fn parse_command(cmd: &str) -> Result<Cmd, String> {
    let mut parts = cmd.split_whitespace();
    let verb = parts.next().unwrap_or("");
    let rest = parts.collect::<Vec<_>>().join(" ");
    match verb {
        "" => Err(String::new()),
        "q" | "quit" | "q!" => Ok(Cmd::Quit),
        "r" | "reload" => Ok(Cmd::Reload),
        "help" | "?" => Ok(Cmd::Help),
        "install" | "i" => {
            if rest.is_empty() {
                Err("usage: :install <ref>".into())
            } else {
                Ok(Cmd::Install(rest))
            }
        }
        "remove" => {
            if rest.is_empty() {
                Err("usage: :remove <name>".into())
            } else {
                Ok(Cmd::Remove(rest))
            }
        }
        "verify" => {
            if rest.is_empty() {
                Err("usage: :verify <name>".into())
            } else {
                Ok(Cmd::Verify(rest))
            }
        }
        "update" | "u" => Ok(Cmd::Update(if rest.is_empty() { None } else { Some(rest) })),
        "pin" | "p" => {
            // `:pin <name> <ref…>` — the ref may contain spaces? No —
            // refs never do; first token is the name, rest is the ref.
            let mut it = rest.splitn(2, char::is_whitespace);
            let name = it.next().unwrap_or("");
            let reference = it.next().unwrap_or("").trim();
            if name.is_empty() || reference.is_empty() {
                Err("usage: :pin <name> <ref>".into())
            } else {
                Ok(Cmd::Pin(name.to_string(), reference.to_string()))
            }
        }
        "search" => Ok(Cmd::Search(rest)),
        "registry" => Ok(Cmd::Registry(if rest.is_empty() {
            None
        } else {
            Some(rest)
        })),
        "clean" | "cache" => Ok(Cmd::Clean(rest == "all" || rest == "--all")),
        "doctor" | "d" => Ok(Cmd::Doctor),
        other => Err(format!("unknown command ':{other}'")),
    }
}

/// The whole TUI state.
pub struct Model {
    /// Roots the ops run against (cloned into every worker).
    ctx: Ctx,
    /// Store root the entries were listed from (watcher target).
    store_root: PathBuf,
    /// Every installed entry, sorted by label (as `Store::entries`).
    pub entries: Vec<Row>,
    /// Last `entries()` failure — the pane shows it instead of rows.
    pub load_err: Option<String>,
    /// Active pane.
    pub pane: Pane,
    /// Index into the *filtered* installed row list (clamped on
    /// rebuild).
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
    /// Prompt buffer (`p`/`i`/`:registry` text entry).
    pub prompt_buf: String,
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
    /// `tick` counter — drives the [`RESCAN_TICKS`] fallback and the
    /// spinner frame.
    pub(crate) tick_n: u32,
    /// List rect from the last frame — `render::draw` writes it so
    /// mouse hits map to rows.
    pub board_area: Rect,
    /// First row index visible in `board_area` — the scroll offset
    /// the `List` widget chose to keep `sel` on screen.
    pub viewport_top: usize,
    /// Tab-bar rect when the frame is tall enough to draw one.
    pub tabs_area: Option<Rect>,

    // --- worker jobs (phase 2/3) ---
    /// Job → UI channel; one worker thread per `spawn`.
    work_tx: Sender<Msg>,
    /// UI ← worker channel, drained in `tick`.
    work_rx: Receiver<Msg>,
    /// The running job's label (`None` = idle) — drives the spinner.
    pub busy: Option<String>,
    /// Op log — phase lines, warnings, results; the log strip shows
    /// the tail.
    pub log: VecDeque<String>,
    /// Confirm/consent dialog, if any.
    pub modal: Option<Modal>,
    /// Detail view (`Enter` on an Installed row).
    pub detail: Option<Detail>,

    // --- registry pane ---
    /// Last search results.
    pub reg_results: Vec<RegistryEntry>,
    /// Cursor into `reg_results`.
    pub reg_sel: usize,
    /// The live query — `/` edits it via `input`; debounced search.
    pub reg_query: String,
    /// `:registry` override — `None` → config/env routing.
    pub reg_target: Option<String>,
    /// Resolved target label the results came from (`default`, a
    /// name, or a url) — shown in the pane header.
    pub reg_target_label: String,
    /// Last search failure (sticky until the next search).
    pub reg_err: Option<String>,
    /// When the query text last changed — the debounce clock.
    search_typed_at: Option<Instant>,
    /// The query the in-flight search was spawned with — results
    /// arriving after the query moved on are dropped as stale.
    search_inflight: Option<String>,
}

impl Model {
    /// First load against `ctx`'s roots.
    pub fn open(ctx: &Ctx) -> Result<Self, String> {
        let store_root = ctx.store_root.clone();
        let (work_tx, work_rx) = channel();
        let mut m = Model {
            ctx: ctx.clone(),
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
            prompt_buf: String::new(),
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
            work_tx,
            work_rx,
            busy: None,
            log: VecDeque::new(),
            modal: None,
            detail: None,
            reg_results: Vec::new(),
            reg_sel: 0,
            reg_query: String::new(),
            reg_target: None,
            reg_target_label: "default".to_string(),
            reg_err: None,
            search_typed_at: None,
            search_inflight: None,
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
    /// events into `dirty`, drain worker messages, fire a debounced
    /// registry search, and reload when dirty or on the
    /// [`RESCAN_TICKS`] fallback.
    pub fn tick(&mut self) {
        self.tick_n += 1;
        if !self.status.is_empty() && !self.status_err && self.status_at.elapsed() >= STATUS_TTL {
            self.status.clear();
        }
        self.drain_work();
        self.maybe_search();
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

    // --- worker plumbing ---

    /// Hand `job` to a worker thread. Refuses while another job runs
    /// — ops are cheap enough that queueing buys nothing.
    fn spawn(&mut self, job: Job) {
        if self.busy.is_some() {
            self.say_err(format!(
                "busy — {} still running",
                self.busy.clone().unwrap_or_default()
            ));
            return;
        }
        self.busy = Some(job.label());
        let ctx = self.ctx.clone();
        let tx = self.work_tx.clone();
        let j = job.clone();
        std::thread::spawn(move || {
            // Phase lines stream to the log as they happen.
            let mut log = |line: String| {
                let _ = tx.send(Msg::Log(line));
            };
            let result = ops::run(&ctx, &j, &mut log);
            // A consent refusal isn't a plain failure — hand the job
            // back so the modal can re-run it with consent.
            let result = match result {
                Err(e) if tamias_install::is_driver_consent_refusal(&e) => {
                    let _ = tx.send(Msg::Consent(j.clone(), e.to_string()));
                    return;
                }
                other => other.map_err(|e| e.to_string()),
            };
            let _ = tx.send(Msg::Done(result));
        });
    }

    /// Drain finished worker messages — log lines append, `Done`
    /// clears `busy` and toasts the summary, `Consent` opens the
    /// bundled-driver modal.
    fn drain_work(&mut self) {
        while let Ok(msg) = self.work_rx.try_recv() {
            match msg {
                Msg::Log(line) => self.push_log(line),
                Msg::Consent(job, reason) => {
                    self.busy = None;
                    self.modal = Some(Modal::Consent { job, reason });
                }
                Msg::Done(Ok(Outcome::Text(text))) => {
                    self.busy = None;
                    self.push_log(format!("ok — {text}"));
                    self.say(text);
                    self.reload();
                }
                Msg::Done(Ok(Outcome::Search { target, results })) => {
                    self.busy = None;
                    // Drop stale hits — the query moved on while the
                    // search was in flight; the debounce will refire.
                    let fresh = self
                        .search_inflight
                        .as_deref()
                        .is_some_and(|q| q == self.reg_query.trim());
                    self.search_inflight = None;
                    if fresh {
                        self.reg_target_label = target;
                        self.reg_results = results;
                        self.reg_sel = 0;
                        self.reg_err = None;
                        let n = self.reg_results.len();
                        self.push_log(format!("search {:?}: {n} result(s)", self.reg_query));
                    }
                }
                Msg::Done(Err(e)) => {
                    self.busy = None;
                    self.push_log(format!("error — {e}"));
                    self.say_err(e);
                }
            }
        }
    }

    /// Debounced registry search — fires when the query settled for
    /// [`SEARCH_DEBOUNCE`] (or immediately on `Enter`, which clears
    /// `search_typed_at` via [`Model::search_now`]).
    fn maybe_search(&mut self) {
        if self.pane != Pane::Registry || self.busy.is_some() {
            return;
        }
        let Some(at) = self.search_typed_at else {
            return;
        };
        if at.elapsed() < SEARCH_DEBOUNCE {
            return;
        }
        self.search_typed_at = None;
        self.fire_search();
    }

    /// Spawn the current `reg_query` search (skips empty queries —
    /// the pane shows its hint instead).
    fn fire_search(&mut self) {
        let query = self.reg_query.trim().to_string();
        if query.is_empty() {
            self.reg_results.clear();
            self.reg_err = None;
            self.search_inflight = None;
            return;
        }
        self.search_inflight = Some(query.clone());
        self.spawn(Job::Search {
            query,
            target: self.reg_target.clone(),
        });
    }

    /// `Enter` in the search box — fire now, don't wait out the
    /// debounce.
    fn search_now(&mut self) {
        self.search_typed_at = None;
        self.fire_search();
    }

    /// One line into the op log (capped).
    fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    // --- selection ---

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

    /// The installed row under the cursor, if the filtered list is
    /// non-empty.
    pub fn selected(&self) -> Option<&Row> {
        let vis = self.visible();
        vis.get(self.sel).and_then(|&i| self.entries.get(i))
    }

    /// The registry result under the cursor.
    pub fn reg_selected(&self) -> Option<&RegistryEntry> {
        self.reg_results.get(self.reg_sel)
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

    /// Pane-aware move — Registry walks `reg_results`, everything
    /// else walks the filtered installed rows.
    fn move_sel(&mut self, delta: isize) {
        if self.pane == Pane::Registry {
            let len = self.reg_results.len();
            if len == 0 {
                self.reg_sel = 0;
                return;
            }
            self.reg_sel = (self.reg_sel as isize + delta).clamp(0, len as isize - 1) as usize;
            return;
        }
        let len = self.visible().len();
        if len == 0 {
            self.sel = 0;
            return;
        }
        self.sel = (self.sel as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    fn switch_pane(&mut self, pane: Pane) {
        self.pane = pane;
        self.detail = None;
        self.sel_label = self.selected().map(|r| r.entry.label());
        // Leaving the registry pane mid-search keeps the query —
        // results stay; re-entering resumes where it left off.
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

    // --- detail ---

    /// `Enter` on an Installed row — open the detail view (or close
    /// it when already open on that row).
    fn toggle_detail(&mut self) {
        if self.detail.is_some() {
            self.detail = None;
            return;
        }
        let Some(row) = self.selected() else {
            return;
        };
        let entry = row.entry.clone();
        let (manifest, manifest_err) = match entry.manifest() {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e.to_string())),
        };
        self.detail = Some(Detail {
            entry,
            manifest,
            manifest_err,
        });
    }

    // --- confirms / prompts ---

    /// Open the confirm modal for a mutating job on `name` — the body
    /// warns about driver teardown when the record's kind isn't
    /// `files` (serve's remove rule, surfaced up front).
    fn confirm_job(&mut self, verb: &str, job: Job) {
        // Resolve the entry for the teardown warning + label.
        let name = match &job {
            Job::Remove(n) | Job::Update { name: n, .. } | Job::Pin { name: n, .. } => n.clone(),
            _ => String::new(),
        };
        let mut body = Vec::new();
        if !name.is_empty() {
            if let Ok(entry) = Store::new(&self.store_root).find(&name) {
                let kind = entry.record.kind.as_deref().unwrap_or("files");
                if kind != "files" && matches!(job, Job::Remove(_)) {
                    body.push(format!(
                        "type '{kind}' — the driver uninstall may leave provider resources"
                    ));
                }
            }
        }
        self.modal = Some(Modal::Confirm {
            title: format!("{verb} — sure? [y/n]"),
            body,
            job,
        });
    }

    /// `y` on a modal — Confirm spawns its job; Consent re-runs the
    /// refused job with `allow_drivers`.
    fn modal_yes(&mut self) {
        match self.modal.take() {
            Some(Modal::Confirm { job, .. }) => self.spawn(job),
            Some(Modal::Consent { job, .. }) => self.spawn(job.with_drivers_allowed()),
            None => {}
        }
    }

    /// Run a `p`/`i`/`:registry` prompt's buffer.
    fn run_prompt(&mut self) {
        let Some(kind) = (match &self.mode {
            Mode::Prompt(k) => Some(k.clone()),
            _ => None,
        }) else {
            return;
        };
        let text = self.prompt_buf.trim().to_string();
        self.mode = Mode::Normal;
        self.prompt_buf.clear();
        match kind {
            PromptKind::Pin { name } => {
                if text.is_empty() {
                    self.say_err("pin: empty ref");
                } else {
                    self.confirm_job(
                        "pin",
                        Job::Pin {
                            name,
                            reference: text,
                        },
                    );
                }
            }
            PromptKind::InstallRef => {
                if text.is_empty() {
                    self.say_err("install: empty ref");
                } else {
                    self.confirm_job(
                        "install",
                        Job::Install {
                            reference: text,
                            allow_drivers: false,
                        },
                    );
                }
            }
            PromptKind::RegistryTarget => {
                if text.is_empty() {
                    self.reg_target = None;
                    self.say("registry target: default routing");
                } else {
                    self.reg_target = Some(text.clone());
                    self.say(format!("registry target: {text}"));
                }
                // Re-run the current query against the new target.
                if !self.reg_query.trim().is_empty() {
                    self.fire_search();
                }
            }
        }
    }

    /// Install the selected registry result — resolve its `.tgz` to
    /// an installable ref (fs path for local registries, pinned http
    /// url for http/cf), then the usual confirm modal.
    fn install_registry_result(&mut self) {
        let Some(entry) = self.reg_selected().cloned() else {
            self.say_err("no registry result selected");
            return;
        };
        match ops::entry_install_ref(&self.ctx, &entry, self.reg_target.as_deref()) {
            Ok(reference) => {
                self.confirm_job(
                    "install",
                    Job::Install {
                        reference,
                        allow_drivers: false,
                    },
                );
            }
            Err(e) => self.say_err(e.to_string()),
        }
    }

    // --- verbs on the selected installed row ---

    /// The selected row's store label, or an error toast.
    fn selected_label(&mut self) -> Option<String> {
        match self.selected() {
            Some(row) => Some(row.entry.label()),
            None => {
                self.say_err("nothing selected");
                None
            }
        }
    }

    /// `v` — verify (read-only; runs straight away).
    fn verb_verify(&mut self) {
        if let Some(label) = self.selected_label() {
            self.spawn(Job::Verify(label));
        }
    }

    /// `u` — update the selected entry from its recorded ref.
    fn verb_update(&mut self) {
        if let Some(label) = self.selected_label() {
            self.confirm_job(
                "update",
                Job::Update {
                    name: label,
                    allow_drivers: false,
                },
            );
        }
    }

    /// `p` — pin prompt for the selected entry.
    fn verb_pin(&mut self) {
        if let Some(label) = self.selected_label() {
            self.prompt_buf.clear();
            self.mode = Mode::Prompt(PromptKind::Pin { name: label });
        }
    }

    /// `r` — remove confirm (teardown warning for non-`files`).
    fn verb_remove(&mut self) {
        if let Some(label) = self.selected_label() {
            self.confirm_job("remove", Job::Remove(label));
        }
    }

    /// `c`/`C` — cache clean confirm (>30d / all).
    fn verb_cache_clean(&mut self, all: bool) {
        self.confirm_job(
            if all {
                "cache clean --all"
            } else {
                "cache clean >30d"
            },
            Job::CacheClean { all },
        );
    }

    /// `d` — doctor summary.
    fn verb_doctor(&mut self) {
        self.spawn(Job::Doctor);
    }

    /// `i` — install-by-ref prompt (Installed) or install the
    /// selected registry result (Registry).
    fn verb_install(&mut self) {
        match self.pane {
            Pane::Registry => self.install_registry_result(),
            _ => {
                self.prompt_buf.clear();
                self.mode = Mode::Prompt(PromptKind::InstallRef);
            }
        }
    }

    // --- `:` commands ---

    /// `:` command line — parse + execute. `q` is handled by the
    /// caller (it returns the quit flag).
    fn run_command(&mut self) {
        let cmd = self.cmdline.trim().trim_start_matches(':').to_string();
        match parse_command(&cmd) {
            Ok(Cmd::Quit) => {}
            Ok(Cmd::Reload) => {
                self.reload();
                self.say("reloaded");
            }
            Ok(Cmd::Help) => self.mode = Mode::Help,
            Ok(Cmd::Install(reference)) => {
                self.confirm_job(
                    "install",
                    Job::Install {
                        reference,
                        allow_drivers: false,
                    },
                );
            }
            Ok(Cmd::Remove(name)) => {
                self.confirm_job("remove", Job::Remove(name));
            }
            Ok(Cmd::Verify(name)) => self.spawn(Job::Verify(name)),
            Ok(Cmd::Update(name)) => match name {
                Some(name) => self.confirm_job(
                    "update",
                    Job::Update {
                        name,
                        allow_drivers: false,
                    },
                ),
                None => {
                    let n = self.entries.len();
                    if n == 0 {
                        self.say_err("nothing installed");
                    } else {
                        self.confirm_job(
                            "update all",
                            Job::UpdateAll {
                                allow_drivers: false,
                            },
                        );
                    }
                }
            },
            Ok(Cmd::Pin(name, reference)) => {
                self.confirm_job("pin", Job::Pin { name, reference });
            }
            Ok(Cmd::Search(query)) => {
                self.switch_pane(Pane::Registry);
                self.reg_query = query.clone();
                self.input = query;
                self.search_typed_at = None;
                self.search_now();
            }
            Ok(Cmd::Registry(name)) => match name {
                Some(name) => {
                    self.reg_target = Some(name.clone());
                    self.say(format!("registry target: {name}"));
                    if !self.reg_query.trim().is_empty() {
                        self.fire_search();
                    }
                }
                None => {
                    self.prompt_buf.clear();
                    self.mode = Mode::Prompt(PromptKind::RegistryTarget);
                }
            },
            Ok(Cmd::Clean(all)) => self.verb_cache_clean(all),
            Ok(Cmd::Doctor) => self.verb_doctor(),
            Err(e) if e.is_empty() => {}
            Err(e) => self.say_err(e),
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

    /// `/` mode keys on Installed — live filter; `Enter` keeps it,
    /// `esc` clears.
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

    /// `/` mode keys on Registry — the query edits `reg_query` live;
    /// `tick` debounces the actual search, `Enter` fires it now.
    fn key_search(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.search_now();
                false
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.reg_query.clear();
                self.reg_results.clear();
                self.reg_err = None;
                self.search_typed_at = None;
                false
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.reg_query = self.input.clone();
                self.search_typed_at = Some(Instant::now());
                false
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.reg_query = self.input.clone();
                self.search_typed_at = Some(Instant::now());
                false
            }
            _ => false,
        }
    }

    /// `p`/`i`/`:registry` prompt keys — text entry; `Enter` runs.
    fn key_prompt(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.run_prompt();
                false
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.prompt_buf.clear();
                false
            }
            KeyCode::Backspace => {
                self.prompt_buf.pop();
                false
            }
            KeyCode::Char(c) => {
                self.prompt_buf.push(c);
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

    /// Modal keys — `y` confirms, `n`/`esc` cancels (default `n`).
    /// Returns `true` when the key was consumed by the modal.
    fn key_modal(&mut self, k: KeyEvent) -> bool {
        if self.modal.is_none() {
            return false;
        }
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.modal_yes();
                true
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.modal = None;
                true
            }
            _ => true, // swallow everything else while a modal is up
        }
    }

    /// One key event → `true` when the app should quit. Ctrl-C is
    /// the force-quit and works from every mode (even under a modal).
    pub fn key(&mut self, k: KeyEvent) -> bool {
        self.dismiss_error();
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if self.key_modal(k) {
            return false;
        }
        match self.mode {
            Mode::Filter => return self.key_filter(k),
            Mode::Search => return self.key_search(k),
            Mode::Command => return self.key_command(k),
            Mode::Prompt(_) => return self.key_prompt(k),
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
            KeyCode::Enter if self.pane == Pane::Installed => self.toggle_detail(),
            KeyCode::Esc | KeyCode::Left if self.detail.is_some() => self.detail = None,
            KeyCode::Char('h') | KeyCode::BackTab => {
                if self.detail.is_some() {
                    self.detail = None;
                } else {
                    self.step_pane(-1);
                }
            }
            KeyCode::Tab | KeyCode::Char('l') => self.step_pane(1),
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_sel(1);
                self.sync_sel_label();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_sel(-1);
                self.sync_sel_label();
            }
            KeyCode::Char('g') => {
                if self.pane == Pane::Registry {
                    self.reg_sel = 0;
                } else {
                    self.sel = 0;
                }
            }
            KeyCode::Char('G') => {
                if self.pane == Pane::Registry {
                    self.reg_sel = self.reg_results.len().saturating_sub(1);
                } else {
                    self.sel = self.visible().len().saturating_sub(1);
                    self.sync_sel_label();
                }
            }
            KeyCode::Char('/') => {
                self.mode = if self.pane == Pane::Registry {
                    Mode::Search
                } else {
                    Mode::Filter
                };
                self.input = if self.pane == Pane::Registry {
                    self.reg_query.clone()
                } else {
                    self.filter.clone()
                };
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
            // --- verbs ---
            KeyCode::Char('v') if self.pane == Pane::Installed => self.verb_verify(),
            KeyCode::Char('u') if self.pane == Pane::Installed => self.verb_update(),
            KeyCode::Char('U') if self.pane == Pane::Installed => self.confirm_job(
                "update all",
                Job::UpdateAll {
                    allow_drivers: false,
                },
            ),
            KeyCode::Char('p') if self.pane == Pane::Installed => self.verb_pin(),
            KeyCode::Char('r') if self.pane == Pane::Installed => self.verb_remove(),
            KeyCode::Char('c') if self.pane == Pane::Installed => self.verb_cache_clean(false),
            KeyCode::Char('C') if self.pane == Pane::Installed => self.verb_cache_clean(true),
            KeyCode::Char('d') if self.pane == Pane::Installed => self.verb_doctor(),
            KeyCode::Char('i') => self.verb_install(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
        false
    }

    /// Mouse — wheel scrolls the selection, a click on the tab bar
    /// switches panes, a click on a row selects it.
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
                    if self.pane == Pane::Registry {
                        if idx < self.reg_results.len() {
                            self.reg_sel = idx;
                        }
                    } else {
                        let vis = self.visible();
                        if idx < vis.len() {
                            self.sel = idx;
                            self.sync_sel_label();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn command_parsing() {
        assert_eq!(parse_command("q").unwrap(), Cmd::Quit);
        assert_eq!(
            parse_command("install ./pkg").unwrap(),
            Cmd::Install("./pkg".into())
        );
        assert_eq!(
            parse_command("i git:x#main").unwrap(),
            Cmd::Install("git:x#main".into())
        );
        assert_eq!(parse_command("remove w").unwrap(), Cmd::Remove("w".into()));
        assert_eq!(parse_command("verify w").unwrap(), Cmd::Verify("w".into()));
        assert_eq!(parse_command("update").unwrap(), Cmd::Update(None));
        assert_eq!(
            parse_command("update @s/w@1").unwrap(),
            Cmd::Update(Some("@s/w@1".into()))
        );
        assert_eq!(
            parse_command("pin w git:x#main").unwrap(),
            Cmd::Pin("w".into(), "git:x#main".into())
        );
        assert!(parse_command("pin w").is_err());
        assert_eq!(
            parse_command("search wid").unwrap(),
            Cmd::Search("wid".into())
        );
        assert_eq!(
            parse_command("registry mine").unwrap(),
            Cmd::Registry(Some("mine".into()))
        );
        assert_eq!(parse_command("registry").unwrap(), Cmd::Registry(None));
        assert_eq!(parse_command("clean").unwrap(), Cmd::Clean(false));
        assert_eq!(parse_command("clean all").unwrap(), Cmd::Clean(true));
        assert_eq!(parse_command("doctor").unwrap(), Cmd::Doctor);
        assert!(parse_command("bogus").is_err());
        assert!(parse_command("install").is_err());
        assert!(parse_command("").is_err());
    }

    #[test]
    fn job_labels_and_consent() {
        let j = Job::Install {
            reference: "./x".into(),
            allow_drivers: false,
        };
        assert!(j.label().contains("./x"));
        match j.with_drivers_allowed() {
            Job::Install { allow_drivers, .. } => assert!(allow_drivers),
            _ => panic!("wrong variant"),
        }
        let v = Job::Verify("w".into());
        assert!(matches!(v.with_drivers_allowed(), Job::Verify(_)));
    }
}
