//! data — one read of tamias state per wire op.
//!
//! The driver is a *reader*: every `view`/`query` resolves the
//! tamias roots, lists the install store's `.tamias-store.json`
//! records, scans `<tamias-home>/drivers/` + `PATH` for
//! `tamias-driver-*` executables, and counts fetch-cache entries —
//! the same on-disk layout `tamias list`/`tamias info`/
//! `tamias doctor` read. No mutations: `tamias install`/`remove`/
//! `cache clean` stay the write surface; this crate only needs what
//! those verbs already show.
//!
//! Root resolution (first hit wins) — mirrors the `tamias` CLI's
//! `store_root()`/`cache_root()`/`registry_root()` plus the
//! `tamias-driver` crate's `tamias_home()`/`drivers_dir()`:
//!
//! - `params.home` — a tamias-home override: `store=home/store`,
//!   `registry=home/registry`, `drivers=home/drivers`,
//!   `cache=home/cache` (the CLI keeps the fetch cache outside the
//!   data home via `TAMIAS_CACHE`/XDG; under an explicit `home`
//!   param `home/cache` is the driver's convention).
//! - `params.store`/`cache`/`registry`/`drivers` — per-root
//!   overrides that beat `home` for that root only.
//! - env/defaults otherwise: `$TAMIAS_HOME` → `$XDG_DATA_HOME/tamias`
//!   → `~/.local/share/tamias` for home+store+registry+drivers;
//!   `$TAMIAS_CACHE` → `$XDG_CACHE_HOME/tamias/fetch` →
//!   `~/.cache/tamias/fetch` for the cache; `./.tamias/*` is the
//!   no-env fallback for all of them — the store/cache/registry roots
//!   here follow the CLI, and `tamias-driver::tamias_home()` falls
//!   back to `./.tamias` too, so `./.tamias/drivers` is where the
//!   drivers scan lands.
//!
//! When the store can't be read at all, [`Snapshot::error`] carries
//! the reason and every query degrades to an `{"error": …}` payload
//! — the wire stays `ok:true` so hosts render the failure inline.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tamias_cache_local::FetchCache;
use tamias_core::{dir_size, Error, StoreEntry, StoreRecord, RECORD_FILE};
use tamias_driver as tdriver;
use tamias_registry::{Registry, RegistryEntry};
use tamias_registry_local::LocalRegistry;
use tamias_store_local::Store;

/// The resolved filesystem roots one snapshot reads.
#[derive(Clone, Debug)]
pub struct Roots {
    /// `<tamias-home>` — informational (store/cache may live
    /// outside it when env vars say so).
    pub home: PathBuf,
    /// Install store root — `<home>/store` by default.
    pub store: PathBuf,
    /// Fetch-cache root — `$TAMIAS_CACHE`/`~/.cache/tamias/fetch`.
    pub cache: PathBuf,
    /// Registry root — `<home>/registry` by default.
    pub registry: PathBuf,
    /// Driver dir — `<home>/drivers` by default.
    pub drivers: PathBuf,
    /// Where the roots came from, for the status view.
    pub source: String,
}

/// Everything the views need, loaded once per op.
pub struct Snapshot {
    /// Resolved roots.
    pub roots: Roots,
    /// Store entries — `[]` when the store is missing/empty.
    pub packages: Vec<StoreEntry>,
    /// `(kind, path)` driver candidates — PATH dirs first, then the
    /// drivers dir; deduped by kind (same order as
    /// `tamias_driver::discovered_drivers`).
    pub drivers: Vec<(String, PathBuf)>,
    /// Fetch-cache entry count.
    pub cache_entries: usize,
    /// Fetch-cache total bytes — `Some` only when the caller asked
    /// for `size` (walking every entry is not free).
    pub cache_bytes: Option<u64>,
    /// Registry index entries (`index.json`).
    pub registry: Vec<RegistryEntry>,
    /// Per-source load problems — surfaced, never fatal.
    pub warnings: Vec<String>,
    /// Why there is no usable store state at all.
    pub error: Option<String>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            roots: Roots {
                home: PathBuf::from("."),
                store: PathBuf::from("."),
                cache: PathBuf::from("."),
                registry: PathBuf::from("."),
                drivers: PathBuf::from("."),
                source: "default".to_string(),
            },
            packages: Vec::new(),
            drivers: Vec::new(),
            cache_entries: 0,
            cache_bytes: None,
            registry: Vec::new(),
            warnings: Vec::new(),
            error: None,
        }
    }
}

impl Snapshot {
    /// The `{"error": …}` payload every query emits when the
    /// snapshot couldn't load — the degradation marker pavo
    /// renderers already understand.
    pub fn error_payload(&self) -> Value {
        serde_json::json!({"error": self.error.clone().unwrap_or_else(|| "no tamias state".into())})
    }

    /// Is there usable store state?
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

fn param_path(params: &Map<String, Value>, key: &str) -> Option<PathBuf> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// `store_root()` — verbatim port of the CLI's resolution.
fn env_store_root() -> PathBuf {
    std::env::var_os("TAMIAS_HOME")
        .map(|h| PathBuf::from(h).join("store"))
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("tamias/store"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/tamias/store"))
        })
        .unwrap_or_else(|| PathBuf::from(".tamias/store"))
}

/// `cache_root()` — verbatim port of the CLI's resolution.
fn env_cache_root() -> PathBuf {
    std::env::var_os("TAMIAS_CACHE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME").map(|d| PathBuf::from(d).join("tamias/fetch"))
        })
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/tamias/fetch")))
        .unwrap_or_else(|| PathBuf::from(".tamias/fetch"))
}

/// `registry_root()` — verbatim port of the CLI's resolution.
fn env_registry_root() -> PathBuf {
    std::env::var_os("TAMIAS_REGISTRY")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("tamias/registry"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/tamias/registry"))
        })
        .unwrap_or_else(|| PathBuf::from(".tamias/registry"))
}

/// Resolve the roots for one op.
fn roots(params: &Map<String, Value>) -> Roots {
    let home_param = param_path(params, "home");
    let leaf = |key: &str, env: fn() -> PathBuf, under_home: &dyn Fn(&Path) -> PathBuf| {
        param_path(params, key)
            .or_else(|| home_param.as_ref().map(|h| under_home(h)))
            .unwrap_or_else(env)
    };
    let home = home_param.clone().unwrap_or_else(tdriver::tamias_home);
    let overridden = ["home", "store", "cache", "registry", "drivers"]
        .iter()
        .filter(|k| params.get(**k).and_then(Value::as_str).is_some())
        .map(|k| format!("{k}="))
        .collect::<Vec<_>>()
        .join(" ");
    Roots {
        home: home.clone(),
        store: leaf("store", env_store_root, &|h| h.join("store")),
        cache: leaf("cache", env_cache_root, &|h| h.join("cache")),
        registry: leaf("registry", env_registry_root, &|h| h.join("registry")),
        drivers: leaf("drivers", tdriver::drivers_dir, &|h| h.join("drivers")),
        source: if overridden.is_empty() {
            "env".to_string()
        } else {
            format!("params: {overridden}")
        },
    }
}

/// `tamias_driver::discovered_drivers` against an arbitrary drivers
/// dir — same scan order (every PATH dir, then the drivers dir) and
/// same dedup (first kind wins), so a `home=`/`drivers=` param sees
/// exactly what tamias would.
fn scan_drivers(drivers_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut scan = |dir: &Path| {
        let Some(read) = std::fs::read_dir(dir).ok() else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let kind = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("tamias-driver-"));
            // `file_type` never follows symlinks — a linked driver isn't
            // dispatchable (mirrors register_bundled_drivers).
            let is_file = entry.file_type().is_ok_and(|t| t.is_file());
            if let Some(kind) = kind {
                if is_file && !found.iter().any(|(k, _)| k == kind) {
                    found.push((kind.to_string(), path));
                }
            }
        }
    };
    for dir in std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        scan(&dir);
    }
    scan(drivers_dir);
    found.sort();
    found
}

/// Store entries via the canonical [`Store::entries`]; on a wholesale
/// failure (one corrupt record fails the whole walk), fall back to a
/// per-record scan that skips bad entries into `warnings` — honest
/// degradation beats an empty view.
fn store_entries(root: &Path, warnings: &mut Vec<String>) -> (Vec<StoreEntry>, Option<String>) {
    let store = Store::new(root);
    match store.entries() {
        Ok(entries) => (entries, None),
        Err(e) => {
            warnings.push(format!("store listing failed ({e}) — per-record fallback"));
            match scan_store(root, warnings) {
                Ok(entries) => (entries, None),
                Err(e2) => (Vec::new(), Some(format!("store unreadable: {e2}"))),
            }
        }
    }
}

/// Recursive `.tamias-store.json` walk — same record file and layout
/// as [`Store::entries`], but tolerant: unparsable records are
/// warnings, not fatal.
fn scan_store(root: &Path, warnings: &mut Vec<String>) -> Result<Vec<StoreEntry>, Error> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    scan_dir(root, &mut out, warnings)?;
    out.sort_by_key(StoreEntry::label);
    Ok(out)
}

fn scan_dir(
    dir: &Path,
    out: &mut Vec<StoreEntry>,
    warnings: &mut Vec<String>,
) -> Result<(), Error> {
    let read = std::fs::read_dir(dir).map_err(Error::io(format!("read {}", dir.display())))?;
    for entry in read {
        let entry = entry.map_err(Error::io("read store entry"))?;
        let path = entry.path();
        // `file_type` never follows symlinks — a linked dir can't recurse
        // the store scan.
        let is_dir = entry
            .file_type()
            .map_err(Error::io(format!("stat {}", path.display())))?
            .is_dir();
        if !is_dir {
            continue;
        }
        let record_path = path.join(RECORD_FILE);
        // no-follow: a linked record file isn't trusted provenance
        let is_file = std::fs::symlink_metadata(&record_path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        if is_file {
            let text = std::fs::read_to_string(&record_path)
                .map_err(Error::io(format!("read {}", record_path.display())))?;
            match serde_json::from_str::<StoreRecord>(&text) {
                Ok(record) => out.push(StoreEntry { dir: path, record }),
                Err(e) => warnings.push(format!("skipping {}: {e}", record_path.display())),
            }
        } else {
            scan_dir(&path, out, warnings)?;
        }
    }
    Ok(())
}

/// Cache stats — entry count always; total bytes only on request
/// (`size`/`detail` param), since it walks every cached payload.
fn cache_stats(root: &Path, want_bytes: bool, warnings: &mut Vec<String>) -> (usize, Option<u64>) {
    let cache = FetchCache::new(root);
    match cache.entries() {
        Ok(entries) => {
            let bytes = if want_bytes {
                let mut total = 0u64;
                for e in &entries {
                    match dir_size(e) {
                        Ok(n) => total += n,
                        Err(err) => warnings.push(format!("cache size {}: {err}", e.display())),
                    }
                }
                Some(total)
            } else {
                None
            };
            (entries.len(), bytes)
        }
        Err(e) => {
            warnings.push(format!("cache listing failed: {e}"));
            (0, None)
        }
    }
}

/// Registry index entries; missing `index.json` is `[]`, a parse
/// failure is a warning.
fn registry_entries(root: &Path, warnings: &mut Vec<String>) -> Vec<RegistryEntry> {
    match LocalRegistry::new(root).entries() {
        Ok(entries) => entries,
        Err(e) => {
            warnings.push(format!("registry index: {e}"));
            Vec::new()
        }
    }
}

/// Truthy param (`1`, `true`, `yes`, `on`, or a nonzero number —
/// `--param size=1` arrives as JSON `1`, not a string).
fn flag(params: &Map<String, Value>, key: &str) -> bool {
    match params.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "yes" | "on"),
        Some(v) => v.as_f64().is_some_and(|n| n != 0.0),
        None => false,
    }
}

/// Resolve + load. `params` are the wire op's caller params —
/// `home`/`store`/`cache`/`registry`/`drivers` steer resolution,
/// `size` asks for cache byte totals; the rest pass through to the
/// query resolvers untouched.
pub fn load(params: &Map<String, Value>) -> Snapshot {
    let roots = roots(params);
    let mut warnings = Vec::new();

    let want_bytes = flag(params, "size") || flag(params, "detail");
    let (cache_entries, cache_bytes) = cache_stats(&roots.cache, want_bytes, &mut warnings);
    let registry = registry_entries(&roots.registry, &mut warnings);
    let drivers = scan_drivers(&roots.drivers);
    let (packages, error) = store_entries(&roots.store, &mut warnings);

    Snapshot {
        roots,
        packages,
        drivers,
        cache_entries,
        cache_bytes,
        registry,
        warnings,
        error,
    }
}
