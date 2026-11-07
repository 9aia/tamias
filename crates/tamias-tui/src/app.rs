//! Model — env tabs, the ref list for the active env, the filter,
//! and the two-step reveal. The default posture is *browse the
//! surface, never spill*: values enter the model only through
//! [`Model::reveal`], an audited `get_in` behind a deliberate
//! `r`→`r` confirm. Everything else is metadata — `list_records`
//! records have their `Secret`s dropped immediately.
//!
//! Read-only by design: there is no mutation path in v1.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tamias_core::{Env, Ref, Secret};
use tamias_ops::Ops;

/// One env tab — its vault route comes from config.
pub struct EnvTab {
    /// The env name.
    pub env: Env,
    /// The `[envs]`-routed (or implicit `local`) vault name.
    pub vault: String,
    /// The vault's backend kind (`age`, `cf`, …) and its
    /// `path`/`url` when configured — detail-pane provenance.
    pub kind: String,
    /// `path` (age) or `url` (cf) from `[vaults.*]`, if set.
    pub location: Option<String>,
}

/// Metadata for one stored ref — never the value.
pub struct Entry {
    /// The ref (`tamias://ns/name`, env-pin stripped by the vault).
    pub rref: Ref,
    /// Write generation.
    pub version: u64,
    /// RFC 3339 — first stored.
    pub created_at: String,
    /// RFC 3339 — last deliberate rotation, if ever.
    pub rotated_at: Option<String>,
}

/// What the active env's load produced — failures are content,
/// not panics: a vault that won't open renders as an error line.
pub enum Listing {
    /// Refs + metadata, sorted (vault `list` order).
    Ready(Vec<Entry>),
    /// `open_vault`/`list` failed — the why, shown in the pane.
    Failed(String),
}

/// The browser state.
pub struct Model<'a> {
    ops: &'a Ops,
    /// Env tabs: well-known envs first, then `[envs]` customs,
    /// then the selected env if it isn't already listed.
    pub tabs: Vec<EnvTab>,
    /// Active tab index.
    pub active: usize,
    /// The active env's listing.
    pub listing: Listing,
    /// Cursor into the *filtered* view.
    pub selected: usize,
    /// `/` filter text (matches the ref path, case-insensitive).
    pub filter: String,
    /// Filter-line editing is active.
    pub filtering: bool,
    /// `?` help overlay is up.
    pub help: bool,
    /// The ref path a first `r` armed — a second `r` reveals.
    pending_reveal: Option<String>,
    /// `(ref path, value)` after a confirmed reveal — `Secret`
    /// kept redacting until render exposes it for the pane.
    pub revealed: Option<(String, Secret)>,
    /// One-line status in the footer (errors, reveal prompts).
    pub status: Option<String>,
}

impl<'a> Model<'a> {
    /// Build the tab list and load the selected env.
    pub fn open(ops: &'a Ops) -> Self {
        // dev/staging/prod always listed; [envs] customs follow
        // sorted; a selected env outside both still gets a tab.
        let mut names: Vec<Env> = Env::WELL_KNOWN
            .iter()
            .filter_map(|n| Env::new(n).ok())
            .collect();
        let mut customs: Vec<Env> = ops
            .config()
            .envs
            .keys()
            .filter(|e| !e.is_well_known())
            .cloned()
            .collect();
        customs.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        names.extend(customs);
        if !names.iter().any(|e| e == ops.env()) {
            names.push(ops.env().clone());
        }
        let tabs: Vec<EnvTab> = names
            .into_iter()
            .map(|env| {
                let vault = ops.config().vault_for(&env).to_string();
                let cfg = ops.config().vault_config(&vault);
                EnvTab {
                    env,
                    kind: cfg.map(|c| c.kind.clone()).unwrap_or_else(|| "age".into()),
                    location: cfg.and_then(|c| c.path.clone().or_else(|| c.url.clone())),
                    vault,
                }
            })
            .collect();
        let active = tabs.iter().position(|t| t.env == *ops.env()).unwrap_or(0);
        let mut m = Model {
            ops,
            tabs,
            active,
            listing: Listing::Ready(Vec::new()),
            selected: 0,
            filter: String::new(),
            filtering: false,
            help: false,
            pending_reveal: None,
            revealed: None,
            status: None,
        };
        m.reload();
        m
    }

    /// (Re)load the active env's listing — on tab switch and `R`.
    /// A failure lands in `listing` and renders as an error line;
    /// the browser keeps running.
    pub fn reload(&mut self) {
        self.clear_value();
        self.selected = 0;
        let env = self.tabs[self.active].env.clone();
        self.listing = match self.ops.list_records(Some(&env)) {
            Ok((_, records)) => Listing::Ready(
                records
                    .into_iter()
                    .map(|(rref, rec)| Entry {
                        rref,
                        version: rec.version,
                        created_at: fmt_ts(&rec.created_at),
                        rotated_at: rec.rotated_at.as_ref().map(fmt_ts),
                    })
                    .collect(),
            ),
            Err(e) => Listing::Failed(e.to_string()),
        };
    }

    /// Indices of entries surviving the filter.
    fn visible(&self) -> Vec<usize> {
        match &self.listing {
            Listing::Failed(_) => Vec::new(),
            Listing::Ready(entries) => {
                let needle = self.filter.to_lowercase();
                entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        needle.is_empty() || e.rref.path().to_lowercase().contains(&needle)
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
        }
    }

    /// The highlighted entry, filter applied.
    pub fn current(&self) -> Option<&Entry> {
        // A failed listing has no visible entries — bail before the
        // index math rather than after it.
        let Listing::Ready(entries) = &self.listing else {
            return None;
        };
        let vis = self.visible();
        let i = *vis.get(self.selected.min(vis.len().saturating_sub(1)))?;
        entries.get(i)
    }

    /// Handle a key; `true` means quit.
    pub fn key(&mut self, k: KeyEvent) -> bool {
        if self.help {
            // Any key closes the overlay except `?` noise is fine.
            self.help = false;
            return false;
        }
        if self.filtering {
            return self.filter_key(k);
        }
        let ctrl_c =
            k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c'));
        match k.code {
            _ if ctrl_c => true,
            KeyCode::Char('q') => true,
            KeyCode::Char('?') => {
                self.help = true;
                false
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.step(1);
                false
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.step(-1);
                false
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = 0;
                self.clear_value();
                false
            }
            KeyCode::Char('G') | KeyCode::End => {
                let last = self.visible().len().saturating_sub(1);
                self.selected = last;
                self.clear_value();
                false
            }
            KeyCode::Tab => {
                self.switch_env(1);
                false
            }
            KeyCode::BackTab => {
                self.switch_env(-1);
                false
            }
            KeyCode::Char(c @ '1'..='9') => {
                let i = (c as usize) - ('1' as usize);
                if i < self.tabs.len() && i != self.active {
                    self.active = i;
                    self.reload();
                }
                false
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                false
            }
            KeyCode::Char('r') => {
                self.reveal_step();
                false
            }
            KeyCode::Char('R') => {
                self.reload();
                self.status = Some("reloaded".into());
                false
            }
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.selected = 0;
                }
                self.clear_value();
                self.status = None;
                false
            }
            _ => false,
        }
    }

    /// Keys while the `/` line is open.
    fn filter_key(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
                self.selected = 0;
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
                self.clear_value();
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.selected = 0;
                self.clear_value();
            }
            _ => {}
        }
        false
    }

    /// Move the cursor inside the filtered view, wrapping.
    fn step(&mut self, dir: i64) {
        let n = self.visible().len();
        if n == 0 {
            return;
        }
        let cur = self.selected.min(n - 1) as i64;
        self.selected = (cur + dir).rem_euclid(n as i64) as usize;
        self.clear_value();
    }

    /// Cycle env tabs (`dir` ±1), then reload.
    fn switch_env(&mut self, dir: i64) {
        let n = self.tabs.len() as i64;
        self.active = ((self.active as i64) + dir).rem_euclid(n) as usize;
        self.reload();
    }

    /// Drop any armed or shown value — navigation, filtering, and
    /// env switches all reset the reveal so a stale value can't
    /// linger under a different cursor.
    fn clear_value(&mut self) {
        self.pending_reveal = None;
        self.revealed = None;
    }

    /// `r` is a deliberate two-step: first press arms the prompt
    /// ("press r again"), second press on the same ref runs an
    /// audited `get_in` — a `get` materialization event — and
    /// holds the value redacting until render. `ops.get_in` is
    /// used (not `get`) because the browsed env isn't necessarily
    /// the selected env, and env-pin games would mis-attribute
    /// the audit event.
    fn reveal_step(&mut self) {
        let Some(entry) = self.current() else {
            self.status = Some("nothing to reveal".into());
            return;
        };
        let path = entry.rref.path();
        if self.pending_reveal.as_deref() == Some(path.as_str()) {
            let env = self.tabs[self.active].env.clone();
            match self.ops.get_in(&env, &entry.rref) {
                Ok(Some(rec)) => {
                    self.revealed = Some((path, rec.value));
                    self.pending_reveal = None;
                    self.status = Some("revealed — Esc or move to hide".into());
                }
                Ok(None) => {
                    self.status = Some(format!("{path} is not stored in {env}"));
                    self.pending_reveal = None;
                }
                Err(e) => {
                    self.status = Some(format!("reveal failed: {e}"));
                    self.pending_reveal = None;
                }
            }
        } else {
            self.revealed = None;
            self.pending_reveal = Some(path.clone());
            self.status = Some(format!("press r again to reveal {path}"));
        }
    }
}

/// RFC 3339 for the detail pane.
fn fmt_ts(t: &time::OffsetDateTime) -> String {
    // Rfc3339 formats any OffsetDateTime — the Err arm is
    // unreachable by construction.
    #[allow(clippy::unwrap_used)]
    t.format(&time::format_description::well_known::Rfc3339)
        .expect("Rfc3339 formats every instant")
}
