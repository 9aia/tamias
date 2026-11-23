//! The TUI model — panes, cursor, modes, modals, and the store reads
//! every key maps to. The Home pane is the landing view (wordmark,
//! stats, recently installed); the Installed pane lists
//! `tamias-store-local` entries with a client-side `/` filter; `Enter` opens the package
//! page (`Detail`) on either the Installed row or a Registry result;
//! the Registry pane searches a registry as-you-type; Services,
//! Manifest, and Author cover provider lifecycle, the project `deps:`
//! form, and pack/publish authoring.
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
use std::process::Child;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tamias_core::{Manifest, StoreEntry};
use tamias_provider::ServiceRow;
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
    /// The payload manifest declares `sealed:` — a ciphertext-only
    /// pack (ADR-0004). Measured once at load like `size`; a manifest
    /// read failure just means no badge.
    pub sealed: bool,
}

/// Which pane the tab bar shows. `ALL` fixes the `1`-`6` jump order
/// and tab labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Home — wordmark, stats strip, recently-installed list (the
    /// landing pane; `j`/`k` cursor + `Enter` opens a package page
    /// via the Installed pane — see [`Model::open_home_detail`]).
    Home,
    /// Store entries — name/type/source/digest/size (full resolved +
    /// digest shas live on the package page's provenance section).
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
            Pane::Home => "HOME",
            Pane::Installed => "INSTALLED",
            Pane::Registry => "REGISTRY",
            Pane::Services => "SERVICES",
            Pane::Manifest => "MANIFEST",
            Pane::Author => "AUTHOR",
        }
    }

    /// Tab order for `h`/`l`, `Tab`/`Shift-Tab`, and `1`-`6`.
    pub const ALL: [Pane; 6] = [
        Pane::Home,
        Pane::Installed,
        Pane::Registry,
        Pane::Services,
        Pane::Manifest,
        Pane::Author,
    ];
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
    /// Manifest pane `a` — a dep ref to install + record (`tamias
    /// add` equivalent).
    AddDep,
    /// Manifest pane `e` — a new ref replacing dep `old`.
    SetDep {
        /// The dep ref being replaced.
        old: String,
    },
    /// Services pane `d` — optional driver `--config` path for
    /// `deploy <name>` (empty = none).
    DeployConfig {
        /// Store label being deployed.
        name: String,
    },
    /// Author pane `p` — `bundle pack` spec: `dir [out.tgz]`.
    PackSpec,
    /// Author pane `P` — `publish` spec: `dir [registry]`.
    PublishSpec,
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
    /// `L`/`:log` — full op-log overlay; `j`/`k` scroll, `g`/`G` jump
    /// the ends, `esc`/`q`/`L` closes. Swallows every other key.
    Log,
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
    /// A stream job (`run`/`logs`) spawned its child — the handle
    /// `x` kills. Readers keep pumping lines after this arrives.
    StreamChild(Arc<Mutex<Child>>),
    /// The package page's background versions fetch landed — `key`
    /// is the entry label it was spawned for; [`Model::drain_work`]
    /// drops it when the page moved on. Carries its own result (not
    /// an [`Outcome`]) because it runs on a dedicated thread outside
    /// `spawn`/`busy` — it must never clear or wait on a user job.
    Versions {
        /// `entry.label()` of the page that asked.
        key: String,
        /// The sibling-version list, or the fetch error.
        result: Result<Vec<RegistryEntry>, String>,
    },
    /// The job finished — summary text or typed search results.
    Done(Result<Outcome, String>),
}

/// The package page — `Enter` on an Installed row or a Registry
/// result opens it (the npm-inspired anatomy lives in
/// `render::draw_detail`). Both sources share the layout; the variant
/// decides which fields exist — a payload manifest + store
/// provenance for installed entries, index metadata + the resolved
/// install ref for registry results.
#[derive(Debug)]
pub enum Detail {
    /// An installed package — the store entry plus its payload
    /// manifest, loaded once on open (a local file read, cheap
    /// enough to stay on the event loop).
    Installed {
        /// The entry the page was opened on (a clone — selection may
        /// move underneath without disturbing the view).
        entry: StoreEntry,
        /// `entry.manifest()` when the payload carries one — boxed:
        /// `Manifest` is the enum's size driver.
        manifest: Option<Box<Manifest>>,
        /// Why the manifest is absent (`None` manifest + `Some` err).
        manifest_err: Option<String>,
    },
    /// A registry search result — no payload manifest exists until
    /// install; the index entry carries display metadata, the
    /// installable ref is resolved once on open (config read + ref
    /// grammar, same cheap-file-read posture as the manifest), and
    /// the sibling versions arrive asynchronously (a dedicated
    /// fetch thread — see [`Model::fetch_versions`]).
    Registry {
        /// The index entry under the page.
        entry: RegistryEntry,
        /// `ops::entry_install_ref` — the `tamias install` argument.
        install_ref: Option<String>,
        /// Why the ref couldn't resolve (`None` ref + `Some` err).
        install_err: Option<String>,
        /// Every indexed version of this `(name, scope)` — `None`
        /// while the fetch is in flight (the page shows a `versions
        /// …` placeholder); `Some` once it lands, possibly just this
        /// entry.
        versions: Option<Vec<RegistryEntry>>,
        /// Why the fetch failed (`None` versions + `Some` err renders
        /// a muted line — the page itself stays usable).
        versions_err: Option<String>,
    },
}

impl Detail {
    /// Page title — `@scope/name@version` for both sources.
    pub fn label(&self) -> String {
        match self {
            Self::Installed { entry, .. } => entry.label(),
            Self::Registry { entry, .. } => entry.label(),
        }
    }

    /// The one-line description — payload manifest first, then the
    /// install-time record snapshot or the index entry.
    pub fn description(&self) -> Option<&str> {
        match self {
            Self::Installed {
                entry, manifest, ..
            } => manifest
                .as_ref()
                .and_then(|m| m.description.as_deref())
                .or(entry.record.description.as_deref()),
            Self::Registry { entry, .. } => entry.description.as_deref(),
        }
    }

    /// Content-type chip — manifest `type:`/record `kind`/index
    /// `kind`, in that precedence.
    pub fn kind(&self) -> Option<&str> {
        match self {
            Self::Installed {
                entry, manifest, ..
            } => manifest
                .as_ref()
                .and_then(|m| m.kind.as_deref())
                .or(entry.record.kind.as_deref()),
            Self::Registry { entry, .. } => entry.kind.as_deref(),
        }
    }

    /// Scope chip (`@scope` already sits inside `label()` — the chip
    /// keeps it scannable in the header row).
    pub fn scope(&self) -> Option<&str> {
        match self {
            Self::Installed { entry, .. } => entry.record.scope.as_deref(),
            Self::Registry { entry, .. } => entry.scope.as_deref(),
        }
    }

    /// License chip — manifest/record/index `license:`.
    pub fn license(&self) -> Option<&str> {
        match self {
            Self::Installed {
                entry, manifest, ..
            } => manifest
                .as_ref()
                .and_then(|m| m.license.as_deref())
                .or(entry.record.license.as_deref()),
            Self::Registry { entry, .. } => entry.license.as_deref(),
        }
    }

    /// Trust-tier word for the colored chip — the record's `trust`
    /// spelling for installed entries; `signed` when the index entry
    /// carries a signature (the install-time tier isn't known until
    /// the package lands).
    pub fn trust(&self) -> Option<String> {
        match self {
            Self::Installed { entry, .. } => entry.record.trust.clone(),
            Self::Registry { entry, .. } => entry.signature.is_some().then(|| "signed".to_string()),
        }
    }

    /// `sealed:` ciphertext-only badge — installed manifests only.
    pub fn sealed(&self) -> bool {
        matches!(self, Self::Installed { manifest: Some(m), .. } if m.sealed.is_some())
    }
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
    /// `:services` — refresh the Services pane's unit states.
    Services,
    /// `:service <verb> <unit>` — start/stop/restart/enable/disable.
    Service {
        /// The systemctl verb.
        action: String,
        /// Unit base name.
        unit: String,
    },
    /// `:run <pkg> [service]` — foreground provider-service exec.
    Run {
        /// Store label.
        package: String,
        /// Provider service name (`None` → sole-service pick).
        service: Option<String>,
    },
    /// `:logs <pkg|unit> [-f]` — journal tail/follow.
    Logs {
        /// Store label or unit base name.
        name: String,
        /// Follow mode.
        follow: bool,
    },
    /// `:deploy <name>` — provider deploy (driver + units).
    Deploy(String),
    /// `:pack <dir> [out]` — `bundle pack`.
    Pack(String),
    /// `:publish <dir> [registry]` — publish flow (confirm modal).
    Publish(String),
    /// `:registries` — configured registry names into the log.
    Registries,
    /// `:manifest` — (re)load the project manifest.
    Manifest,
    /// `:sync [--from-lock]` — reconcile the project dep set
    /// (`--from-lock` treats `tamias.lock` as the spec; Manifest pane
    /// `s`/`S` too).
    Sync {
        /// Install `tamias.lock` entries verbatim.
        from_lock: bool,
    },
    /// `:log` — open the op-log overlay (`L`).
    Log,
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
        "log" => Ok(Cmd::Log),
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
        "services" => Ok(Cmd::Services),
        "service" => {
            // `:service <verb> <unit>` — verb then unit base name.
            let mut it = rest.splitn(2, char::is_whitespace);
            let action = it.next().unwrap_or("");
            let unit = it.next().unwrap_or("").trim();
            if !matches!(action, "start" | "stop" | "restart" | "enable" | "disable")
                || unit.is_empty()
            {
                Err("usage: :service <start|stop|restart|enable|disable> <unit>".into())
            } else {
                Ok(Cmd::Service {
                    action: action.to_string(),
                    unit: unit.to_string(),
                })
            }
        }
        "run" => {
            // `:run <pkg> [service]` — two tokens max; the service
            // name can't contain spaces.
            let mut it = rest.split_whitespace();
            let package = it.next().unwrap_or("");
            let service = it.next();
            if package.is_empty() || it.next().is_some() {
                Err("usage: :run <pkg> [service]".into())
            } else {
                Ok(Cmd::Run {
                    package: package.to_string(),
                    service: service.map(str::to_string),
                })
            }
        }
        "logs" => {
            // `:logs <name> [-f]` — trailing -f/--follow flips follow.
            let mut it = rest.split_whitespace();
            let name = it.next().unwrap_or("");
            let follow = it.any(|t| t == "-f" || t == "--follow" || t == "-F");
            if name.is_empty() {
                Err("usage: :logs <pkg|unit> [-f]".into())
            } else {
                Ok(Cmd::Logs {
                    name: name.to_string(),
                    follow,
                })
            }
        }
        "deploy" => {
            if rest.is_empty() {
                Err("usage: :deploy <name>".into())
            } else {
                Ok(Cmd::Deploy(rest))
            }
        }
        "pack" => {
            if rest.is_empty() {
                Err("usage: :pack <dir> [out]".into())
            } else {
                Ok(Cmd::Pack(rest))
            }
        }
        "publish" => {
            if rest.is_empty() {
                Err("usage: :publish <dir> [registry]".into())
            } else {
                Ok(Cmd::Publish(rest))
            }
        }
        "registries" | "registry-list" => Ok(Cmd::Registries),
        "manifest" => Ok(Cmd::Manifest),
        "sync" | "s" => {
            // `:sync [--from-lock]` — the only flag; anything else is
            // a usage error.
            let mut from_lock = false;
            for tok in rest.split_whitespace() {
                match tok {
                    "--from-lock" => from_lock = true,
                    _ => return Err("usage: :sync [--from-lock]".into()),
                }
            }
            Ok(Cmd::Sync { from_lock })
        }
        other => Err(format!("unknown command ':{other}' — `:help` lists verbs")),
    }
}

/// Display word for a [`tamias_provider::UnitAction`] — confirm-modal
/// titles and `:service` feedback.
fn unit_action_word(action: tamias_provider::UnitAction) -> &'static str {
    match action {
        tamias_provider::UnitAction::Start => "start",
        tamias_provider::UnitAction::Stop => "stop",
        tamias_provider::UnitAction::Restart => "restart",
        tamias_provider::UnitAction::Enable => "enable",
        tamias_provider::UnitAction::Disable => "disable",
    }
}

/// `:service <verb>` word → action (`parse_command` already
/// whitelists, this is the typed bridge).
fn unit_action_from_word(word: &str) -> Option<tamias_provider::UnitAction> {
    use tamias_provider::UnitAction as A;
    Some(match word {
        "start" => A::Start,
        "stop" => A::Stop,
        "restart" => A::Restart,
        "enable" => A::Enable,
        "disable" => A::Disable,
        _ => return None,
    })
}

/// The whole TUI state.
pub struct Model {
    /// Roots the ops run against (cloned into every worker) —
    /// `pub(crate)` so the Home pane's stats strip can show them.
    pub(crate) ctx: Ctx,
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
    /// Cursor into the Home pane's recently-installed list
    /// ([`Model::recent_installed`]) — `j`/`k`/wheel move it, `Enter`
    /// opens the highlighted entry's package page.
    pub home_sel: usize,
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
    /// Live `:` completions for the token under the cursor
    /// ([`crate::complete`]) — refreshed on every buffer edit.
    pub cmd_completions: Vec<crate::complete::Candidate>,
    /// Tab-selected candidate index — `usize::MAX` = none applied
    /// yet, so the first `Tab` lands on candidate 0.
    pub cmd_sel: usize,
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
    /// Home's recent-list data-row rect from the last frame — `y`
    /// is the first row, `height` the rendered rows; a click inside
    /// maps onto [`Model::recent_installed`]. `Rect::default()` when
    /// Home isn't the drawn pane or the list is empty.
    pub home_area: Rect,

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
    /// Package page (`Enter` on an Installed row or Registry result)
    /// — see [`Detail`].
    pub detail: Option<Detail>,
    /// Page scroll offset — `j`/`k`/arrows move it while `detail` is
    /// open; clamped to the body height each frame.
    pub detail_scroll: usize,
    /// Versions-focus mode on a Registry package page — `v` toggles
    /// it once the sibling-version list has landed; while set, `j`/`k`
    /// move [`Model::versions_sel`] instead of scrolling the page and
    /// `i` installs the selected version. Meaningless (and cleared)
    /// for Installed pages.
    pub versions_focus: bool,
    /// Cursor into the page's `versions` list while `versions_focus`
    /// is set — clamped to the list length on every move.
    pub versions_sel: usize,
    /// Log-overlay scroll offset — the top line of `log` on screen
    /// while `mode` is [`Mode::Log`]; clamped to the view each frame
    /// like `detail_scroll`.
    pub log_scroll: usize,
    /// Inner height of the log overlay as last drawn — `PgUp`/`PgDn`
    /// page by it and `open_log` aims the tail at it (0 until the
    /// first frame).
    pub(crate) log_view_h: usize,

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

    // --- services pane (phase 4) ---
    /// Provider service rows (package/service/unit/state/exec) —
    /// refreshed by [`Job::ServicesRefresh`].
    pub svc_rows: Vec<ServiceRow>,
    /// Cursor into `svc_rows`.
    pub svc_sel: usize,
    /// Services rows are stale — pane entry, unit mutations, and
    /// `:services` set it; `tick` turns it into a refresh job.
    pub svc_stale: bool,
    /// Last services-refresh failure.
    pub svc_err: Option<String>,
    /// The running stream's killable child (`run`/`logs -f`) — `x`
    /// kills it; quitting the app kills it too.
    pub stream_child: Option<Arc<Mutex<Child>>>,

    // --- manifest pane (phase 5) ---
    /// The project manifest path the deps form edits.
    pub manifest_path: PathBuf,
    /// The parsed project manifest (`deps:` editable; the rest is
    /// rendered read-only).
    pub project: Option<Manifest>,
    /// Why the manifest is absent/unloadable.
    pub man_err: Option<String>,
    /// Cursor into `project.deps`.
    pub man_sel: usize,

    // --- author pane (phase 6) ---
    /// Configured registry names for the publish picker — loaded on
    /// pane entry (`ops::registry_names`).
    pub author_regs: Vec<(String, String)>,
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
            pane: Pane::Home,
            sel: 0,
            home_sel: 0,
            sel_label: None,
            filter: String::new(),
            mode: Mode::Normal,
            input: String::new(),
            cmdline: String::new(),
            cmd_completions: Vec::new(),
            cmd_sel: usize::MAX,
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
            home_area: Rect::default(),
            work_tx,
            work_rx,
            busy: None,
            log: VecDeque::new(),
            modal: None,
            detail: None,
            detail_scroll: 0,
            versions_focus: false,
            versions_sel: 0,
            log_scroll: 0,
            log_view_h: 0,
            reg_results: Vec::new(),
            reg_sel: 0,
            reg_query: String::new(),
            reg_target: None,
            reg_target_label: "default".to_string(),
            reg_err: None,
            search_typed_at: None,
            search_inflight: None,
            svc_rows: Vec::new(),
            svc_sel: 0,
            svc_stale: true,
            svc_err: None,
            stream_child: None,
            manifest_path: PathBuf::from("tamias.yaml"),
            project: None,
            man_err: None,
            man_sel: 0,
            author_regs: Vec::new(),
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
                        let sealed = entry.manifest().ok().is_some_and(|m| m.sealed.is_some());
                        Row {
                            entry,
                            size,
                            sealed,
                        }
                    })
                    .collect();
                self.load_err = None;
            }
            Err(e) => {
                self.load_err = Some(e.to_string());
            }
        }
        self.rebuild_sel(keep.as_deref(), at);
        // The recent list shrank under the cursor (uninstall, or a
        // fresh install reordering it) — clamp like every other sel.
        let n = self.recent_installed().len();
        self.home_sel = if n == 0 { 0 } else { self.home_sel.min(n - 1) };
        self.arm_watch();
        self.load_manifest();
    }

    /// (Re)read the project manifest for the Manifest pane — a tiny
    /// file read, cheap enough for the event loop (same posture as the
    /// detail view's `entry.manifest()`). Runs on every reload so
    /// dep edits — ours or another `tamias add`'s — show up without a
    /// keypress; `:manifest` forces one.
    pub fn load_manifest(&mut self) {
        match ops::project_manifest() {
            Ok((path, manifest)) => {
                self.manifest_path = path;
                self.man_err = None;
                let n = manifest.deps.len();
                self.project = Some(manifest);
                self.man_sel = self.man_sel.min(n.saturating_sub(1));
            }
            Err(e) => {
                self.man_err = Some(e.to_string());
                self.project = None;
            }
        }
    }

    /// Kill the running stream child (`run`/`logs -f`) — `x` and app
    /// shutdown both land here. The worker's `try_wait` loop observes
    /// the exit and reports `Done`.
    pub fn kill_stream(&mut self) {
        if let Some(child) = self.stream_child.take() {
            if let Ok(mut c) = child.lock() {
                let _ = c.kill();
            }
            self.push_log("killed stream".to_string());
        }
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
        // A stale Services table becomes a refresh job — pane entry,
        // unit mutations, and `:services` all set the flag; `busy`
        // gates it so refreshes never queue behind ops.
        if self.pane == Pane::Services && self.svc_stale && self.busy.is_none() {
            self.svc_stale = false;
            self.spawn(Job::ServicesRefresh);
        }
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
        if j.is_stream() {
            // Stream jobs own a child + reader threads; the handle
            // comes back as Msg::StreamChild for `x` to kill.
            std::thread::spawn(move || {
                let result = ops::stream(&ctx, &j, &tx);
                let _ = tx.send(Msg::Done(result.map_err(|e| e.to_string())));
            });
            return;
        }
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
                Msg::StreamChild(child) => {
                    self.stream_child = Some(child);
                }
                Msg::Versions { key, result } => {
                    // Apply only to the page that asked — a closed or
                    // replaced page drops the result (the same
                    // staleness rule as `search_inflight`). This arm
                    // never touches `busy`: the fetch ran outside
                    // `spawn`, so a user job may be in flight.
                    if let Some(Detail::Registry {
                        entry,
                        versions,
                        versions_err,
                        ..
                    }) = &mut self.detail
                    {
                        if entry.label() == key {
                            match result {
                                Ok(list) => *versions = Some(list),
                                Err(e) => *versions_err = Some(e),
                            }
                        }
                    }
                }
                Msg::Done(Ok(Outcome::Text(text))) => {
                    self.busy = None;
                    self.stream_child = None;
                    self.svc_stale = true;
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
                Msg::Done(Ok(Outcome::Services(rows))) => {
                    self.busy = None;
                    self.svc_err = None;
                    self.svc_rows = rows;
                    self.svc_sel = self.svc_sel.min(self.svc_rows.len().saturating_sub(1));
                    self.push_log(format!("services: {} row(s)", self.svc_rows.len()));
                }
                Msg::Done(Err(e)) => {
                    self.busy = None;
                    self.stream_child = None;
                    self.svc_stale = true;
                    if self.pane == Pane::Services {
                        self.svc_err = Some(e.clone());
                    }
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

    /// One line into the op log (capped). While the log overlay is
    /// parked at the tail it follows new lines; scrolled up it stays
    /// put (`G` jumps back down).
    fn push_log(&mut self, line: String) {
        let at_tail =
            self.mode == Mode::Log && self.log_scroll + self.log_view_h.max(1) >= self.log.len();
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
        if at_tail {
            self.log_scroll = self.log_max_scroll();
        }
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
                let sealed = if r.sealed { " sealed" } else { "" };
                let hay = format!(
                    "{} {} {} {} {}{}",
                    r.entry.label(),
                    rec.kind.as_deref().unwrap_or(""),
                    rec.source_ref,
                    rec.resolved,
                    rec.digest,
                    sealed
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

    /// Indices into `entries` for the Home pane's "recently installed"
    /// list — `installed_at` desc (stable, so ties keep the store's
    /// label order), top 8. The renderer and the `home_sel` cursor
    /// share this ordering so they can never disagree.
    pub fn recent_installed(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.entries.len()).collect();
        idx.sort_by(|&a, &b| {
            self.entries[b]
                .entry
                .record
                .installed_at
                .cmp(&self.entries[a].entry.record.installed_at)
        });
        idx.truncate(8);
        idx
    }

    /// The recent-install row under the Home cursor, if any.
    pub fn home_selected(&self) -> Option<&Row> {
        self.recent_installed()
            .get(self.home_sel)
            .and_then(|&i| self.entries.get(i))
    }

    /// The installed version label for a `(name, scope)` pair — the
    /// "already installed" marker registry rows and package pages
    /// show (`0.2.0`, or the resolved-sha short form when the record
    /// is unversioned). `None` when no store entry matches; version
    /// may differ from the registry row's — that's the point.
    pub fn installed_label_for(&self, name: &str, scope: Option<&str>) -> Option<String> {
        self.entries.iter().find_map(|r| {
            let rec = &r.entry.record;
            if rec.name == name && rec.scope.as_deref() == scope {
                Some(rec.version.clone().unwrap_or_else(|| {
                    let sha = rec
                        .resolved
                        .strip_prefix("sha256:")
                        .unwrap_or(&rec.resolved);
                    sha.chars().take(12).collect()
                }))
            } else {
                None
            }
        })
    }

    /// The Services row under the cursor.
    pub fn svc_selected(&self) -> Option<&ServiceRow> {
        self.svc_rows.get(self.svc_sel)
    }

    /// The dep ref under the Manifest cursor.
    pub fn dep_selected(&self) -> Option<String> {
        self.project
            .as_ref()
            .and_then(|m| m.deps.get(self.man_sel).cloned())
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
        match self.pane {
            Pane::Registry => {
                let len = self.reg_results.len();
                if len == 0 {
                    self.reg_sel = 0;
                    return;
                }
                self.reg_sel = (self.reg_sel as isize + delta).clamp(0, len as isize - 1) as usize;
            }
            Pane::Services => {
                let len = self.svc_rows.len();
                if len == 0 {
                    self.svc_sel = 0;
                    return;
                }
                self.svc_sel = (self.svc_sel as isize + delta).clamp(0, len as isize - 1) as usize;
            }
            Pane::Manifest => {
                let len = self.project.as_ref().map_or(0, |m| m.deps.len());
                if len == 0 {
                    self.man_sel = 0;
                    return;
                }
                self.man_sel = (self.man_sel as isize + delta).clamp(0, len as isize - 1) as usize;
            }
            // Home's cursor walks the recent-installs list, not the
            // filtered installed rows.
            Pane::Home => {
                let n = self.recent_installed().len();
                self.home_sel = if n == 0 {
                    0
                } else {
                    (self.home_sel as isize + delta).clamp(0, n as isize - 1) as usize
                };
            }
            _ => {
                let len = self.visible().len();
                if len == 0 {
                    self.sel = 0;
                    return;
                }
                self.sel = (self.sel as isize + delta).clamp(0, len as isize - 1) as usize;
            }
        }
    }

    fn switch_pane(&mut self, pane: Pane) {
        self.pane = pane;
        self.detail = None;
        self.detail_scroll = 0;
        self.versions_focus = false;
        self.versions_sel = 0;
        self.sel_label = self.selected().map(|r| r.entry.label());
        match pane {
            // Services entry marks the table stale — `tick` fires the
            // refresh once the worker frees up.
            Pane::Services => self.svc_stale = true,
            // Manifest entry re-reads the project manifest — a tiny
            // file read, same posture as the detail view.
            Pane::Manifest => self.load_manifest(),
            // Author entry refreshes the configured-registry list.
            Pane::Author => self.author_regs = ops::registry_names(&self.ctx),
            _ => {}
        }
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

    /// `Enter` — open the package page for the cursor row (Installed)
    /// or the registry result under the cursor (Registry); closes it
    /// when already open.
    fn toggle_detail(&mut self) {
        if self.detail.is_some() {
            self.detail = None;
            self.detail_scroll = 0;
            self.versions_focus = false;
            self.versions_sel = 0;
            return;
        }
        match self.pane {
            Pane::Registry => {
                let Some(entry) = self.reg_selected().cloned() else {
                    return;
                };
                // Resolve the installable ref once, on open — the page
                // shows `tamias install <ref>` (or the failure inline).
                let (install_ref, install_err) =
                    match ops::entry_install_ref(&self.ctx, &entry, self.reg_target.as_deref()) {
                        Ok(r) => (Some(r), None),
                        Err(e) => (None, Some(e.to_string())),
                    };
                self.fetch_versions(&entry);
                self.detail = Some(Detail::Registry {
                    entry,
                    install_ref,
                    install_err,
                    versions: None,
                    versions_err: None,
                });
            }
            _ => {
                let Some(row) = self.selected() else {
                    return;
                };
                let entry = row.entry.clone();
                let (manifest, manifest_err) = match entry.manifest() {
                    Ok(m) => (Some(Box::new(m)), None),
                    Err(e) => (None, Some(e.to_string())),
                };
                self.detail = Some(Detail::Installed {
                    entry,
                    manifest,
                    manifest_err,
                });
            }
        }
        self.detail_scroll = 0;
        self.versions_focus = false;
        self.versions_sel = 0;
    }

    /// `Enter` on Home — open the highlighted recent install's package
    /// page. The page verbs (`v`/`u`/`p`/`r`/`c`/`d`) key off `pane`,
    /// and `toggle_detail` builds `Detail::Installed` from `selected()`
    /// — so the clean route is the Installed pane itself: switch
    /// there, park the row cursor on the entry (dropping a stale `/`
    /// filter that would hide it), then run the normal detail path.
    /// Esc-back lands on the Installed table rather than Home — the
    /// deliberate trade-off for reusing the whole detail surface.
    fn open_home_detail(&mut self) {
        let Some(label) = self.home_selected().map(|r| r.entry.label()) else {
            return;
        };
        let mut vis = self.visible();
        let mut pos = vis.iter().position(|&i| {
            self.entries
                .get(i)
                .is_some_and(|r| r.entry.label() == label)
        });
        if pos.is_none() && !self.filter.is_empty() {
            // The filter is an Installed-pane artifact — Home can't
            // show it hiding the target, so drop it and retry.
            self.filter.clear();
            self.input.clear();
            vis = self.visible();
            pos = vis.iter().position(|&i| {
                self.entries
                    .get(i)
                    .is_some_and(|r| r.entry.label() == label)
            });
        }
        let Some(pos) = pos else { return };
        self.switch_pane(Pane::Installed);
        self.sel = pos;
        self.toggle_detail();
    }

    /// Kick off the background sibling-versions fetch for a registry
    /// package page. A dedicated thread — *not* `spawn`: the fetch is
    /// a read-only index lookup that must never occupy `busy` (a user
    /// pressing `i` mid-fetch can't be told "busy") nor queue behind
    /// an in-flight op. The result comes back as [`Msg::Versions`]
    /// keyed by the entry label; [`Model::drain_work`] drops it when
    /// the page closed or moved to a different package. Re-opening
    /// the same package simply refetches — each page owns its fetch.
    fn fetch_versions(&mut self, entry: &RegistryEntry) {
        let key = entry.label();
        let ctx = self.ctx.clone();
        let tx = self.work_tx.clone();
        let name = entry.name.clone();
        let scope = entry.scope.clone();
        let target = self.reg_target.clone();
        std::thread::spawn(move || {
            let result = ops::versions(&ctx, &name, scope.as_deref(), target.as_deref())
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Versions { key, result });
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
            PromptKind::AddDep => {
                if text.is_empty() {
                    self.say_err("add dep: empty ref");
                } else {
                    self.spawn(Job::AddDep {
                        reference: text,
                        allow_drivers: false,
                    });
                }
            }
            PromptKind::SetDep { old } => {
                if text.is_empty() {
                    self.say_err("set dep: empty ref");
                } else {
                    self.confirm_job(
                        &format!("set dep {old} → {text}"),
                        Job::SetDep {
                            old,
                            new: text,
                            allow_drivers: false,
                        },
                    );
                }
            }
            PromptKind::DeployConfig { name } => {
                // Empty answer = no driver config, same as `:deploy`.
                let config = if text.is_empty() { None } else { Some(text) };
                self.confirm_job(
                    &format!("deploy {name}"),
                    Job::ProviderDeploy { name, config },
                );
            }
            PromptKind::PackSpec => {
                let mut it = text.split_whitespace();
                let dir = it.next().unwrap_or("");
                let out = it.next();
                if dir.is_empty() {
                    self.say_err("pack: empty spec — usage: dir [out.tgz]");
                } else {
                    self.spawn(Job::Pack {
                        dir: dir.to_string(),
                        out: out.map(str::to_string),
                    });
                }
            }
            PromptKind::PublishSpec => {
                let mut it = text.split_whitespace();
                let dir = it.next().unwrap_or("");
                let registry = it.next();
                if dir.is_empty() {
                    self.say_err("publish: empty spec — usage: dir [registry]");
                } else {
                    self.confirm_job(
                        &format!("publish {dir}"),
                        Job::Publish {
                            dir: dir.to_string(),
                            registry: registry.map(str::to_string),
                        },
                    );
                }
            }
        }
    }

    /// Install a registry entry — resolve its `.tgz` to an installable
    /// ref (fs path for local registries, pinned http url for
    /// http/cf), then the usual confirm modal. `i` on an open package
    /// page installs the entry the page shows, not the row cursor
    /// underneath — what you see is what installs.
    fn install_registry_entry(&mut self, entry: RegistryEntry) {
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
            Err(e) => self.say_err(format!("{e} — `:registry <name>` picks another target")),
        }
    }

    /// `i` on the Registry pane — the open package page's entry wins
    /// over the row cursor; otherwise the selected result installs.
    fn install_registry_result(&mut self) {
        let entry = match &self.detail {
            Some(Detail::Registry { entry, .. }) => Some(entry.clone()),
            _ => self.reg_selected().cloned(),
        };
        match entry {
            Some(entry) => self.install_registry_entry(entry),
            None => self.say_err("no registry result selected"),
        }
    }

    // --- versions focus (registry package page) ---

    /// `versions_focus` only means something on an open Registry
    /// page — the flag clears with the page, so this also guards
    /// against any stale state.
    pub fn versions_focused(&self) -> bool {
        self.versions_focus && matches!(self.detail, Some(Detail::Registry { .. }))
    }

    /// The landed sibling-version count on the open Registry page.
    fn versions_len(&self) -> usize {
        match &self.detail {
            Some(Detail::Registry {
                versions: Some(list),
                ..
            }) => list.len(),
            _ => 0,
        }
    }

    /// The version row under the versions cursor.
    pub fn versions_selected(&self) -> Option<&RegistryEntry> {
        match &self.detail {
            Some(Detail::Registry {
                versions: Some(list),
                ..
            }) => list.get(self.versions_sel),
            _ => None,
        }
    }

    /// `v` on a Registry package page — toggle versions focus (the
    /// npm version-selector mode). Engages only once the background
    /// fetch has landed a non-empty list; a pending or failed fetch
    /// gets a muted status instead. The cursor starts on the page's
    /// own entry (`← current`, matched by blob `file` — the same
    /// rule the renderer uses).
    fn toggle_versions_focus(&mut self) {
        if self.versions_focused() {
            self.versions_focus = false;
            return;
        }
        // Decide before writing the status — the detail borrow ends
        // ahead of `say`'s `&mut self`.
        let (focus_at, note) = match &self.detail {
            Some(Detail::Registry {
                entry, versions, ..
            }) => match versions {
                Some(list) if !list.is_empty() => (
                    Some(list.iter().position(|v| v.file == entry.file).unwrap_or(0)),
                    None,
                ),
                Some(_) => (None, Some("no versions indexed")),
                None => (None, Some("versions still loading…")),
            },
            _ => (None, None),
        };
        if let Some(i) = focus_at {
            self.versions_focus = true;
            self.versions_sel = i;
        } else if let Some(note) = note {
            self.say(note);
        }
    }

    /// `j`/`k` inside versions focus — clamped to the landed list,
    /// same posture as [`Model::move_sel`].
    fn move_versions_sel(&mut self, delta: isize) {
        let len = self.versions_len();
        if len == 0 {
            self.versions_sel = 0;
            return;
        }
        self.versions_sel =
            (self.versions_sel as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    /// `i` inside versions focus — install the selected sibling
    /// version through the same resolve→confirm path as the page
    /// entry. A version already in the store gets an informational
    /// toast instead of a reinstall confirm.
    fn install_focused_version(&mut self) {
        let Some(entry) = self.versions_selected().cloned() else {
            return;
        };
        if self.version_installed(&entry) {
            self.say(format!("{} already installed", entry.label()));
            return;
        }
        self.install_registry_entry(entry);
    }

    /// Is this registry version already in the store? Matches on
    /// (name, scope) plus version equality *or* digest equality —
    /// the digest catches unversioned/re-published entries; an
    /// absent version on either side never counts as a match.
    fn version_installed(&self, entry: &RegistryEntry) -> bool {
        self.entries.iter().any(|r| {
            let rec = &r.entry.record;
            let version_match = match (rec.version.as_deref(), entry.version.as_deref()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            let digest_match = !entry.digest.is_empty() && rec.digest == entry.digest;
            rec.name == entry.name
                && rec.scope.as_deref() == entry.scope.as_deref()
                && (version_match || digest_match)
        })
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

    /// `x` — remove confirm (teardown warning for non-`files`).
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

    // --- Services pane verbs ---

    /// `s`/`S`/`!`/`e`/`E`/`d`/`x`/`t`/`T`/`n`/`R` on the selected
    /// service row — start/stop/restart/enable/disable, deploy,
    /// uninstall, logs tail/follow, foreground `run`, refresh.
    /// Restart rides `!` (not `r`) so `r` keeps a single meaning.
    fn svc_verb(&mut self, key: char) {
        let Some(row) = self.svc_selected().cloned() else {
            self.say_err("no service selected");
            return;
        };
        use tamias_provider::UnitAction as A;
        let action = match key {
            's' => Some(A::Start),
            'S' => Some(A::Stop),
            '!' => Some(A::Restart),
            'e' => Some(A::Enable),
            'E' => Some(A::Disable),
            _ => None,
        };
        if let Some(action) = action {
            self.confirm_job(
                unit_action_word(action),
                Job::UnitAction {
                    unit: row.unit.clone(),
                    action,
                },
            );
            return;
        }
        match key {
            'd' => {
                self.prompt_buf.clear();
                self.mode = Mode::Prompt(PromptKind::DeployConfig {
                    name: row.package.clone(),
                });
            }
            'x' => {
                self.confirm_job(
                    &format!("uninstall {}", row.package),
                    Job::ProviderUninstall {
                        name: row.package.clone(),
                    },
                );
            }
            't' => self.spawn(Job::Logs {
                name: row.package.clone(),
                follow: false,
            }),
            'T' => self.spawn(Job::Logs {
                name: row.package.clone(),
                follow: true,
            }),
            // Foreground run — streams the service's stdout/stderr
            // into the log; `x` kills the child.
            'n' => self.spawn(Job::RunService {
                package: row.package.clone(),
                service: Some(row.service.clone()),
            }),
            'R' => {
                self.svc_stale = true;
                self.say("services refresh queued");
            }
            _ => {}
        }
    }

    // --- Manifest pane verbs ---

    /// `a`/`x`/`d`/`e`/`s` on the deps form — add/remove/set-ref/sync.
    fn man_verb(&mut self, key: char) {
        match key {
            'a' => {
                self.prompt_buf.clear();
                self.mode = Mode::Prompt(PromptKind::AddDep);
            }
            // `s` reconciles the whole `deps:` set (`tamias sync`) —
            // confirm first like the other mutating verbs. `S` is the
            // lock-spec variant (`tamias sync --from-lock`).
            's' | 'S' => self.confirm_job(
                if key == 'S' {
                    "sync --from-lock"
                } else {
                    "sync deps"
                },
                Job::Sync {
                    allow_drivers: false,
                    from_lock: key == 'S',
                },
            ),
            // `d`/`x` both drop the dep (the proposal's two remove
            // keys); `a` adds, `e` edits the ref.
            'x' | 'd' => match self.dep_selected() {
                Some(dep) => {
                    self.confirm_job("remove dep", Job::RemoveDep { reference: dep });
                }
                None => self.say_err("no dep selected"),
            },
            'e' => match self.dep_selected() {
                Some(old) => {
                    self.prompt_buf.clear();
                    self.mode = Mode::Prompt(PromptKind::SetDep { old });
                }
                None => self.say_err("no dep selected"),
            },
            'R' => self.load_manifest(),
            _ => {}
        }
    }

    // --- Author pane verbs ---

    /// `p`/`P`/`r`/`R` on the Author pane — pack/publish/registries.
    fn author_verb(&mut self, key: char) {
        match key {
            'p' => {
                self.prompt_buf.clear();
                self.mode = Mode::Prompt(PromptKind::PackSpec);
            }
            'P' => {
                self.prompt_buf.clear();
                self.mode = Mode::Prompt(PromptKind::PublishSpec);
            }
            'r' => self.spawn(Job::Registries),
            'R' => {
                self.author_regs = ops::registry_names(&self.ctx);
            }
            _ => {}
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
            Ok(Cmd::Log) => self.open_log(),
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
                        self.say_err("nothing installed — `i` installs by ref");
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
            Ok(Cmd::Services) => {
                self.svc_stale = true;
                self.say("services refresh queued");
            }
            Ok(Cmd::Service { action, unit }) => {
                let Some(action) = unit_action_from_word(&action) else {
                    self.say_err(format!("unknown service verb '{action}'"));
                    return;
                };
                self.confirm_job(unit_action_word(action), Job::UnitAction { unit, action });
            }
            Ok(Cmd::Run { package, service }) => self.spawn(Job::RunService { package, service }),
            Ok(Cmd::Logs { name, follow }) => self.spawn(Job::Logs { name, follow }),
            Ok(Cmd::Deploy(name)) => {
                self.prompt_buf.clear();
                self.mode = Mode::Prompt(PromptKind::DeployConfig { name });
            }
            Ok(Cmd::Pack(spec)) => {
                let mut it = spec.split_whitespace();
                let dir = it.next().unwrap_or("");
                let out = it.next();
                if dir.is_empty() {
                    self.say_err("pack: empty spec — usage: dir [out.tgz]");
                } else {
                    self.spawn(Job::Pack {
                        dir: dir.to_string(),
                        out: out.map(str::to_string),
                    });
                }
            }
            Ok(Cmd::Publish(spec)) => {
                let mut it = spec.split_whitespace();
                let dir = it.next().unwrap_or("");
                let registry = it.next();
                if dir.is_empty() {
                    self.say_err("publish: empty spec — usage: dir [registry]");
                } else {
                    self.confirm_job(
                        &format!("publish {dir}"),
                        Job::Publish {
                            dir: dir.to_string(),
                            registry: registry.map(str::to_string),
                        },
                    );
                }
            }
            Ok(Cmd::Registries) => self.spawn(Job::Registries),
            Ok(Cmd::Manifest) => {
                self.load_manifest();
                self.say("manifest reloaded");
            }
            Ok(Cmd::Sync { from_lock }) => {
                self.confirm_job(
                    if from_lock {
                        "sync --from-lock"
                    } else {
                        "sync deps"
                    },
                    Job::Sync {
                        allow_drivers: false,
                        from_lock,
                    },
                );
            }
            Err(e) if e.is_empty() => {}
            Err(e) => self.say_err(e),
        }
    }

    /// Recompute the `:` completion candidates for the current
    /// buffer; any edit resets the Tab selection to "none applied".
    fn refresh_completions(&mut self) {
        self.cmd_completions = crate::complete::complete(&self.cmdline);
        self.cmd_sel = usize::MAX;
    }

    /// `Tab`/`BackTab` in `:` mode — cycle the candidate list and
    /// replace the trailing token with the pick. The token boundary
    /// is the last whitespace (or buffer start); a trailing space
    /// means we're completing a fresh token, so `rfind` lands on it
    /// and the replace is a pure append.
    fn apply_completion(&mut self, forward: bool) {
        let n = self.cmd_completions.len();
        if n == 0 {
            return;
        }
        self.cmd_sel = if forward {
            if self.cmd_sel == usize::MAX || self.cmd_sel + 1 >= n {
                0
            } else {
                self.cmd_sel + 1
            }
        } else if self.cmd_sel == usize::MAX || self.cmd_sel == 0 {
            n - 1
        } else {
            self.cmd_sel - 1
        };
        let insert = match self.cmd_completions.get(self.cmd_sel) {
            Some(c) => c.insert.clone(),
            None => return,
        };
        let start = self.cmdline.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        self.cmdline.replace_range(start.., &insert);
        self.refresh_completions();
    }

    /// `:` mode keys — `Enter` runs, `esc` drops the line, `Tab`
    /// cycles [`crate::complete`] candidates into the buffer.
    fn key_command(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.cmd_completions.clear();
                let quit = matches!(self.cmdline.trim(), "q" | "quit" | "q!");
                if !quit {
                    self.run_command();
                }
                quit
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.cmdline.clear();
                self.cmd_completions.clear();
                false
            }
            KeyCode::Backspace => {
                self.cmdline.pop();
                self.refresh_completions();
                false
            }
            KeyCode::Tab => {
                self.apply_completion(true);
                false
            }
            KeyCode::BackTab => {
                self.apply_completion(false);
                false
            }
            KeyCode::Char(c) => {
                self.cmdline.push(c);
                self.refresh_completions();
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

    /// Open the op-log overlay (`L`/`:log`) scrolled to the newest
    /// lines — the strip only ever shows the tail, so the overlay
    /// opens there too.
    fn open_log(&mut self) {
        self.mode = Mode::Log;
        self.log_scroll = self.log_max_scroll();
    }

    /// Top-line scroll offset that puts the log tail on screen —
    /// `log.len() - 1` until `draw` reports the overlay's inner
    /// height, then the true bottom.
    fn log_max_scroll(&self) -> usize {
        self.log.len().saturating_sub(self.log_view_h.max(1))
    }

    /// Log-overlay keys — scroll the op log; `esc`/`q`/`L` closes.
    /// Everything else is swallowed while the overlay is up (same
    /// posture as `key_help`).
    fn key_log(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('L') => {
                self.mode = Mode::Normal;
                self.log_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.log_scroll = (self.log_scroll + 1).min(self.log_max_scroll());
            }
            KeyCode::PageDown => {
                self.log_scroll =
                    (self.log_scroll + self.log_view_h.max(1)).min(self.log_max_scroll());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.log_scroll = self.log_scroll.saturating_sub(self.log_view_h.max(1));
            }
            KeyCode::Char('g') => self.log_scroll = 0,
            KeyCode::Char('G') => self.log_scroll = self.log_max_scroll(),
            _ => {}
        }
        false
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

    /// Close the package page — the shared `esc`/`h`/`←`/`q` close;
    /// also drops versions focus and resets both scroll/cursor state.
    fn close_detail(&mut self) {
        self.detail = None;
        self.detail_scroll = 0;
        self.versions_focus = false;
        self.versions_sel = 0;
    }

    /// One key event → `true` when the app should quit. Ctrl-C is
    /// the force-quit and works from every mode (even under a modal);
    /// quitting kills a running stream child first.
    pub fn key(&mut self, k: KeyEvent) -> bool {
        self.dismiss_error();
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.kill_stream();
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
            Mode::Log => return self.key_log(k),
            Mode::Normal => {}
        }
        match k.code {
            // `q` backs out of an open package page first (same
            // close as esc/h/← — quitting from under a detail view
            // is surprising); only at pane level is it the quit.
            // Ctrl-C still force-quits from anywhere.
            KeyCode::Char('q') => {
                if self.detail.is_some() {
                    self.close_detail();
                } else {
                    self.kill_stream();
                    return true;
                }
            }
            // `1`-`6` jump to the tab at that position in `Pane::ALL`
            // (Home is `1`); out-of-range digits fall through.
            KeyCode::Char(c @ '1'..='9') => {
                if let Some(p) = Pane::ALL.get(c as usize - '1' as usize) {
                    self.switch_pane(*p);
                }
            }
            KeyCode::Enter => match self.pane {
                // Home's recent list opens through the Installed
                // pane (see `open_home_detail`).
                Pane::Home => self.open_home_detail(),
                Pane::Installed | Pane::Registry => self.toggle_detail(),
                _ => {}
            },
            // Versions focus eats esc first — one level out (back to
            // page scroll) before the page itself closes.
            KeyCode::Esc if self.versions_focused() => self.versions_focus = false,
            KeyCode::Esc | KeyCode::Left if self.detail.is_some() => self.close_detail(),
            KeyCode::Char('h') | KeyCode::BackTab => {
                if self.detail.is_some() {
                    self.close_detail();
                } else {
                    self.step_pane(-1);
                }
            }
            KeyCode::Tab | KeyCode::Char('l') => self.step_pane(1),
            // With the package page open j/k scroll it; the row
            // cursor (and the v/u/p/x/c/d verbs keyed off it) stays put.
            // Versions focus reroutes them onto the version list.
            KeyCode::Char('j') | KeyCode::Down => {
                if self.versions_focused() {
                    self.move_versions_sel(1);
                } else if self.detail.is_some() {
                    self.detail_scroll += 1;
                } else {
                    self.move_sel(1);
                    self.sync_sel_label();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.versions_focused() {
                    self.move_versions_sel(-1);
                } else if self.detail.is_some() {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                } else {
                    self.move_sel(-1);
                    self.sync_sel_label();
                }
            }
            KeyCode::Char('g') => match self.pane {
                Pane::Home => self.home_sel = 0,
                Pane::Registry => self.reg_sel = 0,
                Pane::Services => self.svc_sel = 0,
                Pane::Manifest => self.man_sel = 0,
                _ => self.sel = 0,
            },
            KeyCode::Char('G') => match self.pane {
                Pane::Home => {
                    self.home_sel = self.recent_installed().len().saturating_sub(1);
                }
                Pane::Registry => self.reg_sel = self.reg_results.len().saturating_sub(1),
                Pane::Services => self.svc_sel = self.svc_rows.len().saturating_sub(1),
                Pane::Manifest => {
                    self.man_sel = self
                        .project
                        .as_ref()
                        .map_or(0, |m| m.deps.len().saturating_sub(1));
                }
                _ => {
                    self.sel = self.visible().len().saturating_sub(1);
                    self.sync_sel_label();
                }
            },
            KeyCode::Char('/') => {
                if self.pane == Pane::Home {
                    // Home has no filter of its own — `/` is the way
                    // into registry search: switch panes and land in
                    // Search mode with an empty query.
                    self.switch_pane(Pane::Registry);
                    self.reg_query.clear();
                    self.reg_results.clear();
                    self.reg_err = None;
                    self.input.clear();
                    self.mode = Mode::Search;
                } else {
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
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.cmdline.clear();
                self.refresh_completions();
            }
            KeyCode::Char('F') => {
                self.filter.clear();
                self.rebuild_sel(None, self.sel);
            }
            KeyCode::Char('R') if matches!(self.pane, Pane::Installed | Pane::Registry) => {
                self.reload();
                self.say("reloaded");
            }
            // `x` while a stream child runs kills it — takes
            // precedence over the pane's own `x` (uninstall/remove
            // dep) since a live child is the more urgent target.
            KeyCode::Char('x') if self.stream_child.is_some() => self.kill_stream(),
            // --- verbs ---
            // `v` on an open Registry package page toggles versions
            // focus — the npm version-selector mode. `v` on the bare
            // Registry pane stays unbound.
            KeyCode::Char('v') if self.pane == Pane::Registry && self.detail.is_some() => {
                self.toggle_versions_focus()
            }
            KeyCode::Char('v') if self.pane == Pane::Installed => self.verb_verify(),
            KeyCode::Char('u') if self.pane == Pane::Installed => self.verb_update(),
            KeyCode::Char('U') if self.pane == Pane::Installed => self.confirm_job(
                "update all",
                Job::UpdateAll {
                    allow_drivers: false,
                },
            ),
            KeyCode::Char('p') if self.pane == Pane::Installed => self.verb_pin(),
            // `x` removes — the same destructive key as Services'
            // uninstall and Manifest's dep removal. (`x` with a live
            // stream child still kills the stream first — see above.)
            KeyCode::Char('x') if self.pane == Pane::Installed => self.verb_remove(),
            KeyCode::Char('c') if self.pane == Pane::Installed => self.verb_cache_clean(false),
            KeyCode::Char('C') if self.pane == Pane::Installed => self.verb_cache_clean(true),
            KeyCode::Char('d') if self.pane == Pane::Installed => self.verb_doctor(),
            // Versions focus: `i` installs the selected sibling
            // version, not the page's own entry.
            KeyCode::Char('i') if self.versions_focused() => self.install_focused_version(),
            KeyCode::Char('i') => self.verb_install(),
            // Services-pane verbs route through `svc_verb` — the keys
            // don't collide with Installed's (pane-gated). `!` is
            // restart: `r` stays exclusive to the Author pane's
            // registries report so the key keeps one meaning app-wide.
            KeyCode::Char(
                c @ ('s' | 'S' | '!' | 'e' | 'E' | 'd' | 'x' | 't' | 'T' | 'n' | 'R'),
            ) if self.pane == Pane::Services => self.svc_verb(c),
            // Manifest pane: add/remove/edit dep + sync + reload.
            KeyCode::Char(c @ ('a' | 'x' | 'd' | 'e' | 's' | 'R'))
                if self.pane == Pane::Manifest =>
            {
                self.man_verb(c)
            }
            // Author pane: pack/publish/registries.
            KeyCode::Char(c @ ('p' | 'P' | 'r' | 'R')) if self.pane == Pane::Author => {
                self.author_verb(c)
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            // `L` opens the full op log — `l` (lowercase) stays pane
            // cycling, so this rides the shifted char only.
            KeyCode::Char('L') => self.open_log(),
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
                // Home's recents are a wrapped Paragraph, not a
                // table — `home_area` (recorded by `draw_home`) maps
                // the click row onto `recent_installed()` →
                // `home_sel`. Plain click selects only; `Enter`
                // opens, same as the other panes.
                if self.pane == Pane::Home {
                    let area = self.home_area;
                    if area.height > 0 && area.contains((ev.column, ev.row).into()) {
                        let idx = (ev.row - area.y) as usize;
                        if idx < self.recent_installed().len() {
                            self.home_sel = idx;
                        }
                    }
                    return;
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
                    match self.pane {
                        Pane::Registry => {
                            if idx < self.reg_results.len() {
                                self.reg_sel = idx;
                            }
                        }
                        Pane::Services => {
                            if idx < self.svc_rows.len() {
                                self.svc_sel = idx;
                            }
                        }
                        Pane::Manifest => {
                            let n = self.project.as_ref().map_or(0, |m| m.deps.len());
                            if idx < n {
                                self.man_sel = idx;
                            }
                        }
                        _ => {
                            let vis = self.visible();
                            if idx < vis.len() {
                                self.sel = idx;
                                self.sync_sel_label();
                            }
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
        assert_eq!(parse_command("log").unwrap(), Cmd::Log);
        assert!(parse_command("bogus").is_err());
        assert!(parse_command("install").is_err());
        assert!(parse_command("").is_err());
    }

    #[test]
    fn phase4_6_command_parsing() {
        assert_eq!(parse_command("services").unwrap(), Cmd::Services);
        assert_eq!(
            parse_command("service restart my-unit").unwrap(),
            Cmd::Service {
                action: "restart".into(),
                unit: "my-unit".into(),
            }
        );
        assert!(parse_command("service bogus u").is_err());
        assert!(parse_command("service start").is_err());
        assert_eq!(
            parse_command("run @s/w web").unwrap(),
            Cmd::Run {
                package: "@s/w".into(),
                service: Some("web".into()),
            }
        );
        assert_eq!(
            parse_command("run @s/w").unwrap(),
            Cmd::Run {
                package: "@s/w".into(),
                service: None,
            }
        );
        assert_eq!(
            parse_command("logs @s/w -f").unwrap(),
            Cmd::Logs {
                name: "@s/w".into(),
                follow: true,
            }
        );
        assert_eq!(
            parse_command("logs my-unit").unwrap(),
            Cmd::Logs {
                name: "my-unit".into(),
                follow: false,
            }
        );
        assert_eq!(
            parse_command("deploy @s/w").unwrap(),
            Cmd::Deploy("@s/w".into())
        );
        assert_eq!(
            parse_command("pack ./pkg out.tgz").unwrap(),
            Cmd::Pack("./pkg out.tgz".into())
        );
        assert_eq!(
            parse_command("publish ./pkg mine").unwrap(),
            Cmd::Publish("./pkg mine".into())
        );
        assert_eq!(parse_command("registries").unwrap(), Cmd::Registries);
        assert_eq!(parse_command("manifest").unwrap(), Cmd::Manifest);
        assert_eq!(
            parse_command("sync").unwrap(),
            Cmd::Sync { from_lock: false }
        );
        assert_eq!(parse_command("s").unwrap(), Cmd::Sync { from_lock: false });
        assert_eq!(
            parse_command("sync --from-lock").unwrap(),
            Cmd::Sync { from_lock: true }
        );
        assert!(parse_command("sync bogus").is_err());
        assert!(parse_command("pack").is_err());
        assert!(parse_command("publish").is_err());
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
        let s = Job::Sync {
            allow_drivers: false,
            from_lock: true,
        };
        assert!(s.label().contains("from-lock"));
        match s.with_drivers_allowed() {
            Job::Sync {
                allow_drivers,
                from_lock,
            } => {
                assert!(allow_drivers);
                assert!(from_lock);
            }
            _ => panic!("wrong variant"),
        }
        let v = Job::Verify("w".into());
        assert!(matches!(v.with_drivers_allowed(), Job::Verify(_)));
    }

    /// A registry index entry for the versions tests — `@s/w@<ver>`.
    fn reg_entry(version: &str) -> RegistryEntry {
        RegistryEntry {
            name: "w".into(),
            scope: Some("s".into()),
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
            file: format!("@s/w/{version}.tgz"),
        }
    }

    /// Home is the landing pane: `/` jumps into Registry search,
    /// `i` opens the install-by-ref prompt, `1`-`6` map onto
    /// `Pane::ALL` positions.
    #[test]
    fn home_pane_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            None,
        );
        let mut m = Model::open(&ctx).unwrap();
        assert_eq!(m.pane, Pane::Home);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // `/` on Home → Registry pane in Search mode, empty query.
        m.key(key(KeyCode::Char('/')));
        assert_eq!(m.pane, Pane::Registry);
        assert_eq!(m.mode, Mode::Search);
        assert!(m.reg_query.is_empty());
        // esc leaves search mode; `1` is Home, `2` Installed, `3`
        // Registry — digits follow `Pane::ALL` order.
        m.key(key(KeyCode::Esc));
        assert_eq!(m.mode, Mode::Normal);
        m.key(key(KeyCode::Char('1')));
        assert_eq!(m.pane, Pane::Home);
        m.key(key(KeyCode::Char('2')));
        assert_eq!(m.pane, Pane::Installed);
        // `i` on Home opens the install-by-ref prompt (non-Registry
        // verb path), not a registry-result install.
        m.key(key(KeyCode::Char('1')));
        m.key(key(KeyCode::Char('i')));
        assert_eq!(m.mode, Mode::Prompt(PromptKind::InstallRef));
        // j/k on Home are no-ops — the installed cursor stays put.
        m.key(key(KeyCode::Esc));
        m.sel = 0;
        m.key(key(KeyCode::Char('j')));
        assert_eq!(m.sel, 0);
        assert_eq!(m.pane, Pane::Home);
    }

    /// A store row for cursor/ordering tests — `name` unscoped at
    /// version `installed_at` so labels stay distinct.
    fn store_row(name: &str, installed_at: u64) -> Row {
        Row {
            entry: tamias_core::StoreEntry {
                dir: PathBuf::from(format!("/store/{name}")),
                record: tamias_core::StoreRecord {
                    name: name.into(),
                    scope: None,
                    version: Some(installed_at.to_string()),
                    kind: None,
                    description: None,
                    keywords: Vec::new(),
                    homepage: None,
                    repository: None,
                    license: None,
                    authors: Vec::new(),
                    source_ref: "./x".into(),
                    resolved: format!("sha256:{installed_at}"),
                    digest: format!("sha256:d{installed_at}"),
                    signature: None,
                    signer: None,
                    trust: None,
                    installed_at,
                },
            },
            size: None,
            sealed: false,
        }
    }

    /// Home's recent list is a real cursor: `recent_installed`
    /// orders `installed_at` desc capped at 8, j/k/g/G move
    /// `home_sel` clamped, and `Enter` routes through the Installed
    /// pane into the package page with the row cursor parked on it.
    #[test]
    fn home_recent_cursor_and_open() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            Some(tmp.path().join("no-such-config.yml")),
        );
        let mut m = Model::open(&ctx).unwrap();
        for i in 0..10u64 {
            m.entries.push(store_row(&format!("p{i}"), i + 1));
        }
        // Newest first, capped at 8 — p0/p1 (oldest) fall off.
        let recent = m.recent_installed();
        assert_eq!(recent.len(), 8);
        assert_eq!(m.entries[recent[0]].entry.record.name, "p9");
        assert_eq!(m.entries[recent[7]].entry.record.name, "p2");
        assert_eq!(
            m.home_selected().map(|r| r.entry.record.name.as_str()),
            Some("p9")
        );
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // j/k clamp at both ends; g/G jump.
        m.key(key(KeyCode::Char('k')));
        assert_eq!(m.home_sel, 0);
        m.key(key(KeyCode::Char('j')));
        m.key(key(KeyCode::Char('j')));
        assert_eq!(m.home_sel, 2);
        assert_eq!(
            m.home_selected().map(|r| r.entry.record.name.as_str()),
            Some("p7")
        );
        m.key(key(KeyCode::Char('G')));
        assert_eq!(m.home_sel, 7);
        m.key(key(KeyCode::Char('g')));
        assert_eq!(m.home_sel, 0);
        // Enter on p9 → Installed pane, package page open on that row.
        m.key(key(KeyCode::Char('j'))); // p8
        m.key(key(KeyCode::Enter));
        assert_eq!(m.pane, Pane::Installed);
        assert_eq!(m.sel, 8); // p8 is entries[8]; visible() keeps store order
        match &m.detail {
            Some(Detail::Installed { entry, .. }) => {
                assert_eq!(entry.record.name, "p8");
            }
            other => panic!("expected installed page, got {other:?}"),
        }
        // Esc closes the page onto the Installed table (documented
        // trade-off — back does not return to Home).
        m.key(key(KeyCode::Esc));
        assert!(m.detail.is_none());
        assert_eq!(m.pane, Pane::Installed);
        assert_eq!(
            m.selected().map(|r| r.entry.record.name.as_str()),
            Some("p8")
        );
    }

    /// `installed_label_for` matches on (name, scope) regardless of
    /// version and falls back to the resolved-sha short form.
    #[test]
    fn installed_label_for_matches_name_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            None,
        );
        let mut m = Model::open(&ctx).unwrap();
        assert_eq!(m.installed_label_for("w", Some("s")), None);
        let entry = tamias_core::StoreEntry {
            dir: tmp.path().join("store/@s/w@0.2.0"),
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
                resolved: "sha256:0123456789abcdef".into(),
                digest: "sha256:d".into(),
                signature: None,
                signer: None,
                trust: None,
                installed_at: 0,
            },
        };
        m.entries.push(crate::app::Row {
            entry,
            size: None,
            sealed: false,
        });
        assert_eq!(
            m.installed_label_for("w", Some("s")).as_deref(),
            Some("0.2.0")
        );
        // scope and name must both match — no cross-talk.
        assert_eq!(m.installed_label_for("w", None), None);
        assert_eq!(m.installed_label_for("other", Some("s")), None);
    }

    /// `Msg::Versions` applies only to the page whose entry label it
    /// was keyed with — a closed or replaced page drops the result,
    /// and the arm never touches `busy`.
    #[test]
    fn versions_result_lands_only_on_the_matching_page() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            Some(tmp.path().join("no-such-config.yml")),
        );
        let mut m = Model::open(&ctx).unwrap();
        let page = reg_entry("1.0.0");
        m.detail = Some(Detail::Registry {
            entry: page.clone(),
            install_ref: None,
            install_err: None,
            versions: None,
            versions_err: None,
        });
        let tx = m.work_tx.clone();
        let versions_of = |m: &Model| match &m.detail {
            Some(Detail::Registry { versions, .. }) => versions.clone(),
            _ => panic!("no registry page"),
        };
        // A result keyed to a different label is stale — dropped.
        tx.send(Msg::Versions {
            key: "@s/w@2.0.0".into(),
            result: Ok(vec![reg_entry("2.0.0")]),
        })
        .unwrap();
        m.drain_work();
        assert!(versions_of(&m).is_none());
        // The matching key lands; `busy` is untouched either way.
        m.busy = Some("install x".into());
        tx.send(Msg::Versions {
            key: page.label(),
            result: Ok(vec![reg_entry("0.9.0"), page.clone()]),
        })
        .unwrap();
        m.drain_work();
        assert_eq!(m.busy.as_deref(), Some("install x"));
        let list = versions_of(&m).unwrap();
        assert_eq!(list.len(), 2);
        // An error lands as `versions_err`, not a busy-clearing Done.
        tx.send(Msg::Versions {
            key: page.label(),
            result: Err("index gone".into()),
        })
        .unwrap();
        m.drain_work();
        match &m.detail {
            Some(Detail::Registry { versions_err, .. }) => {
                assert_eq!(versions_err.as_deref(), Some("index gone"))
            }
            _ => panic!("no registry page"),
        }
    }

    /// A model on the Registry pane with `page` as its open package
    /// page — `versions` preset, fetch already "landed".
    fn registry_page_model(
        ctx: &Ctx,
        page: &RegistryEntry,
        versions: Option<Vec<RegistryEntry>>,
    ) -> Model {
        let mut m = Model::open(ctx).unwrap();
        m.pane = Pane::Registry;
        m.detail = Some(Detail::Registry {
            entry: page.clone(),
            install_ref: None,
            install_err: None,
            versions,
            versions_err: None,
        });
        m
    }

    /// `v` toggles versions focus only once the fetch has landed;
    /// while focused `j`/`k` move `versions_sel` (clamped, starting
    /// on the page's own entry) and leave `detail_scroll` alone, and
    /// `esc` backs out one level before closing the page.
    #[test]
    fn versions_focus_toggle_and_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            Some(tmp.path().join("no-such-config.yml")),
        );
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let page = reg_entry("1.0.0");
        // Pending fetch — `v` is a muted no-op.
        let mut m = registry_page_model(&ctx, &page, None);
        m.key(key(KeyCode::Char('v')));
        assert!(!m.versions_focused());
        assert!(m.status.contains("loading"));
        // Landed — focus engages, cursor starts on `← current`.
        if let Some(Detail::Registry { versions, .. }) = &mut m.detail {
            *versions = Some(vec![reg_entry("0.9.0"), page.clone(), reg_entry("1.1.0")]);
        }
        m.key(key(KeyCode::Char('v')));
        assert!(m.versions_focused());
        assert_eq!(m.versions_sel, 1);
        assert_eq!(
            m.versions_selected().and_then(|v| v.version.as_deref()),
            Some("1.0.0")
        );
        // j/k move the version cursor, not the page scroll.
        m.detail_scroll = 3;
        m.key(key(KeyCode::Char('j')));
        assert_eq!(m.versions_sel, 2);
        m.key(key(KeyCode::Char('k')));
        assert_eq!(m.versions_sel, 1);
        m.key(key(KeyCode::Char('k')));
        assert_eq!(m.versions_sel, 0);
        m.key(key(KeyCode::Char('k'))); // clamps at the top
        assert_eq!(m.versions_sel, 0);
        m.key(key(KeyCode::Char('j')));
        m.key(key(KeyCode::Char('j')));
        m.key(key(KeyCode::Char('j'))); // clamps at the bottom
        assert_eq!(m.versions_sel, 2);
        assert_eq!(m.detail_scroll, 3);
        // esc backs out to scroll mode — the page stays open.
        m.key(key(KeyCode::Esc));
        assert!(!m.versions_focused());
        assert!(m.detail.is_some());
        // `v` toggles back off too.
        m.key(key(KeyCode::Char('v')));
        assert!(m.versions_focused());
        m.key(key(KeyCode::Char('v')));
        assert!(!m.versions_focused());
        // Closing the page (esc twice: focus, then page) resets.
        m.key(key(KeyCode::Char('v')));
        m.key(key(KeyCode::Esc));
        m.key(key(KeyCode::Esc));
        assert!(m.detail.is_none());
        assert!(!m.versions_focused());
        assert_eq!(m.versions_sel, 0);
    }

    /// `i` inside versions focus installs the *selected* sibling —
    /// the confirm modal carries its resolved ref — and reports
    /// "already installed" instead of reinstalling a stored version.
    #[test]
    fn versions_focus_install_selected() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            Some(tmp.path().join("no-such-config.yml")),
        );
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let page = reg_entry("1.0.0");
        let mut m = registry_page_model(&ctx, &page, Some(vec![reg_entry("0.9.0"), page.clone()]));
        m.versions_focus = true;
        m.versions_sel = 0; // 0.9.0 — not in the store
        m.key(key(KeyCode::Char('i')));
        match &m.modal {
            Some(Modal::Confirm {
                job: Job::Install { reference, .. },
                ..
            }) => assert!(reference.contains("0.9.0"), "ref: {reference}"),
            other => panic!("expected install confirm, got {other:?}"),
        }
        // 1.0.0 installed at the same version → informational toast,
        // no modal.
        m.modal = None;
        m.entries.push(crate::app::Row {
            entry: tamias_core::StoreEntry {
                dir: tmp.path().join("store/@s/w@1.0.0"),
                record: tamias_core::StoreRecord {
                    name: "w".into(),
                    scope: Some("s".into()),
                    version: Some("1.0.0".into()),
                    kind: None,
                    description: None,
                    keywords: Vec::new(),
                    homepage: None,
                    repository: None,
                    license: None,
                    authors: Vec::new(),
                    source_ref: "./x".into(),
                    resolved: "sha256:0".into(),
                    digest: "sha256:1.0.0".into(),
                    signature: None,
                    signer: None,
                    trust: None,
                    installed_at: 0,
                },
            },
            size: None,
            sealed: false,
        });
        m.versions_sel = 1;
        m.key(key(KeyCode::Char('i')));
        assert!(m.modal.is_none());
        assert!(m.status.contains("already installed"), "{}", m.status);
    }

    /// `L`/`:log` opens the op-log overlay on the tail; j/k and the
    /// PgUp/PgDn page by the drawn view height, g/G jump the ends,
    /// and every other key is swallowed while it's up. New log lines
    /// follow only while parked at the bottom.
    #[test]
    fn log_overlay_toggle_scroll_and_gating() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            None,
        );
        let mut m = Model::open(&ctx).unwrap();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for i in 0..50 {
            m.push_log(format!("line {i}"));
        }
        m.log_view_h = 10; // as `draw_log_overlay` would report
                           // `L` opens at the bottom (newest last).
        m.key(key(KeyCode::Char('L')));
        assert_eq!(m.mode, Mode::Log);
        assert_eq!(m.log_scroll, 40); // 50 lines - 10 visible
                                      // j clamps at the bottom; k scrolls up one.
        m.key(key(KeyCode::Char('j')));
        assert_eq!(m.log_scroll, 40);
        m.key(key(KeyCode::Char('k')));
        assert_eq!(m.log_scroll, 39);
        // PgUp/PgDn page by the view height.
        m.key(key(KeyCode::PageUp));
        assert_eq!(m.log_scroll, 29);
        m.key(key(KeyCode::PageDown));
        assert_eq!(m.log_scroll, 39);
        // Parked at the tail, new lines keep it pinned to the bottom.
        m.key(key(KeyCode::Char('j')));
        m.push_log("line 50".to_string());
        assert_eq!(m.log_scroll, 41); // 51 - 10
                                      // Scrolled up it stays put.
        m.key(key(KeyCode::Char('g')));
        assert_eq!(m.log_scroll, 0);
        m.push_log("line 51".to_string());
        assert_eq!(m.log_scroll, 0);
        // Other keys are swallowed — no pane verbs or modes fire.
        let pane = m.pane;
        m.key(key(KeyCode::Char('2')));
        assert_eq!(m.pane, pane);
        m.key(key(KeyCode::Char('?')));
        assert_eq!(m.mode, Mode::Log);
        m.key(key(KeyCode::Char(':')));
        assert_eq!(m.mode, Mode::Log);
        // g/G jump the ends.
        m.key(key(KeyCode::Char('G')));
        assert_eq!(m.log_scroll, 42); // 52 - 10
                                      // `L` toggles closed; reopen and `q` closes too.
        m.key(key(KeyCode::Char('L')));
        assert_eq!(m.mode, Mode::Normal);
        m.key(key(KeyCode::Char('L')));
        assert_eq!(m.mode, Mode::Log);
        m.key(key(KeyCode::Char('q')));
        assert_eq!(m.mode, Mode::Normal);
        // esc closes as well.
        m.key(key(KeyCode::Char('L')));
        m.key(key(KeyCode::Esc));
        assert_eq!(m.mode, Mode::Normal);
        // `:log` opens it through the command line — and `L` typed
        // inside `:` is just text, not the overlay.
        m.key(key(KeyCode::Char(':')));
        assert_eq!(m.mode, Mode::Command);
        m.key(key(KeyCode::Char('L')));
        assert_eq!(m.mode, Mode::Command);
        assert_eq!(m.cmdline, "L");
        m.cmdline = "log".to_string();
        m.run_command();
        assert_eq!(m.mode, Mode::Log);
        // An empty log still opens — the overlay draws its muted
        // "no operations yet" line.
        let mut empty = Model::open(&ctx).unwrap();
        empty.key(key(KeyCode::Char('L')));
        assert_eq!(empty.mode, Mode::Log);
        assert_eq!(empty.log_scroll, 0);
    }

    /// `q` with a package page open closes the page instead of
    /// quitting — the second `q` (now at pane level) is the quit.
    /// Ctrl-C still force-quits even with detail open.
    #[test]
    fn q_closes_detail_before_quitting() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            Some(tmp.path().join("no-such-config.yml")),
        );
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let page = reg_entry("1.0.0");
        let mut m = registry_page_model(&ctx, &page, None);
        // Page open → `q` closes it, no quit.
        assert!(!m.key(key(KeyCode::Char('q'))));
        assert!(m.detail.is_none());
        assert_eq!(m.pane, Pane::Registry);
        // Pane level → `q` quits.
        assert!(m.key(key(KeyCode::Char('q'))));
        // Ctrl-C force-quits even with the page open.
        let mut m = registry_page_model(&ctx, &page, None);
        assert!(m.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    /// A click inside `home_area` selects the matching
    /// `recent_installed()` row (plain click selects only — `Enter`
    /// opens); clicks outside the rect are no-ops.
    #[test]
    fn home_click_selects_recent_row() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            None,
        );
        let mut m = Model::open(&ctx).unwrap();
        for i in 0..4u64 {
            m.entries.push(store_row(&format!("p{i}"), i + 1));
        }
        assert_eq!(m.pane, Pane::Home);
        // As `draw_home` would report: rows start at y=3, 4 rows tall.
        m.home_area = Rect::new(0, 3, 60, 4);
        let click = |row: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // recent_installed is installed_at desc → row 0 is p3.
        m.mouse(click(3));
        assert_eq!(m.home_sel, 0);
        assert_eq!(
            m.home_selected().map(|r| r.entry.record.name.as_str()),
            Some("p3")
        );
        m.mouse(click(5));
        assert_eq!(m.home_sel, 2);
        assert_eq!(
            m.home_selected().map(|r| r.entry.record.name.as_str()),
            Some("p1")
        );
        // Below the last row and above the rect: no-ops.
        m.mouse(click(7));
        m.mouse(click(2));
        assert_eq!(m.home_sel, 2);
        // Empty home_area (never drawn / empty list): no-op.
        m.home_area = Rect::default();
        m.mouse(click(4));
        assert_eq!(m.home_sel, 2);
    }
}
