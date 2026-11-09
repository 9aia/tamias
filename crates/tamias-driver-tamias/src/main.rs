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
//!
//! Which tamias home: every `view`/`query` resolves its own roots —
//! `params.home`/`store`/`cache`/`registry`/`drivers` → the same
//! `$TAMIAS_HOME`/`$TAMIAS_CACHE`/`$TAMIAS_REGISTRY`/XDG chain the
//! `tamias` CLI uses (see `data.rs`). `manifest`/`views` are
//! root-free so pavo can enumerate drivers anywhere.
//!
//! The driver is **read-only**: it walks `<store>/…/.tamias-store.json`
//! records, counts cache entries, and scans the driver dir — the
//! same reads `tamias list`/`info`/`doctor` do. `tamias install`/
//! `remove`/`cache clean` stay the write surface.
//!
//! Note: the executable name `tamias-driver-tamias` is also what
//! tamias's content-driver host looks up for a content type named
//! `tamias` — no such type exists today, and this driver's
//! `capabilities` claims no `types`, so only an explicit
//! `type: tamias` package would ever route install verbs here (and
//! they would fail loudly, not silently).

mod data;
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

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
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
        Some("serve") => std::process::exit(serve()),
        _ => {
            eprintln!("usage: tamias-driver-tamias manifest|capabilities|serve");
            std::process::exit(1);
        }
    }
}
