//! views — the driver's manifest: every ViewSpec plus the query
//! resolvers that feed them.
//!
//! Read path: all data comes from [`crate::data::Snapshot`] — one
//! scan of the resolved tamias roots per wire op, through
//! `tamias-store-local`/`tamias-cache-local`/`tamias-registry-local`/
//! `tamias-driver` exactly like the `tamias` CLI verbs. Nothing here
//! proxies the `tamias` binary or `tamias serve`: the reads are
//! in-process dir walks over `.tamias-store.json` records, and
//! keeping the driver a pure reader means it can never mutate the
//! store, cache, or registry.
//!
//! Verb → view mapping:
//!
//! | `tamias` surface                  | pavo view                |
//! |----------------------------------|---------------------------|
//! | `tamias list` / `tamias info`     | `tamias:packages` table   |
//! | `tamias info <name>`              | `tamias:package` (`--param name=`) |
//! | content types + `tamias-driver-*` | `tamias:types` table      |
//! | `tamias verify`                   | `tamias:verify` table     |
//! | `tamias cache clean` stats        | `tamias:cache` kv         |
//! | kind → driver dispatch           | `tamias:graph` graph      |
//! | roots + counts                    | `tamias:status` kv        |

use pavo_driver_kit as kit;
use pavo_protocol::{DriverManifest, ViewNode, ViewSpec};
use serde_json::{json, Map, Value};
use tamias_core::StoreEntry;

use crate::data::Snapshot;

// ---------------------------------------------------------------- helpers
//
// Thin local wrappers over `pavo-driver-kit`'s node constructors —
// they pin this driver's conventions (every declared node is
// query-bound and titled; tables are sortable) in one place.

fn col(title: &str, children: Vec<ViewNode>) -> ViewNode {
    kit::column(kit::base().title(title), children)
}

fn kv(title: &str, query: &str) -> ViewNode {
    kit::kv(kit::base().title(title).query(query), Vec::new())
}

fn table(title: &str, query: &str, columns: &[(&str, &str)]) -> ViewNode {
    kit::sortable_table(
        kit::base().title(title).query(query),
        columns
            .iter()
            .map(|&(key, t)| kit::table_column(key, t))
            .collect(),
        Vec::new(),
    )
}

fn graph(title: &str, query: &str) -> ViewNode {
    kit::graph(
        kit::base().title(title).query(query),
        Vec::new(),
        Vec::new(),
    )
}

fn md(title: &str, text: &str) -> ViewNode {
    kit::markdown(kit::base().title(title), text)
}

fn spec(
    id: &str,
    title: &str,
    route: &str,
    description: &str,
    queries: &[&str],
    root: ViewNode,
) -> ViewSpec {
    let mut view = kit::view(id, title, route, root).description(description);
    for &q in queries {
        // Local binding: op `query`, the binding name is the query
        // name `query_data` dispatches on, no static params.
        view = view.query(kit::binding(q, "query"));
    }
    view.build()
}

// ---------------------------------------------------------------- manifest

/// Every view the driver declares — static (no store state needed;
/// `manifest` must answer anywhere).
pub fn specs() -> Vec<ViewSpec> {
    vec![
        spec(
            "status",
            "Status",
            "/tamias/status",
            "Tamias digest — resolved roots, package/type/driver/cache/registry counts, warnings (`tamias doctor` at a glance).",
            &["summary"],
            col("Tamias status", vec![kv("Tamias", "summary")]),
        ),
        spec(
            "packages",
            "Packages",
            "/tamias/packages",
            "Installed packages — every store entry's record: name, content type, version, source ref, resolved sha, install time (`tamias list` + provenance).",
            &["packages"],
            table(
                "Packages",
                "packages",
                &[
                    ("label", "Label"),
                    ("name", "Name"),
                    ("scope", "Scope"),
                    ("type", "Type"),
                    ("version", "Version"),
                    ("source", "Source ref"),
                    ("resolved", "Resolved"),
                    ("installed", "Installed"),
                    ("digest", "Digest"),
                ],
            ),
        ),
        spec(
            "package",
            "Package",
            "/tamias/package",
            "One package's manifest + full provenance — source ref, resolved sha, payload digest, install time, payload size (`tamias info`). Requires --param name=<name|@scope/name@label>.",
            &["package"],
            col(
                "Package",
                vec![
                    kv("Package", "package"),
                    md(
                        "Provenance",
                        "Every install records its source ref, resolved sha, and payload digest — content provenance is sacred. `verify=full` on tamias:verify recomputes the digest; the default quick check only confirms the record carries one.",
                    ),
                ],
            ),
        ),
        spec(
            "types",
            "Types",
            "/tamias/types",
            "Content types in play — the union of types installed packages declare and types `tamias-driver-*` executables claim (the meta view: which tamias drivers exist).",
            &["types"],
            table(
                "Content types",
                "types",
                &[
                    ("type", "Type"),
                    ("packages", "Packages"),
                    ("drivers", "Drivers"),
                    ("driver_paths", "Driver paths"),
                ],
            ),
        ),
        spec(
            "verify",
            "Verify",
            "/tamias/verify",
            "Digest-check every installed package. Default `quick` mode confirms each record carries a well-formed digest and its payload dir exists; `--param mode=full` recomputes `sha256` over every payload (what `tamias verify` does — expensive on large stores). `--param name=` checks one package.",
            &["verify"],
            table(
                "Verify",
                "verify",
                &[
                    ("label", "Label"),
                    ("name", "Name"),
                    ("recorded", "Recorded digest"),
                    ("status", "Status"),
                    ("detail", "Detail"),
                ],
            ),
        ),
        spec(
            "cache",
            "Cache",
            "/tamias/cache",
            "Fetch-cache stats — root, entry count, and (with --param size=1) total payload bytes. The cache is content-addressed; entries are what `tamias cache clean` evicts.",
            &["cache"],
            col("Tamias cache", vec![kv("Cache", "cache")]),
        ),
        spec(
            "graph",
            "Graph",
            "/tamias/graph",
            "Package → driver dispatch edges — each installed package's content type resolved to the `tamias-driver-<type>` that would handle its install/deploy verbs. Packages whose type has no driver get no edge (e.g. `files`).",
            &["graph"],
            col(
                "Tamias graph",
                vec![
                    graph("Dispatch", "graph"),
                    md(
                        "Note",
                        "Edges are the content-type dispatch tamias performs: `install`/`deploy`/`uninstall` verbs route to `tamias-driver-<kind>` found on `PATH` or in `<tamias-home>/drivers/`. Deps/includes live in project manifests (`tamias.yaml`), not store records, so dependency edges are out of scope here.",
                    ),
                ],
            ),
        ),
    ]
}

/// The `manifest` op result — built with `pavo-driver-kit`'s
/// [`kit::Manifest`] (defaults already match: `verbs`
/// `["manifest","serve"]`, `capabilities` `["pavo"]`).
pub fn manifest() -> DriverManifest {
    let mut m = kit::Manifest::new("tamias", "Tamias")
        .version(env!("CARGO_PKG_VERSION"))
        .icon("🐿");
    for spec in specs() {
        m = m.view(spec);
    }
    // `priority` is the only non-default field each contribution
    // sets — the kit's `contribution` leaves the public fields
    // mutable for exactly this.
    let mut navbar = kit::contribution("packages", "Tamias", "/tamias/packages");
    navbar.priority = 30;
    let mut dashboard = kit::contribution("status", "Tamias status", "/tamias/status");
    dashboard.priority = 40;
    let mut statusbar = kit::contribution("packages", "Tamias packages", "/tamias/packages");
    statusbar.priority = 50;
    m.navbar(navbar)
        .dashboard(dashboard)
        .statusbar(statusbar)
        .build()
}

// ---------------------------------------------------------------- queries

/// Every query name the driver answers — the `query` op rejects
/// anything outside this set with `ok:false`.
pub fn known_query(name: &str) -> bool {
    matches!(
        name,
        "summary" | "packages" | "package" | "types" | "verify" | "cache" | "graph"
    )
}

/// `{"op":"query","query":name,"params":{…}}` → the data payload.
/// Unknown names return an `{"error": …}` object (defensive — the
/// wire layer rejects them before this runs).
pub fn query_data(name: &str, params: &Map<String, Value>, snap: &Snapshot) -> Value {
    if !snap.ok() {
        return snap.error_payload();
    }
    match name {
        "summary" => summary(snap),
        "packages" => packages_rows(snap),
        "package" => package_detail(snap, params),
        "types" => types_rows(snap),
        "verify" => verify_rows(snap, params),
        "cache" => cache_payload(snap, params),
        "graph" => graph_payload(snap),
        other => json!({"error": format!("unknown query '{other}'")}),
    }
}

// ---------------------------------------------------------------- shared

/// Unix seconds → `YYYY-MM-DD HH:MM:SS` UTC — no chrono dep.
fn fmt_ts(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    if mo <= 2 {
        y += 1;
    }
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// Short digest — `sha256:abcdef…` → `abcdef123456`.
fn short_digest(digest: &str) -> String {
    let s = digest.strip_prefix("sha256:").unwrap_or(digest);
    s.chars().take(12).collect()
}

/// Optional string param.
fn param(params: &Map<String, Value>, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn kind_of(entry: &StoreEntry) -> &str {
    entry.record.kind.as_deref().unwrap_or("files")
}

// ---------------------------------------------------------------- queries

/// `summary` query — the kv object: roots + counts + warnings.
fn summary(snap: &Snapshot) -> Value {
    let mut out = Map::new();
    out.insert("home".into(), json!(snap.roots.home.display().to_string()));
    out.insert(
        "store".into(),
        json!(snap.roots.store.display().to_string()),
    );
    out.insert(
        "cache".into(),
        json!(snap.roots.cache.display().to_string()),
    );
    out.insert(
        "registry".into(),
        json!(snap.roots.registry.display().to_string()),
    );
    out.insert(
        "drivers_dir".into(),
        json!(snap.roots.drivers.display().to_string()),
    );
    out.insert("roots_source".into(), json!(snap.roots.source));
    out.insert("packages".into(), json!(snap.packages.len()));
    let mut types: Vec<&str> = snap.packages.iter().map(kind_of).collect();
    types.sort_unstable();
    types.dedup();
    out.insert("content_types".into(), json!(types));
    out.insert("drivers".into(), json!(snap.drivers.len()));
    out.insert("cache_entries".into(), json!(snap.cache_entries));
    out.insert("registry_entries".into(), json!(snap.registry.len()));
    out.insert("warnings".into(), json!(snap.warnings));
    Value::Object(out)
}

/// `packages` query — one row per store entry (`tamias list` +
/// provenance columns).
fn packages_rows(snap: &Snapshot) -> Value {
    Value::Array(
        snap.packages
            .iter()
            .map(|e| {
                let r = &e.record;
                json!({
                    "label": e.label(),
                    "name": r.name,
                    "scope": r.scope,
                    "type": kind_of(e),
                    "version": r.version,
                    "source": r.source_ref,
                    "resolved": r.resolved,
                    "installed": fmt_ts(r.installed_at),
                    "digest": short_digest(&r.digest),
                })
            })
            .collect(),
    )
}

/// `package` query — one entry's manifest + provenance as a kv
/// object (`tamias info`). Param `name` — exact label, `@scope/name`,
/// or bare name (ambiguity errors surface as `{"error": …}`).
fn package_detail(snap: &Snapshot, params: &Map<String, Value>) -> Value {
    let Some(name) = param(params, "name") else {
        return json!({"error": "missing required param 'name' (e.g. --param name=myco-config)"});
    };
    // `Store::find` handles exact labels, bare names, and the
    // several-installs ambiguity error — same lookup `tamias info`.
    let store = tamias_store_local::Store::new(&snap.roots.store);
    let entry = match store.find(&name) {
        Ok(e) => e,
        Err(err) => return json!({"error": err.to_string()}),
    };
    let r = &entry.record;
    let mut out = Map::new();
    out.insert("label".into(), json!(entry.label()));
    out.insert("name".into(), json!(r.name));
    if let Some(scope) = &r.scope {
        out.insert("scope".into(), json!(scope));
    }
    out.insert(
        "version".into(),
        json!(r.version.clone().unwrap_or_else(|| "-".into())),
    );
    out.insert("type".into(), json!(kind_of(&entry)));
    out.insert("source_ref".into(), json!(r.source_ref));
    out.insert("resolved".into(), json!(r.resolved));
    out.insert("digest".into(), json!(r.digest));
    out.insert("installed_at".into(), json!(fmt_ts(r.installed_at)));
    out.insert("dir".into(), json!(entry.dir.display().to_string()));
    match entry.size() {
        Ok(size) => {
            out.insert("size_bytes".into(), json!(size));
        }
        Err(e) => {
            out.insert("size_error".into(), json!(e.to_string()));
        }
    }
    // Payload manifest — description/include/deps when present.
    match entry.manifest() {
        Ok(m) => {
            if let Some(d) = &m.description {
                out.insert("description".into(), json!(d));
            }
            if !m.include.is_empty() {
                out.insert("include".into(), json!(m.include));
            }
            if !m.deps.is_empty() {
                out.insert("deps".into(), json!(m.deps));
            }
        }
        Err(e) => {
            out.insert("manifest_error".into(), json!(e.to_string()));
        }
    }
    Value::Object(out)
}

/// The types a driver executable claims — filename kind plus the
/// `capabilities` `types` array (same probe `tamias-driver`'s
/// `driver_handles` runs). Probe failures degrade to filename-only.
fn driver_claims(path: &std::path::Path, filename_kind: &str) -> Vec<String> {
    let mut kinds = vec![filename_kind.to_string()];
    let Ok(out) = std::process::Command::new(path)
        .arg("capabilities")
        .output()
    else {
        return kinds;
    };
    if !out.status.success() {
        return kinds;
    }
    let Ok(value) = serde_json::from_slice::<Value>(&out.stdout) else {
        return kinds;
    };
    if let Some(types) = value.get("types").and_then(Value::as_array) {
        for t in types.iter().filter_map(Value::as_str) {
            if !kinds.iter().any(|k| k == t) {
                kinds.push(t.to_string());
            }
        }
    }
    kinds
}

/// `types` query — content types in play: every kind installed
/// packages declare ∪ every kind a discovered driver claims.
fn types_rows(snap: &Snapshot) -> Value {
    // type → (package count, driver paths claiming it)
    let mut table: std::collections::BTreeMap<String, (usize, Vec<String>)> =
        std::collections::BTreeMap::new();
    for e in &snap.packages {
        table.entry(kind_of(e).to_string()).or_default().0 += 1;
    }
    for (kind, path) in &snap.drivers {
        for claimed in driver_claims(path, kind) {
            table
                .entry(claimed)
                .or_default()
                .1
                .push(path.display().to_string());
        }
    }
    // Drivers claiming nothing still show under their filename kind.
    Value::Array(
        table
            .into_iter()
            .map(|(t, (packages, drivers))| {
                json!({
                    "type": t,
                    "packages": packages,
                    "drivers": drivers.len(),
                    "driver_paths": drivers,
                })
            })
            .collect(),
    )
}

/// `verify` query — per-package digest status.
///
/// `mode=quick` (default): the record carries a `sha256:` digest and
/// the payload dir still exists — O(1) per entry, what a status
/// surface can afford. `mode=full`: [`Store::verify`] recomputes
/// `dir_digest` over every payload file — identical to
/// `tamias verify <name>`; expensive on large stores, opt-in via
/// `--param mode=full`. `name=` limits the check to one package.
fn verify_rows(snap: &Snapshot, params: &Map<String, Value>) -> Value {
    let full = param(params, "mode").is_some_and(|m| m == "full" || m == "true");
    let only = param(params, "name");
    let store = tamias_store_local::Store::new(&snap.roots.store);
    let mut rows = Vec::new();
    for e in &snap.packages {
        if let Some(n) = &only {
            if e.label() != *n && e.record.name != *n {
                continue;
            }
        }
        let recorded = e.record.digest.clone();
        let well_formed = recorded.starts_with("sha256:") && recorded.len() > 7;
        let dir_exists = e.dir.is_dir();
        let (status, detail) = if full {
            match store.verify(&e.label()) {
                Ok(report) => {
                    if report.ok {
                        (
                            "ok".to_string(),
                            format!("recomputed {}", short_digest(&report.computed)),
                        )
                    } else {
                        (
                            "mismatch".to_string(),
                            format!(
                                "recorded {} computed {}",
                                short_digest(&report.recorded),
                                short_digest(&report.computed)
                            ),
                        )
                    }
                }
                Err(err) => ("error".to_string(), err.to_string()),
            }
        } else if !well_formed {
            (
                "no-digest".to_string(),
                "record carries no sha256 digest".to_string(),
            )
        } else if !dir_exists {
            (
                "missing-dir".to_string(),
                format!("payload dir gone: {}", e.dir.display()),
            )
        } else {
            (
                "recorded".to_string(),
                "digest present; payload dir exists".to_string(),
            )
        };
        rows.push(json!({
            "label": e.label(),
            "name": e.record.name,
            "recorded": short_digest(&recorded),
            "status": status,
            "detail": detail,
        }));
    }
    if let Some(n) = &only {
        if rows.is_empty() {
            rows.push(json!({
                "label": n,
                "name": n,
                "recorded": "",
                "status": "not-installed",
                "detail": "no store entry matches",
            }));
        }
    }
    Value::Array(rows)
}

/// `cache` query — fetch-cache kv object. Bytes only when the
/// snapshot computed them (`size`/`detail` param on the op).
fn cache_payload(snap: &Snapshot, _params: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert("root".into(), json!(snap.roots.cache.display().to_string()));
    out.insert("entries".into(), json!(snap.cache_entries));
    match snap.cache_bytes {
        Some(bytes) => {
            out.insert("bytes".into(), json!(bytes));
        }
        None => {
            out.insert(
                "bytes".into(),
                json!("not computed — pass size=1 to total payload bytes"),
            );
        }
    }
    out.insert(
        "note".into(),
        json!("content-addressed fetch cache — `tamias cache clean [--all]` evicts; entries are immutable blobs"),
    );
    Value::Object(out)
}

/// `graph` query — `{"nodes":[…],"edges":[…]}`: installed packages
/// and discovered drivers, with an edge wherever a driver claims the
/// package's content type (the dispatch `tamias install` performs).
fn graph_payload(snap: &Snapshot) -> Value {
    let mut nodes: Vec<Value> = Vec::new();
    let mut edges: Vec<Value> = Vec::new();
    // Driver nodes — one per discovered executable; claims = filename
    // kind + capabilities.types (same probe as the types view).
    let mut driver_claims_map: Vec<(String, Vec<String>)> = Vec::new();
    for (kind, path) in &snap.drivers {
        let claims = driver_claims(path, kind);
        driver_claims_map.push((kind.clone(), claims.clone()));
        nodes.push(json!({
            "id": format!("drv:{kind}"),
            "label": format!("tamias-driver-{kind}"),
            "kind": "driver",
            "props": {"path": path.display().to_string(), "claims": claims},
        }));
    }
    for e in &snap.packages {
        let pkg_kind = kind_of(e).to_string();
        nodes.push(json!({
            "id": format!("pkg:{}", e.label()),
            "label": e.label(),
            "kind": "package",
            "props": {"type": pkg_kind, "source_ref": e.record.source_ref},
        }));
        for (dkind, claims) in &driver_claims_map {
            if claims.contains(&pkg_kind) {
                edges.push(json!({
                    "from": format!("pkg:{}", e.label()),
                    "to": format!("drv:{dkind}"),
                    "label": pkg_kind,
                }));
            }
        }
    }
    json!({"nodes": nodes, "edges": edges})
}

// ---------------------------------------------------------------- view op

/// `{"op":"view","view":id,"params":{…}}` → [`pavo_protocol::wire::ViewResult`].
pub fn view_result(
    view_id: &str,
    params: &Map<String, Value>,
) -> Result<pavo_protocol::wire::ViewResult, String> {
    let spec = specs()
        .into_iter()
        .find(|v| v.id == view_id)
        .ok_or_else(|| format!("unknown view: {view_id}"))?;

    let snap = crate::data::load(params);
    let mut data = Map::new();
    for b in &spec.queries {
        data.insert(b.name.clone(), query_data(&b.name, params, &snap));
    }

    Ok(pavo_protocol::wire::ViewResult { view: spec, data })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::Map;

    #[test]
    fn manifest_advertises_pavo_and_lists_every_view() {
        let m = manifest();
        assert!(m.verbs.iter().any(|v| v == "serve"));
        assert!(m.capabilities.iter().any(|c| c == "pavo"));
        let ids: std::collections::BTreeSet<&str> = m.views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids.len(), m.views.len(), "duplicate view ids");
        for want in [
            "status", "packages", "package", "types", "verify", "cache", "graph",
        ] {
            assert!(ids.contains(want), "missing view {want}");
        }
        assert!(m
            .contributes
            .navbar
            .iter()
            .any(|c| c.route == "/tamias/packages"));
        assert!(m
            .contributes
            .dashboard
            .iter()
            .any(|c| c.route == "/tamias/status"));
        assert!(m
            .contributes
            .statusbar
            .iter()
            .any(|c| c.route == "/tamias/packages"));
    }

    #[test]
    fn every_binding_is_a_known_query() {
        for v in specs() {
            for b in &v.queries {
                assert!(known_query(&b.name), "{}: unknown query {}", v.id, b.name);
            }
        }
    }

    #[test]
    fn fmt_ts_renders_unix_utc() {
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00");
        assert_eq!(fmt_ts(1_700_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn short_digest_strips_prefix() {
        assert_eq!(short_digest("sha256:0123456789abcdef"), "0123456789ab");
        assert_eq!(short_digest("deadbeef"), "deadbeef");
    }

    #[test]
    fn package_detail_requires_name() {
        let snap = Snapshot {
            error: None,
            ..Snapshot::default()
        };
        let v = package_detail(&snap, &Map::new());
        assert!(v["error"].as_str().unwrap().contains("name"));
        let v2 = package_detail(
            &snap,
            &Map::from_iter([("name".to_string(), Value::String("ghost".into()))]),
        );
        assert!(v2["error"].as_str().unwrap().contains("not installed"));
    }

    #[test]
    fn graph_payload_shape() {
        let snap = Snapshot::default();
        let v = graph_payload(&snap);
        assert!(v["nodes"].as_array().unwrap().is_empty());
        assert!(v["edges"].as_array().unwrap().is_empty());
    }

    #[test]
    fn verify_quick_marks_missing_digest() {
        use tamias_core::StoreRecord;
        let dir = tempfile::tempdir().unwrap();
        let entry = StoreEntry {
            dir: dir.path().to_path_buf(),
            record: StoreRecord {
                name: "w".into(),
                scope: None,
                version: Some("1.0.0".into()),
                kind: None,
                source_ref: "./x".into(),
                resolved: "sha256:abc".into(),
                digest: String::new(),
                installed_at: 0,
            },
        };
        let snap = Snapshot {
            packages: vec![entry],
            ..Snapshot::default()
        };
        let rows = verify_rows(&snap, &Map::new());
        assert_eq!(rows[0]["status"], "no-digest");
        let full = verify_rows(
            &snap,
            &Map::from_iter([("mode".to_string(), json!("full"))]),
        );
        // dir exists but is empty → digest of empty tree won't match ""
        assert!(full[0]["status"].as_str().unwrap() != "ok" || full[0]["status"] == "error");
    }
}
