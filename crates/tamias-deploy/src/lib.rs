#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-deploy — the deploy port: provision a remote registry, plus
//! the shared per-user *service spec* every local backend renders
//! (systemd units, Task Scheduler tasks, launchd agents, …).

use std::path::Path;

use tamias_core::Error;

/// A registry deployer — materializes the deployable artifacts (Worker,
/// R2 binding, secret setup) so an operator can ship a hosted registry.
pub trait Deploy {
    /// Write the deployable artifacts under `out`; return the next steps.
    ///
    /// # Errors
    /// Propagates write failures.
    fn deploy(&self, out: &Path) -> Result<Vec<String>, Error>;
}

/// What to deploy as a per-user service — the platform-neutral spec
/// every local backend (`tamias-deploy-systemd`, `tamias-deploy-windows`,
/// `tamias-deploy-launchd`, …) renders into its own unit/task/plist
/// format. A package's
/// `provider.services[]` manifest block supplies the domain data; each
/// backend decides what it can honor (see its crate docs for the
/// per-field mapping and honest gaps).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Service base name — the backend derives its own object name
    /// (`<name>.service`, a Task Scheduler task name, …).
    pub name: String,
    /// Short description.
    pub description: String,
    /// Command argv — rendered with the backend's quoting (systemd
    /// `ExecStart`, a generated `.cmd` wrapper, …). Parse a
    /// command-line string with [`split_exec`] first.
    pub exec: Vec<String>,
    /// `K=V` environment entries.
    pub env: Vec<(String, String)>,
    /// Optional env-file path (`K=V` lines, `#` comments) — always
    /// treated as *optional* (a missing file must not fail the
    /// service). `~` expands to the user home at render time.
    pub env_file: Option<String>,
    /// Restart policy — backend-defined vocabulary (`on-failure`,
    /// `always`, `no`, …); `None` leaves the backend default.
    pub restart: Option<String>,
    /// Restart delay in seconds — `None` leaves the backend default.
    pub restart_sec: Option<u32>,
    /// Working directory — `~` expands to the user home at render
    /// time.
    pub workdir: Option<String>,
    /// Extra ordering targets (systemd `After=`/`Wants=`) — meaningful
    /// only to backends with a dependency graph; others ignore it.
    pub after: Vec<String>,
}

/// Expand a leading `~` (or `~/…`) to the user home — the manifest
/// `workdir` / `env_file` fields allow `~`, and YAML needs it quoted
/// (`"~"`) since a bare `~` parses as null. `~user` forms and a
/// missing home var pass through unchanged. On Windows `USERPROFILE`
/// wins; elsewhere `HOME`.
#[must_use]
pub fn expand_home(path: &str) -> String {
    let home = || {
        if cfg!(windows) {
            std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME"))
        } else {
            std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))
        }
    };
    if path == "~" {
        return home().unwrap_or_else(|_| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home()
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|_| path.to_string());
    }
    path.to_string()
}

/// Split a command-line string into argv — a deliberately small
/// POSIX-shell subset, no external deps:
///
/// - unquoted whitespace separates arguments;
/// - `'…'` is literal (no escapes);
/// - `"…"` is literal except `\"` → `"` and `\\` → `\`;
/// - `\c` outside quotes yields `c` (so `\ ` escapes a space);
/// - unterminated quotes are an error; empty/whitespace input yields an
///   empty argv (callers reject it — a service needs an exec argv).
///
/// `#` comments, `$VAR`, globs, and `;`/`|`/`&` are *not* special — the
/// string is data, never re-parsed by a shell.
///
/// # Errors
/// [`Error::Failed`] on an unterminated quote.
pub fn split_exec(exec: &str) -> Result<Vec<String>, Error> {
    #[derive(PartialEq)]
    enum State {
        Bare,
        Single,
        Double,
    }
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut state = State::Bare;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            State::Bare => match c {
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        argv.push(std::mem::take(&mut cur));
                    }
                }
                '\'' => state = State::Single,
                '"' => state = State::Double,
                '\\' => {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    } else {
                        cur.push('\\');
                    }
                }
                _ => cur.push(c),
            },
            State::Single => {
                if c == '\'' {
                    state = State::Bare;
                } else {
                    cur.push(c);
                }
            }
            State::Double => match c {
                '"' => state = State::Bare,
                '\\' => match chars.peek() {
                    Some('"' | '\\') => cur.push(chars.next().unwrap_or('\\')),
                    _ => cur.push('\\'),
                },
                _ => cur.push(c),
            },
        }
    }
    if state != State::Bare {
        return Err(Error::Failed(format!(
            "unterminated quote in exec '{exec}'"
        )));
    }
    if !cur.is_empty() {
        argv.push(cur);
    }
    Ok(argv)
}
