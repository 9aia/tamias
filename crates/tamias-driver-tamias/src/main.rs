//! tamias-driver-tamias — the pavo view driver for tamias itself:
//! the install store, fetch cache, registry index, and the
//! `tamias-driver-*` driver dir, as declarative ViewNode views.
//!
//! Verbs (the tamias driver contract):
//!
//! - `manifest` — print the [`DriverManifest`] JSON (discovery
//!   probe; never touches store state).
//! - `capabilities` — `{"capabilities": ["pavo"], "verbs": […],
//!   "types": []}`. The empty `types` keeps tamias's own
//!   capability-fallback driver lookup from claiming content types
//!   through this binary (its install verbs are pavo's, not a
//!   content type's).
//! - `serve` — ndjson JSON-RPC on stdio: `{"op":"manifest"}`,
//!   `{"op":"views"}`, `{"op":"view","view":…,"params":{…}}`,
//!   `{"op":"query","query":…,"params":{…}}` →
//!   `{"ok":true,"result":…}` / `{"ok":false,"error":…}`.
//!   `query:"ops"` answers the engine ops manifest; `query:"cli.<verb>"`
//!   runs a `tamias` engine op (see `ops.rs`).
//!
//! Engine ops surface (P-021/T-007): the `tamias` CLI is lib-ified
//! (`tamias::command()`/`tamias::run_from`), and this driver exposes
//! it three ways —
//!
//! - `ops` — the ops manifest: every bounded `tamias` verb as a
//!   `cli.<verb>` wire query derived from the engine's own clap
//!   tree; unbounded surfaces (`serve`, `ui`, `registry.serve`,
//!   `run`) are listed, never ops.
//! - `run <argv>` — engine passthrough: `tamias-driver-tamias run
//!   list` ≡ `tamias list`. Also the child side of `cli.*` wire
//!   queries (`ops::run_verb` re-execs this same verb so engine
//!   output never touches the serve stream).
//! - `cli <argv>` — the `bins[]` shim verb shape: `tamias bin exec`
//!   calls drivers with the standard `--dir`/`--name` flags ahead of
//!   the user's argv; they're stripped and the rest runs through
//!   `tamias::run_from` in-process with inherited stdio. No `tamias`
//!   pack ships this shim today — `tamias` is the substrate every
//!   other shim execs — but the verb keeps full argv parity
//!   available and gives ops/tests a passthrough identical to the
//!   sibling engines'.
//!
//! Which tamias home: views/queries resolve their own roots —
//! `params.home`/`store`/`cache`/`registry`/`drivers` → the same
//! `$TAMIAS_HOME`/`$TAMIAS_CACHE`/`$TAMIAS_REGISTRY`/XDG chain the
//! `tamias` CLI uses (see `data.rs`). `manifest`/`views` are
//! root-free so pavo can enumerate drivers anywhere. Ops children
//! inherit the `serve` process's cwd + env, so the same `TAMIAS_*`
//! chain applies to `cli.*` ops.
//!
//! The view surface stays **read-only**: it walks
//! `<store>/…/.tamias-store.json` records, counts cache entries, and
//! scans the driver dir — the same reads `tamias list`/`info`/
//! `doctor` do. Writes ride the engine ops (`cli.<verb>`), which
//! re-enter the `tamias` lib's own verbs — never a parallel
//! implementation.
//!
//! Note: the executable name `tamias-driver-tamias` is also what
//! tamias's content-driver host looks up for a content type named
//! `tamias` — no such type exists today, and this driver's
//! `capabilities` claims no `types`, so only an explicit
//! `type: tamias` package would ever route install verbs here (and
//! they would fail loudly, not silently).

mod data;
mod ops;
mod views;

use pavo_driver_kit as kit;
use pavo_protocol::wire::{self, Request, Response, ViewSummary};

fn manifest_json() -> serde_json::Value {
    serde_json::to_value(views::manifest()).unwrap_or(serde_json::Value::Null)
}

fn capabilities_json() -> serde_json::Value {
    let m = views::manifest();
    // The kit's shared shape plus this driver's extras: `types` (the
    // empty claim that keeps tamias's content-driver host from
    // routing install verbs here) and the view count.
    let mut value = kit::capabilities_value(&m);
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("types".to_string(), serde_json::json!([]));
        map.insert("views".to_string(), serde_json::json!(m.views.len()));
    }
    value
}

/// Dispatch one parsed request to its response.
fn dispatch(req: &Request) -> Response {
    match req {
        Request::Manifest => Response::ok_typed(&views::manifest()),
        Request::Views => {
            let summaries: Vec<ViewSummary> =
                views::specs().iter().map(ViewSummary::from).collect();
            Response::ok_typed(&summaries)
        }
        Request::View { view, params } => match views::view_result(view, params) {
            Ok(result) => Response::ok_typed(&result),
            Err(e) => Response::err(e),
        },
        Request::Query { query, params } => {
            // `ops` and `cli.<verb>` are engine ops; view queries
            // answer from the store/cache/registry snapshot.
            if query == "ops" {
                return Response::ok_typed(&wire::QueryResult {
                    query: query.clone(),
                    data: ops::manifest(),
                });
            }
            if ops::op_of(query).is_some() {
                return ops::run_verb(&query["cli.".len()..], params);
            }
            // Unknown names fail the op; everything else answers
            // `ok:true` — degraded snapshots travel as
            // `{"error": …}` data (the marker renderers understand),
            // matching the reference drivers' contract.
            if !views::known_query(query) {
                return Response::err(format!("unknown query: {query}"));
            }
            let snap = data::load(params);
            let payload = views::query_data(query, params, &snap);
            Response::ok_typed(&wire::QueryResult {
                query: query.clone(),
                data: payload,
            })
        }
        // `render` — this driver ships no renderer plugins; hosts
        // only call it for kinds the manifest claims, so answering
        // an explicit error keeps the wire honest.
        Request::Render { kind, .. } => Response::err(format!(
            "tamias-driver-tamias claims no renderers (kind '{kind}')"
        )),
        // `submit` — this driver is read-only and ships no forms; the
        // wire contract says drivers without forms answer `ok:false`.
        Request::Submit { .. } => {
            Response::err("tamias-driver-tamias is read-only and implements no forms")
        }
        // `jobs`/`notifications` — the manifest declares no
        // `contributes.jobs`/`contributes.notifications` slots, so
        // there is no feed to answer; `ok:false` is the "no feed"
        // convention hosts read.
        Request::Jobs => Response::err("tamias-driver-tamias provides no jobs feed"),
        Request::Notifications => {
            Response::err("tamias-driver-tamias provides no notifications feed")
        }
    }
}

fn serve() -> i32 {
    if kit::serve_stdio(dispatch).is_ok() {
        0
    } else {
        1
    }
}

/// `run`/`cli` verb argv → engine argv (argv[0] included). `tamias
/// bin exec`/`driver exec` prepend the standard driver flags —
/// `--dir <payload>` `--name <pkg>` — which the engine doesn't
/// know; consume the leading run of those two (`--flag v` and
/// `--flag=v` forms) and pass everything else through untouched.
fn engine_argv(args: &[String]) -> Vec<String> {
    let mut out = vec!["tamias".to_string()];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--dir" || a == "--name" {
            // flag + separate value — skip both (a trailing flag
            // with no value just ends the scan).
            i = (i + 2).min(args.len());
        } else if a.starts_with("--dir=") || a.starts_with("--name=") {
            i += 1;
        } else {
            break;
        }
    }
    out.extend(args[i..].iter().cloned());
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rest: Vec<String> = args.by_ref().collect();
    match rest.first().map(String::as_str) {
        Some("manifest") => {
            println!(
                "{}",
                serde_json::to_string_pretty(&manifest_json()).unwrap_or_default()
            );
        }
        Some("capabilities") => {
            println!(
                "{}",
                serde_json::to_string_pretty(&capabilities_json()).unwrap_or_default()
            );
        }
        Some("ops") => {
            println!(
                "{}",
                serde_json::to_string_pretty(&ops::manifest()).unwrap_or_default()
            );
        }
        // `run <tamias argv>` — raw engine passthrough (also the
        // `cli.*` wire op's child): `run list` ≡ `tamias list`.
        Some("run") => {
            let argv = std::iter::once("tamias".to_string()).chain(rest[1..].iter().cloned());
            std::process::exit(tamias::run_from(argv));
        }
        // `cli <argv>` — the bins[] shim verb shape: `tamias bin
        // exec`/`driver exec` prepend `--dir`/`--name`, stripped
        // here, then identical passthrough with inherited stdio.
        Some("cli") => std::process::exit(tamias::run_from(engine_argv(&rest[1..]))),
        Some("serve") => std::process::exit(serve()),
        _ => {
            eprintln!(
                "usage: tamias-driver-tamias manifest|capabilities|ops|run <argv>|cli <argv>|serve"
            );
            std::process::exit(1);
        }
    }
}
