//! The TUI model — panes, cursor, modes, modals, and the store reads
//! every key maps to. The Home pane is the landing view (wordmark,
//! stats, recently installed); the Installed pane lists
//! `tamias-store-local` entries with a client-side `/` filter; `Enter` opens the package
//! page (`Detail`) on either the Installed row or a Registry result;
//! the Registry pane searches a registry as-you-type; Services,
//! Project, and Publish cover provider lifecycle, the project `deps:`
//! form, and pack/publish authoring. (`Pane::Manifest`/`Pane::Author`
//! keep their enum names — only the tab labels and docs say
//! PROJECT/PUBLISH.)
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

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tamias_core::{Manifest, StoreEntry, StoreRecord};
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

/// `:` history depth — session-scoped, most recent first.
const CMD_HIST_CAP: usize = 50;

/// Dynamic `:` completion cap — installed labels / registry names /
/// unit names offered per arg position.
const CMD_COMPLETION_CAP: usize = 20;

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

/// One collapsed Installed row — the store keeps every version ever
/// installed, so the pane groups entries by `(name, scope)` and shows
/// only the latest. `rep` indexes `Model::entries`; `members` holds
/// every group entry's index (store order) for the `/` filter's
/// any-member match.
struct InstGroup {
    /// Index into `Model::entries` of the semver-max member.
    rep: usize,
    /// All member indices into `Model::entries`, store order.
    members: Vec<usize>,
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
            Pane::Manifest => "PROJECT",
            Pane::Author => "PUBLISH",
        }
    }

    /// Tab order for `h`/`l`, `Tab`/`Shift-Tab`, and `1`-`6` —
    /// browsing panes first (Installed → Services → Project →
    /// Publish), search (Registry) last.
    pub const ALL: [Pane; 6] = [
        Pane::Home,
        Pane::Installed,
        Pane::Services,
        Pane::Manifest,
        Pane::Author,
        Pane::Registry,
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
    /// Project pane `a` — a dep ref to install + record (`tamias
    /// add` equivalent).
    AddDep,
    /// Project pane `e` — a new ref replacing dep `old`.
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
    /// Publish pane `p` — `bundle pack` spec: `dir [out.tgz]`.
    PackSpec,
    /// Publish pane `P` — `publish` spec: `dir [registry]`.
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
    /// `?` — keybind overlay; it scrolls (j/k · PgUp/PgDn · g/G ·
    /// wheel) and `esc`/`?`/`q` closes.
    Help,
    /// `L`/`:log` — the op-log view: a right-docked sidebar (centered
    /// overlay under ~50 cols); `j`/`k` scroll, `g`/`G` jump the
    /// ends, `esc`/`q`/`L` closes. Swallows every other key — it's a
    /// view, not split focus.
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
    /// `:remove <name>` — light store remove (driver `uninstall`
    /// dispatch + `Store::remove`).
    Remove(String),
    /// `:uninstall <name>` — full provider teardown
    /// (`Job::ProviderUninstall`: driver uninstall, shared-unit
    /// survivor re-render, unit removal, then store remove) — the
    /// Services pane `x` equivalent.
    Uninstall(String),
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
    /// (`--from-lock` treats `tamias.lock` as the spec; Project pane
    /// `s`/`S` too).
    Sync {
        /// Install `tamias.lock` entries verbatim.
        from_lock: bool,
    },
    /// `:prune [--keep N]` — drop superseded store versions (confirm
    /// modal; the job is `tamias prune --yes` semantics).
    Prune {
        /// Newest versions kept per package.
        keep: usize,
    },
    /// `:log` — open the op-log sidebar (`L`).
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
        "uninstall" => {
            if rest.is_empty() {
                Err("usage: :uninstall <name>".into())
            } else {
                Ok(Cmd::Uninstall(rest))
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
        "prune" => {
            // `:prune [--keep N]` — the only flag; anything else is a
            // usage error (same posture as `:sync`).
            let mut keep = 1usize;
            let mut it = rest.split_whitespace();
            while let Some(tok) = it.next() {
                if let Some(v) = tok.strip_prefix("--keep=") {
                    keep = v
                        .parse()
                        .map_err(|_| "usage: :prune [--keep N]".to_string())?;
                } else if tok == "--keep" {
                    let v = it.next().ok_or("usage: :prune [--keep N]")?;
                    keep = v
                        .parse()
                        .map_err(|_| "usage: :prune [--keep N]".to_string())?;
                } else {
                    return Err("usage: :prune [--keep N]".into());
                }
            }
            Ok(Cmd::Prune { keep })
        }
        other => Err(format!("unknown command ':{other}' — `:help` lists verbs")),
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

/// Ctrl/Alt chords are never verb or text input — Shift is fine
/// (it arrives as the shifted char itself, e.g. `Char('G')` +
/// `SHIFT`). Ctrl-C force-quit is handled before this gate in
/// [`Model::key`].
fn chorded(k: &KeyEvent) -> bool {
    k.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

// --- shared single-line editing (`:`/`/`/prompt buffers) ---
//
// The cursor is a **byte** offset kept on a char boundary; every
// helper below maintains that invariant, so callers can slice
// `text[..cur]` freely. `line_edit` is the one key dispatcher the
// four text modes share — Left/Right/Home/End, Backspace/Delete,
// printable chars (inserted at the cursor), and the readline chords
// Ctrl-A/E (line ends), Ctrl-U (clear), Ctrl-W (kill word back).

/// Byte offset of the char boundary one char before `cur`.
fn prev_boundary(text: &str, cur: usize) -> usize {
    text[..cur]
        .chars()
        .next_back()
        .map_or(0, |c| cur - c.len_utf8())
}

/// Byte offset of the char boundary one char after `cur`.
fn next_boundary(text: &str, cur: usize) -> usize {
    text[cur..]
        .chars()
        .next()
        .map_or(text.len(), |c| cur + c.len_utf8())
}

/// Ctrl-W — delete from `cur` back over the whitespace run and the
/// word before it (readline `unix-word-rubout`). Returns `true` when
/// anything was deleted.
fn kill_word_back(text: &mut String, cur: &mut usize) -> bool {
    let start = {
        let before = text[..*cur].trim_end();
        before.rfind(char::is_whitespace).map_or(0, |i| {
            i + before[i..].chars().next().map_or(1, |c| c.len_utf8())
        })
    };
    if start < *cur {
        text.drain(start..*cur);
        *cur = start;
        true
    } else {
        false
    }
}

/// What [`line_edit`] did with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    /// Not an editing key — the mode ignores it.
    Ignored,
    /// Consumed but the buffer is unchanged — a cursor move or an
    /// edit at the boundary (Backspace on empty, Left at col 0).
    /// `:` mode still refreshes completions (they're cursor-relative);
    /// `/`-mode callers skip their post-edit hook so arrow keys don't
    /// re-fire the debounced search.
    Move,
    /// The buffer contents changed.
    Mutate,
}

/// One editing key against `(text, cur)`. Unhandled keys return
/// [`Edit::Ignored`] and change nothing; callers run their post-edit
/// hook on [`Edit::Mutate`] (and on `Move` where derived state is
/// cursor-relative, like `:` completions).
fn line_edit(text: &mut String, cur: &mut usize, k: &KeyEvent) -> Edit {
    // Clamp + re-boundary: direct buffer writes (tests, paste
    // callers) may leave the cursor mid-char.
    *cur = (*cur).min(text.len());
    while !text.is_char_boundary(*cur) {
        *cur -= 1;
    }
    match k.code {
        KeyCode::Char(c) => {
            if k.modifiers.contains(KeyModifiers::CONTROL) {
                match c {
                    'a' | 'A' => *cur = 0,
                    'e' | 'E' => *cur = text.len(),
                    'u' | 'U' => {
                        text.clear();
                        *cur = 0;
                        return Edit::Mutate;
                    }
                    'w' | 'W' => {
                        return if kill_word_back(text, cur) {
                            Edit::Mutate
                        } else {
                            Edit::Move
                        };
                    }
                    _ => return Edit::Ignored,
                }
                Edit::Move
            } else if k.modifiers.contains(KeyModifiers::ALT) {
                Edit::Ignored
            } else {
                text.insert(*cur, c);
                *cur += c.len_utf8();
                Edit::Mutate
            }
        }
        KeyCode::Backspace => {
            if *cur > 0 {
                let p = prev_boundary(text, *cur);
                text.drain(p..*cur);
                *cur = p;
                Edit::Mutate
            } else {
                Edit::Move
            }
        }
        KeyCode::Delete => {
            if *cur < text.len() {
                let n = next_boundary(text, *cur);
                text.drain(*cur..n);
                Edit::Mutate
            } else {
                Edit::Move
            }
        }
        KeyCode::Left => {
            *cur = prev_boundary(text, *cur);
            Edit::Move
        }
        KeyCode::Right => {
            *cur = next_boundary(text, *cur);
            Edit::Move
        }
        KeyCode::Home => {
            *cur = 0;
            Edit::Move
        }
        KeyCode::End => {
            *cur = text.len();
            Edit::Move
        }
        _ => Edit::Ignored,
    }
}

/// Byte offset where the token under `cur` starts — the char after
/// the last whitespace before the cursor (0 at buffer start).
fn token_start(text: &str, cur: usize) -> usize {
    text[..cur].rfind(char::is_whitespace).map_or(0, |i| {
        i + text[i..].chars().next().map_or(1, |c| c.len_utf8())
    })
}

/// Byte offset where the token under `cur` ends — the next
/// whitespace at/after the cursor, or buffer end.
fn token_end(text: &str, cur: usize) -> usize {
    text[cur..]
        .find(char::is_whitespace)
        .map_or(text.len(), |i| cur + i)
}

/// Collapse raw search hits to one row per package — the index
/// returns every retained version, so without this a 3-version
/// package renders 3 near-identical rows. Groups by `(name, scope)`
/// in first-seen order and keeps the newest version
/// ([`version_cmp`]); the survivor is a real index entry, so
/// `reg_selected`, `installed_label_for`, and the detail page's
/// versions-focus (`file`-matched `← current`) all keep working.
/// Returns the rows plus a parallel per-package version count —
/// `>1` renders `· N versions` in the name cell.
fn dedupe_search_results(results: Vec<RegistryEntry>) -> (Vec<RegistryEntry>, Vec<usize>) {
    let mut rows: Vec<RegistryEntry> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut slots: std::collections::HashMap<(String, Option<String>), usize> =
        std::collections::HashMap::new();
    for e in results {
        let key = (e.name.clone(), e.scope.clone());
        match slots.get(&key).copied() {
            Some(i) => {
                counts[i] += 1;
                if version_cmp(e.version.as_deref(), rows[i].version.as_deref())
                    == Ordering::Greater
                {
                    rows[i] = e;
                }
            }
            None => {
                slots.insert(key, rows.len());
                rows.push(e);
                counts.push(1);
            }
        }
    }
    (rows, counts)
}

/// The version tail of a store label — `name@1.2.3` and
/// `@scope/name@1.2.3` both yield `1.2.3`; unversioned records carry a
/// resolved-sha short form which compares as a plain string. Used to
/// pick the newest provider when several store entries back one
/// service unit.
fn svc_version(label: &str) -> &str {
    label.rsplit('@').next().unwrap_or(label)
}

/// Collapse service rows to one per unit — the store keeps every
/// version ever installed and each re-declares the same `provider:`
/// units, so the raw list repeats e.g. `peregrin-campaign` once per
/// `pr-review@x`. Groups by `unit` in first-seen order; the survivor
/// is the row whose package label carries the newest version
/// ([`version_str_cmp`] over [`svc_version`]) — the provider `Enter`,
/// deploy, and the log/run verbs act on. Returns the rows plus a
/// parallel per-unit provider count (`>1` renders `×N` in the
/// package cell).
fn dedupe_service_rows(rows: Vec<ServiceRow>) -> (Vec<ServiceRow>, Vec<usize>) {
    let mut out: Vec<ServiceRow> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut slots: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for r in rows {
        match slots.get(&r.unit).copied() {
            Some(i) => {
                counts[i] += 1;
                if version_str_cmp(svc_version(&r.package), svc_version(&out[i].package))
                    == Ordering::Greater
                {
                    out[i] = r;
                }
            }
            None => {
                slots.insert(r.unit.clone(), out.len());
                out.push(r);
                counts.push(1);
            }
        }
    }
    (out, counts)
}

/// The Installed pane's group identity — `@scope/name`, the store
/// label minus its `@version` tail. Stable across version bumps, so
/// it's what `sel_key` remembers between reloads.
fn group_key(rec: &StoreRecord) -> String {
    match &rec.scope {
        Some(scope) => format!("@{scope}/{}", rec.name),
        None => rec.name.clone(),
    }
}

/// Order two version labels for "newest wins" — `None` sorts lowest;
/// parseable versions go through [`version_str_cmp`].
fn version_cmp(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => version_str_cmp(x, y),
    }
}

/// Split a version label into `(numeric core, prerelease tail)` —
/// `+build` metadata is dropped per semver; the `-` tail keeps its
/// leading dash for the string compare.
fn version_split(v: &str) -> (&str, &str) {
    let no_build = v.split('+').next().unwrap_or(v);
    match no_build.find('-') {
        Some(i) => (&no_build[..i], &no_build[i..]),
        None => (no_build, ""),
    }
}

/// A small semver approximation for version ordering — the `semver`
/// crate isn't a tamias-tui dep, and the index itself only
/// string-sorts versions (where `0.9.0` would wrongly beat
/// `0.10.0`). The numeric core compares segment-wise; a bare release
/// outranks its `-prerelease`; remaining tails compare as strings.
fn version_str_cmp(a: &str, b: &str) -> Ordering {
    let (ac, at) = version_split(a);
    let (bc, bt) = version_split(b);
    let mut ai = ac.split('.');
    let mut bi = bc.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(p), Ok(q)) => p.cmp(&q),
                    _ => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
    match (at.is_empty(), bt.is_empty()) {
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    at.cmp(bt)
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
    /// Index into the *filtered* collapsed installed row list — one
    /// row per `(name, scope)` group, reps only (clamped on rebuild).
    pub sel: usize,
    /// Cursor into the Home pane's recently-installed list
    /// ([`Model::recent_installed`]) — `j`/`k`/wheel move it, `Enter`
    /// opens the highlighted entry's package page.
    pub home_sel: usize,
    /// `recent_installed` cache — the `installed_at`-desc top-8 index
    /// order, rebuilt on reload instead of re-sorted per call/frame.
    recent: Vec<usize>,
    /// `(name, scope)` → installed version label, sorted by key —
    /// rebuilt on reload; the Registry pane's per-row `· installed`
    /// marker and the package page's marker query it per frame.
    installed_labels: Vec<((String, Option<String>), String)>,
    /// Group identity (`@scope/name`) of the selected row before the
    /// last rebuild — keeps the cursor on the same package across
    /// reloads, including when a newer version becomes the rep.
    sel_key: Option<String>,
    /// `/` live filter — case-insensitive substring over label,
    /// source ref, kind, and resolved sha.
    pub filter: String,
    /// Current mode.
    pub mode: Mode,
    /// `/` input buffer (the filter text while typing).
    pub input: String,
    /// Cursor into `input` — Filter and Search share the buffer, so
    /// they share the cursor (only one is active at a time).
    input_cur: usize,
    /// `:` command-line buffer — separate from `input` so a filter
    /// draft isn't clobbered.
    pub cmdline: String,
    /// Cursor into `cmdline` — a byte offset kept on a char boundary
    /// by the shared [`line_edit`] helpers.
    cmd_cur: usize,
    /// `:` history — most recent first, session-scoped, deduped
    /// against consecutive repeats, capped at [`CMD_HIST_CAP`].
    cmd_history: VecDeque<String>,
    /// History recall cursor — `Some(i)` indexes `cmd_history`
    /// (0 = newest); `None` = editing a fresh line.
    cmd_hist: Option<usize>,
    /// The buffer stashed when recall began — `Down` past the newest
    /// entry restores it.
    cmd_draft: String,
    /// Live `:` completions for the token under the cursor
    /// ([`crate::complete`]) — refreshed on every buffer edit.
    pub cmd_completions: Vec<crate::complete::Candidate>,
    /// Tab-selected candidate index — `usize::MAX` = none applied
    /// yet, so the first `Tab` lands on candidate 0.
    pub cmd_sel: usize,
    /// Prompt buffer (`p`/`i`/`:registry` text entry).
    pub prompt_buf: String,
    /// Cursor into `prompt_buf`.
    prompt_cur: usize,
    /// Status-bar message (last action / error) — expires after
    /// [`STATUS_TTL`] unless it's an error (errors are sticky).
    pub status: String,
    /// When `status` was last set.
    status_at: Instant,
    /// `status` is an error — holds the bar until the next input.
    pub status_err: bool,
    /// Clipboard payload queued by `y` on the package page —
    /// `run` drains it into an OSC 52 write between frames (the TUI
    /// owns the terminal, so the model only records the request).
    pub copy_request: Option<String>,
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
    /// The log view's block rect from the last frame (sidebar or
    /// overlay) — wheel events inside it scroll the log while
    /// `mode` is [`Mode::Log`]. `Rect::default()` until drawn.
    pub log_area: Rect,
    /// Help-overlay scroll offset — `j`/`k`/`PgUp`/`PgDn`/`g`/`G`/
    /// wheel move it while `mode` is [`Mode::Help`]; the offset
    /// counts *wrapped* display rows (the overlay's Paragraph wraps
    /// at the box width) and clamps each frame like `detail_scroll`.
    pub help_scroll: usize,
    /// Inner height of the help box as last drawn — `PgUp`/`PgDn`
    /// page by it (0 until the first frame).
    pub(crate) help_view_h: usize,
    /// Last frame's max `help_scroll` (wrapped help lines minus the
    /// box's inner height) — `G` jumps to the bottom; `draw_help`
    /// refreshes it every frame the overlay is up.
    pub(crate) help_max_scroll: usize,
    /// The help overlay's block rect from the last frame — wheel
    /// events inside it scroll the text while `mode` is
    /// [`Mode::Help`]. `Rect::default()` until drawn.
    pub help_area: Rect,

    // --- worker jobs (phase 2/3) ---
    /// Job → UI channel; one worker thread per `spawn`.
    work_tx: Sender<Msg>,
    /// UI ← worker channel, drained in `tick`.
    work_rx: Receiver<Msg>,
    /// The running job's label (`None` = idle) — drives the spinner.
    pub busy: Option<String>,
    /// The running [`Job`] itself — `Done` arms read it to decide
    /// whether the store/services views need a refresh (read-only
    /// jobs skip both). Cleared alongside `busy`.
    busy_job: Option<Job>,
    /// Op log — phase lines, warnings, results; the `L` sidebar
    /// (or its narrow-terminal overlay fallback) shows the history.
    pub log: VecDeque<String>,
    /// Confirm/consent dialog, if any.
    pub modal: Option<Modal>,
    /// Package page (`Enter` on an Installed row or Registry result)
    /// — see [`Detail`].
    pub detail: Option<Detail>,
    /// The pane an `open_label_detail` page was opened from (Home or
    /// Services) — [`Model::close_detail`] restores it so Esc lands
    /// back where `Enter` was pressed, not always on Installed.
    /// `None` for pages opened on their own pane (Installed/Registry
    /// `Enter`) and cleared by every pane switch.
    detail_origin: Option<Pane>,
    /// Page scroll offset — `j`/`k`/arrows/wheel move it while
    /// `detail` is open; clamped to the body height each frame.
    /// Counts *display* rows: the body Paragraph wraps, and
    /// `Paragraph::scroll` skips wrapped lines.
    pub detail_scroll: usize,
    /// Last frame's max `detail_scroll` (the body's wrapped line
    /// count minus the body area) — `G` jumps straight to the
    /// bottom; `draw_detail` refreshes it every frame the page is
    /// open.
    pub(crate) detail_max_scroll: usize,
    /// Versions-focus mode on a Registry package page — `v` toggles
    /// it once the sibling-version list has landed; while set, `j`/`k`
    /// move [`Model::versions_sel`] instead of scrolling the page and
    /// `i` installs the selected version. Meaningless (and cleared)
    /// for Installed pages.
    pub versions_focus: bool,
    /// Cursor into the page's `versions` list while `versions_focus`
    /// is set — clamped to the list length on every move.
    pub versions_sel: usize,
    /// Log-view scroll offset — the top line of `log` on screen
    /// while `mode` is [`Mode::Log`]; clamped to the view each frame
    /// like `detail_scroll`.
    pub log_scroll: usize,
    /// Inner height of the log view as last drawn — `PgUp`/`PgDn`
    /// page by it and `open_log` aims the tail at it (0 until the
    /// first frame).
    pub(crate) log_view_h: usize,

    // --- registry pane ---
    /// Last search results — deduped to one row per `(name, scope)`
    /// package (the newest version survives; see
    /// [`dedupe_search_results`]).
    pub reg_results: Vec<RegistryEntry>,
    /// Cursor into `reg_results`.
    pub reg_sel: usize,
    /// Version multiplicity per `reg_results` row — parallel vec;
    /// `>1` renders `· N versions` in the name cell.
    pub reg_version_counts: Vec<usize>,
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
    /// Parallel to `svc_rows` — how many store entries provide each
    /// row's unit (`>1` renders `×N` in the package cell). Rows are
    /// deduped to one per unit at ingest ([`dedupe_service_rows`]).
    pub svc_counts: Vec<usize>,
    /// Cursor into `svc_rows`.
    pub svc_sel: usize,
    /// Services rows are stale — pane entry, unit mutations,
    /// `:services`, and mutating-job completions set it; `tick`
    /// turns it into a refresh job from any pane.
    pub svc_stale: bool,
    /// Last services-refresh failure.
    pub svc_err: Option<String>,
    /// The running stream's killable child (`run`/`logs -f`) — `x`
    /// kills it; quitting the app kills it too.
    pub stream_child: Option<Arc<Mutex<Child>>>,

    // --- project pane (phase 5) ---
    /// The project manifest path the deps form edits.
    pub manifest_path: PathBuf,
    /// The parsed project manifest (`deps:` editable; the rest is
    /// rendered read-only).
    pub project: Option<Manifest>,
    /// Why the manifest is absent/unloadable.
    pub man_err: Option<String>,
    /// Cursor into `project.deps`.
    pub man_sel: usize,

    // --- publish pane (phase 6) ---
    /// Configured registries for the publish pane's table — loaded on
    /// pane entry (`ops::registry_rows`).
    pub author_regs: Vec<ops::RegistryRow>,
    /// Cursor into `author_regs`.
    pub author_sel: usize,
    /// Registry names for `:` completion (`:registry`, `:publish`'s
    /// registry arg) — refreshed on every store `reload`; the
    /// Registry pane's empty-query browse list reads it too.
    pub reg_names: Vec<String>,
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
            recent: Vec::new(),
            installed_labels: Vec::new(),
            sel_key: None,
            filter: String::new(),
            mode: Mode::Normal,
            input: String::new(),
            input_cur: 0,
            cmdline: String::new(),
            cmd_cur: 0,
            cmd_history: VecDeque::new(),
            cmd_hist: None,
            cmd_draft: String::new(),
            cmd_completions: Vec::new(),
            cmd_sel: usize::MAX,
            prompt_buf: String::new(),
            prompt_cur: 0,
            status: String::new(),
            status_at: Instant::now(),
            status_err: false,
            copy_request: None,
            watcher: None,
            watch_rx: None,
            dirty: false,
            tick_n: 0,
            board_area: Rect::default(),
            viewport_top: 0,
            tabs_area: None,
            home_area: Rect::default(),
            log_area: Rect::default(),
            work_tx,
            work_rx,
            busy: None,
            busy_job: None,
            log: VecDeque::new(),
            modal: None,
            detail: None,
            detail_origin: None,
            detail_scroll: 0,
            detail_max_scroll: 0,
            versions_focus: false,
            versions_sel: 0,
            log_scroll: 0,
            log_view_h: 0,
            help_scroll: 0,
            help_view_h: 0,
            help_max_scroll: 0,
            help_area: Rect::default(),
            reg_results: Vec::new(),
            reg_sel: 0,
            reg_version_counts: Vec::new(),
            reg_query: String::new(),
            reg_target: None,
            reg_target_label: "default".to_string(),
            reg_err: None,
            search_typed_at: None,
            search_inflight: None,
            svc_rows: Vec::new(),
            svc_counts: Vec::new(),
            svc_sel: 0,
            // Lazy: pane entry, `:services`, unit mutations, and
            // mutating-job completions set it — `tick` fires the
            // refresh wherever the user happens to be.
            svc_stale: false,
            svc_err: None,
            stream_child: None,
            manifest_path: PathBuf::from("tamias.yaml"),
            project: None,
            man_err: None,
            man_sel: 0,
            author_regs: Vec::new(),
            author_sel: 0,
            reg_names: Vec::new(),
        };
        m.reload();
        Ok(m)
    }

    /// Re-list the store from disk, keeping the cursor on the same
    /// label when it survives. `R`, `:reload`, watcher events, and
    /// the rescan fallback all land here.
    pub fn reload(&mut self) {
        let at = self.sel;
        let keep = self.sel_key.clone();
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
        self.rebuild_derived();
        // The recent list shrank under the cursor (uninstall, or a
        // fresh install reordering it) — clamp like every other sel.
        let n = self.recent_installed().len();
        self.home_sel = if n == 0 { 0 } else { self.home_sel.min(n - 1) };
        self.arm_watch();
        self.load_manifest();
        // Registry names feed `:` completion — refresh with the rest
        // of the reload so `:registry`/`:publish` args stay current.
        self.reg_names = ops::registry_names(&self.ctx)
            .into_iter()
            .map(|(name, _)| name)
            // The `publish.default → x`/`search.default → x` pseudo
            // rows aren't registries a `:registry` target can name.
            .filter(|n| !n.contains('→'))
            .collect();
    }

    /// (Re)read the project manifest for the Project pane — a tiny
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
        // unit mutations, `:services`, and mutating-job completions
        // all set the flag; `busy` serializes it so refreshes never
        // queue behind ops, and the refresh runs from any pane.
        if self.svc_stale && self.busy.is_none() {
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
        self.busy_job = Some(job.clone());
        let ctx = self.ctx.clone();
        let tx = self.work_tx.clone();
        let j = job;
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
                    self.busy_job = None;
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
                    // Only mutating jobs invalidate the store/services
                    // views — verify/doctor/registries/pack/publish/
                    // stream exits are read-only and skip the reload.
                    let mutating = self.busy_job.as_ref().is_some_and(Job::mutates);
                    self.busy = None;
                    self.busy_job = None;
                    self.stream_child = None;
                    if mutating {
                        self.svc_stale = true;
                    }
                    // Reports (doctor, registries) arrive `\n`-joined —
                    // first line to the status bar, every line its own
                    // log entry instead of one giant wrapped row.
                    let mut lines = text.lines();
                    let first = lines.next().unwrap_or("done");
                    self.push_log(format!("ok — {first}"));
                    self.say(first.to_string());
                    for line in lines {
                        self.push_log(line.to_string());
                    }
                    if mutating {
                        self.reload();
                    }
                }
                Msg::Done(Ok(Outcome::Search { target, results })) => {
                    self.busy = None;
                    self.busy_job = None;
                    // Drop stale hits — the query moved on while the
                    // search was in flight; the debounce will refire.
                    let fresh = self
                        .search_inflight
                        .as_deref()
                        .is_some_and(|q| q == self.reg_query.trim());
                    self.search_inflight = None;
                    if fresh {
                        self.reg_target_label = target;
                        let (rows, counts) = dedupe_search_results(results);
                        self.reg_results = rows;
                        self.reg_version_counts = counts;
                        self.reg_sel = 0;
                        self.reg_err = None;
                        let n = self.reg_results.len();
                        let versions: usize = self.reg_version_counts.iter().sum();
                        self.push_log(format!(
                            "search {:?}: {n} package(s) · {versions} version(s)",
                            self.reg_query
                        ));
                    }
                }
                Msg::Done(Ok(Outcome::Services(mut rows))) => {
                    self.busy = None;
                    self.busy_job = None;
                    self.svc_err = None;
                    // Keep the cursor on the same unit across
                    // refreshes — rows rebuild wholesale and `svc_sel`
                    // is index-based.
                    let keep = self.svc_selected().map(|r| r.unit.clone());
                    // The Services pane dedupes to one row per unit —
                    // every retained store version re-declares the same
                    // provider units. Rows arrive in store order,
                    // which happens to sort by label today but isn't
                    // a `service_rows` contract, so sort defensively.
                    // Stable sort by (package, service, unit); the
                    // dedupe keeps first-seen positions with the
                    // newest-version provider as each row's content.
                    rows.sort_by(|a, b| {
                        a.package
                            .cmp(&b.package)
                            .then_with(|| a.service.cmp(&b.service))
                            .then_with(|| a.unit.cmp(&b.unit))
                    });
                    let (rows, counts) = dedupe_service_rows(rows);
                    self.svc_rows = rows;
                    self.svc_counts = counts;
                    self.svc_sel = keep
                        .and_then(|u| self.svc_rows.iter().position(|r| r.unit == u))
                        .unwrap_or_else(|| self.svc_sel.min(self.svc_rows.len().saturating_sub(1)));
                    self.push_log(format!("services: {} row(s)", self.svc_rows.len()));
                }
                Msg::Done(Err(e)) => {
                    // A failed mutating job may have changed provider
                    // state before erroring — mark services stale;
                    // read-only failures (incl. a failed refresh
                    // itself) must not re-arm the refresh.
                    let mutating = self.busy_job.as_ref().is_some_and(Job::mutates);
                    self.busy = None;
                    self.busy_job = None;
                    self.stream_child = None;
                    if mutating {
                        self.svc_stale = true;
                    }
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
    /// the pane shows its hint instead). While a job holds `busy`,
    /// `spawn` would refuse and the query would be lost — re-arm the
    /// debounce instead so [`Model::maybe_search`] retries once the
    /// job lands.
    fn fire_search(&mut self) {
        if self.busy.is_some() {
            self.search_typed_at = Some(Instant::now());
            return;
        }
        let query = self.reg_query.trim().to_string();
        if query.is_empty() {
            self.reg_results.clear();
            self.reg_version_counts.clear();
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

    /// One line into the op log (capped). While the log view is
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

    /// Indices of the rows passing the `/` filter, in store order —
    /// one per `(name, scope)` group, each the representative
    /// (latest-version) entry's index into `entries`. The filter is a
    /// case-insensitive substring over what the row shows — label,
    /// kind, source ref, description, sealed badge — matched against
    /// *any* group member, so an old version's label still finds the
    /// package. Provenance (`resolved`/`digest`) is deliberately
    /// excluded: hex fragments would match nearly everything.
    pub fn visible(&self) -> Vec<usize> {
        self.visible_rows()
            .into_iter()
            .map(|(rep, _)| rep)
            .collect()
    }

    /// The visible collapsed rows — `(rep entry index, version count)`
    /// per group passing the `/` filter; the renderer's view of
    /// [`Model::visible`]. Indices are into `entries`, reps only.
    pub fn visible_rows(&self) -> Vec<(usize, usize)> {
        let needle = self.filter.to_lowercase();
        self.inst_groups()
            .into_iter()
            .filter(|g| {
                needle.is_empty()
                    || g.members.iter().any(|&i| {
                        let r = &self.entries[i];
                        let rec = &r.entry.record;
                        let sealed = if r.sealed { " sealed" } else { "" };
                        let hay = format!(
                            "{} {} {} {}{}",
                            r.entry.label(),
                            rec.kind.as_deref().unwrap_or(""),
                            rec.source_ref,
                            rec.description.as_deref().unwrap_or(""),
                            sealed
                        )
                        .to_lowercase();
                        hay.contains(&needle)
                    })
            })
            .map(|g| (g.rep, g.members.len()))
            .collect()
    }

    /// Collapse `entries` into one [`InstGroup`] per `(name, scope)` —
    /// first-seen (store/label) order, representative = the semver-max
    /// version ([`version_cmp`]; ties keep the first-seen entry).
    /// Recomputed per call: `entries` is `pub` and tests mutate it
    /// directly, so a cache would drift.
    fn inst_groups(&self) -> Vec<InstGroup> {
        let mut groups: Vec<InstGroup> = Vec::new();
        let mut slots: std::collections::HashMap<(String, Option<String>), usize> =
            std::collections::HashMap::new();
        for (i, r) in self.entries.iter().enumerate() {
            let rec = &r.entry.record;
            let key = (rec.name.clone(), rec.scope.clone());
            match slots.get(&key).copied() {
                Some(g) => {
                    groups[g].members.push(i);
                    let rep = groups[g].rep;
                    if version_cmp(
                        rec.version.as_deref(),
                        self.entries[rep].entry.record.version.as_deref(),
                    ) == Ordering::Greater
                    {
                        groups[g].rep = i;
                    }
                }
                None => {
                    slots.insert(key, groups.len());
                    groups.push(InstGroup {
                        rep: i,
                        members: vec![i],
                    });
                }
            }
        }
        groups
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
    /// label order), top 8. Cached by [`Model::rebuild_derived`] on
    /// reload; the renderer and the `home_sel` cursor share this
    /// ordering so they can never disagree.
    pub fn recent_installed(&self) -> &[usize] {
        &self.recent
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
    /// Binary-searches the [`Model::installed_labels`] cache rebuilt
    /// on reload — O(log n), no per-row scan per frame.
    pub fn installed_label_for(&self, name: &str, scope: Option<&str>) -> Option<String> {
        self.installed_labels
            .binary_search_by(|probe| {
                probe
                    .0
                     .0
                    .as_str()
                    .cmp(name)
                    .then_with(|| probe.0 .1.as_deref().cmp(&scope))
            })
            .ok()
            .map(|i| self.installed_labels[i].1.clone())
    }

    /// Rebuild the per-reload derived caches — the sorted
    /// `(name, scope)` → version-label table [`Model::installed_label_for`]
    /// searches and the [`Model::recent`] ordering. Called at the end
    /// of `reload`; tests that plant `entries` directly call it too.
    fn rebuild_derived(&mut self) {
        // Every (name, scope) → label pair, sorted by key; the stable
        // sort keeps store order inside equal keys and `dedup_by`
        // keeps the first — the same row the old linear `find_map`
        // returned.
        let mut labels: Vec<((String, Option<String>), String)> = self
            .entries
            .iter()
            .map(|r| {
                let rec = &r.entry.record;
                let label = rec.version.clone().unwrap_or_else(|| {
                    let sha = rec
                        .resolved
                        .strip_prefix("sha256:")
                        .unwrap_or(&rec.resolved);
                    sha.chars().take(12).collect()
                });
                ((rec.name.clone(), rec.scope.clone()), label)
            })
            .collect();
        labels.sort_by(|a, b| a.0.cmp(&b.0));
        labels.dedup_by(|a, b| a.0 == b.0);
        self.installed_labels = labels;
        let mut idx: Vec<usize> = (0..self.entries.len()).collect();
        idx.sort_by(|&a, &b| {
            self.entries[b]
                .entry
                .record
                .installed_at
                .cmp(&self.entries[a].entry.record.installed_at)
        });
        idx.truncate(8);
        self.recent = idx;
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

    /// Keep the cursor on `keep`'s group (`@scope/name`) when it
    /// survives the reload, else clamp `at` into the filtered range.
    /// Matching the group — not the rep's `name@version` label — keeps
    /// the cursor put when a newer version becomes the rep.
    fn rebuild_sel(&mut self, keep: Option<&str>, at: usize) {
        let vis = self.visible();
        if let Some(key) = keep {
            if let Some(pos) = vis.iter().position(|&i| {
                self.entries
                    .get(i)
                    .is_some_and(|r| group_key(&r.entry.record) == key)
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
            // Publish walks its registries table; an unconfigured
            // pane has nothing to select — say so.
            Pane::Author => {
                let len = self.author_regs.len();
                if len == 0 {
                    self.say("nothing to select on this pane");
                    return;
                }
                self.author_sel =
                    (self.author_sel as isize + delta).clamp(0, len as isize - 1) as usize;
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
        self.detail_origin = None;
        self.detail_scroll = 0;
        self.detail_max_scroll = 0;
        self.versions_focus = false;
        self.versions_sel = 0;
        self.sel_key = self.selected().map(|r| group_key(&r.entry.record));
        match pane {
            // Services entry marks the table stale — `tick` fires the
            // refresh once the worker frees up.
            Pane::Services => self.svc_stale = true,
            // Project entry re-reads the project manifest — a tiny
            // file read, same posture as the detail view.
            Pane::Manifest => self.load_manifest(),
            // Publish entry refreshes the configured-registry list.
            Pane::Author => {
                self.author_regs = ops::registry_rows(&self.ctx);
                self.author_sel = self
                    .author_sel
                    .min(self.author_regs.len().saturating_sub(1));
            }
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

    /// Record the selected row's group key — call after moves too, so
    /// a `j` then `R` lands back on the same package.
    fn sync_sel_key(&mut self) {
        self.sel_key = self.selected().map(|r| group_key(&r.entry.record));
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
            // Same close as esc/h/← — restores the origin pane a
            // Home/Services `Enter` recorded.
            self.close_detail();
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
                // The collapsed row's representative — the latest
                // version's entry — is what the page and every
                // row-keyed verb act on.
                let Some(row) = self.selected() else {
                    return;
                };
                self.open_installed_detail(row.entry.clone());
            }
        }
        self.detail_scroll = 0;
        self.detail_max_scroll = 0;
        self.versions_focus = false;
        self.versions_sel = 0;
    }

    /// Open the package page for one store entry — the shared tail of
    /// [`Model::toggle_detail`] (the selected group's rep) and
    /// [`Model::open_label_detail`] (Home/Services name an exact
    /// `name@version`, which may be an older group member).
    fn open_installed_detail(&mut self, entry: StoreEntry) {
        let (manifest, manifest_err) = match entry.manifest() {
            Ok(m) => (Some(Box::new(m)), None),
            Err(e) => (None, Some(e.to_string())),
        };
        self.detail = Some(Detail::Installed {
            entry,
            manifest,
            manifest_err,
        });
        self.detail_scroll = 0;
        self.detail_max_scroll = 0;
        self.versions_focus = false;
        self.versions_sel = 0;
    }

    /// `Enter` on Home — open the highlighted recent install's package
    /// page through [`Model::open_label_detail`].
    fn open_home_detail(&mut self) {
        let Some(label) = self.home_selected().map(|r| r.entry.label()) else {
            return;
        };
        self.open_label_detail(&label);
    }

    /// `Enter` on a Services row — open the owning package's page.
    /// Same route as Home's recents: the detail verbs are pane-gated
    /// to Installed, so the pane switches and the normal detail path
    /// runs. A package missing from the store gets a hint instead of
    /// a page (rows come from the store, so this is a stale-table
    /// edge, not the common path).
    fn open_service_detail(&mut self) {
        let Some(label) = self.svc_selected().map(|r| r.package.clone()) else {
            return;
        };
        if !self.open_label_detail(&label) {
            self.say("package not in the installed list — `:services` refreshes");
        }
    }

    /// Shared tail of the Home/Services `Enter` paths — `label` names
    /// one store entry exactly (a full `name@version` label, which may
    /// be an older group member, not the rep). Park the Installed
    /// cursor on that entry's *group* (dropping a stale `/` filter
    /// that would hide it) and open the page for the labeled entry
    /// itself. The origin pane is recorded so [`Model::close_detail`]
    /// lands back on it — Esc/Enter-close return to Home or Services
    /// rather than always Installed. Returns `false` when no entry or
    /// visible group matches; the caller decides what to say.
    fn open_label_detail(&mut self, label: &str) -> bool {
        // Resolve the exact entry across *all* entries — the `/`
        // filter only hides rows, it can't change which version a
        // label names.
        let Some(entry_idx) = self.entries.iter().position(|r| r.entry.label() == label) else {
            return false;
        };
        let key = group_key(&self.entries[entry_idx].entry.record);
        let group_pos = |m: &Model, vis: &[usize]| {
            vis.iter().position(|&i| {
                m.entries
                    .get(i)
                    .is_some_and(|r| group_key(&r.entry.record) == key)
            })
        };
        let mut vis = self.visible();
        let mut pos = group_pos(self, &vis);
        if pos.is_none() && !self.filter.is_empty() {
            // The filter is an Installed-pane artifact — the origin
            // pane can't show it hiding the target, so drop it and
            // retry.
            self.filter.clear();
            self.input.clear();
            self.input_cur = 0;
            vis = self.visible();
            pos = group_pos(self, &vis);
        }
        let Some(pos) = pos else {
            return false;
        };
        let origin = self.pane;
        self.switch_pane(Pane::Installed);
        self.sel = pos;
        // `switch_pane` recorded the *old* cursor's key — re-sync so a
        // reload lands back on the group just opened.
        self.sync_sel_key();
        self.open_installed_detail(self.entries[entry_idx].entry.clone());
        if self.detail.is_some() {
            self.detail_origin = Some(origin);
        }
        true
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

    /// Open the confirm modal for a mutating `job` — the title is the
    /// job's own label (`install nine:tamias@0.3.0 — sure? [y/n]`), so
    /// the dialog always names exactly what `y` will mutate. The body
    /// warns about driver teardown when the record's kind isn't
    /// `files` (serve's remove rule, surfaced up front).
    fn confirm_job(&mut self, job: Job) {
        // Resolve the entry for the teardown warning.
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
            title: format!("{} — sure? [y/n]", job.label()),
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

    /// `n`/`esc` on a modal — cancels (the safe default). A declined
    /// driver-consent install says so on the status bar instead of
    /// vanishing silently.
    fn modal_no(&mut self) {
        if matches!(self.modal, Some(Modal::Consent { .. })) {
            self.say("install declined — bundled drivers not allowed");
        }
        self.modal = None;
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
        self.prompt_cur = 0;
        match kind {
            PromptKind::Pin { name } => {
                if text.is_empty() {
                    self.say_err("pin: empty ref");
                } else {
                    self.confirm_job(Job::Pin {
                        name,
                        reference: text,
                    });
                }
            }
            PromptKind::InstallRef => {
                if text.is_empty() {
                    self.say_err("install: empty ref");
                } else {
                    self.confirm_job(Job::Install {
                        reference: text,
                        allow_drivers: false,
                    });
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
                    // Same confirm posture as the sibling mutating
                    // verbs — the title names the ref being added.
                    self.confirm_job(Job::AddDep {
                        reference: text,
                        allow_drivers: false,
                    });
                }
            }
            PromptKind::SetDep { old } => {
                if text.is_empty() {
                    self.say_err("set dep: empty ref");
                } else {
                    self.confirm_job(Job::SetDep {
                        old,
                        new: text,
                        allow_drivers: false,
                    });
                }
            }
            PromptKind::DeployConfig { name } => {
                // Empty answer = no driver config, same as `:deploy`.
                let config = if text.is_empty() { None } else { Some(text) };
                self.confirm_job(Job::ProviderDeploy { name, config });
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
                    self.confirm_job(Job::Publish {
                        dir: dir.to_string(),
                        registry: registry.map(str::to_string),
                    });
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
                self.confirm_job(Job::Install {
                    reference,
                    allow_drivers: false,
                });
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
            self.confirm_job(Job::Update {
                name: label,
                allow_drivers: false,
            });
        }
    }

    /// `p` — pin prompt for the selected entry.
    fn verb_pin(&mut self) {
        if let Some(label) = self.selected_label() {
            self.prompt_buf.clear();
            self.prompt_cur = 0;
            self.mode = Mode::Prompt(PromptKind::Pin { name: label });
        }
    }

    /// `x` — remove confirm (teardown warning for non-`files`).
    fn verb_remove(&mut self) {
        if let Some(label) = self.selected_label() {
            self.confirm_job(Job::Remove(label));
        }
    }

    /// `c`/`C` — cache clean confirm (>30d / all).
    fn verb_cache_clean(&mut self, all: bool) {
        self.confirm_job(Job::CacheClean { all });
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
                self.prompt_cur = 0;
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
            self.confirm_job(Job::UnitAction {
                unit: row.unit.clone(),
                action,
            });
            return;
        }
        match key {
            'd' => {
                self.prompt_buf.clear();
                self.prompt_cur = 0;
                self.mode = Mode::Prompt(PromptKind::DeployConfig {
                    name: row.package.clone(),
                });
            }
            'x' => {
                self.confirm_job(Job::ProviderUninstall {
                    name: row.package.clone(),
                });
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

    // --- Project pane verbs ---

    /// `a`/`x`/`d`/`e`/`s` on the deps form — add/remove/set-ref/sync.
    fn man_verb(&mut self, key: char) {
        match key {
            'a' => {
                self.prompt_buf.clear();
                self.prompt_cur = 0;
                self.mode = Mode::Prompt(PromptKind::AddDep);
            }
            // `s` reconciles the whole `deps:` set (`tamias sync`) —
            // confirm first like the other mutating verbs. `S` is the
            // lock-spec variant (`tamias sync --from-lock`).
            's' | 'S' => self.confirm_job(Job::Sync {
                allow_drivers: false,
                from_lock: key == 'S',
            }),
            // `d`/`x` both drop the dep (the proposal's two remove
            // keys); `a` adds, `e` edits the ref.
            'x' | 'd' => match self.dep_selected() {
                Some(dep) => {
                    self.confirm_job(Job::RemoveDep { reference: dep });
                }
                None => self.say_err("no dep selected"),
            },
            'e' => match self.dep_selected() {
                Some(old) => {
                    self.prompt_buf.clear();
                    self.prompt_cur = 0;
                    self.mode = Mode::Prompt(PromptKind::SetDep { old });
                }
                None => self.say_err("no dep selected"),
            },
            'R' => self.load_manifest(),
            _ => {}
        }
    }

    // --- Publish pane verbs ---

    /// `p`/`P`/`r`/`R` on the Publish pane — pack/publish/registries.
    fn author_verb(&mut self, key: char) {
        match key {
            'p' => {
                self.prompt_buf.clear();
                self.prompt_cur = 0;
                self.mode = Mode::Prompt(PromptKind::PackSpec);
            }
            'P' => {
                self.prompt_buf.clear();
                self.prompt_cur = 0;
                self.mode = Mode::Prompt(PromptKind::PublishSpec);
            }
            'r' => self.spawn(Job::Registries),
            'R' => {
                self.author_regs = ops::registry_rows(&self.ctx);
                self.author_sel = self
                    .author_sel
                    .min(self.author_regs.len().saturating_sub(1));
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
            Ok(Cmd::Help) => self.open_help(),
            Ok(Cmd::Log) => self.open_log(),
            Ok(Cmd::Install(reference)) => {
                self.confirm_job(Job::Install {
                    reference,
                    allow_drivers: false,
                });
            }
            Ok(Cmd::Remove(name)) => {
                self.confirm_job(Job::Remove(name));
            }
            Ok(Cmd::Uninstall(name)) => {
                self.confirm_job(Job::ProviderUninstall { name });
            }
            Ok(Cmd::Verify(name)) => self.spawn(Job::Verify(name)),
            Ok(Cmd::Update(name)) => match name {
                Some(name) => self.confirm_job(Job::Update {
                    name,
                    allow_drivers: false,
                }),
                None => {
                    let n = self.entries.len();
                    if n == 0 {
                        self.say_err("nothing installed — `:install <ref>` installs by ref");
                    } else {
                        self.confirm_job(Job::UpdateAll {
                            allow_drivers: false,
                        });
                    }
                }
            },
            Ok(Cmd::Pin(name, reference)) => {
                self.confirm_job(Job::Pin { name, reference });
            }
            Ok(Cmd::Search(query)) => {
                self.switch_pane(Pane::Registry);
                self.reg_query = query.clone();
                self.input = query;
                self.input_cur = self.input.len();
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
                    self.prompt_cur = 0;
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
                self.confirm_job(Job::UnitAction { unit, action });
            }
            Ok(Cmd::Run { package, service }) => self.spawn(Job::RunService { package, service }),
            Ok(Cmd::Logs { name, follow }) => self.spawn(Job::Logs { name, follow }),
            Ok(Cmd::Deploy(name)) => {
                self.prompt_buf.clear();
                self.prompt_cur = 0;
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
                    self.confirm_job(Job::Publish {
                        dir: dir.to_string(),
                        registry: registry.map(str::to_string),
                    });
                }
            }
            Ok(Cmd::Registries) => self.spawn(Job::Registries),
            Ok(Cmd::Manifest) => {
                self.load_manifest();
                self.say("manifest reloaded");
            }
            Ok(Cmd::Sync { from_lock }) => {
                self.confirm_job(Job::Sync {
                    allow_drivers: false,
                    from_lock,
                });
            }
            Ok(Cmd::Prune { keep }) => {
                self.confirm_job(Job::Prune { keep });
            }
            Err(e) if e.is_empty() => {}
            Err(e) => self.say_err(e),
        }
    }

    /// Recompute the `:` completion candidates for the buffer *up to
    /// the cursor* (completion is cursor-relative, not whole-line);
    /// any edit resets the Tab selection to "none applied".
    fn refresh_completions(&mut self) {
        let line = self.cmd_prefix();
        let dynamic = self.dynamic_candidates();
        self.cmd_completions = crate::complete::complete_with(&line, &dynamic);
        self.cmd_sel = usize::MAX;
    }

    /// `cmdline[..cmd_cur]` clamped to a char boundary — the text the
    /// completer sees (the token under the cursor is its last word).
    fn cmd_prefix(&self) -> String {
        let mut cur = self.cmd_cur.min(self.cmdline.len());
        while !self.cmdline.is_char_boundary(cur) {
            cur -= 1;
        }
        self.cmdline[..cur].to_string()
    }

    /// Dynamic completion candidates for the arg position under the
    /// cursor — the model-backed half of [`crate::complete_with`].
    /// Tokenizes the same way the completer does (verb = token 0,
    /// token index = arg position) and picks a source per verb:
    ///
    /// - package-name verbs (`remove`/`verify`/`update`/`uninstall`/
    ///   `deploy`/`install`/`run`/`logs`/`pin` first arg) → installed
    ///   entry labels;
    /// - `registry` and `publish`'s registry arg → configured
    ///   registry names;
    /// - `service`'s unit arg → unit base names, `run`'s service arg
    ///   → that package's provider service names.
    ///
    /// Dedupe + [`CMD_COMPLETION_CAP`] cap; inserts carry a trailing
    /// space so the next arg position is already open.
    fn dynamic_candidates(&self) -> Vec<crate::complete::Candidate> {
        use crate::complete::Candidate;
        let body = self
            .cmd_prefix()
            .trim_start()
            .trim_start_matches(':')
            .trim_start()
            .to_string();
        let tokens: Vec<&str> = body.split_whitespace().collect();
        if tokens.is_empty() {
            return Vec::new();
        }
        let fresh = body.chars().last().is_none_or(|c| c.is_whitespace());
        let position = if fresh {
            tokens.len()
        } else {
            tokens.len() - 1
        };
        if position == 0 {
            return Vec::new();
        }
        let verb = tokens[0].to_lowercase();
        let labels = |hint: &str| -> Vec<Candidate> {
            self.entries
                .iter()
                .map(|r| Candidate {
                    insert: format!("{} ", r.entry.label()),
                    hint: hint.to_string(),
                })
                .collect()
        };
        let names = |hint: &str, names: Vec<String>| -> Vec<Candidate> {
            names
                .into_iter()
                .map(|n| Candidate {
                    insert: format!("{n} "),
                    hint: hint.to_string(),
                })
                .collect()
        };
        let out = match verb.as_str() {
            // First-arg package positions.
            "remove" | "uninstall" | "verify" | "update" | "u" | "deploy" | "install" | "i"
            | "logs" | "pin" | "p"
                if position == 1 =>
            {
                labels("installed")
            }
            // `:run <pkg> [service]` — arg 1 is the package, arg 2
            // the provider service within it.
            "run" if position == 1 => labels("installed"),
            "run" if position == 2 => {
                let pkg = tokens.get(1).copied().unwrap_or("");
                let svcs: Vec<String> = self
                    .svc_rows
                    .iter()
                    .filter(|r| r.package == pkg)
                    .map(|r| r.service.clone())
                    .collect();
                names("provider service", svcs)
            }
            // `:service <verb> <unit>` — position 2 is the unit.
            "service" if position >= 2 => names(
                "unit",
                self.svc_rows.iter().map(|r| r.unit.clone()).collect(),
            ),
            // `:registry <name>` and `:publish <dir> [registry]`.
            "registry" if position == 1 => names("registry", self.reg_names.clone()),
            "publish" if position == 2 => names("registry", self.reg_names.clone()),
            _ => Vec::new(),
        };
        // Dedupe on insert, cap — prefix filtering is complete_with's.
        let mut seen = std::collections::HashSet::new();
        out.into_iter()
            .filter(|c| seen.insert(c.insert.clone()))
            .take(CMD_COMPLETION_CAP)
            .collect()
    }

    /// `Tab`/`BackTab` in `:` mode — cycle the candidate list and
    /// replace the token under the cursor with the pick. The token
    /// bounds come from [`token_start`]/[`token_end`] at `cmd_cur`,
    /// so completion works mid-line, not just at the end.
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
        let mut cur = self.cmd_cur.min(self.cmdline.len());
        while !self.cmdline.is_char_boundary(cur) {
            cur -= 1;
        }
        let start = token_start(&self.cmdline, cur);
        let end = token_end(&self.cmdline, cur);
        self.cmdline.replace_range(start..end, &insert);
        self.cmd_cur = start + insert.len();
        self.refresh_completions();
    }

    /// Push the submitted line onto `:` history — non-empty only,
    /// consecutive duplicates collapse (like readline).
    fn record_command(&mut self) {
        let line = self.cmdline.trim().to_string();
        if line.is_empty() {
            return;
        }
        if self.cmd_history.front().is_some_and(|f| *f == line) {
            return;
        }
        self.cmd_history.push_front(line);
        while self.cmd_history.len() > CMD_HIST_CAP {
            self.cmd_history.pop_back();
        }
    }

    /// `Up`/`Down` history recall — `Up` walks back (most recent
    /// first), `Down` forward; stepping past the newest entry
    /// restores the line that was being edited when recall began.
    fn hist_move(&mut self, down: bool) {
        if self.cmd_history.is_empty() {
            return;
        }
        match self.cmd_hist {
            None => {
                if down {
                    return;
                }
                self.cmd_draft = self.cmdline.clone();
                self.cmd_hist = Some(0);
            }
            Some(i) => {
                if down {
                    if i == 0 {
                        self.cmdline = std::mem::take(&mut self.cmd_draft);
                        self.cmd_hist = None;
                        self.cmd_cur = self.cmdline.len();
                        self.refresh_completions();
                        return;
                    }
                    self.cmd_hist = Some(i - 1);
                } else {
                    let last = self.cmd_history.len() - 1;
                    self.cmd_hist = Some((i + 1).min(last));
                }
            }
        }
        if let Some(i) = self.cmd_hist {
            if let Some(line) = self.cmd_history.get(i) {
                self.cmdline = line.clone();
            }
        }
        self.cmd_cur = self.cmdline.len();
        self.refresh_completions();
    }

    /// `:` mode keys — `Enter` runs (and records history), `esc`
    /// drops the line, `Tab`/`BackTab` cycle [`crate::complete`]
    /// candidates into the buffer at the cursor, `Up`/`Down` recall
    /// history, everything else is shared line editing.
    fn key_command(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.record_command();
                self.cmd_hist = None;
                self.mode = Mode::Normal;
                self.cmd_completions.clear();
                let quit = matches!(self.cmdline.trim(), "q" | "quit" | "q!");
                if quit {
                    // Same teardown as bare `q`/Ctrl-C — a stream
                    // child (`run`/`logs -f`) must not outlive quit.
                    self.kill_stream();
                } else {
                    self.run_command();
                }
                quit
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.cmdline.clear();
                self.cmd_cur = 0;
                self.cmd_hist = None;
                self.cmd_completions.clear();
                false
            }
            KeyCode::Up => {
                self.hist_move(false);
                false
            }
            KeyCode::Down => {
                self.hist_move(true);
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
            _ => {
                // Any consumed edit — including cursor moves —
                // refreshes completions (they're cursor-relative).
                if line_edit(&mut self.cmdline, &mut self.cmd_cur, &k) != Edit::Ignored {
                    self.refresh_completions();
                }
                false
            }
        }
    }

    /// Post-edit hook shared by `key_filter`/`key_search`/`paste` —
    /// push `input` into the mode's real field and refresh.
    fn after_input_edit(&mut self) {
        match self.mode {
            Mode::Filter => {
                self.filter = self.input.clone();
                self.rebuild_sel(None, self.sel);
            }
            Mode::Search => {
                self.reg_query = self.input.clone();
                self.search_typed_at = Some(Instant::now());
            }
            _ => {}
        }
    }

    /// Insert `text` at a char-boundary-clamped cursor.
    fn insert_at(text: &mut String, cur: &mut usize, s: &str) {
        *cur = (*cur).min(text.len());
        while !text.is_char_boundary(*cur) {
            *cur -= 1;
        }
        text.insert_str(*cur, s);
        *cur += s.len();
    }

    /// Bracketed paste (`Event::Paste`) — insert the text at the
    /// cursor of whichever text mode is active; a no-op elsewhere.
    /// Newlines are kept verbatim (the modes are single-line — a
    /// pasted `\n` just becomes part of the buffer text).
    pub fn paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.mode {
            Mode::Command => {
                Self::insert_at(&mut self.cmdline, &mut self.cmd_cur, text);
                self.refresh_completions();
            }
            Mode::Filter | Mode::Search => {
                Self::insert_at(&mut self.input, &mut self.input_cur, text);
                self.after_input_edit();
            }
            Mode::Prompt(_) => {
                Self::insert_at(&mut self.prompt_buf, &mut self.prompt_cur, text);
            }
            _ => {}
        }
    }

    /// `/` mode keys on Installed — live filter; `Enter` keeps it,
    /// `esc` clears. Editing keys are the shared [`line_edit`] set.
    fn key_filter(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.input_cur = 0;
                false
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.input_cur = 0;
                self.filter.clear();
                self.rebuild_sel(None, self.sel);
                false
            }
            _ => {
                if line_edit(&mut self.input, &mut self.input_cur, &k) == Edit::Mutate {
                    self.after_input_edit();
                }
                false
            }
        }
    }

    /// Enter Search mode seeded from `reg_query` — the query and its
    /// results are the Registry pane's persistent state, so re-entering
    /// `/` resumes the last search. An empty query drops stale results.
    fn enter_search(&mut self) {
        self.mode = Mode::Search;
        self.input = self.reg_query.clone();
        self.input_cur = self.input.len();
        if self.reg_query.is_empty() {
            self.reg_results.clear();
            self.reg_version_counts.clear();
            self.reg_err = None;
        }
    }

    /// `/` mode keys on Registry — the query edits `reg_query` live;
    /// `tick` debounces the actual search, `Enter` fires it now.
    /// `esc` only leaves the mode — the query and results are the
    /// pane's persistent state (pane switches keep them too).
    fn key_search(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.input_cur = 0;
                self.search_now();
                false
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                false
            }
            _ => {
                if line_edit(&mut self.input, &mut self.input_cur, &k) == Edit::Mutate {
                    self.after_input_edit();
                }
                false
            }
        }
    }

    /// `p`/`i`/`:registry` prompt keys — text entry; `Enter` runs.
    /// Editing keys are the shared [`line_edit`] set.
    fn key_prompt(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Enter => {
                self.run_prompt();
                false
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.prompt_buf.clear();
                self.prompt_cur = 0;
                false
            }
            _ => {
                let _ = line_edit(&mut self.prompt_buf, &mut self.prompt_cur, &k);
                false
            }
        }
    }

    /// Open the `?` keybind overlay — scroll starts at the top.
    fn open_help(&mut self) {
        self.mode = Mode::Help;
        self.help_scroll = 0;
    }

    /// `?` overlay — scrolls like the log view: `j`/`k`/arrows step,
    /// `PgUp`/`PgDn` page by the last-drawn inner height, `g`/`G`
    /// jump the ends, and `esc`/`?`/`q` closes. Everything else is
    /// swallowed (same posture as `key_log`).
    fn key_help(&mut self, k: KeyEvent) -> bool {
        if chorded(&k) {
            return false;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.help_scroll = (self.help_scroll + 1).min(self.help_max_scroll);
            }
            KeyCode::PageDown => {
                self.help_scroll =
                    (self.help_scroll + self.help_view_h.max(1)).min(self.help_max_scroll);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.help_scroll = self.help_scroll.saturating_sub(self.help_view_h.max(1));
            }
            KeyCode::Char('g') => self.help_scroll = 0,
            KeyCode::Char('G') => self.help_scroll = self.help_max_scroll,
            _ => {}
        }
        false
    }

    /// Open the op-log view (`L`/`:log`) scrolled to the newest
    /// lines — it docks as a right sidebar (centered overlay under
    /// ~50 cols), so it opens on the tail either way.
    fn open_log(&mut self) {
        self.mode = Mode::Log;
        self.log_scroll = self.log_max_scroll();
    }

    /// Top-line scroll offset that puts the log tail on screen —
    /// `log.len() - 1` until `draw` reports the view's inner
    /// height, then the true bottom.
    fn log_max_scroll(&self) -> usize {
        self.log.len().saturating_sub(self.log_view_h.max(1))
    }

    /// Log-view keys — scroll the op log; `esc`/`q`/`L` closes.
    /// Everything else is swallowed while the view is up (same
    /// posture as `key_help`): the pane stays visible behind the
    /// sidebar but takes no input — it's a view, not split focus.
    fn key_log(&mut self, k: KeyEvent) -> bool {
        if chorded(&k) {
            return false;
        }
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
    /// Chord-modified chars (Ctrl/Alt) never count — Ctrl+Y must not
    /// confirm a destructive op. Returns `true` when the key was
    /// consumed by the modal.
    fn key_modal(&mut self, k: KeyEvent) -> bool {
        if self.modal.is_none() {
            return false;
        }
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') if !chorded(&k) => {
                self.modal_yes();
                true
            }
            KeyCode::Char('n') | KeyCode::Char('N') if !chorded(&k) => {
                self.modal_no();
                true
            }
            KeyCode::Esc => {
                self.modal_no();
                true
            }
            _ => true, // swallow everything else while a modal is up
        }
    }

    /// Close the package page — the shared `esc`/`h`/`←`/`q`/`Enter`
    /// close; also drops versions focus, resets both scroll/cursor
    /// state, and restores the origin pane a Home/Services `Enter`
    /// recorded (a page opened on its own pane has none — the pane
    /// stays put).
    fn close_detail(&mut self) {
        let origin = self.detail_origin.take();
        self.detail = None;
        self.detail_scroll = 0;
        self.detail_max_scroll = 0;
        self.versions_focus = false;
        self.versions_sel = 0;
        if let Some(pane) = origin {
            self.pane = pane;
        }
    }

    /// `y` on the package page — yank the install ref the page
    /// displays (`tamias install <ref>`'s argument) into the
    /// clipboard. The model only queues the payload
    /// ([`Self::copy_request`]); `run` emits the OSC 52 sequence
    /// between frames.
    fn copy_install_ref(&mut self) {
        let r = match self.detail.as_ref() {
            Some(Detail::Installed { entry, .. }) => Some(entry.record.source_ref.clone()),
            Some(Detail::Registry { install_ref, .. }) => install_ref.clone(),
            None => None,
        };
        match r {
            Some(r) => {
                self.copy_request = Some(r);
                self.say("copied install ref");
            }
            None => self.say("no install ref on this page"),
        }
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
        // Ctrl/Alt chords never reach the pane verbs below — every
        // `Char` arm is a plain keystroke (Shift arrives as the
        // shifted char itself). Ctrl-C already force-quit above.
        if matches!(k.code, KeyCode::Char(_)) && chorded(&k) {
            return false;
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
                // Home's recent list and Services rows both resolve
                // to an installed package's page (see
                // `open_home_detail`/`open_service_detail`).
                Pane::Home => self.open_home_detail(),
                Pane::Installed | Pane::Registry => self.toggle_detail(),
                Pane::Services => self.open_service_detail(),
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
                    self.sync_sel_key();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.versions_focused() {
                    self.move_versions_sel(-1);
                } else if self.detail.is_some() {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                } else {
                    self.move_sel(-1);
                    self.sync_sel_key();
                }
            }
            // g/G follow the visible cursor: versions focus jumps the
            // version list, an open package page jumps its scroll —
            // never the hidden row cursor underneath.
            KeyCode::Char('g') => {
                if self.versions_focused() {
                    self.versions_sel = 0;
                } else if self.detail.is_some() {
                    self.detail_scroll = 0;
                } else {
                    match self.pane {
                        Pane::Home => self.home_sel = 0,
                        Pane::Registry => self.reg_sel = 0,
                        Pane::Services => self.svc_sel = 0,
                        Pane::Manifest => self.man_sel = 0,
                        Pane::Author => {
                            if self.author_regs.is_empty() {
                                self.say("nothing to select on this pane");
                            } else {
                                self.author_sel = 0;
                            }
                        }
                        _ => self.sel = 0,
                    }
                }
            }
            KeyCode::Char('G') => {
                if self.versions_focused() {
                    self.versions_sel = self.versions_len().saturating_sub(1);
                } else if self.detail.is_some() {
                    self.detail_scroll = self.detail_max_scroll;
                } else {
                    match self.pane {
                        Pane::Home => {
                            self.home_sel = self.recent_installed().len().saturating_sub(1);
                        }
                        Pane::Registry => {
                            self.reg_sel = self.reg_results.len().saturating_sub(1);
                        }
                        Pane::Services => {
                            self.svc_sel = self.svc_rows.len().saturating_sub(1);
                        }
                        Pane::Manifest => {
                            self.man_sel = self
                                .project
                                .as_ref()
                                .map_or(0, |m| m.deps.len().saturating_sub(1));
                        }
                        Pane::Author => {
                            if self.author_regs.is_empty() {
                                self.say("nothing to select on this pane");
                            } else {
                                self.author_sel = self.author_regs.len() - 1;
                            }
                        }
                        _ => {
                            self.sel = self.visible().len().saturating_sub(1);
                            self.sync_sel_key();
                        }
                    }
                }
            }
            KeyCode::Char('/') => match self.pane {
                Pane::Home => {
                    // Home has no search of its own — `/` is the way
                    // into registry search: switch panes and seed the
                    // query like the Registry arm does, so a previous
                    // search resumes instead of being wiped.
                    self.switch_pane(Pane::Registry);
                    self.enter_search();
                }
                Pane::Registry => self.enter_search(),
                Pane::Installed => {
                    self.mode = Mode::Filter;
                    self.input = self.filter.clone();
                    self.input_cur = self.input.len();
                }
                // The `/` filter only exists on Installed — anywhere
                // else it would edit hidden state, so say where it
                // lives instead of silently mutating `self.filter`.
                _ => self.say("filter is Installed-only — `2` jumps there"),
            },
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.cmdline.clear();
                self.cmd_cur = 0;
                self.cmd_hist = None;
                self.refresh_completions();
            }
            // `F` clears the `/` filter — Installed-only state, so on
            // any other pane it would mutate hidden state; say where
            // the filter lives instead (same posture as the `/` arm).
            KeyCode::Char('F') => match self.pane {
                Pane::Installed => {
                    self.filter.clear();
                    self.rebuild_sel(None, self.sel);
                }
                _ => self.say("filter is Installed-only — `2` jumps there"),
            },
            KeyCode::Char('R') if matches!(self.pane, Pane::Installed | Pane::Registry) => {
                self.reload();
                self.say("reloaded");
            }
            // `y` yanks the open package page's install ref to the
            // clipboard — the OSC 52 write happens in `run` between
            // frames (`copy_request` is the hand-off). Modal `y`
            // (confirm) is consumed earlier by `key_modal`.
            KeyCode::Char('y') if self.detail.is_some() => self.copy_install_ref(),
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
            KeyCode::Char('U') if self.pane == Pane::Installed => {
                self.confirm_job(Job::UpdateAll {
                    allow_drivers: false,
                })
            }
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
            // restart: `r` stays exclusive to the Publish pane's
            // registries report so the key keeps one meaning app-wide.
            KeyCode::Char(
                c @ ('s' | 'S' | '!' | 'e' | 'E' | 'd' | 'x' | 't' | 'T' | 'n' | 'R'),
            ) if self.pane == Pane::Services => self.svc_verb(c),
            // Project pane: add/remove/edit dep + sync + reload.
            // (`S` is the `sync --from-lock` variant — `man_verb`
            // handles it; the route must let it through.)
            KeyCode::Char(c @ ('a' | 'x' | 'd' | 'e' | 's' | 'S' | 'R'))
                if self.pane == Pane::Manifest =>
            {
                self.man_verb(c)
            }
            // Publish pane: pack/publish/registries.
            KeyCode::Char(c @ ('p' | 'P' | 'r' | 'R')) if self.pane == Pane::Author => {
                self.author_verb(c)
            }
            // Pane-gated verbs pressed on a pane that doesn't bind
            // them used to die silently — every pane-gated arm above
            // already claimed its own char (Services' `s S ! e E d x
            // t T n R`, Project's `a x d e s S R`, Publish's `p P r R`,
            // Installed's `v u U p x c C d`, Registry's detail `v`,
            // the `R` reload). `i` is global (install prompt /
            // registry-result install) and `F` reports its own gate.
            KeyCode::Char(
                c @ ('v' | 'u' | 'U' | 'p' | 'P' | 'x' | 'c' | 'C' | 'd' | 's' | 'S' | '!' | 'e'
                | 'E' | 't' | 'T' | 'n' | 'R' | 'a' | 'r'),
            ) => {
                self.say(format!(
                    "no '{c}' on {} — `:help` lists the keys",
                    self.pane.label()
                ));
            }
            KeyCode::Char('?') => self.open_help(),
            // `L` opens the full op log — `l` (lowercase) stays pane
            // cycling, so this rides the shifted char only.
            KeyCode::Char('L') => self.open_log(),
            _ => {}
        }
        false
    }

    /// Mouse — wheel scrolls the selection, a click on the tab bar
    /// switches panes, a click on a row selects it. While the log
    /// view or the help overlay is up it owns the pointer instead:
    /// the wheel inside its rect scrolls and every other event is
    /// swallowed (same posture as the swallowed keys).
    pub fn mouse(&mut self, ev: MouseEvent) {
        if self.mode == Mode::Help {
            if self.help_area.contains((ev.column, ev.row).into()) {
                match ev.kind {
                    MouseEventKind::ScrollUp => {
                        self.help_scroll = self.help_scroll.saturating_sub(1);
                    }
                    MouseEventKind::ScrollDown => {
                        self.help_scroll = (self.help_scroll + 1).min(self.help_max_scroll);
                    }
                    _ => {}
                }
            }
            return;
        }
        if self.mode == Mode::Log {
            if self.log_area.contains((ev.column, ev.row).into()) {
                match ev.kind {
                    MouseEventKind::ScrollUp => {
                        self.log_scroll = self.log_scroll.saturating_sub(1);
                    }
                    MouseEventKind::ScrollDown => {
                        self.log_scroll = (self.log_scroll + 1).min(self.log_max_scroll());
                    }
                    _ => {}
                }
            }
            return;
        }
        match ev.kind {
            // Wheel scrolls the open package page's body — versions
            // focus rides the same offset (`scroll_to_show` keeps the
            // cursor row visible); otherwise it walks the pane's
            // selection as before.
            MouseEventKind::ScrollUp => {
                if self.detail.is_some() {
                    self.detail_scroll = self.detail_scroll.saturating_sub(3);
                } else {
                    self.move_sel(-3);
                    self.sync_sel_key();
                }
            }
            MouseEventKind::ScrollDown => {
                if self.detail.is_some() {
                    self.detail_scroll += 3;
                } else {
                    self.move_sel(3);
                    self.sync_sel_key();
                }
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
                        Pane::Author => {
                            if idx < self.author_regs.len() {
                                self.author_sel = idx;
                            }
                        }
                        _ => {
                            let vis = self.visible();
                            if idx < vis.len() {
                                self.sel = idx;
                                self.sync_sel_key();
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
        assert_eq!(
            parse_command("uninstall @s/w@1").unwrap(),
            Cmd::Uninstall("@s/w@1".into())
        );
        assert!(parse_command("uninstall").is_err());
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
        // esc leaves search mode; `1` is Home, `2` Installed, `6`
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
        // `/` on Home resumes a previous registry search — the query,
        // results, and counts survive the pane hop (only an empty
        // query clears them, via `enter_search`).
        m.reg_query = "wid".into();
        m.reg_results = vec![reg_entry("1.0.0")];
        m.reg_version_counts = vec![2];
        m.key(key(KeyCode::Char('/')));
        assert_eq!(m.pane, Pane::Registry);
        assert_eq!(m.mode, Mode::Search);
        assert_eq!(m.input, "wid");
        assert_eq!(m.input_cur, 3);
        assert_eq!(m.reg_query, "wid");
        assert_eq!(m.reg_results.len(), 1);
        assert_eq!(m.reg_version_counts, vec![2]);
        // …but a fresh (empty) query still lands a clean search.
        m.key(key(KeyCode::Esc));
        m.reg_query.clear();
        m.reg_results = vec![reg_entry("1.0.0")];
        m.key(key(KeyCode::Char('1')));
        m.key(key(KeyCode::Char('/')));
        assert!(m.reg_results.is_empty());
        assert!(m.reg_version_counts.is_empty());
        assert!(m.input.is_empty());
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
        m.rebuild_derived();
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
        // Esc closes the page and restores the origin pane — back
        // lands on Home, where Enter was pressed.
        m.key(key(KeyCode::Esc));
        assert!(m.detail.is_none());
        assert_eq!(m.pane, Pane::Home);
        assert_eq!(
            m.home_selected().map(|r| r.entry.record.name.as_str()),
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
        m.rebuild_derived();
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

    // --- model-level coverage: keys, modes, workers, panes ---

    use crate::testkit::EnvGuard;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tamias_core::StoreRecord;

    /// Roots under `tmp` — nothing needs to exist for model tests.
    fn tctx(tmp: &Path) -> Ctx {
        Ctx::new(
            tmp.join("store"),
            tmp.join("cache"),
            tmp.join("registry"),
            Some(tmp.join("cfg.yml")),
        )
    }

    fn open(tmp: &Path) -> Model {
        Model::open(&tctx(tmp)).unwrap()
    }

    fn key(m: &mut Model, code: KeyCode) {
        m.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn chr(m: &mut Model, c: char) {
        key(m, KeyCode::Char(c));
    }

    /// Drain worker messages until `busy` clears — bounded so a
    /// stuck job can't hang the suite. Real installs on a starved
    /// worker have exceeded 4s under full-suite parallel load, so
    /// the ceiling is ~20s; a genuinely stuck job still fails the
    /// test, just later.
    fn settle(m: &mut Model) {
        for _ in 0..2000 {
            m.drain_work();
            if m.busy.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Run a `:` command line end-to-end — mode + buffer + Enter.
    fn run_cmd(m: &mut Model, line: &str) -> bool {
        m.mode = Mode::Command;
        m.cmdline = line.into();
        m.refresh_completions();
        m.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    /// A payload dir whose `tamias.yaml` is `manifest` verbatim plus
    /// one payload file.
    fn pkg(dir: &Path, name: &str, manifest: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("tamias.yaml"), manifest).unwrap();
        std::fs::write(p.join("a.txt"), "a").unwrap();
        p
    }

    /// The record `Store::install` writes for a hand-placed entry.
    fn record(name: &str, payload: &Path, kind: Option<&str>) -> StoreRecord {
        StoreRecord {
            name: name.into(),
            scope: None,
            version: Some("1.0.0".into()),
            kind: kind.map(str::to_string),
            description: None,
            keywords: Vec::new(),
            homepage: None,
            repository: None,
            license: None,
            authors: Vec::new(),
            source_ref: payload.display().to_string(),
            resolved: "sha256:0123456789abcdef".into(),
            digest: String::new(),
            signature: None,
            signer: None,
            trust: None,
            installed_at: 0,
        }
    }

    /// Place `payload` in `root`'s store directly (no pipeline).
    fn place_at(root: &Path, payload: &Path, name: &str, kind: Option<&str>) -> StoreEntry {
        Store::new(root)
            .install(payload, record(name, payload, kind))
            .unwrap()
    }

    /// Place `payload` in the model's store + reload.
    fn place(m: &mut Model, payload: &Path, name: &str, kind: Option<&str>) {
        place_at(&m.store_root, payload, name, kind);
        m.reload();
    }

    /// A provider service row for the Services pane.
    fn svc_row(package: &str, service: &str) -> ServiceRow {
        ServiceRow {
            package: package.into(),
            service: service.into(),
            unit: format!("{package}-{service}"),
            state: Some("active".into()),
            exec: "/bin/echo hi".into(),
        }
    }

    /// Set a prompt's buffer and hit Enter.
    fn run_prompt(m: &mut Model, kind: PromptKind, buf: &str) {
        m.mode = Mode::Prompt(kind);
        m.prompt_buf = buf.into();
        key(m, KeyCode::Enter);
    }

    #[test]
    fn command_parsing_extras() {
        assert!(parse_command("remove").is_err());
        assert!(parse_command("verify").is_err());
        assert!(parse_command("run").is_err());
        assert!(parse_command("run a b c").is_err());
        assert!(parse_command("logs").is_err());
        assert!(parse_command("deploy").is_err());
        assert!(parse_command("pack").is_err());
        assert!(parse_command("publish").is_err());
        assert!(matches!(
            parse_command("logs u --follow").unwrap(),
            Cmd::Logs { follow: true, .. }
        ));
        assert!(matches!(
            parse_command("logs u -F").unwrap(),
            Cmd::Logs { follow: true, .. }
        ));
        assert!(matches!(
            parse_command("clean --all").unwrap(),
            Cmd::Clean(true)
        ));
        assert!(matches!(
            parse_command("cache all").unwrap(),
            Cmd::Clean(true)
        ));
        assert!(matches!(
            parse_command("u w").unwrap(),
            Cmd::Update(Some(_))
        ));
        assert!(matches!(parse_command("i x").unwrap(), Cmd::Install(_)));
        assert!(matches!(parse_command("d").unwrap(), Cmd::Doctor));
        assert!(matches!(parse_command("s").unwrap(), Cmd::Sync { .. }));
        assert!(matches!(
            parse_command("registry-list").unwrap(),
            Cmd::Registries
        ));
        // the typed word → action bridge
        for (w, ok) in [
            ("start", true),
            ("stop", true),
            ("restart", true),
            ("enable", true),
            ("disable", true),
            ("bogus", false),
        ] {
            assert_eq!(unit_action_from_word(w).is_some(), ok, "{w}");
        }
        for a in [
            tamias_provider::UnitAction::Start,
            tamias_provider::UnitAction::Stop,
            tamias_provider::UnitAction::Restart,
            tamias_provider::UnitAction::Enable,
            tamias_provider::UnitAction::Disable,
        ] {
            assert!(!crate::ops::unit_action_verb(a).is_empty());
        }
    }

    #[test]
    fn keys_modes_mouse_and_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = open(tmp.path());
        // pane digits + h/l/Tab/BackTab stepping
        for c in ['2', '3', '4', '5', '6', '2'] {
            chr(&mut m, c);
        }
        assert_eq!(m.pane, Pane::Installed);
        key(&mut m, KeyCode::Tab);
        assert_eq!(m.pane, Pane::Services);
        key(&mut m, KeyCode::BackTab);
        assert_eq!(m.pane, Pane::Installed);
        chr(&mut m, 'l');
        assert_eq!(m.pane, Pane::Services);
        chr(&mut m, 'h');
        assert_eq!(m.pane, Pane::Installed);
        // g/G on empty lists
        for c in ['g', 'G', 'j', 'k'] {
            chr(&mut m, c);
        }
        // `/` filter on Installed — type, edit, keep, reopen+esc
        chr(&mut m, '/');
        assert_eq!(m.mode, Mode::Filter);
        for c in "xy".chars() {
            chr(&mut m, c);
        }
        assert_eq!(m.input, "xy");
        assert_eq!(m.filter, "xy");
        key(&mut m, KeyCode::Backspace);
        assert_eq!(m.filter, "x");
        key(&mut m, KeyCode::Enter);
        assert_eq!(m.mode, Mode::Normal);
        chr(&mut m, '/');
        key(&mut m, KeyCode::Esc);
        assert_eq!(m.mode, Mode::Normal);
        assert!(m.filter.is_empty());
        // `F` clears the filter outright
        m.filter = "z".into();
        chr(&mut m, 'F');
        assert!(m.filter.is_empty());
        // `:` command mode — typing, completions, Tab cycling, esc
        chr(&mut m, ':');
        assert_eq!(m.mode, Mode::Command);
        for c in "ver".chars() {
            chr(&mut m, c);
        }
        assert_eq!(m.cmdline, "ver");
        assert!(!m.cmd_completions.is_empty());
        key(&mut m, KeyCode::Tab);
        // Tab replaced the trailing token with the picked candidate
        // (and refresh reset the selection to "none applied")
        assert_ne!(m.cmdline, "ver");
        assert_eq!(m.cmd_sel, usize::MAX);
        key(&mut m, KeyCode::BackTab);
        key(&mut m, KeyCode::Backspace);
        assert_eq!(m.mode, Mode::Command);
        key(&mut m, KeyCode::Esc);
        assert_eq!(m.mode, Mode::Normal);
        assert!(m.cmdline.is_empty());
        // `?` help overlay — most keys swallowed, ?/esc/q close
        chr(&mut m, '?');
        assert_eq!(m.mode, Mode::Help);
        key(&mut m, KeyCode::Char('j'));
        assert_eq!(m.mode, Mode::Help);
        chr(&mut m, '?');
        assert_eq!(m.mode, Mode::Normal);
        chr(&mut m, '?');
        chr(&mut m, 'q');
        assert_eq!(m.mode, Mode::Normal);
        // R reload + verbs on an empty selection → error toast
        // (`p`/`i` open prompts — esc between so later keys stay verbs)
        chr(&mut m, 'R');
        assert_eq!(m.status, "reloaded");
        for c in ['v', 'u', 'r'] {
            chr(&mut m, c);
        }
        // cache cleans confirm unconditionally (no selection needed)
        for c in ['c', 'C'] {
            chr(&mut m, c);
            assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
            chr(&mut m, 'n');
        }
        chr(&mut m, 'd');
        settle(&mut m); // the doctor job lands
        chr(&mut m, 'p'); // no selection → verb_pin is a no-op
        assert_eq!(m.mode, Mode::Normal);
        // 'i' on Installed opens the InstallRef prompt — esc closes
        chr(&mut m, 'i');
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::InstallRef)));
        key(&mut m, KeyCode::Esc);
        // Enter/Esc/Left with no detail and no selection — harmless
        key(&mut m, KeyCode::Enter);
        key(&mut m, KeyCode::Esc);
        key(&mut m, KeyCode::Left);
        chr(&mut m, 'x'); // no stream child — not the kill arm
        chr(&mut m, 'z'); // unmapped — `_` arm
        key(&mut m, KeyCode::Up); // unmapped in Normal
                                  // mouse — wheel, tab click, board clicks
        m.tabs_area = Some(Rect::new(0, 0, 100, 1));
        m.board_area = Rect::new(0, 2, 80, 10);
        m.viewport_top = 0;
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        m.mouse(mouse(MouseEventKind::ScrollUp, 1, 1));
        m.mouse(mouse(MouseEventKind::ScrollDown, 1, 1));
        // click the SERVICES tab (find a column tab_hit maps to it —
        // index 2 in the browsing-first Pane::ALL order)
        let col = (1..100u16)
            .find(|&x| crate::render::tab_hit(100, x) == Some(2))
            .unwrap();
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), col, 0));
        assert_eq!(m.pane, Pane::Services);
        // board clicks with empty rows — out-of-range guards
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 3));
        // quit paths
        let mut m2 = open(tmp.path());
        assert!(m2.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        let mut m3 = open(tmp.path());
        assert!(m3.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn installed_rows_detail_and_verbs() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = tctx(tmp.path());
        let mut m = Model::open(&c).unwrap();
        m.pane = Pane::Installed;
        // bare first alphabetically — its payload has no manifest, so
        // the detail page takes the manifest_err arm
        let bare = tmp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("a.txt"), "a").unwrap();
        place(&mut m, &bare, "bare", None);
        let w = pkg(
            tmp.path(),
            "w",
            "name: w\nversion: 1.0.0\ndescription: the w package\nlicense: MIT\ntype: stuff\nkeywords: [a, b]\n",
        );
        place(&mut m, &w, "w", Some("stuff"));
        // a non-files kind for the remove-confirm teardown warning
        let s = pkg(tmp.path(), "s", "name: s\nversion: 1.0.0\n");
        place(&mut m, &s, "s", Some("weird"));
        // a manifest with a sealed: block for the badge arm
        let sec = pkg(
            tmp.path(),
            "sec",
            "name: sec\nversion: 1.0.0\nsealed:\n  format: age/v1\n  recipients: recipients.txt\n  files:\n    k: v.age\n",
        );
        place(&mut m, &sec, "sec", None);
        assert_eq!(m.entries.len(), 4);
        // visible() + the / filter over label/kind/ref/sha/sealed
        assert_eq!(m.visible().len(), 4);
        m.filter = "w@".into();
        assert_eq!(m.visible().len(), 1);
        m.filter = "sealed".into();
        let sealed_vis = m.visible();
        m.filter.clear();
        m.rebuild_sel(None, 0);
        assert!(sealed_vis
            .iter()
            .any(|&i| m.entries[i].entry.label() == "sec@1.0.0"));
        // selection accessors
        m.sel = 0;
        m.sync_sel_key();
        assert!(m.selected().is_some());
        assert!(m.reg_selected().is_none());
        assert!(m.svc_selected().is_none());
        assert!(m.dep_selected().is_none());
        // detail on `bare` — manifest_err arm
        key(&mut m, KeyCode::Enter);
        match &m.detail {
            Some(Detail::Installed {
                manifest,
                manifest_err,
                ..
            }) => {
                assert!(manifest.is_none());
                assert!(manifest_err.is_some());
            }
            _ => panic!("expected installed detail"),
        }
        let d = m.detail.as_ref().unwrap();
        assert!(d.description().is_none());
        assert!(d.kind().is_none());
        assert!(!d.sealed());
        key(&mut m, KeyCode::Esc);
        assert!(m.detail.is_none());
        // detail on `w` — full manifest path
        let wi = m
            .entries
            .iter()
            .position(|r| r.entry.label() == "w@1.0.0")
            .unwrap();
        m.sel = wi;
        m.sync_sel_key();
        key(&mut m, KeyCode::Enter);
        let d = m.detail.as_ref().unwrap();
        assert!(matches!(d, Detail::Installed { .. }));
        assert_eq!(d.description(), Some("the w package"));
        assert_eq!(d.kind(), Some("stuff"));
        assert_eq!(d.license(), Some("MIT"));
        assert_eq!(d.scope(), None);
        assert_eq!(d.trust(), None);
        // j/k scroll the page; Enter toggles it closed
        chr(&mut m, 'j');
        assert!(m.detail_scroll >= 1);
        chr(&mut m, 'k');
        key(&mut m, KeyCode::Enter);
        assert!(m.detail.is_none());
        // h/BackTab with a page open closes it instead of stepping
        key(&mut m, KeyCode::Enter);
        assert!(m.detail.is_some());
        chr(&mut m, 'h');
        assert!(m.detail.is_none());
        key(&mut m, KeyCode::Enter);
        key(&mut m, KeyCode::BackTab);
        assert!(m.detail.is_none());
        // verbs — verify spawns for real; the rest open modals/prompts
        chr(&mut m, 'v');
        settle(&mut m);
        assert!(m.status.contains("w@1.0.0: ok") || m.status.contains("ok"));
        chr(&mut m, 'u');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        chr(&mut m, 'U');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        chr(&mut m, 'p');
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::Pin { .. })));
        key(&mut m, KeyCode::Esc);
        // remove on the `weird`-kind entry → teardown warning body
        let si = m
            .entries
            .iter()
            .position(|r| r.entry.label() == "s@1.0.0")
            .unwrap();
        m.sel = si;
        m.sync_sel_key();
        chr(&mut m, 'x');
        match &m.modal {
            Some(Modal::Confirm { body, .. }) => {
                assert!(body.iter().any(|b| b.contains("type 'weird'")));
            }
            _ => panic!("expected confirm"),
        }
        chr(&mut m, 'n');
        // cache cleans + doctor
        chr(&mut m, 'c');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'y'); // modal_yes → spawn CacheClean → real run
        settle(&mut m);
        chr(&mut m, 'C');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        chr(&mut m, 'd');
        settle(&mut m);
        // filter to nothing → selected_label's error arm
        m.filter = "zzz".into();
        m.rebuild_sel(None, m.sel);
        chr(&mut m, 'v');
        assert!(m.status_err);
        assert!(m.status.contains("nothing selected"));
        m.filter.clear();
        m.rebuild_sel(None, 0);
    }

    #[test]
    fn registry_pane_search_detail_install() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = tctx(tmp.path());
        let mut m = Model::open(&c).unwrap();
        chr(&mut m, '6');
        assert_eq!(m.pane, Pane::Registry);
        // moves on empty results
        for c in ['j', 'k', 'g', 'G'] {
            chr(&mut m, c);
        }
        assert!(m.reg_selected().is_none());
        // a named registry with kind http but no url → install_ref Err
        std::fs::write(
            tmp.path().join("cfg.yml"),
            "registries:\n  bad:\n    kind: http\n",
        )
        .unwrap();
        m.reg_results = vec![reg_entry("1.0.0"), reg_entry("2.0.0")];
        m.reg_query = "w".into();
        m.reg_sel = 0;
        for c in ['j', 'k', 'G', 'g'] {
            chr(&mut m, c);
        }
        assert!(m.reg_selected().is_some());
        // `/` on Registry → Search mode editing reg_query live
        chr(&mut m, '/');
        assert_eq!(m.mode, Mode::Search);
        assert_eq!(m.input, "w");
        for c in "id".chars() {
            chr(&mut m, c);
        }
        assert_eq!(m.reg_query, "wid");
        key(&mut m, KeyCode::Backspace);
        assert_eq!(m.reg_query, "wi");
        // Esc leaves search mode but keeps the query + results —
        // clearing is `F`, not a mis-aimed esc.
        key(&mut m, KeyCode::Esc);
        assert_eq!(m.mode, Mode::Normal);
        assert_eq!(m.reg_query, "wi");
        // retype + Enter → immediate search (real job, empty registry)
        m.reg_query = "w".into();
        m.reg_results = vec![reg_entry("1.0.0")];
        chr(&mut m, '/');
        key(&mut m, KeyCode::Enter);
        assert_eq!(m.mode, Mode::Normal);
        settle(&mut m);
        // repopulate for the detail/install flow
        m.reg_results = vec![reg_entry("1.0.0")];
        m.reg_sel = 0;
        // registry target that can't resolve an install ref
        m.reg_target = Some("bad".into());
        key(&mut m, KeyCode::Enter);
        match &m.detail {
            Some(Detail::Registry {
                install_ref,
                install_err,
                ..
            }) => {
                assert!(install_ref.is_none());
                assert!(install_err.is_some());
            }
            _ => panic!("expected registry detail"),
        }
        // the versions fetch thread lands its Msg::Versions (Ok or Err)
        settle(&mut m);
        assert!(matches!(m.detail, Some(Detail::Registry { .. })));
        chr(&mut m, 'j');
        key(&mut m, KeyCode::Esc);
        assert!(m.detail.is_none());
        m.reg_target = None;
        // `i` on a selected result → install confirm modal
        chr(&mut m, 'i');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        // `i` with the package page open installs the page's entry —
        // and a failed ref resolution toasts instead
        key(&mut m, KeyCode::Enter);
        assert!(matches!(m.detail, Some(Detail::Registry { .. })));
        settle(&mut m);
        m.reg_target = Some("bad".into());
        chr(&mut m, 'i');
        assert!(m.status_err);
        m.reg_target = None;
        key(&mut m, KeyCode::Esc);
        assert!(m.detail.is_none());
        // `i` with nothing selected → error toast
        m.reg_results.clear();
        m.reg_sel = 0;
        chr(&mut m, 'i');
        assert!(m.status_err);
        assert!(m.status.contains("no registry result selected"));
        // stale vs fresh Done(Search) outcomes
        m.search_inflight = Some("other".into());
        m.reg_query = "w".into();
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Search {
                target: "t".into(),
                results: vec![reg_entry("9.9.9")],
            })))
            .unwrap();
        m.drain_work();
        assert!(m.reg_results.is_empty());
        m.search_inflight = Some("w".into());
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Search {
                target: "t".into(),
                results: vec![reg_entry("9.9.9")],
            })))
            .unwrap();
        m.drain_work();
        assert_eq!(m.reg_results.len(), 1);
        assert_eq!(m.reg_target_label, "t");
        // Done(Err) on the Services pane records svc_err
        m.pane = Pane::Services;
        m.work_tx.send(Msg::Done(Err("boom".into()))).unwrap();
        m.drain_work();
        assert_eq!(m.svc_err.as_deref(), Some("boom"));
    }

    #[test]
    fn pane_specific_verbs() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = tctx(tmp.path());
        // a project manifest for the Manifest pane
        std::fs::write(
            tmp.path().join("tamias.yaml"),
            "name: proj\ndeps:\n  - ./a\n  - ./b\n",
        )
        .unwrap();
        let mut m = Model::open(&c).unwrap();
        // --- Manifest pane ---
        chr(&mut m, '4');
        assert_eq!(m.pane, Pane::Manifest);
        assert_eq!(m.project.as_ref().map(|p| p.deps.len()), Some(2));
        assert_eq!(m.dep_selected().as_deref(), Some("./a"));
        for c in ['j', 'k', 'G', 'g'] {
            chr(&mut m, c);
        }
        chr(&mut m, 'a');
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::AddDep)));
        key(&mut m, KeyCode::Esc);
        chr(&mut m, 'x');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        chr(&mut m, 'd');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        chr(&mut m, 'e');
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::SetDep { .. })));
        key(&mut m, KeyCode::Esc);
        chr(&mut m, 's');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        chr(&mut m, 'S');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        chr(&mut m, 'R'); // reload manifest
        assert!(m.project.is_some());
        // out-of-range dep selection → error arms
        m.man_sel = 99;
        chr(&mut m, 'x');
        assert!(m.status_err);
        chr(&mut m, 'e');
        assert!(m.status_err);
        m.man_sel = 0;
        // --- Services pane ---
        chr(&mut m, '3');
        assert_eq!(m.pane, Pane::Services);
        assert!(m.svc_stale);
        m.svc_rows = vec![svc_row("pkg", "web"), svc_row("pkg2", "api")];
        m.svc_sel = 0;
        assert!(m.svc_selected().is_some());
        for c in ['j', 'k', 'G', 'g'] {
            chr(&mut m, c);
        }
        for c in ['s', 'S', '!', 'e', 'E'] {
            chr(&mut m, c);
            assert!(matches!(m.modal, Some(Modal::Confirm { .. })), "{c}");
            chr(&mut m, 'n');
        }
        chr(&mut m, 'd');
        assert!(matches!(
            m.mode,
            Mode::Prompt(PromptKind::DeployConfig { .. })
        ));
        key(&mut m, KeyCode::Esc);
        chr(&mut m, 'x');
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        for c in ['t', 'T', 'n'] {
            chr(&mut m, c);
            settle(&mut m);
        }
        chr(&mut m, 'R');
        assert!(m.svc_stale);
        assert_eq!(m.status, "services refresh queued");
        // 'i' on Services → install-by-ref prompt (not registry)
        chr(&mut m, 'i');
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::InstallRef)));
        key(&mut m, KeyCode::Esc);
        // empty rows → "no service selected"
        m.svc_rows.clear();
        chr(&mut m, 's');
        assert!(m.status_err);
        // --- Author pane ---
        chr(&mut m, '5');
        assert_eq!(m.pane, Pane::Author);
        chr(&mut m, 'p');
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::PackSpec)));
        key(&mut m, KeyCode::Esc);
        chr(&mut m, 'P');
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::PublishSpec)));
        key(&mut m, KeyCode::Esc);
        chr(&mut m, 'r');
        settle(&mut m);
        chr(&mut m, 'R');
        // --- Installed verbs with a real selection ---
        chr(&mut m, '2');
        let w = pkg(tmp.path(), "w", "name: w\nversion: 1.0.0\n");
        place(&mut m, &w, "w", Some("weird"));
        for c in ['v', 'u', 'p', 'x', 'c', 'C', 'd', 'i', 'U'] {
            chr(&mut m, c);
            match c {
                'v' | 'd' => settle(&mut m),
                'i' => {
                    assert!(matches!(m.mode, Mode::Prompt(PromptKind::InstallRef)));
                    key(&mut m, KeyCode::Esc);
                }
                'p' => {
                    assert!(matches!(m.mode, Mode::Prompt(PromptKind::Pin { .. })));
                    key(&mut m, KeyCode::Esc);
                }
                _ => {
                    if m.modal.is_some() {
                        chr(&mut m, 'n');
                    }
                }
            }
        }
    }

    #[test]
    fn prompt_kinds_run_their_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = tctx(tmp.path());
        let w = pkg(tmp.path(), "w", "name: w\nversion: 1.0.0\n");
        let w_ref = w.display().to_string();
        let w2 = pkg(tmp.path(), "w2", "name: w2\nversion: 2.0.0\n");
        let w2_ref = w2.display().to_string();
        let mut m = Model::open(&c).unwrap();
        // Pin — empty errs, text confirms
        run_prompt(&mut m, PromptKind::Pin { name: "w".into() }, "");
        assert!(m.status_err);
        run_prompt(&mut m, PromptKind::Pin { name: "w".into() }, "./r");
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        // InstallRef — same shape
        run_prompt(&mut m, PromptKind::InstallRef, "");
        assert!(m.status_err);
        run_prompt(&mut m, PromptKind::InstallRef, "./x");
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        // RegistryTarget — empty resets, text sets + refires the query
        m.reg_query = "q".into();
        run_prompt(&mut m, PromptKind::RegistryTarget, "");
        assert_eq!(m.reg_target, None);
        assert_eq!(m.status, "registry target: default routing");
        run_prompt(&mut m, PromptKind::RegistryTarget, "mine");
        assert_eq!(m.reg_target.as_deref(), Some("mine"));
        settle(&mut m); // the refired search lands
                        // AddDep — empty errs; a real ref installs + writes the manifest
        run_prompt(&mut m, PromptKind::AddDep, "");
        assert!(m.status_err);
        run_prompt(&mut m, PromptKind::AddDep, &w_ref);
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'y');
        settle(&mut m);
        assert!(
            m.project
                .as_ref()
                .is_some_and(|p| p.deps.iter().any(|d| d == &w_ref)),
            "deps: {:?}",
            m.project.as_ref().map(|p| &p.deps)
        );
        // SetDep — empty errs; a real ref swaps the dep on confirm-yes
        run_prompt(&mut m, PromptKind::SetDep { old: w_ref.clone() }, "");
        assert!(m.status_err);
        run_prompt(&mut m, PromptKind::SetDep { old: w_ref.clone() }, &w2_ref);
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'y');
        settle(&mut m);
        assert!(
            m.project
                .as_ref()
                .is_some_and(|p| p.deps.iter().any(|d| d == &w2_ref)),
            "deps: {:?}",
            m.project.as_ref().map(|p| &p.deps)
        );
        // DeployConfig — empty means no driver config
        run_prompt(&mut m, PromptKind::DeployConfig { name: "w".into() }, "");
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        run_prompt(
            &mut m,
            PromptKind::DeployConfig { name: "w".into() },
            "/tmp/cfg.yml",
        );
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        // PackSpec / PublishSpec — empty errs, spec confirms/spawns
        run_prompt(&mut m, PromptKind::PackSpec, "");
        assert!(m.status_err);
        run_prompt(&mut m, PromptKind::PackSpec, &format!("{w_ref} out.tgz"));
        settle(&mut m);
        run_prompt(&mut m, PromptKind::PublishSpec, "");
        assert!(m.status_err);
        run_prompt(&mut m, PromptKind::PublishSpec, "./d reg");
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        // prompt-mode keys — chars, backspace, esc, unmapped
        m.mode = Mode::Prompt(PromptKind::InstallRef);
        for c in "ab".chars() {
            chr(&mut m, c);
        }
        assert_eq!(m.prompt_buf, "ab");
        key(&mut m, KeyCode::Backspace);
        assert_eq!(m.prompt_buf, "a");
        key(&mut m, KeyCode::Up);
        key(&mut m, KeyCode::Esc);
        assert_eq!(m.mode, Mode::Normal);
        assert!(m.prompt_buf.is_empty());
    }

    #[test]
    fn modal_consent_stream_and_worker_msgs() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = tctx(tmp.path());
        let mut m = Model::open(&c).unwrap();
        let confirm = || Modal::Confirm {
            title: "t".into(),
            body: vec!["b".into()],
            job: Job::Verify("ghost".into()),
        };
        // Confirm — swallowed key, n/N/esc cancel, y spawns
        m.modal = Some(confirm());
        chr(&mut m, 'j');
        assert!(m.modal.is_some());
        chr(&mut m, 'n');
        assert!(m.modal.is_none());
        m.modal = Some(confirm());
        chr(&mut m, 'N');
        assert!(m.modal.is_none());
        m.modal = Some(confirm());
        key(&mut m, KeyCode::Esc);
        assert!(m.modal.is_none());
        m.modal = Some(confirm());
        chr(&mut m, 'y');
        assert!(m.modal.is_none());
        settle(&mut m); // the Verify job errors — ghost isn't installed
                        // spawn while busy → the busy-refusal arm
        m.busy = Some("job".into());
        m.spawn(Job::Doctor);
        assert!(m.status_err);
        assert!(m.status.contains("busy"));
        m.busy = None;
        // Consent — n cancels; y re-runs with drivers allowed (a real
        // bundled-driver install under the guarded TAMIAS_HOME)
        let drv = pkg(tmp.path(), "drv", "name: drv\nversion: 1.0.0\n");
        let drv_dir = drv.join("drivers");
        std::fs::create_dir_all(&drv_dir).unwrap();
        let driver = drv_dir.join("tamias-driver-x");
        std::fs::write(&driver, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let drv_ref = drv.display().to_string();
        let consent = || Modal::Consent {
            job: Job::Install {
                reference: drv_ref.clone(),
                allow_drivers: false,
            },
            reason: "ships drivers/".into(),
        };
        m.modal = Some(consent());
        chr(&mut m, 'n');
        assert!(m.modal.is_none());
        m.modal = Some(consent());
        chr(&mut m, 'y');
        settle(&mut m);
        assert!(m.modal.is_none());
        assert!(
            m.entries.iter().any(|r| r.entry.label() == "drv@1.0.0"),
            "entries: {:?}",
            m.entries
                .iter()
                .map(|r| r.entry.label())
                .collect::<Vec<_>>()
        );
        // Msg::StreamChild lands the killable handle; `x` kills it
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let arc = Arc::new(Mutex::new(child));
        m.work_tx.send(Msg::StreamChild(arc)).unwrap();
        m.drain_work();
        assert!(m.stream_child.is_some());
        chr(&mut m, 'x');
        assert!(m.stream_child.is_none());
        assert!(m.log.iter().any(|l| l.contains("killed stream")));
        // ctrl-c with a live child kills it, then quits
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        m.stream_child = Some(Arc::new(Mutex::new(child)));
        assert!(m.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        // the log ring caps at LOG_CAP
        for i in 0..(LOG_CAP + 5) {
            m.work_tx.send(Msg::Log(format!("l{i}"))).unwrap();
        }
        m.drain_work();
        assert_eq!(m.log.len(), LOG_CAP);
        // Done(Services) fills the table; Done(Text) toasts + reloads
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Services(vec![svc_row("p", "s")]))))
            .unwrap();
        m.drain_work();
        assert_eq!(m.svc_rows.len(), 1);
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Text("done-thing".into()))))
            .unwrap();
        m.drain_work();
        assert_eq!(m.status, "done-thing");
    }

    #[test]
    fn services_ingest_sorts_by_package() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let mut m = Model::open(&tctx(tmp.path())).unwrap();
        // Unsorted ingest — the pane's package grouping only blanks
        // repeat cells when same-package rows land consecutively.
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Services(vec![
                svc_row("b-pkg", "s1"),
                svc_row("a-pkg", "s2"),
                svc_row("b-pkg", "s1b"),
            ]))))
            .unwrap();
        m.drain_work();
        let order: Vec<(&str, &str)> = m
            .svc_rows
            .iter()
            .map(|r| (r.package.as_str(), r.service.as_str()))
            .collect();
        assert_eq!(order, [("a-pkg", "s2"), ("b-pkg", "s1"), ("b-pkg", "s1b")]);
    }

    #[test]
    fn colon_commands_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = tctx(tmp.path());
        let mut m = Model::open(&c).unwrap();
        let w = pkg(tmp.path(), "w", "name: w\nversion: 1.0.0\n");
        place(&mut m, &w, "w", None);
        // read-only + modal commands
        assert!(!run_cmd(&mut m, "reload"));
        assert_eq!(m.status, "reloaded");
        run_cmd(&mut m, "help");
        assert_eq!(m.mode, Mode::Help);
        chr(&mut m, 'q'); // help's own close key
        assert_eq!(m.mode, Mode::Normal);
        run_cmd(&mut m, "install ./x");
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        run_cmd(&mut m, "remove w@1.0.0");
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'n');
        // verify runs for real against the placed entry
        run_cmd(&mut m, "verify w@1.0.0");
        settle(&mut m);
        assert!(m.status.contains("ok"), "{}", m.status);
        for line in [
            "update w@1.0.0",
            "update",
            "pin w@1.0.0 ./r",
            "clean",
            "clean all",
            "cache",
            "service restart w-unit",
            "sync",
            "sync --from-lock",
            "publish ./d mine",
        ] {
            run_cmd(&mut m, line);
            assert!(
                matches!(m.modal, Some(Modal::Confirm { .. })),
                "{line} → {:?}",
                m.modal
            );
            chr(&mut m, 'n');
        }
        // search jumps panes + fires; registry target sets + refires
        run_cmd(&mut m, "search qq");
        assert_eq!(m.pane, Pane::Registry);
        settle(&mut m);
        m.reg_query = "q".into();
        run_cmd(&mut m, "registry mine");
        assert_eq!(m.reg_target.as_deref(), Some("mine"));
        settle(&mut m);
        run_cmd(&mut m, "registry");
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::RegistryTarget)));
        key(&mut m, KeyCode::Esc);
        // spawns that error or succeed against the tmp roots
        for line in [
            "doctor",
            "service bogus u", // parse error, not a spawn
            "run w@1.0.0 web",
            "logs w-unit -f",
            "pack ./d out.tgz",
            "registries",
            "manifest",
            "bogus",
            "sync bogus",
            "pack",
            "publish",
        ] {
            run_cmd(&mut m, line);
        }
        settle(&mut m);
        assert!(m.status_err || !m.status.is_empty());
        // `:deploy <name>` opens the config prompt
        run_cmd(&mut m, "deploy w@1.0.0");
        assert!(matches!(
            m.mode,
            Mode::Prompt(PromptKind::DeployConfig { .. })
        ));
        key(&mut m, KeyCode::Esc);
        // `update` on an empty store → the nothing-installed arm
        let tmp2 = tempfile::tempdir().unwrap();
        let mut m2 = open(tmp2.path());
        run_cmd(&mut m2, "update");
        assert!(m2.status_err);
        assert!(m2.status.contains("nothing installed"));
        // `q`/`quit`/`q!` through the command line quits
        for q in ["q", "quit", "q!"] {
            let mut mq = open(tmp.path());
            assert!(run_cmd(&mut mq, q), "{q}");
        }
    }

    #[test]
    fn tick_drain_watch_and_status() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        std::fs::create_dir_all(tmp.path().join("store")).unwrap();
        let c = tctx(tmp.path());
        let mut m = Model::open(&c).unwrap();
        // the watcher is armed on the existing store root; a write
        // lands as a dirty flag → reload on the next tick
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(tmp.path().join("store/f"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        m.tick();
        // status TTL — normal toasts expire, errors are sticky
        m.status = "hello".into();
        m.status_at = Instant::now() - Duration::from_secs(10);
        m.status_err = false;
        m.tick();
        assert!(m.status.is_empty());
        m.status = "err".into();
        m.status_err = true;
        m.status_at = Instant::now() - Duration::from_secs(10);
        m.tick();
        assert_eq!(m.status, "err");
        m.status_err = false;
        // the rescan fallback boundary reloads too
        m.tick_n = RESCAN_TICKS - 1;
        m.tick();
        // debounced search: Registry pane + settled query + free worker
        m.pane = Pane::Registry;
        m.reg_query = "deb".into();
        m.search_typed_at = Some(Instant::now() - Duration::from_millis(500));
        m.tick(); // maybe_search → fire_search → spawn
        settle(&mut m);
        // the Services stale-flag → refresh job on tick
        m.pane = Pane::Services;
        m.svc_stale = true;
        m.tick();
        settle(&mut m);
        // busy gates the refresh
        m.busy = Some("job".into());
        m.svc_stale = true;
        m.tick();
        assert!(m.busy.is_some());
        m.busy = None;
    }

    #[test]
    fn load_error_and_manifest_error_states() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        // an unreadable store root → entries() fails → load_err
        let store = tmp.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let c = Ctx::new(
            &store,
            tmp.path().join("cache"),
            tmp.path().join("reg"),
            None,
        );
        let mut m = Model::open(&c).unwrap();
        assert!(m.load_err.is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // a lone tamias.toml → the YAML-only refusal arm
        std::fs::write(tmp.path().join("tamias.toml"), "x = 1\n").unwrap();
        m.load_manifest();
        assert!(m.man_err.is_some());
        assert!(m.project.is_none());
    }

    /// Edge arms: stream kill, modal yes (both kinds), prompt kinds
    /// not yet driven, per-pane empty moves, mouse row/tab clicks,
    /// command/parser leftovers, completion wrap edges.
    #[test]
    fn edge_arms_keys_prompts_mouse() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = open(tmp.path());

        // --- kill_stream with a live child ---
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        m.stream_child = Some(Arc::new(Mutex::new(child)));
        chr(&mut m, 'x');
        assert!(m.stream_child.is_none());
        assert!(m.log.iter().any(|l| l == "killed stream"));

        // --- modal yes: Confirm spawns, Consent re-runs allowed ---
        m.modal = Some(Modal::Confirm {
            title: "t".into(),
            body: vec![],
            job: Job::Doctor,
        });
        chr(&mut m, 'y');
        assert!(m.busy.is_some());
        settle(&mut m);
        m.modal = Some(Modal::Consent {
            job: Job::Doctor,
            reason: "r".into(),
        });
        m.modal_yes(); // direct — same arm the 'y' key routes to
        assert!(m.busy.is_some());
        settle(&mut m);
        // run_prompt with no prompt mode → early return
        m.mode = Mode::Normal;
        m.run_prompt();
        assert_eq!(m.mode, Mode::Normal);

        // --- RegistryTarget prompt: empty re-runs the query, text
        //     sets the target and re-fires ---
        m.pane = Pane::Registry;
        m.reg_query = "w".into();
        m.search_typed_at = None;
        run_prompt(&mut m, PromptKind::RegistryTarget, "");
        assert!(m.busy.is_some()); // fire_search spawned
        settle(&mut m);
        run_prompt(&mut m, PromptKind::RegistryTarget, "named");
        assert_eq!(m.reg_target.as_deref(), Some("named"));
        settle(&mut m);
        // empty query → fire_search takes the clear arm
        m.reg_query = "   ".into();
        run_prompt(&mut m, PromptKind::RegistryTarget, "x");
        settle(&mut m);

        // --- Enter opens the registry detail page (install-ref
        //     resolution + versions fetch thread) ---
        m.reg_results = vec![reg_entry("1.0.0")];
        m.reg_sel = 0;
        key(&mut m, KeyCode::Enter);
        assert!(matches!(m.detail, Some(Detail::Registry { .. })));
        settle(&mut m);
        key(&mut m, KeyCode::Esc);
        assert!(m.detail.is_none());

        // --- per-pane empty moves (j/k/g/G on zero rows) ---
        m.reg_results.clear();
        m.svc_rows.clear();
        m.project = None;
        for pane in [
            Pane::Registry,
            Pane::Services,
            Pane::Manifest,
            Pane::Author,
            Pane::Installed,
        ] {
            m.pane = pane;
            m.detail = None;
            for c in ['j', 'k', 'g', 'G'] {
                chr(&mut m, c);
            }
        }
        // non-empty moves on the row panes
        m.svc_rows = vec![svc_row("p", "s"), svc_row("p", "t")];
        m.reg_results = vec![reg_entry("1.0.0"), reg_entry("2.0.0")];
        m.project = Some(Manifest {
            deps: vec!["a".into(), "b".into()],
            ..Default::default()
        });
        for pane in [Pane::Registry, Pane::Services, Pane::Manifest] {
            m.pane = pane;
            for c in ['j', 'k', 'g', 'G'] {
                chr(&mut m, c);
            }
        }
        // 'R' on Services marks stale + toasts
        m.pane = Pane::Services;
        chr(&mut m, 'R');
        assert_eq!(m.status, "services refresh queued");
        // Manifest 'R' reloads the project manifest
        m.pane = Pane::Manifest;
        chr(&mut m, 'R');
        // Author 'r' spawns Registries, 'R' re-reads names
        m.pane = Pane::Author;
        chr(&mut m, 'r');
        settle(&mut m);
        chr(&mut m, 'R');

        // --- `:` command leftovers ---
        run_cmd(&mut m, "registry"); // bare → target prompt
        assert!(matches!(m.mode, Mode::Prompt(PromptKind::RegistryTarget)));
        key(&mut m, KeyCode::Esc);
        run_cmd(&mut m, "registry named"); // named target arm
        assert_eq!(m.reg_target.as_deref(), Some("named"));
        run_cmd(&mut m, "service bogus x"); // parse whitelist rejects
        assert!(m.status.contains("usage: :service"));
        run_cmd(&mut m, "pack"); // bare verb → usage error
        assert!(m.status.contains("usage: :pack"));
        run_cmd(&mut m, "publish"); // bare verb → usage error
        assert!(m.status.contains("usage: :publish"));
        run_cmd(&mut m, "pack /tmp out.tgz"); // spawns Pack
        settle(&mut m);
        run_cmd(&mut m, "publish /tmp reg"); // Confirm modal → yes spawns
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
        chr(&mut m, 'y');
        settle(&mut m);
        run_cmd(&mut m, ""); // parse edge — empty/whitespace
                             // unmapped keys inside Command/Filter/Search modes
        m.mode = Mode::Command;
        key(&mut m, KeyCode::Up);
        m.mode = Mode::Filter;
        key(&mut m, KeyCode::Up);
        m.mode = Mode::Search;
        m.input.clear(); // '/' seeds input from reg_query; mimic that
        key(&mut m, KeyCode::Up);
        // Search typing arms — chars + backspace update the query
        chr(&mut m, 'q');
        assert_eq!(m.reg_query, "q");
        key(&mut m, KeyCode::Backspace);
        assert!(m.reg_query.is_empty());
        // Enter fires the (empty→clear) search now
        key(&mut m, KeyCode::Enter);
        assert_eq!(m.mode, Mode::Normal);

        // --- completion wrap edges ---
        // `apply_completion` recomputes candidates + resets `cmd_sel`
        // to MAX on every pick, so the cycle position itself is never
        // observable — drive every arm (MAX/first/last, both
        // directions) and assert the buffer mutation instead.
        m.mode = Mode::Command;
        m.cmdline = "ver".into();
        m.refresh_completions();
        let before = m.cmdline.clone();
        for _ in 0..2 {
            key(&mut m, KeyCode::Tab);
        }
        assert_ne!(m.cmdline, before);
        for _ in 0..2 {
            key(&mut m, KeyCode::BackTab);
        }
        // manual sel seeds still exercise the wrap arithmetic even
        // though refresh hides the result
        m.cmdline = "ver".into();
        m.refresh_completions();
        m.cmd_sel = usize::MAX;
        key(&mut m, KeyCode::BackTab);
        m.cmd_sel = 0;
        key(&mut m, KeyCode::BackTab);
        m.mode = Mode::Normal;

        // --- debounced search early-return (query still fresh) ---
        m.pane = Pane::Registry;
        m.busy = None;
        m.reg_query = "fresh".into();
        m.search_typed_at = Some(Instant::now());
        m.tick();
        assert!(m.search_typed_at.is_some()); // not yet due
                                              // due → fires; empty query takes the clear arm
        m.search_typed_at = Some(Instant::now() - SEARCH_DEBOUNCE - Duration::from_millis(1));
        m.tick();
        settle(&mut m);
        m.reg_query = String::new();
        m.search_typed_at = Some(Instant::now() - SEARCH_DEBOUNCE - Duration::from_millis(1));
        m.tick();
        assert!(m.reg_results.is_empty());

        // --- mouse: tab click + per-pane row clicks ---
        m.entries = vec![test_row("alpha"), test_row("beta")];
        m.reg_results = vec![reg_entry("1.0.0"), reg_entry("2.0.0")];
        m.svc_rows = vec![svc_row("p", "s"), svc_row("p", "t")];
        m.project = Some(Manifest {
            deps: vec!["a".into(), "b".into()],
            ..Default::default()
        });
        m.man_sel = 0;
        m.tabs_area = Some(Rect::new(0, 0, 100, 1));
        m.board_area = Rect::new(0, 2, 80, 10);
        m.viewport_top = 0;
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // click the SERVICES tab (col inside its label span — index 2
        // in the browsing-first Pane::ALL order)
        let col = (1..100u16)
            .find(|&x| crate::render::tab_hit(100, x) == Some(2))
            .unwrap();
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), col, 0));
        assert_eq!(m.pane, Pane::Services);
        // row click on the services table → svc_sel
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 4));
        assert_eq!(m.svc_sel, 1);
        // header row click → ignored (checked_sub(1) → None)
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 3));
        // outside the board → ignored
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 30));
        // manifest pane row click
        m.pane = Pane::Manifest;
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 4));
        assert_eq!(m.man_sel, 1);
        // registry pane row click
        m.pane = Pane::Registry;
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 4));
        assert_eq!(m.reg_sel, 1);
        // installed pane row click → sel + sel_key sync
        m.pane = Pane::Installed;
        m.filter.clear();
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 4));
        assert_eq!(m.sel, 1);
        assert!(m.sel_key.is_some());
        // tab-bar click past the last label → tab_hit None → no-op
        m.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 95, 0));
        assert_eq!(m.pane, Pane::Installed);
    }

    /// An installed-pane [`Row`] without touching disk — the dir is
    /// display-only (detail/manifest reads would fail, which is fine
    /// for selection tests).
    fn test_row(name: &str) -> Row {
        Row {
            entry: StoreEntry {
                dir: PathBuf::from("/store").join(name),
                record: record(name, Path::new("/store"), Some("files")),
            },
            size: Some(1024),
            sealed: false,
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

    /// `L`/`:log` opens the op-log view (right sidebar, centered
    /// overlay on narrow terminals) on the tail; j/k and the
    /// PgUp/PgDn page by the drawn view height, g/G jump the ends,
    /// and every other key is swallowed while it's up. New log lines
    /// follow only while parked at the bottom.
    #[test]
    fn log_view_toggle_scroll_and_gating() {
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
        m.log_view_h = 10; // as `draw_log_view` would report
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
        // inside `:` is just text, not the log view.
        m.key(key(KeyCode::Char(':')));
        assert_eq!(m.mode, Mode::Command);
        m.key(key(KeyCode::Char('L')));
        assert_eq!(m.mode, Mode::Command);
        assert_eq!(m.cmdline, "L");
        m.cmdline = "log".to_string();
        m.run_command();
        assert_eq!(m.mode, Mode::Log);
        // An empty log still opens — the view draws its muted
        // "no operations yet" line.
        let mut empty = Model::open(&ctx).unwrap();
        empty.key(key(KeyCode::Char('L')));
        assert_eq!(empty.mode, Mode::Log);
        assert_eq!(empty.log_scroll, 0);
    }

    /// `?`/`:help` opens the scrollable keybind overlay: j/k and the
    /// arrows step, PgUp/PgDn page by the drawn inner height, g/G
    /// jump the ends, opening resets the offset, and every other key
    /// is swallowed while it's up.
    #[test]
    fn help_view_scrolls_and_gates() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // Stand in for a drawn frame's measurements.
        m.help_max_scroll = 30;
        m.help_view_h = 10;
        m.help_scroll = 7;
        // `?` reopens at the top.
        m.key(key(KeyCode::Char('?')));
        assert_eq!(m.mode, Mode::Help);
        assert_eq!(m.help_scroll, 0);
        // j/k step, clamped at both ends.
        m.key(key(KeyCode::Char('j')));
        m.key(key(KeyCode::Char('j')));
        assert_eq!(m.help_scroll, 2);
        m.key(key(KeyCode::Char('k')));
        assert_eq!(m.help_scroll, 1);
        m.key(key(KeyCode::Char('k')));
        m.key(key(KeyCode::Char('k')));
        assert_eq!(m.help_scroll, 0);
        // PgDn/PgUp page by the view height.
        m.key(key(KeyCode::PageDown));
        assert_eq!(m.help_scroll, 10);
        m.key(key(KeyCode::PageDown));
        assert_eq!(m.help_scroll, 20);
        m.key(key(KeyCode::PageDown));
        assert_eq!(m.help_scroll, 30); // clamped at max
        m.key(key(KeyCode::PageUp));
        assert_eq!(m.help_scroll, 20);
        // Arrows step too.
        m.key(key(KeyCode::Down));
        assert_eq!(m.help_scroll, 21);
        m.key(key(KeyCode::Up));
        assert_eq!(m.help_scroll, 20);
        // g/G jump the ends.
        m.key(key(KeyCode::Char('g')));
        assert_eq!(m.help_scroll, 0);
        m.key(key(KeyCode::Char('G')));
        assert_eq!(m.help_scroll, 30);
        // Other keys are swallowed — no pane verbs or modes fire.
        let pane = m.pane;
        m.key(key(KeyCode::Char('2')));
        assert_eq!(m.pane, pane);
        m.key(key(KeyCode::Char(':')));
        assert_eq!(m.mode, Mode::Help);
        // `?` toggles closed; esc and q close too.
        m.key(key(KeyCode::Char('?')));
        assert_eq!(m.mode, Mode::Normal);
        m.key(key(KeyCode::Char('?')));
        m.key(key(KeyCode::Esc));
        assert_eq!(m.mode, Mode::Normal);
        m.key(key(KeyCode::Char('?')));
        m.key(key(KeyCode::Char('q')));
        assert_eq!(m.mode, Mode::Normal);
        // `:help` lands in the same mode through the command line.
        m.key(key(KeyCode::Char(':')));
        m.cmdline = "help".to_string();
        m.run_command();
        assert_eq!(m.mode, Mode::Help);
        assert_eq!(m.help_scroll, 0);
    }

    /// The wheel scrolls the help overlay while its rect is up —
    /// inside `help_area` it steps the offset, outside it's a no-op,
    /// and every non-wheel event is swallowed.
    #[test]
    fn help_view_wheel_scrolls() {
        let (_tmp, mut m) = bare_model();
        let wheel = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        m.mode = Mode::Help;
        m.help_area = Rect::new(10, 5, 40, 12);
        m.help_max_scroll = 20;
        m.help_scroll = 4;
        // Inside the box: down steps, up steps back, clamped at max.
        m.mouse(wheel(MouseEventKind::ScrollDown, 20, 10));
        assert_eq!(m.help_scroll, 5);
        m.help_scroll = 20;
        m.mouse(wheel(MouseEventKind::ScrollDown, 20, 10));
        assert_eq!(m.help_scroll, 20);
        m.mouse(wheel(MouseEventKind::ScrollUp, 20, 10));
        assert_eq!(m.help_scroll, 19);
        // Outside the box: swallowed, no scroll.
        m.mouse(wheel(MouseEventKind::ScrollDown, 1, 1));
        assert_eq!(m.help_scroll, 19);
        // Clicks don't leak through to the pane either.
        m.mouse(wheel(MouseEventKind::Down(MouseButton::Left), 20, 10));
        assert_eq!(m.help_scroll, 19);
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
        m.rebuild_derived();
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

    /// A bare model on a temp store — the shared fixture for the
    /// key/confirm/drain tests below.
    fn bare_model() -> (tempfile::TempDir, Model) {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            Some(tmp.path().join("no-such-config.yml")),
        );
        (tmp, Model::open(&ctx).unwrap())
    }

    /// Ctrl/Alt chords never fire verbs: Ctrl+Y can't confirm a
    /// modal, Ctrl+Q can't quit, Ctrl+X can't remove — while plain
    /// and Shift-modified chars still work (Shift arrives as the
    /// shifted char itself).
    #[test]
    fn modifier_chords_do_not_fire_verbs() {
        let (_tmp, mut m) = bare_model();
        let key = |code, mods| KeyEvent::new(code, mods);
        // A confirm modal up: Ctrl+Y is swallowed, `y` confirms.
        m.modal = Some(Modal::Confirm {
            title: "remove w — sure? [y/n]".into(),
            body: Vec::new(),
            job: Job::Remove("w".into()),
        });
        assert!(!m.key(key(KeyCode::Char('y'), KeyModifiers::CONTROL)));
        assert!(m.modal.is_some());
        assert!(m.busy.is_none());
        // Alt+Y likewise; plain `y` is the real confirm.
        assert!(!m.key(key(KeyCode::Char('y'), KeyModifiers::ALT)));
        assert!(m.modal.is_some());
        m.key(key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(m.modal.is_none());
        assert!(m.busy.is_some());
        // Ctrl+Q at pane level is not the quit.
        assert!(!m.key(key(KeyCode::Char('q'), KeyModifiers::CONTROL)));
        // Ctrl+X on Installed doesn't open the remove confirm.
        m.busy = None;
        m.busy_job = None;
        m.pane = Pane::Installed;
        m.entries.push(store_row("w", 1));
        m.key(key(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(m.modal.is_none());
        assert!(m.busy.is_none());
        // Shift+'U' still fires update-all — the shifted char is the
        // input, SHIFT isn't a chord.
        m.key(key(KeyCode::Char('U'), KeyModifiers::SHIFT));
        assert!(matches!(m.modal, Some(Modal::Confirm { .. })));
    }

    /// Every confirm title names its target — the job's own label —
    /// and Manifest `a` (AddDep) confirms like its siblings.
    #[test]
    fn confirm_title_names_the_target() {
        let (_tmp, mut m) = bare_model();
        let title = |m: &Model| match &m.modal {
            Some(Modal::Confirm { title, .. }) => title.clone(),
            other => panic!("expected confirm modal, got {other:?}"),
        };
        // Install-by-ref prompt.
        m.mode = Mode::Prompt(PromptKind::InstallRef);
        m.prompt_buf = "nine:tamias@0.3.0".into();
        m.run_prompt();
        assert!(title(&m).contains("nine:tamias@0.3.0"), "{}", title(&m));
        // `:remove` names the store label.
        m.modal = None;
        m.cmdline = "remove w@1.0.0".into();
        m.run_command();
        assert!(title(&m).contains("w@1.0.0"), "{}", title(&m));
        // Manifest add-dep confirms (was a bare spawn) and names the ref.
        m.modal = None;
        m.mode = Mode::Prompt(PromptKind::AddDep);
        m.prompt_buf = "git:x#main".into();
        m.run_prompt();
        let t = title(&m);
        assert!(t.contains("git:x#main"), "{t}");
        assert!(matches!(
            m.modal,
            Some(Modal::Confirm {
                job: Job::AddDep { .. },
                ..
            })
        ));
    }

    /// `:q`/`:quit` quit *and* kill a running stream child — same
    /// teardown as bare `q`/Ctrl-C.
    #[test]
    fn colon_quit_kills_stream_child() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // A real (portable) child — the test binary's own `--list`
        // exits immediately; `kill` on it is a harmless no-op, what
        // matters is the handle is taken and logged.
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        m.stream_child = Some(Arc::new(Mutex::new(child)));
        m.key(key(KeyCode::Char(':')));
        m.key(key(KeyCode::Char('q')));
        assert!(m.key(key(KeyCode::Enter)));
        assert!(m.stream_child.is_none());
        assert!(m.log.iter().any(|l| l.contains("killed stream")));
    }

    /// `/` only filters where a filter exists: Installed → Filter,
    /// Registry → Search, Home → the Registry jump; on the other
    /// panes it says where the filter lives instead of editing
    /// hidden state.
    #[test]
    fn slash_filter_is_gated_to_installed() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for pane in [Pane::Services, Pane::Manifest, Pane::Author] {
            m.pane = pane;
            m.mode = Mode::Normal;
            m.filter = "keep".into();
            m.key(key(KeyCode::Char('/')));
            assert_eq!(m.mode, Mode::Normal, "{pane:?}");
            assert_eq!(m.filter, "keep", "{pane:?}");
            assert!(
                m.status.contains("Installed-only"),
                "{pane:?}: {}",
                m.status
            );
        }
        m.pane = Pane::Installed;
        m.key(key(KeyCode::Char('/')));
        assert_eq!(m.mode, Mode::Filter);
        m.key(key(KeyCode::Esc));
        m.pane = Pane::Registry;
        m.key(key(KeyCode::Char('/')));
        assert_eq!(m.mode, Mode::Search);
    }

    /// A multi-line `Outcome::Text` (doctor/registries reports) lands
    /// one line per log entry with the first on the status bar — and
    /// a read-only job's `Done` skips the store reload + services
    /// staleness a mutating job would trigger.
    #[test]
    fn done_text_splits_lines_and_readonly_skips_reload() {
        let (tmp, mut m) = bare_model();
        // A real (empty) store dir — a reload would rebuild
        // `entries` from it and drop the planted row.
        std::fs::create_dir_all(tmp.path().join("store")).unwrap();
        m.entries.push(store_row("ghost", 1));
        m.busy = Some("doctor".into());
        m.busy_job = Some(Job::Doctor);
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Text(
                "store ok\ncache ok\nregistry ok".into(),
            ))))
            .unwrap();
        m.drain_work();
        assert_eq!(m.status, "store ok");
        assert!(m.busy.is_none());
        assert!(m.entries.iter().any(|r| r.entry.record.name == "ghost"));
        assert!(!m.svc_stale);
        let tail: Vec<&str> = m.log.iter().rev().take(3).map(String::as_str).collect();
        assert_eq!(tail, ["registry ok", "cache ok", "ok — store ok"]);
        // The same Done for a mutating job reloads + marks services.
        m.entries.push(store_row("ghost", 1));
        m.busy = Some("remove ghost".into());
        m.busy_job = Some(Job::Remove("ghost".into()));
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Text("removed ghost".into()))))
            .unwrap();
        m.drain_work();
        assert!(m.entries.is_empty());
        assert!(m.svc_stale);
    }

    /// `Enter` in registry search while a job holds `busy` doesn't
    /// lose the query — the debounce re-arms and the search fires
    /// once the job lands.
    #[test]
    fn search_enter_during_busy_rearms_debounce() {
        let (_tmp, mut m) = bare_model();
        m.pane = Pane::Registry;
        m.busy = Some("install x".into());
        m.reg_query = "wid".into();
        m.search_now();
        assert!(m.search_typed_at.is_some());
        assert!(m.search_inflight.is_none());
        // Job lands; the armed debounce fires the pending query.
        m.busy = None;
        m.search_typed_at = Some(Instant::now() - SEARCH_DEBOUNCE - Duration::from_millis(1));
        m.maybe_search();
        assert!(m.busy.is_some());
        assert_eq!(m.search_inflight.as_deref(), Some("wid"));
    }

    /// `:services` (and any other stale marker) refreshes from any
    /// pane — the Services-pane gate is gone; `busy` still serializes.
    #[test]
    fn services_refresh_runs_off_the_services_pane() {
        let (_tmp, mut m) = bare_model();
        m.pane = Pane::Home;
        m.svc_stale = true;
        m.tick();
        assert_eq!(m.busy.as_deref(), Some("services refresh"));
        // A second stale tick while busy queues nothing extra.
        m.svc_stale = true;
        m.tick();
        assert_eq!(m.busy.as_deref(), Some("services refresh"));
    }

    /// Declining the bundled-driver consent modal is honest — `n`
    /// and `esc` both land a status line instead of silence.
    #[test]
    fn consent_decline_says_so() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let consent = || Modal::Consent {
            job: Job::Install {
                reference: "./x".into(),
                allow_drivers: false,
            },
            reason: "ships drivers/".into(),
        };
        m.modal = Some(consent());
        m.key(key(KeyCode::Char('n')));
        assert!(m.modal.is_none());
        assert!(m.status.contains("declined"), "{}", m.status);
        m.modal = Some(consent());
        m.key(key(KeyCode::Esc));
        assert!(m.modal.is_none());
        assert!(m.status.contains("declined"), "{}", m.status);
        // A plain Confirm cancel stays silent.
        m.status.clear();
        m.modal = Some(Modal::Confirm {
            title: "remove w — sure?".into(),
            body: Vec::new(),
            job: Job::Remove("w".into()),
        });
        m.key(key(KeyCode::Char('n')));
        assert!(m.modal.is_none());
        assert!(!m.status.contains("declined"));
    }

    /// Search-mode `esc` exits without wiping the query/results —
    /// they are the pane's persistent state, same as a pane switch.
    #[test]
    fn search_esc_keeps_query_and_results() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        m.pane = Pane::Registry;
        m.reg_query = "wid".into();
        m.reg_results = vec![reg_entry("1.0.0")];
        m.reg_err = Some("warn".into());
        m.key(key(KeyCode::Char('/')));
        assert_eq!(m.mode, Mode::Search);
        m.key(key(KeyCode::Esc));
        assert_eq!(m.mode, Mode::Normal);
        assert_eq!(m.reg_query, "wid");
        assert_eq!(m.reg_results.len(), 1);
        assert_eq!(m.reg_err.as_deref(), Some("warn"));
    }

    /// A raw search hit for the dedupe tests — `name`/`scope`/
    /// `version` parameterized, everything else empty.
    fn hit(name: &str, scope: Option<&str>, version: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            scope: scope.map(str::to_string),
            version: version.map(str::to_string),
            kind: None,
            description: None,
            keywords: Vec::new(),
            homepage: None,
            repository: None,
            license: None,
            authors: Vec::new(),
            digest: String::new(),
            signature: None,
            publisher: None,
            file: format!("{name}@{}", version.unwrap_or("?")),
        }
    }

    /// Search hits collapse to one row per `(name, scope)` — the
    /// newest version wins with numeric-aware ordering (`0.10.0` >
    /// `0.9.0`, a bare release > its prerelease), first-seen row order
    /// holds, and the parallel count feeds the `· N versions` cell.
    #[test]
    fn search_results_dedupe_to_latest_per_package() {
        let results = vec![
            hit("w", Some("s"), Some("0.9.0")),
            hit("w", Some("s"), Some("0.10.0")),
            hit("w", Some("s"), Some("0.10.0-rc.1")),
            hit("x", None, Some("1.0.0")),
            hit("w", None, Some("2.0.0")), // unscoped w — a different package
        ];
        let (rows, counts) = dedupe_search_results(results);
        assert_eq!(rows.len(), 3);
        assert_eq!(counts, vec![3, 1, 1]);
        assert_eq!(rows[0].scope.as_deref(), Some("s"));
        assert_eq!(rows[0].version.as_deref(), Some("0.10.0"));
        assert_eq!(rows[1].name, "x");
        assert_eq!(rows[2].name, "w");
        assert_eq!(rows[2].scope, None);
        // A versionless hit loses to any versioned sibling.
        let (rows, counts) =
            dedupe_search_results(vec![hit("a", None, None), hit("a", None, Some("1.0.0"))]);
        assert_eq!(rows.len(), 1);
        assert_eq!(counts, vec![2]);
        assert_eq!(rows[0].version.as_deref(), Some("1.0.0"));
        // Empty input stays empty.
        assert!(dedupe_search_results(Vec::new()).0.is_empty());
    }

    /// `Enter` on a Services row opens the owning package's detail
    /// page — through the Installed pane, since the detail verbs are
    /// pane-gated — with the row cursor parked on the package. A row
    /// whose package isn't in the store gets a hint and stays put.
    #[test]
    fn services_enter_opens_package_detail() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let pkg = store_row("w", 1);
        let label = pkg.entry.label();
        m.entries.push(pkg);
        m.pane = Pane::Services;
        m.svc_rows = vec![
            ServiceRow {
                package: label,
                service: "web".into(),
                unit: "tamias-w.service".into(),
                state: Some("active".into()),
                exec: "w serve".into(),
            },
            ServiceRow {
                package: "@s/ghost@9.9.9".into(),
                service: "x".into(),
                unit: "ghost.service".into(),
                state: None,
                exec: String::new(),
            },
        ];
        // Row 0 → the installed package's page on the Installed pane.
        m.key(key(KeyCode::Enter));
        assert_eq!(m.pane, Pane::Installed);
        match &m.detail {
            Some(Detail::Installed { entry, .. }) => assert_eq!(entry.record.name, "w"),
            other => panic!("expected installed page, got {other:?}"),
        }
        // Esc back restores the Services pane the page came from.
        m.key(key(KeyCode::Esc));
        assert!(m.detail.is_none());
        assert_eq!(m.pane, Pane::Services);
        // A service row whose package isn't installed says so and
        // stays on Services.
        m.pane = Pane::Services;
        m.svc_sel = 1;
        m.key(key(KeyCode::Enter));
        assert_eq!(m.pane, Pane::Services);
        assert!(m.detail.is_none());
        assert!(
            m.status.contains("not in the installed list"),
            "{}",
            m.status
        );
    }

    /// A store row for an arbitrary `name@version` — `store_row` ties
    /// the version to `installed_at`, so version-grouping tests need
    /// explicit labels.
    fn versioned_row(name: &str, version: &str) -> Row {
        let mut r = store_row(name, 1);
        r.entry.record.version = Some(version.into());
        r.entry.dir = PathBuf::from(format!("/store/{name}@{version}"));
        r
    }

    /// The Installed pane collapses every stored version of a package
    /// into one row — the rep is the semver-max (not lexicographic,
    /// not first-seen), `visible_rows` reports the group size for the
    /// `· N versions` marker, and the row verbs/detail act on the
    /// rep's label. The `/` filter still matches through non-rep
    /// members.
    #[test]
    fn installed_groups_versions_and_routes_to_latest() {
        let (_tmp, mut m) = bare_model();
        m.pane = Pane::Installed;
        m.entries = vec![
            versioned_row("w", "0.9.0"),
            versioned_row("w", "0.10.0"),
            versioned_row("other", "1.0.0"),
        ];
        // 0.10.0 > 0.9.0 semantically — a lexicographic max would
        // pick 0.9.0 ('9' > '1').
        let vis = m.visible();
        assert_eq!(vis.len(), 2);
        assert_eq!(m.entries[vis[0]].entry.label(), "w@0.10.0");
        assert_eq!(m.visible_rows(), vec![(vis[0], 2), (vis[1], 1)]);
        // Enter opens the rep's page.
        m.sel = 0;
        key(&mut m, KeyCode::Enter);
        match &m.detail {
            Some(Detail::Installed { entry, .. }) => {
                assert_eq!(entry.label(), "w@0.10.0");
            }
            other => panic!("expected installed page, got {other:?}"),
        }
        key(&mut m, KeyCode::Esc);
        // `x` confirms removal of the rep's label — the latest
        // version's store entry, not the whole group.
        chr(&mut m, 'x');
        match &m.modal {
            Some(Modal::Confirm { title, .. }) => {
                assert!(title.contains("w@0.10.0"), "{title}");
            }
            other => panic!("expected confirm, got {other:?}"),
        }
        // `/` matches any member — filtering on the old version's
        // label still surfaces the group.
        m.modal = None;
        m.filter = "0.9.0".into();
        assert_eq!(m.visible().len(), 1);
        m.filter.clear();
    }

    /// `sel_key` tracks the group (`@scope/name`), not the rep's
    /// `name@version` label — a reload that re-sorts entries or a
    /// newer version becoming the rep keeps the cursor on the same
    /// package.
    #[test]
    fn installed_sel_key_survives_reorder_and_rep_change() {
        let (_tmp, mut m) = bare_model();
        m.pane = Pane::Installed;
        m.entries = vec![store_row("b", 1), store_row("a", 1)];
        m.sel = 1; // "a"
        m.sync_sel_key();
        // A reload re-sorts entries label-asc — "a" moves to position
        // 0; the keep key must pull the cursor with it.
        m.entries = vec![store_row("a", 1), store_row("b", 1)];
        m.rebuild_sel(m.sel_key.clone().as_deref(), m.sel);
        assert_eq!(m.sel, 0);
        assert_eq!(
            m.selected().map(|r| r.entry.record.name.as_str()),
            Some("a")
        );
        // A newer version lands — the rep changes but the group (and
        // the cursor) stays.
        m.entries.push(versioned_row("a", "2.0.0"));
        m.rebuild_sel(m.sel_key.clone().as_deref(), m.sel);
        assert_eq!(m.sel, 0);
        assert_eq!(m.selected().unwrap().entry.label(), "a@2.0.0");
    }

    /// Services rows dedupe to one per unit at ingest — the survivor
    /// is the newest-version provider (semver over the label's
    /// version tail, not label order), `svc_counts` feeds the `×N`
    /// marker, the cursor re-anchors on the unit across refreshes,
    /// and `Enter` opens the representative's package page.
    #[test]
    fn services_dedupe_by_unit() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let unit_row = |package: &str| ServiceRow {
            package: package.into(),
            service: "campaign".into(),
            unit: "peregrin-campaign".into(),
            state: Some("active".into()),
            exec: "/bin/camp".into(),
        };
        // Label order puts 0.10.0 before 0.9.0 — the semver rep must
        // still be 1.0.0.
        let rows = vec![
            unit_row("pr@0.9.0"),
            unit_row("pr@0.10.0"),
            unit_row("pr@1.0.0"),
            svc_row("solo", "web"),
        ];
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Services(rows))))
            .unwrap();
        m.drain_work();
        assert_eq!(m.svc_rows.len(), 2);
        assert_eq!(m.svc_counts, vec![3, 1]);
        assert_eq!(m.svc_rows[0].package, "pr@1.0.0");
        assert_eq!(m.svc_rows[0].unit, "peregrin-campaign");
        assert_eq!(m.svc_rows[1].unit, "solo-web");
        // A refresh with the cursor on `solo-web` keeps it there.
        m.svc_sel = 1;
        m.work_tx
            .send(Msg::Done(Ok(Outcome::Services(vec![
                svc_row("solo", "web"),
                unit_row("pr@1.0.0"),
            ]))))
            .unwrap();
        m.drain_work();
        assert_eq!(m.svc_sel, 1);
        assert_eq!(m.svc_selected().unwrap().unit, "solo-web");
        // Enter on the deduped row opens the newest provider's page.
        m.entries.push(versioned_row("pr", "1.0.0"));
        m.pane = Pane::Services;
        m.svc_sel = 0;
        m.key(key(KeyCode::Enter));
        assert_eq!(m.pane, Pane::Installed);
        match &m.detail {
            Some(Detail::Installed { entry, .. }) => {
                assert_eq!(entry.label(), "pr@1.0.0");
            }
            other => panic!("expected installed page, got {other:?}"),
        }
    }

    // --- `:` line editing, history, dynamic completion ---

    /// Type `s` into the `:` buffer char by char.
    fn type_colon(m: &mut Model, s: &str) {
        let key = |code, mods| KeyEvent::new(code, mods);
        for c in s.chars() {
            m.key(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// `:` mode gets full line editing: arrows/Home/End move the
    /// cursor, Backspace/Delete edit around it, Ctrl-A/E/U/W are the
    /// readline chords — and unicode survives (cursor is a char
    /// boundary, not a byte count).
    #[test]
    fn colon_line_editing() {
        let (_tmp, mut m) = bare_model();
        let key = |code, mods| KeyEvent::new(code, mods);
        let none = KeyModifiers::NONE;
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "remove w");
        assert_eq!(m.cmdline, "remove w");
        assert_eq!(m.cmd_cur, 8);
        // Left lands before 'w'; typing inserts there.
        m.key(key(KeyCode::Left, none));
        type_colon(&mut m, "X");
        assert_eq!(m.cmdline, "remove Xw");
        // Backspace removes before the cursor ('X'), not 'w'.
        m.key(key(KeyCode::Backspace, none));
        assert_eq!(m.cmdline, "remove w");
        // Delete removes *at* the cursor — Left then Delete eats 'w'.
        m.key(key(KeyCode::Delete, none));
        assert_eq!(m.cmdline, "remove ");
        // Home + type prepends; End jumps back.
        m.key(key(KeyCode::Home, none));
        type_colon(&mut m, ">");
        assert_eq!(m.cmdline, ">remove ");
        m.key(key(KeyCode::End, none));
        assert_eq!(m.cmd_cur, m.cmdline.len());
        // Ctrl-A is Home, Ctrl-E is End.
        m.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(m.cmd_cur, 0);
        m.key(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(m.cmd_cur, m.cmdline.len());
        // Ctrl-W kills back to the previous whitespace — ">remove "
        // trims to ">remove" (no inner space), so the whole word goes.
        m.key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(m.cmdline, "");
        // Ctrl-U clears the line outright.
        m.key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(m.cmdline, "");
        assert_eq!(m.cmd_cur, 0);
        // Unicode: 'é' is two bytes — Left/Right cross it as one
        // char and Backspace at end eats it whole (no split char).
        type_colon(&mut m, "café");
        assert_eq!(m.cmdline, "café");
        m.key(key(KeyCode::Left, none)); // cursor before 'é'
        m.key(key(KeyCode::Right, none)); // back to end
        m.key(key(KeyCode::Backspace, none));
        assert_eq!(m.cmdline, "caf");
        // Esc drops the line and resets the cursor.
        m.key(key(KeyCode::Esc, none));
        assert_eq!(m.mode, Mode::Normal);
        assert_eq!(m.cmdline, "");
    }

    /// Ctrl-W word-kill semantics on a plain line.
    #[test]
    fn ctrl_w_kills_word_back() {
        let mut text = String::from("foo bar baz");
        let mut cur = text.len();
        let k = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(line_edit(&mut text, &mut cur, &k('w')), Edit::Mutate);
        assert_eq!(text, "foo bar ");
        assert_eq!(cur, 8);
        assert_eq!(line_edit(&mut text, &mut cur, &k('w')), Edit::Mutate);
        assert_eq!(text, "foo ");
        // Cursor mid-word kills only the fragment before it —
        // "one two" with the cursor after 't' drops just the 't'.
        let mut text = String::from("one two");
        let mut cur = 5;
        assert_eq!(line_edit(&mut text, &mut cur, &k('w')), Edit::Mutate);
        assert_eq!(text, "one wo");
        assert_eq!(cur, 4);
    }

    /// `:` history: Enter records, Up recalls newest-first, Down
    /// walks forward and restores the in-progress draft.
    #[test]
    fn colon_history_recalls() {
        let (_tmp, mut m) = bare_model();
        let key = |code, mods| KeyEvent::new(code, mods);
        let none = KeyModifiers::NONE;
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "r");
        m.key(key(KeyCode::Enter, none));
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "services");
        m.key(key(KeyCode::Enter, none));
        // Reopen — Up gives the most recent first.
        m.key(key(KeyCode::Char(':'), none));
        m.key(key(KeyCode::Up, none));
        assert_eq!(m.cmdline, "services");
        m.key(key(KeyCode::Up, none));
        assert_eq!(m.cmdline, "r");
        // Clamped at the oldest entry.
        m.key(key(KeyCode::Up, none));
        assert_eq!(m.cmdline, "r");
        m.key(key(KeyCode::Down, none));
        assert_eq!(m.cmdline, "services");
        // Past the newest entry the draft (empty here) returns.
        m.key(key(KeyCode::Down, none));
        assert_eq!(m.cmdline, "");
        // A typed draft is stashed and restored.
        type_colon(&mut m, "draft");
        m.key(key(KeyCode::Up, none));
        assert_eq!(m.cmdline, "services");
        m.key(key(KeyCode::Down, none));
        assert_eq!(m.cmdline, "draft");
        // Consecutive duplicates collapse.
        m.key(key(KeyCode::Esc, none));
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "services");
        m.key(key(KeyCode::Enter, none));
        assert_eq!(m.cmd_history.len(), 2);
        assert_eq!(m.cmd_history[0].as_str(), "services");
    }

    /// Dynamic completion: `:remove <prefix>` offers installed
    /// labels, `:service <verb> <u>` offers unit names, `:registry`
    /// offers registry names — all prefix-filtered, Tab-spliced at
    /// the cursor.
    #[test]
    fn colon_dynamic_completion() {
        let (_tmp, mut m) = bare_model();
        let key = |code, mods| KeyEvent::new(code, mods);
        let none = KeyModifiers::NONE;
        m.entries.push(store_row("widget", 1));
        m.entries.push(store_row("gadget", 2));
        m.svc_rows = vec![ServiceRow {
            package: "widget@1".into(),
            service: "web".into(),
            unit: "tamias-widget-web".into(),
            state: Some("active".into()),
            exec: "w".into(),
        }];
        m.reg_names = vec!["mine".into(), "org".into()];
        // Installed labels at a package arg position.
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "remove wid");
        let inserts: Vec<String> = m.cmd_completions.iter().map(|c| c.insert.clone()).collect();
        assert_eq!(inserts, vec!["widget@1 ".to_string()]);
        // Tab splices at the cursor — replaces the partial token.
        m.key(key(KeyCode::Tab, none));
        assert_eq!(m.cmdline, "remove widget@1 ");
        // `:uninstall` gets the same label candidates.
        m.key(key(KeyCode::Esc, none));
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "uninstall gad");
        let inserts: Vec<String> = m.cmd_completions.iter().map(|c| c.insert.clone()).collect();
        assert_eq!(inserts, vec!["gadget@2 ".to_string()]);
        // `:service`'s unit position offers unit names.
        m.key(key(KeyCode::Esc, none));
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "service start tamias-w");
        let inserts: Vec<String> = m.cmd_completions.iter().map(|c| c.insert.clone()).collect();
        assert_eq!(inserts, vec!["tamias-widget-web ".to_string()]);
        // `:registry` offers configured names.
        m.key(key(KeyCode::Esc, none));
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "registry m");
        let inserts: Vec<String> = m.cmd_completions.iter().map(|c| c.insert.clone()).collect();
        assert_eq!(inserts, vec!["mine ".to_string()]);
    }

    /// Completion works mid-line: the cursor, not the buffer end,
    /// decides which token is completed and where the insert lands.
    #[test]
    fn colon_completion_at_cursor() {
        let (_tmp, mut m) = bare_model();
        let key = |code, mods| KeyEvent::new(code, mods);
        let none = KeyModifiers::NONE;
        m.entries.push(store_row("widget", 1));
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "remove wid extra");
        // Move the cursor back into the 'wid' token.
        for _ in 0..6 {
            m.key(key(KeyCode::Left, none)); // "extra" is 5 + space
        }
        // Now inside/at-end of 'wid' — completions reflect 'wid'.
        assert!(
            m.cmd_completions.iter().any(|c| c.insert == "widget@1 "),
            "{:?}",
            m.cmd_completions
        );
        m.key(key(KeyCode::Tab, none));
        assert_eq!(m.cmdline, "remove widget@1  extra");
    }

    /// `:uninstall` dispatches `Job::ProviderUninstall` (full
    /// teardown) while `:remove` keeps the light `Job::Remove`.
    #[test]
    fn colon_uninstall_dispatches_provider_teardown() {
        let (_tmp, mut m) = bare_model();
        m.cmdline = "uninstall w@1".into();
        m.run_command();
        match &m.modal {
            Some(Modal::Confirm {
                job: Job::ProviderUninstall { name },
                ..
            }) => assert_eq!(name, "w@1"),
            other => panic!("expected ProviderUninstall confirm, got {other:?}"),
        }
        m.modal = None;
        m.cmdline = "remove w@1".into();
        m.run_command();
        assert!(matches!(
            m.modal,
            Some(Modal::Confirm {
                job: Job::Remove(_),
                ..
            })
        ));
    }

    /// The shared [`line_edit`] set also drives `/` filter/search
    /// and the prompts — cursor movement included.
    #[test]
    fn shared_line_editing_in_other_text_modes() {
        let (_tmp, mut m) = bare_model();
        let key = |code, mods| KeyEvent::new(code, mods);
        let none = KeyModifiers::NONE;
        // Prompt mode.
        m.mode = Mode::Prompt(PromptKind::InstallRef);
        for c in "abc".chars() {
            m.key(key(KeyCode::Char(c), none));
        }
        m.key(key(KeyCode::Left, none));
        m.key(key(KeyCode::Char('X'), none));
        assert_eq!(m.prompt_buf, "abXc");
        m.key(key(KeyCode::Esc, none));
        assert_eq!(m.prompt_buf, "");
        // Filter mode — editing rebuilds the filter live.
        m.pane = Pane::Installed;
        m.entries.push(store_row("widget", 1));
        m.key(key(KeyCode::Char('/'), none));
        assert_eq!(m.mode, Mode::Filter);
        for c in "wid".chars() {
            m.key(key(KeyCode::Char(c), none));
        }
        assert_eq!(m.filter, "wid");
        // Mid-line edit: Left then 'g' → "wigd".
        m.key(key(KeyCode::Left, none));
        m.key(key(KeyCode::Char('g'), none));
        assert_eq!(m.input, "wigd");
        assert_eq!(m.filter, "wigd");
        // Ctrl-U clears both buffer and filter.
        m.key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(m.input, "");
        assert_eq!(m.filter, "");
    }

    /// Bracketed paste lands at the cursor in every text mode.
    #[test]
    fn paste_inserts_at_cursor() {
        let (_tmp, mut m) = bare_model();
        let key = |code, mods| KeyEvent::new(code, mods);
        let none = KeyModifiers::NONE;
        m.key(key(KeyCode::Char(':'), none));
        type_colon(&mut m, "remove");
        m.paste(" w");
        assert_eq!(m.cmdline, "remove w");
        m.key(key(KeyCode::Left, none));
        m.paste("X");
        assert_eq!(m.cmdline, "remove Xw");
        // Filter mode.
        m.mode = Mode::Filter;
        m.input = "ab".into();
        m.input_cur = 1;
        m.paste("Z");
        assert_eq!(m.input, "aZb");
        assert_eq!(m.filter, "aZb");
        // Prompt mode.
        m.mode = Mode::Prompt(PromptKind::InstallRef);
        m.prompt_buf = "ab".into();
        m.prompt_cur = 2;
        m.paste("cd");
        assert_eq!(m.prompt_buf, "abcd");
        // Normal mode swallows it.
        m.mode = Mode::Normal;
        m.paste("nope");
        assert_eq!(m.prompt_buf, "abcd");
    }

    /// `g`/`G` follow the visible cursor: on an open package page
    /// they jump `detail_scroll` (never the hidden row `sel`), and
    /// inside versions focus they jump `versions_sel`.
    #[test]
    fn detail_g_g_jumps_page_scroll() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let page = reg_entry("1.0.0");
        m.pane = Pane::Registry;
        m.detail = Some(Detail::Registry {
            entry: page.clone(),
            install_ref: None,
            install_err: None,
            versions: Some(vec![reg_entry("0.9.0"), page.clone(), reg_entry("1.1.0")]),
            versions_err: None,
        });
        m.detail_scroll = 5;
        m.sel = 3; // hidden installed cursor — must not move
        m.key(key(KeyCode::Char('g')));
        assert_eq!(m.detail_scroll, 0);
        assert_eq!(m.sel, 3);
        // G jumps to the last frame's max scroll.
        m.detail_max_scroll = 42;
        m.key(key(KeyCode::Char('G')));
        assert_eq!(m.detail_scroll, 42);
        assert_eq!(m.sel, 3);
        // Versions focus: g/G walk the version list instead.
        m.versions_focus = true;
        m.versions_sel = 2;
        m.key(key(KeyCode::Char('g')));
        assert_eq!(m.versions_sel, 0);
        m.key(key(KeyCode::Char('G')));
        assert_eq!(m.versions_sel, 2);
        assert_eq!(m.detail_scroll, 42);
    }

    /// The wheel scrolls the open package page's body; with no page
    /// it keeps walking the pane selection.
    #[test]
    fn wheel_scrolls_detail_page() {
        let (_tmp, mut m) = bare_model();
        let wheel = |kind| MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        m.entries.push(store_row("w", 1));
        m.pane = Pane::Installed;
        m.sel = 0;
        // No detail — wheel moves the row selection (clamped at 0).
        m.mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(m.sel, 0);
        // Detail open — wheel moves detail_scroll, sel stays put.
        m.detail = Some(Detail::Installed {
            entry: m.entries[0].entry.clone(),
            manifest: None,
            manifest_err: None,
        });
        m.mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(m.detail_scroll, 3);
        m.mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(m.detail_scroll, 6);
        m.mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(m.detail_scroll, 3);
        assert_eq!(m.sel, 0);
    }

    /// `y` on the package page queues the install ref for the OSC 52
    /// clipboard write (`copy_request` — `run` emits it between
    /// frames) and confirms via the status toast. Installed copies
    /// the record's `source_ref`, Registry the resolved
    /// `install_ref`; a page with no ref says so instead.
    #[test]
    fn y_copies_install_ref() {
        let (_tmp, mut m) = bare_model();
        m.entries.push(store_row("w", 1));
        m.pane = Pane::Installed;
        m.sel = 0;
        m.sync_sel_key();
        key(&mut m, KeyCode::Enter); // open the package page
        assert!(m.detail.is_some());
        key(&mut m, KeyCode::Char('y'));
        assert_eq!(m.copy_request.as_deref(), Some("./x"));
        assert_eq!(m.status, "copied install ref");
        // Registry page — the resolved install_ref is what copies.
        m.copy_request = None;
        m.detail = Some(Detail::Registry {
            entry: reg_entry("1.0.0"),
            install_ref: Some("named:@s/w@1.0.0".into()),
            install_err: None,
            versions: None,
            versions_err: None,
        });
        key(&mut m, KeyCode::Char('y'));
        assert_eq!(m.copy_request.as_deref(), Some("named:@s/w@1.0.0"));
        // No ref → honest toast, nothing queued for the clipboard.
        m.copy_request = None;
        if let Some(Detail::Registry { install_ref, .. }) = &mut m.detail {
            *install_ref = None;
        }
        key(&mut m, KeyCode::Char('y'));
        assert!(m.copy_request.is_none());
        assert_eq!(m.status, "no install ref on this page");
    }

    /// Dead keys say so: j/k/g/G on the listless Publish pane and
    /// pane-gated verbs on panes that don't bind them land a status
    /// hint instead of silence.
    #[test]
    fn dead_keys_get_a_status_hint() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let dead = |m: &mut Model, pane: Pane, chars: &[char]| {
            m.pane = pane;
            m.status.clear();
            for &c in chars {
                m.key(key(KeyCode::Char(c)));
                assert!(
                    m.status.contains(&format!("no '{c}' on {}", pane.label())),
                    "{pane:?} '{c}': {}",
                    m.status
                );
                m.status.clear();
            }
        };
        m.pane = Pane::Author;
        m.sel = 2;
        m.key(key(KeyCode::Char('j')));
        assert!(m.status.contains("nothing to select"), "{}", m.status);
        m.key(key(KeyCode::Char('G')));
        assert!(m.status.contains("nothing to select"), "{}", m.status);
        // Every pane-gated verb char that the pane doesn't bind —
        // Home binds none of them (`i` is global, `F` has its own
        // gate, `R` reports here).
        dead(
            &mut m,
            Pane::Home,
            &[
                'v', 'u', 'U', 'p', 'P', 'x', 'c', 'C', 'd', 's', 'S', '!', 'e', 'E', 't', 'T',
                'n', 'R', 'a', 'r',
            ],
        );
        // Registry binds `v` only with a detail page open and `R` as
        // the reload — the rest of the verb set is dead there.
        dead(
            &mut m,
            Pane::Registry,
            &[
                'v', 'u', 'U', 'p', 'P', 'x', 'c', 'C', 'd', 's', 'S', '!', 'e', 'E', 't', 'T',
                'n', 'a', 'r',
            ],
        );
        // Services' own set still claims its chars — only the
        // Installed/Project/Publish leftovers are dead.
        dead(
            &mut m,
            Pane::Services,
            &['v', 'u', 'U', 'p', 'P', 'c', 'C', 'a', 'r'],
        );
        // Project binds `a x d e s S R` — the rest report.
        dead(
            &mut m,
            Pane::Manifest,
            &['v', 'u', 'U', 'p', 'P', 'c', 'C', '!', 't', 'T', 'n', 'r'],
        );
        // Publish binds `p P r R` — everything else reports.
        dead(
            &mut m,
            Pane::Author,
            &[
                'v', 'u', 'U', 'x', 'c', 'C', 'd', 's', 'S', '!', 'e', 'E', 't', 'T', 'n', 'a',
            ],
        );
        // Installed binds its own set + `R` reload — Services'/
        // Project's/Publish's leftovers report.
        dead(
            &mut m,
            Pane::Installed,
            &['P', 's', 'S', '!', 'e', 'E', 't', 'T', 'n', 'a', 'r'],
        );
        // …but the pane's own verbs still work — 'i' installs.
        m.key(key(KeyCode::Char('i')));
        assert!(m.status.is_empty() || !m.status.contains("no 'i'"));
    }

    /// `F` clears the `/` filter on Installed only — on any other
    /// pane it says where the filter lives instead of mutating
    /// hidden state (same gate as the `/` arm).
    #[test]
    fn clear_filter_is_installed_only() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for pane in [
            Pane::Home,
            Pane::Registry,
            Pane::Services,
            Pane::Manifest,
            Pane::Author,
        ] {
            m.pane = pane;
            m.filter = "keep".into();
            m.key(key(KeyCode::Char('F')));
            assert_eq!(m.filter, "keep", "{pane:?}");
            assert!(
                m.status.contains("filter is Installed-only"),
                "{pane:?}: {}",
                m.status
            );
        }
        m.pane = Pane::Installed;
        m.filter = "keep".into();
        m.key(key(KeyCode::Char('F')));
        assert!(m.filter.is_empty());
    }

    /// The `/` filter matches what the row shows — label, kind,
    /// source ref, description, sealed — never the provenance hex
    /// (`resolved`/`digest`), where fragments like "sha" matched
    /// everything.
    #[test]
    fn filter_ignores_provenance_hex() {
        let (_tmp, mut m) = bare_model();
        m.entries.push(store_row("widget", 1)); // resolved sha256:1, digest sha256:d1
        m.filter = "sha256".into();
        assert!(m.visible().is_empty());
        m.filter = "d1".into();
        assert!(m.visible().is_empty());
        // Source ref still matches.
        m.filter = "./x".into();
        assert_eq!(m.visible(), vec![0]);
        // And the label.
        m.filter = "wid".into();
        assert_eq!(m.visible(), vec![0]);
    }

    /// The Publish pane's registries table is selectable: j/k/g/G
    /// walk `author_regs` (clamped), a board-area row click selects,
    /// and `R` reloads the list without losing the cursor position.
    #[test]
    fn author_pane_selection() {
        let (_tmp, mut m) = bare_model();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let reg = |name: &str| ops::RegistryRow {
            name: name.into(),
            kind: "local".into(),
            target: String::new(),
        };
        m.pane = Pane::Author;
        m.author_regs = vec![reg("a"), reg("b"), reg("c")];
        m.key(key(KeyCode::Char('j')));
        assert_eq!(m.author_sel, 1);
        m.key(key(KeyCode::Char('k')));
        assert_eq!(m.author_sel, 0);
        m.key(key(KeyCode::Char('G')));
        assert_eq!(m.author_sel, 2);
        m.key(key(KeyCode::Char('g')));
        assert_eq!(m.author_sel, 0);
        // Row click — board_area row 0 is the header, data starts at 1.
        m.board_area = Rect::new(0, 2, 80, 10);
        m.viewport_top = 0;
        m.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(m.author_sel, 1);
        // `R` reloads from config (empty here) and clamps the cursor.
        m.key(key(KeyCode::Char('R')));
        assert!(m.author_regs.is_empty());
        assert_eq!(m.author_sel, 0);
    }
}
