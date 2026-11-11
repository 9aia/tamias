//! Engine ops — `cli.<verb>` wire queries dispatch into the `tamias`
//! engine lib through this binary's own `run` verb: each call
//! re-execs `tamias-driver-tamias run <argv>` with captured output,
//! so `tamias::dispatch` writes to the child's stdout and never
//! touches the serve loop's ndjson stream — and every mutation keeps
//! the engine's own semantics (store commit, provenance record,
//! bins[] deploy).
//!
//! `ops` answers the ops manifest as JSON — derived from the
//! engine's own clap tree ([`tamias::command`]), so a verb can't
//! exist on the CLI and be missing here. Unbounded/host-interactive
//! verbs (`serve`, `ui`, `registry.serve`, `run`) are deliberately
//! *not* ops — they never return or need inherited stdio, so they
//! live in `surfaces` and stay reachable through the `run`/`cli`
//! passthroughs.
//!
//! Unlike the sibling engines this driver does **not** back a
//! `bins[]` shim: `tamias` itself is the shim helper every other
//! pack execs (`tamias bin exec`), so the `tamias` binary stays
//! native forever — see the tamias repo's T-007 report. The ops
//! surface exists for pavo embedding (`query:"ops"`,
//! `query:"cli.<verb>"`), not to retire the CLI.
//!
//! Wire params carry each verb's declared param schema: `arg` =
//! positional, `opt` = `--name <v>`, `flag` = `--name` — the
//! contract `pavo-driver-kit`'s surface layer renders args from.

use clap::{Arg, ArgAction, Command};
use pavo_protocol::wire::{QueryResult, Response};
use serde_json::{json, Map, Value};

/// Unbounded/host-interactive surfaces — verbs that never return
/// (`serve`, `registry.serve`, `ui`, `run`) or need inherited
/// stdio/tty, so they can never be request/response ops.
///
/// - `serve` — the ndjson JSON-RPC loop (`tamias serve`); embedding
///   already rides `tamias-serve`, this driver's own `serve` is the
///   pavo wire.
/// - `ui` — the ratatui TUI; also absent from the engine tree when
///   the driver builds `tamias` with `default-features = false`
///   (no `tui`), so it can never appear as an op either way.
/// - `registry.serve` — the HTTP registry server.
/// - `run` — a provider service's foreground exec; runs until
///   killed, needs inherited stdio.
const SURFACES: &[&str] = &["serve", "ui", "registry.serve", "run"];

/// Params excluded from ops — flags that turn a bounded verb into
/// a stream. `logs`/`service logs` follow the journal forever with
/// `--follow`; the bounded read stays an op and the streaming mode
/// stays reachable through `run`/`cli` passthrough.
const SKIP_PARAMS: &[&str] = &["follow"];

/// Read-only verbs — everything else writes the store, cache,
/// registry, bin dir, or host services. Mutators belong behind a
/// deliberate trigger, not ambient view polling. `driver.exec` and
/// `self-upgrade` stay mutating: a kind driver's verb can do
/// anything, and `self-upgrade --check` shares the verb with the
/// re-install.
const READ_ONLY: &[&str] = &[
    "version",
    "manifest",
    "schema",
    "gen-cli-docs",
    "completions",
    "doctor",
    "config",
    "list",
    "info",
    "verify",
    "search",
    "drivers",
    "provider.list",
    "bin.list",
    "registry.get",
    "registry.list",
    "logs",
    "service.status",
    "service.logs",
    "driver.config-schema",
];

/// `(name, clap spec)` for every leaf verb. Hidden plumbing and
/// clap's `help` are skipped — they stay reachable through `run`
/// passthrough. (`bin exec` — the bins[] shim internals — is
/// `hide = true`, so it never becomes an op.)
fn verbs() -> Vec<(String, Command)> {
    fn walk(prefix: &str, cmd: &Command, out: &mut Vec<(String, Command)>) {
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            if name == "help" || sub.is_hide_set() {
                continue;
            }
            let dotted = match prefix {
                "" => name.to_string(),
                _ => format!("{prefix}.{name}"),
            };
            if sub.has_subcommands() {
                walk(&dotted, sub, out);
            } else {
                out.push((dotted, sub.clone()));
            }
        }
    }
    let mut out = Vec::new();
    walk("", &tamias::command(), &mut out);
    out
}

fn op_cmd(name: &str) -> Option<Command> {
    verbs()
        .into_iter()
        .find(|(n, _)| n == name && !SURFACES.contains(&n.as_str()))
        .map(|(_, c)| c)
}

/// Args that can't ride the wire: streaming flags (above) and
/// clap's own help/version actions.
fn skipped(a: &Arg) -> bool {
    SKIP_PARAMS.contains(&a.get_id().as_str())
        || matches!(
            a.get_action(),
            ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
        )
}

/// A clap arg → the manifest's typed param kind — `arg` =
/// positional, `opt` = `--name <v>`, `flag` = `--name` — the
/// contract `pavo-driver-kit`'s surface layer renders args from.
fn kind(a: &Arg) -> &'static str {
    if a.is_positional() {
        "arg"
    } else if matches!(
        a.get_action(),
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count
    ) {
        "flag"
    } else {
        "opt"
    }
}

fn param_json(a: &Arg) -> Value {
    // Wire name = the flag name (`--force` → `force`), not the
    // field id — generated surfaces spell params `--<name>`.
    let id = a.get_id().as_str();
    let name = match a.is_positional() {
        true => id,
        false => a.get_long().unwrap_or(id),
    };
    json!({
        "name": name,
        "kind": kind(a),
        "doc": a.get_help().map(|d| d.to_string()).unwrap_or_default(),
    })
}

/// The `ops` query/`ops` argv verb payload — self-describing so
/// hosts and agents discover the engine surface without argv help.
/// tamias has no global flags; instead every op honors the same
/// `TAMIAS_*`/XDG env the CLI does — ops children inherit the
/// `serve` process's environment and cwd verbatim.
pub fn manifest() -> Value {
    json!({
        "engine": "tamias",
        "invoke": "query `cli.<verb>` with the listed params, or argv `tamias-driver-tamias run <tamias argv>`",
        "env": [
            {"name": "TAMIAS_HOME", "doc": "Store root override (default ~/.local/share/tamias/store)."},
            {"name": "TAMIAS_CACHE", "doc": "Fetch-cache root override."},
            {"name": "TAMIAS_REGISTRY", "doc": "Registry root override."},
            {"name": "TAMIAS_BIN_DIR", "doc": "Bin dir override (else XDG_BIN_HOME → ~/.local/bin)."},
            {"name": "TAMIAS_CONFIG", "doc": "Explicit tamias.yml/tamias.toml config file."},
            {"name": "TAMIAS_ALLOW_DRIVERS", "doc": "Global bundled-driver consent (= --allow-drivers)."},
        ],
        "ops": verbs().iter()
            .filter(|(n, _)| !SURFACES.contains(&n.as_str()))
            .map(|(n, c)| json!({
                "query": format!("cli.{n}"),
                "doc": c.get_about().map(|d| d.to_string()).unwrap_or_default(),
                "params": c.get_arguments()
                    .filter(|a| !skipped(a))
                    .map(param_json)
                    .collect::<Vec<_>>(),
                "mutates": !READ_ONLY.contains(&n.as_str()),
            }))
            .collect::<Vec<_>>(),
        "surfaces": SURFACES,
    })
}

/// Is `query` an engine op (`cli.<verb>`)? Returns the verb name.
pub fn op_of(query: &str) -> Option<String> {
    query
        .strip_prefix("cli.")
        .filter(|n| op_cmd(n).is_some())
        .map(str::to_string)
}

/// A wire scalar → its argv token (numbers/bools stringify).
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// All values of a param — arrays flatten, scalars wrap.
fn values(v: &Value) -> Vec<String> {
    match v {
        Value::Array(xs) => xs.iter().filter_map(scalar).collect(),
        other => scalar(other).into_iter().collect(),
    }
}

/// `cli.<verb>` params → tamias argv (no argv[0] — `run` prepends
/// it). Declared params map by kind — positionals land last, in
/// schema order; a trailing-var-arg positional (`driver exec`'s
/// args) only parses after a `--` separator.
fn argv(name: &str, cmd: &Command, params: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    out.extend(name.split('.').map(String::from));
    for a in cmd.get_arguments() {
        if skipped(a) || a.is_positional() {
            continue;
        }
        let id = a.get_id().as_str();
        let long = a.get_long().unwrap_or(id);
        let raw = params
            .get(long)
            .or_else(|| params.get(id))
            .or_else(|| params.get(id.replace('_', "-").as_str()));
        let Some(raw) = raw else { continue };
        match kind(a) {
            "flag" => {
                if raw.as_bool().unwrap_or(false) {
                    out.push(format!("--{long}"));
                }
            }
            _ => {
                for s in values(raw) {
                    out.push(format!("--{long}"));
                    out.push(s);
                }
            }
        }
    }
    // `trailing_var_arg` positionals only parse after a `--`
    // separator — `driver exec`'s passthrough args need it.
    if cmd.get_positionals().any(|a| a.is_last_set()) {
        out.push("--".into());
    }
    let mut positionals: Vec<String> = Vec::new();
    for a in cmd.get_positionals() {
        if skipped(a) {
            continue;
        }
        let id = a.get_id().as_str();
        if let Some(raw) = params
            .get(id)
            .or_else(|| params.get(id.replace('_', "-").as_str()))
        {
            positionals.extend(values(raw));
        }
    }
    out.extend(positionals);
    out
}

/// Run one engine op by name: re-exec `run` in a child with
/// captured output, and hand back `{op, argv, code, stdout,
/// stderr}` as the query payload. `ok:false` is reserved for spawn
/// failures — a nonzero `code` is the engine's own exit status
/// (e.g. `doctor` findings, `verify` mismatches), honest data. The
/// child's stdin is null and it inherits this process's cwd + env —
/// the `TAMIAS_*` chain picks the store/cache/registry exactly like
/// the `tamias` CLI, and host-interactive verbs (`serve`/`ui`/
/// `registry.serve`/`run`) are surfaces, never ops, precisely
/// because they need inherited stdio.
pub fn run_verb(name: &str, params: &Map<String, Value>) -> Response {
    let Some(cmd) = op_cmd(name) else {
        return Response::err(format!("unknown op: {name}"));
    };
    let args = argv(name, &cmd, params);
    let out = std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe)
            .arg("run")
            .args(&args)
            .output()
    });
    let data = match out {
        Ok(o) => json!({
            "op": name,
            "argv": args,
            "code": o.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&o.stdout),
            "stderr": String::from_utf8_lossy(&o.stderr),
        }),
        Err(e) => return Response::err(format!("run {name}: {e}")),
    };
    Response::ok_typed(&QueryResult {
        query: format!("cli.{name}"),
        data,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn op_of_accepts_cli_prefix_only() {
        assert_eq!(op_of("cli.list").unwrap(), "list");
        assert_eq!(op_of("cli.bin.deploy").unwrap(), "bin.deploy");
        assert_eq!(op_of("cli.registry.list").unwrap(), "registry.list");
        assert!(op_of("cli.serve").is_none()); // unbounded surface
        assert!(op_of("cli.ui").is_none()); // interactive surface
        assert!(op_of("cli.registry.serve").is_none()); // http surface
        assert!(op_of("cli.run").is_none()); // foreground surface
        assert!(op_of("cli.bin").is_none()); // group, not a leaf
        assert!(op_of("cli.bin.exec").is_none()); // hidden plumbing
        assert!(op_of("list").is_none()); // view query, not an op
        assert!(op_of("cli.bogus").is_none());
    }

    #[test]
    fn manifest_lists_every_non_surface_verb_as_cli_query() {
        let m = manifest();
        let ops = m["ops"].as_array().unwrap();
        for (n, _) in verbs()
            .iter()
            .filter(|(n, _)| !SURFACES.contains(&n.as_str()))
        {
            assert!(
                ops.iter().any(|o| o["query"] == json!(format!("cli.{n}"))),
                "{n} missing from ops manifest"
            );
        }
        assert_eq!(m["surfaces"], json!(SURFACES));
        for o in ops {
            for p in o["params"].as_array().unwrap() {
                assert!(["arg", "opt", "flag"].contains(&p["kind"].as_str().unwrap()));
                // streaming params never ride the wire
                assert!(!SKIP_PARAMS.contains(&p["name"].as_str().unwrap()));
            }
        }
        // mutates honesty — spot-check the read set stayed reads
        for q in [
            "cli.version",
            "cli.list",
            "cli.info",
            "cli.verify",
            "cli.doctor",
            "cli.search",
            "cli.drivers",
        ] {
            let op = ops.iter().find(|o| o["query"] == json!(q)).unwrap();
            assert_eq!(op["mutates"], json!(false), "{q} must be read-only");
        }
        for q in [
            "cli.install",
            "cli.remove",
            "cli.update",
            "cli.publish",
            "cli.bin.deploy",
            "cli.cache.clean",
            "cli.driver.exec",
        ] {
            let op = ops.iter().find(|o| o["query"] == json!(q)).unwrap();
            assert_eq!(op["mutates"], json!(true), "{q} must mutate");
        }
    }

    #[test]
    fn argv_maps_params_to_tamias_flags() {
        let cmd = op_cmd("install").unwrap();
        let mut p = Map::new();
        p.insert("reference".into(), json!("./pkg"));
        p.insert("allow_drivers".into(), json!(true));
        p.insert("no_bins".into(), json!(true));
        p.insert("plan".into(), json!(true));
        assert_eq!(
            argv("install", &cmd, &p),
            vec!["install", "--allow-drivers", "--no-bins", "--plan", "./pkg"]
        );
    }

    #[test]
    fn argv_splits_dotted_names() {
        let cmd = op_cmd("registry.list").unwrap();
        let mut p = Map::new();
        p.insert("json".into(), json!(true));
        assert_eq!(
            argv("registry.list", &cmd, &p),
            vec!["registry", "list", "--json"]
        );
    }

    #[test]
    fn argv_maps_trailing_var_arg_positionals() {
        // `driver exec`'s passthrough `args` is a trailing-var-arg
        // positional — clap's `allow_hyphen_values` grabs the rest
        // once the leading positionals fill, no `--` needed (the
        // engine's own parse tests pin `driver exec pkg health
        // --help` → args `["--help"]`).
        let cmd = op_cmd("driver.exec").unwrap();
        let mut p = Map::new();
        p.insert("name".into(), json!("pkg"));
        p.insert("verb".into(), json!("hooks"));
        p.insert("args".into(), json!(["register", "--dry-run"]));
        assert_eq!(
            argv("driver.exec", &cmd, &p),
            vec!["driver", "exec", "pkg", "hooks", "register", "--dry-run"]
        );
    }

    #[test]
    fn argv_skips_streaming_params() {
        let cmd = op_cmd("logs").unwrap();
        let mut p = Map::new();
        p.insert("name".into(), json!("pkg"));
        p.insert("lines".into(), json!(50));
        p.insert("follow".into(), json!(true)); // SKIP_PARAMS — never emitted
        assert_eq!(argv("logs", &cmd, &p), vec!["logs", "--lines", "50", "pkg"]);
    }
}
