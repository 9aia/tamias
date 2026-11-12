//! Deploy overrides — the durable operator-config persistence channel
//! (T-026).
//!
//! Kind drivers regenerate operator config on every `deploy`/`install`
//! (e.g. `~/.config/peregrin/serve/<name>.yaml`), wiping hand-applied
//! edits. tamias owns the *storage* side of the fix; the driver owns
//! the merge (it alone knows the generated document's shape):
//!
//! - Drop-in dir per installed package:
//!   `<config>/tamias/deploy.d/<kind>/<name>/` — `<config>` is
//!   `$XDG_CONFIG_HOME/tamias` (`~/.config/tamias` fallback), or
//!   `$TAMIAS_DEPLOY_D` when set. Every `*.yaml`/`*.yml`/`*.json` file
//!   in it is a partial document, merged in sorted-filename order —
//!   later files win — with [`tamias_core::merge_values`] semantics
//!   (the same deep-merge `include:` overlays use: dicts per-key,
//!   arrays concat high-first with `id` dedupe).
//! - `tamias deploy <pkg> --set key=value` (repeatable) edits the
//!   `cli.yaml` fragment inside that dir — dotted keys build nested
//!   maps (`a.b.c=v`), values parse as YAML (`--set 'nodes=[1,2]'`,
//!   `--set 'x={k: v}'`); `--unset key` removes a path,
//!   `--clear-overrides` drops the package's whole dir. Because
//!   `cli.yaml` is just one fragment, hand-written files sorting after
//!   it (`zz-*.yaml`) still win over `--set` values.
//! - At dispatch time [`dispatch`](crate::dispatch) hands
//!   the driver the drop-in dir via [`OVERRIDES_ENV`] whenever it
//!   exists — every lifecycle verb (`install`/`deploy`/`uninstall`)
//!   and `driver exec` passthrough, so CLI, `tamias serve`, sync, and
//!   the TUI all carry it. Drivers that generate config merge the
//!   fragments over the generated doc before writing; drivers that
//!   don't simply ignore the variable.
//!
//! The `<kind>/<name>` key is exactly what the driver sees on argv
//! (`--name` is the bare record name — scope isn't part of it), so two
//! scopes sharing a name+kind share overrides; that's inherent to the
//! driver contract, not this channel.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tamias_core::{is_valid_kind, is_valid_name, Error};

/// Env var naming the package's deploy-override drop-in dir, injected
/// into every dispatched driver whose dir exists. Drivers merge its
/// fragments over generated config — see the module docs.
pub const OVERRIDES_ENV: &str = "TAMIAS_DEPLOY_OVERRIDES";

/// Env var naming the overrides *root* (the `deploy.d` dir itself) —
/// an escape hatch for tests and nonstandard config homes; overrides
/// `$XDG_CONFIG_HOME`/`$HOME` discovery when set.
pub const OVERRIDES_ROOT_ENV: &str = "TAMIAS_DEPLOY_D";

/// The fragment `tamias deploy --set/--unset` manages inside a package
/// drop-in dir. Sorts mid-pack by filename: fragments named `zz-*`
/// still outrank it.
const CLI_FRAGMENT: &str = "cli.yaml";

/// The overrides root — `$TAMIAS_DEPLOY_D`, else
/// `$XDG_CONFIG_HOME/tamias/deploy.d`, else `~/.config/tamias/deploy.d`.
/// `None` when no home/config dir resolves (same posture as
/// `tamias-registry-open`'s `user_config_dir`).
#[must_use]
pub fn overrides_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(OVERRIDES_ROOT_ENV) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(|d| PathBuf::from(d).join("tamias/deploy.d"))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/tamias/deploy.d"))
        })
}

/// The drop-in dir for one `(kind, name)` under `root` — `None` when
/// either component fails its charset (a hand-built store record can't
/// smuggle `../` into the join; dispatch then simply injects nothing).
#[must_use]
pub fn package_dir(root: &Path, kind: &str, name: &str) -> Option<PathBuf> {
    (is_valid_kind(kind) && is_valid_name(name)).then(|| root.join(kind).join(name))
}

/// [`package_dir`] resolved against the ambient [`overrides_root`] —
/// the dispatch-time lookup. `Some` only when the dir exists on disk.
fn ambient_package_dir(kind: &str, name: &str) -> Option<PathBuf> {
    let dir = package_dir(&overrides_root()?, kind, name)?;
    dir.is_dir().then_some(dir)
}

/// The dir to hand a driver via [`OVERRIDES_ENV`], when one exists.
pub(crate) fn env_dir(kind: &str, name: &str) -> Option<PathBuf> {
    ambient_package_dir(kind, name)
}

/// Mergeable fragment formats — same dispatch as `tamias-core`'s
/// overlay doc detection.
#[derive(Clone, Copy)]
enum FragmentFormat {
    Yaml,
    Json,
}

fn fragment_format(path: &Path) -> Option<FragmentFormat> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yaml" | "yml") => Some(FragmentFormat::Yaml),
        Some("json") => Some(FragmentFormat::Json),
        _ => None,
    }
}

fn parse_fragment(path: &Path, format: FragmentFormat) -> Result<Value, Error> {
    let text =
        std::fs::read_to_string(path).map_err(Error::io(format!("read {}", path.display())))?;
    match format {
        FragmentFormat::Yaml => serde_yaml_ng::from_str(&text)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display()))),
        FragmentFormat::Json => serde_json::from_str(&text)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display()))),
    }
}

/// Fold every fragment in `dir` into one document — sorted by
/// filename, later files higher priority, [`tamias_core::merge_values`]
/// per file (deep-merge: dicts per-key, arrays concat high-first with
/// `id` dedupe). `Ok(None)` when the dir is absent or holds no
/// fragments; a malformed fragment is an error, never silently skipped.
///
/// # Errors
/// Propagates read/parse failures.
pub fn merged_overrides(dir: &Path) -> Result<Option<Value>, Error> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("read {}", dir.display()))(e)),
    };
    let mut files: Vec<(String, PathBuf, FragmentFormat)> = Vec::new();
    for entry in read {
        let entry = entry.map_err(Error::io("read override entry"))?;
        let path = entry.path();
        let is_file = entry
            .file_type()
            .map_err(Error::io(format!("stat {}", path.display())))?
            .is_file();
        let format = if is_file {
            fragment_format(&path)
        } else {
            None
        };
        if let Some(format) = format {
            let name = entry.file_name().to_string_lossy().into_owned();
            files.push((name, path, format));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut merged: Option<Value> = None;
    for (_, path, format) in files {
        let frag = parse_fragment(&path, format)?;
        merged = Some(match merged {
            Some(acc) => tamias_core::merge_values(&frag, &acc),
            None => frag,
        });
    }
    Ok(merged)
}

/// Parse one `--set` argument — `key=value` where `key` is a dotted
/// path (`a.b.c`) and `value` parses as YAML (plain scalars, lists,
/// inline maps all work; unparseable values stay literal strings).
///
/// # Errors
/// Fails on a missing `=` or an empty key/path segment.
pub fn parse_set_arg(arg: &str) -> Result<(String, Value), Error> {
    let (key, raw) = arg
        .split_once('=')
        .ok_or_else(|| Error::Failed(format!("--set expects key=value, got {arg:?}")))?;
    validate_key(key)?;
    let value =
        serde_yaml_ng::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    Ok((key.to_string(), value))
}

/// A dotted override path — non-empty `.`-separated segments.
fn validate_key(key: &str) -> Result<(), Error> {
    if key.split('.').all(|s| !s.is_empty()) && !key.is_empty() {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "invalid override key {key:?} — expected dotted path like a.b.c"
        )))
    }
}

/// Set `key` (dotted path) to `value` inside `doc`, creating
/// intermediate maps (or arrays when the next segment is numeric).
/// Returns the fragment path written.
///
/// # Errors
/// Fails on an invalid key or when the path can't be descended (a
/// scalar/array in the way).
pub fn set_override(
    root: &Path,
    kind: &str,
    name: &str,
    key: &str,
    value: Value,
) -> Result<PathBuf, Error> {
    let dir = package_dir(root, kind, name)
        .ok_or_else(|| Error::Failed(format!("invalid package coordinates {kind:?}/{name:?}")))?;
    std::fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let frag_path = dir.join(CLI_FRAGMENT);
    let mut doc = if frag_path.is_file() {
        let text = std::fs::read_to_string(&frag_path)
            .map_err(Error::io(format!("read {}", frag_path.display())))?;
        serde_yaml_ng::from_str::<Value>(&text)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", frag_path.display())))?
    } else {
        Value::Object(Map::new())
    };
    if !doc.is_object() {
        doc = Value::Object(Map::new());
    }
    set_path(&mut doc, &key.split('.').collect::<Vec<_>>(), value)?;
    write_fragment(&frag_path, &doc)?;
    Ok(frag_path)
}

/// Remove `key` (dotted path) from the package's `cli.yaml` fragment.
/// `Ok(true)` when something was removed; the file is deleted when it
/// empties out.
///
/// # Errors
/// Fails on an invalid key or an unreadable/corrupt fragment.
pub fn unset_override(root: &Path, kind: &str, name: &str, key: &str) -> Result<bool, Error> {
    validate_key(key)?;
    let Some(dir) = package_dir(root, kind, name) else {
        return Err(Error::Failed(format!(
            "invalid package coordinates {kind:?}/{name:?}"
        )));
    };
    let frag_path = dir.join(CLI_FRAGMENT);
    if !frag_path.is_file() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&frag_path)
        .map_err(Error::io(format!("read {}", frag_path.display())))?;
    let mut doc = serde_yaml_ng::from_str::<Value>(&text)
        .map_err(|e| Error::Failed(format!("parse {}: {e}", frag_path.display())))?;
    let removed = unset_path(&mut doc, &key.split('.').collect::<Vec<_>>())?;
    if doc.as_object().is_some_and(Map::is_empty) || doc.is_null() {
        let _ = std::fs::remove_file(&frag_path);
    } else {
        write_fragment(&frag_path, &doc)?;
    }
    Ok(removed)
}

/// Drop the package's whole drop-in dir — every fragment, `cli.yaml`
/// included. `Ok(true)` when a dir was removed.
///
/// # Errors
/// Propagates removal failures.
pub fn clear_overrides(root: &Path, kind: &str, name: &str) -> Result<bool, Error> {
    let Some(dir) = package_dir(root, kind, name) else {
        return Err(Error::Failed(format!(
            "invalid package coordinates {kind:?}/{name:?}"
        )));
    };
    if !dir.is_dir() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).map_err(Error::io(format!("remove {}", dir.display())))?;
    Ok(true)
}

/// The merged persisted overrides for one package — what `--show`
/// prints. `Ok(None)` when nothing is persisted.
///
/// # Errors
/// Propagates [`merged_overrides`] failures.
pub fn package_overrides(root: &Path, kind: &str, name: &str) -> Result<Option<Value>, Error> {
    let Some(dir) = package_dir(root, kind, name) else {
        return Err(Error::Failed(format!(
            "invalid package coordinates {kind:?}/{name:?}"
        )));
    };
    merged_overrides(&dir)
}

fn write_fragment(path: &Path, doc: &Value) -> Result<(), Error> {
    let text = serde_yaml_ng::to_string(doc)
        .map_err(|e| Error::Failed(format!("serialize {}: {e}", path.display())))?;
    tamias_core::write_atomic(path, text.as_bytes())
}

/// Container for a not-yet-created child: array when the next segment
/// is a numeric index, map otherwise.
fn new_container(next: Option<&str>) -> Value {
    match next {
        Some(seg) if seg.parse::<usize>().is_ok() => Value::Array(Vec::new()),
        _ => Value::Object(Map::new()),
    }
}

fn set_path(doc: &mut Value, segs: &[&str], value: Value) -> Result<(), Error> {
    let Some((seg, rest)) = segs.split_first() else {
        return Ok(());
    };
    if rest.is_empty() {
        return match doc {
            Value::Object(map) => {
                map.insert((*seg).to_string(), value);
                Ok(())
            }
            Value::Array(items) => {
                let idx = seg
                    .parse::<usize>()
                    .map_err(|_| Error::Failed(format!("array index expected, got {seg:?}")))?;
                if items.len() <= idx {
                    items.resize(idx + 1, Value::Null);
                }
                items[idx] = value;
                Ok(())
            }
            _ => Err(Error::Failed(format!(
                "cannot set {seg:?} — path crosses a scalar"
            ))),
        };
    }
    match doc {
        Value::Object(map) => {
            let child = map
                .entry((*seg).to_string())
                .or_insert_with(|| new_container(rest.first().copied()));
            // A scalar in the way gets replaced only when it's null —
            // overwriting a real value would silently drop operator data.
            if child.is_null() {
                *child = new_container(rest.first().copied());
            }
            set_path(child, rest, value)
        }
        Value::Array(items) => {
            let idx = seg
                .parse::<usize>()
                .map_err(|_| Error::Failed(format!("array index expected, got {seg:?}")))?;
            if items.len() <= idx {
                items.resize(idx + 1, Value::Null);
            }
            let child = &mut items[idx];
            if child.is_null() {
                *child = new_container(rest.first().copied());
            }
            set_path(child, rest, value)
        }
        _ => Err(Error::Failed(format!(
            "cannot descend into {seg:?} — path crosses a scalar"
        ))),
    }
}

fn unset_path(doc: &mut Value, segs: &[&str]) -> Result<bool, Error> {
    let Some((seg, rest)) = segs.split_first() else {
        return Ok(false);
    };
    if rest.is_empty() {
        return match doc {
            Value::Object(map) => Ok(map.remove(*seg).is_some()),
            Value::Array(items) => match seg.parse::<usize>() {
                Ok(idx) if idx < items.len() => {
                    items.remove(idx);
                    Ok(true)
                }
                _ => Ok(false),
            },
            _ => Ok(false),
        };
    }
    match doc {
        Value::Object(map) => match map.get_mut(*seg) {
            Some(child) => unset_path(child, rest),
            None => Ok(false),
        },
        Value::Array(items) => match seg.parse::<usize>() {
            Ok(idx) if idx < items.len() => unset_path(&mut items[idx], rest),
            _ => Ok(false),
        },
        _ => Ok(false),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parse_set_arg_scalars_and_structures() {
        let (k, v) = parse_set_arg("config.public_url=https://x").unwrap();
        assert_eq!(k, "config.public_url");
        assert_eq!(v, Value::String("https://x".into()));
        let (_, v) = parse_set_arg("n=42").unwrap();
        assert_eq!(v, Value::from(42));
        let (_, v) = parse_set_arg("repos=[a, b]").unwrap();
        assert_eq!(v, serde_json::json!(["a", "b"]));
        let (_, v) = parse_set_arg("x={k: v}").unwrap();
        assert_eq!(v["k"], "v");
        // '=' inside the value is fine — split at the first one
        let (_, v) = parse_set_arg("q=a=b").unwrap();
        assert_eq!(v, Value::String("a=b".into()));
        assert!(parse_set_arg("noequals").is_err());
        assert!(parse_set_arg(".a=1").is_err());
        assert!(parse_set_arg("a..b=1").is_err());
    }

    #[test]
    fn set_unset_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let frag = set_override(
            root,
            "peregrin-bundle",
            "pr-heal",
            "config.repos.0.name",
            Value::from("o/r"),
        )
        .unwrap();
        assert!(frag.ends_with("cli.yaml"));
        // second set deep-merges into the same fragment
        set_override(
            root,
            "peregrin-bundle",
            "pr-heal",
            "config.repos.0.secret_env",
            Value::from("WH"),
        )
        .unwrap();
        let merged = package_overrides(root, "peregrin-bundle", "pr-heal")
            .unwrap()
            .unwrap();
        assert_eq!(merged["config"]["repos"][0]["name"], "o/r");
        assert_eq!(merged["config"]["repos"][0]["secret_env"], "WH");
        assert!(unset_override(
            root,
            "peregrin-bundle",
            "pr-heal",
            "config.repos.0.secret_env"
        )
        .unwrap());
        let merged = package_overrides(root, "peregrin-bundle", "pr-heal")
            .unwrap()
            .unwrap();
        assert!(merged["config"]["repos"][0].get("secret_env").is_none());
        // unwinding the rest leaves an empty doc → the fragment file is removed
        for key in [
            "config.repos.0.name",
            "config.repos.0",
            "config.repos",
            "config",
        ] {
            assert!(unset_override(root, "peregrin-bundle", "pr-heal", key).unwrap());
        }
        assert!(!frag.exists());
        assert!(!unset_override(root, "peregrin-bundle", "pr-heal", "config").unwrap());
    }

    #[test]
    fn merged_overrides_orders_and_deep_merges() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("00-base.yaml"),
            "a: 1\nlist: [x]\nnodes:\n  - id: g\n    v: 0\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("zz-ops.yaml"),
            "b: 2\nlist: [y]\nnodes:\n  - id: g\n    v: 9\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let merged = merged_overrides(dir).unwrap().unwrap();
        assert_eq!(merged["a"], 1);
        assert_eq!(merged["b"], 2);
        // later file's array entries come first
        assert_eq!(merged["list"], serde_json::json!(["y", "x"]));
        // node dedupe by id — the later file's definition wins
        assert_eq!(merged["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(merged["nodes"][0]["v"], 9);
        assert!(merged_overrides(&tmp.path().join("missing"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn clear_and_invalid_coords() {
        let tmp = tempfile::tempdir().unwrap();
        set_override(tmp.path(), "k", "n", "a.b", Value::from(1)).unwrap();
        assert!(clear_overrides(tmp.path(), "k", "n").unwrap());
        assert!(!clear_overrides(tmp.path(), "k", "n").unwrap());
        assert!(set_override(tmp.path(), "../x", "n", "a", Value::Null).is_err());
        assert!(set_override(tmp.path(), "k", "../n", "a", Value::Null).is_err());
    }

    #[test]
    fn set_path_descends_scalars_safely() {
        let mut doc = Value::Object(Map::new());
        set_path(&mut doc, &["a", "b"], Value::from(1)).unwrap();
        assert_eq!(doc["a"]["b"], 1);
        // crossing a scalar errors rather than silently dropping data
        let mut doc = serde_json::json!({"a": 5});
        assert!(set_path(&mut doc, &["a", "b"], Value::from(1)).is_err());
    }
}
