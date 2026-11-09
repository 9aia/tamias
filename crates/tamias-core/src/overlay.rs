//! `include:` overlay composition. A payload deep-merges over its bases:
//! local wins, dicts merge per-key, arrays concat local-first. Earlier
//! `include:` entries outrank later ones. Conflicting `.yaml`/`.yml`/
//! `.json` files merge document-wise; other files resolve whole-file with
//! the higher priority winning.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::digest::payload_files;
use crate::{Error, Manifest};

/// The payload root inside a fetched dir — `dir` itself when it holds a
/// manifest, else the single child dir it wraps everything under.
///
/// # Errors
/// Fails when no manifest turns up before the tree widens.
pub fn payload_root(dir: &Path) -> Result<PathBuf, Error> {
    let mut current = dir.to_path_buf();
    loop {
        if Manifest::path_in(&current).is_some() {
            return Ok(current);
        }
        let mut children = Vec::new();
        let read =
            fs::read_dir(&current).map_err(Error::io(format!("read {}", current.display())))?;
        for child in read {
            children.push(child.map_err(Error::io("read dir entry"))?.path());
        }
        if children.len() == 1 {
            let child = children.into_iter().next().unwrap_or_default();
            // `symlink_metadata` never follows links — a lone symlink
            // (even `a -> .`) can't unwrap the root or loop the search.
            if fs::symlink_metadata(&child).is_ok_and(|m| m.is_dir()) {
                current = child;
                continue;
            }
        }
        return Err(Error::Failed(format!(
            "no tamias manifest under {}",
            dir.display()
        )));
    }
}

/// Compose `local` over `bases` into `dest`. `bases` list the `include:`d
/// payload dirs in manifest order — earlier includes win over later ones,
/// `local` wins over everything.
///
/// # Errors
/// Propagates read/write failures while merging layers.
pub fn overlay(bases: &[&Path], local: &Path, dest: &Path) -> Result<(), Error> {
    fs::create_dir_all(dest).map_err(Error::io(format!("create {}", dest.display())))?;
    for base in bases.iter().rev() {
        apply_layer(base, dest)?;
    }
    apply_layer(local, dest)
}

/// Mergeable document format — same relpath on both sides shares it.
#[derive(Clone, Copy)]
enum DocFormat {
    Yaml,
    Json,
}

fn doc_format(path: &Path) -> Option<DocFormat> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yaml" | "yml") => Some(DocFormat::Yaml),
        Some("json") => Some(DocFormat::Json),
        _ => None,
    }
}

fn parse_doc(path: &Path, format: DocFormat) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    match format {
        DocFormat::Yaml => serde_yaml_ng::from_str(&text).ok(),
        DocFormat::Json => serde_json::from_str(&text).ok(),
    }
}

fn write_doc(path: &Path, format: DocFormat, value: &Value) -> Result<(), Error> {
    let text = match format {
        DocFormat::Yaml => serde_yaml_ng::to_string(value)
            .map_err(|e| Error::Failed(format!("serialize {}: {e}", path.display())))?,
        DocFormat::Json => serde_json::to_string_pretty(value)
            .map_err(|e| Error::Failed(format!("serialize {}: {e}", path.display())))?,
    };
    crate::write_atomic(path, text.as_bytes())
}

/// Deep-merge `high` over `low`: dicts merge per-key, arrays concat
/// high-first, everything else takes `high`.
fn merge_documents(high: &Value, low: &Value) -> Value {
    match (high, low) {
        (Value::Object(h), Value::Object(l)) => {
            let mut out = l.clone();
            for (k, hv) in h {
                let merged = match out.get(k) {
                    Some(lv) => merge_documents(hv, lv),
                    None => hv.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (Value::Array(h), Value::Array(l)) => {
            let mut out = h.clone();
            out.extend(l.iter().cloned());
            dedup_by_id(&mut out);
            Value::Array(out)
        }
        _ => high.clone(),
    }
}

/// Dedupe array elements by their string `id` — the higher-priority (earlier)
/// occurrence wins, so a re-declared node replaces its base definition
/// instead of double-firing. Elements without an `id` are left alone.
fn dedup_by_id(items: &mut Vec<Value>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| match item.get("id").and_then(Value::as_str) {
        Some(id) => seen.insert(id.to_string()),
        None => true,
    });
}

/// Lay one priority layer over `dest`. Files that both parse as docs
/// merge; every other conflict takes the layer's bytes.
fn apply_layer(layer: &Path, dest: &Path) -> Result<(), Error> {
    for rel in payload_files(layer)? {
        let src = layer.join(&rel);
        let target = dest.join(&rel);
        if target.is_file() {
            if let (Some(format), Some(incoming), Some(existing)) = (
                doc_format(&rel),
                doc_format(&rel).and_then(|f| parse_doc(&src, f)),
                doc_format(&rel).and_then(|f| parse_doc(&target, f)),
            ) {
                write_doc(&target, format, &merge_documents(&incoming, &existing))?;
                continue;
            }
        }
        let parent = target.parent().unwrap_or(dest);
        fs::create_dir_all(parent).map_err(Error::io(format!("create {}", parent.display())))?;
        fs::copy(&src, &target).map_err(Error::io(format!(
            "copy {} → {}",
            src.display(),
            target.display()
        )))?;
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn merge_semantics() {
        let high = serde_json::json!({"a": 1, "list": [1], "nested": {"x": 2}});
        let low = serde_json::json!({"b": 3, "list": [9], "nested": {"y": 4}});
        let m = merge_documents(&high, &low);
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"], 3);
        assert_eq!(m["list"], serde_json::json!([1, 9]));
        assert_eq!(m["nested"]["x"], 2);
        assert_eq!(m["nested"]["y"], 4);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod overlay_tests {
    use super::*;

    #[test]
    fn payload_root_direct_nested_and_missing() {
        let direct = tempfile::tempdir().unwrap();
        std::fs::write(direct.path().join("tamias.yaml"), "name: x\n").unwrap();
        assert_eq!(payload_root(direct.path()).unwrap(), direct.path());

        let nested = tempfile::tempdir().unwrap();
        let inner = nested.path().join("pkg-1.0");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("tamias.yaml"), "name: x\n").unwrap();
        assert_eq!(payload_root(nested.path()).unwrap(), inner);

        let missing = tempfile::tempdir().unwrap();
        std::fs::write(missing.path().join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(missing.path().join("b")).unwrap();
        assert!(payload_root(missing.path()).is_err());
    }

    #[test]
    fn overlay_merges_yaml_and_json_and_copies() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(
            base.path().join("main.yaml"),
            "a: 1\nnodes:\n  - id: g\n    x: 0\n",
        )
        .unwrap();
        std::fs::write(base.path().join("only-base.txt"), "b").unwrap();
        std::fs::write(base.path().join("c.json"), "{\"a\": 1}").unwrap();

        let local = tempfile::tempdir().unwrap();
        std::fs::write(
            local.path().join("main.yaml"),
            "b: 2\nnodes:\n  - id: g\n    x: 1\n",
        )
        .unwrap();
        std::fs::write(local.path().join("c.json"), "{\"b\": 2}").unwrap();

        let dest = tempfile::tempdir().unwrap();
        overlay(&[base.path()], local.path(), dest.path()).unwrap();
        let merged = std::fs::read_to_string(dest.path().join("main.yaml")).unwrap();
        assert!(merged.contains("a: 1") && merged.contains("b: 2"));
        // node dedupe by id
        assert_eq!(merged.matches("id: g").count(), 1);
        assert!(dest.path().join("only-base.txt").is_file());
        let cj = std::fs::read_to_string(dest.path().join("c.json")).unwrap();
        assert!(cj.contains('a') && cj.contains('b'));
    }

    #[test]
    fn payload_root_no_manifest_errors() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.txt"), "x").unwrap();
        assert!(payload_root(d.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn payload_root_never_follows_symlinks() {
        // a lone symlink — even one pointing at a real payload dir —
        // must not unwrap the root (or loop on `a -> .`)
        let real = tempfile::tempdir().unwrap();
        std::fs::write(real.path().join("tamias.yaml"), "name: x\n").unwrap();
        let wrapped = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(real.path(), wrapped.path().join("link")).unwrap();
        assert!(payload_root(wrapped.path()).is_err());

        let looping = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(".", looping.path().join("a")).unwrap();
        assert!(payload_root(looping.path()).is_err());
    }

    #[test]
    fn merge_scalar_and_unknown_ext() {
        assert_eq!(
            merge_documents(&serde_json::json!(1), &serde_json::json!(2)),
            serde_json::json!(1)
        );
        assert!(doc_format(Path::new("x.txt")).is_none());
    }

    #[test]
    fn apply_layer_copy_error() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("x"), "b").unwrap();
        let local = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dest.path().join("x")).unwrap();
        assert!(overlay(&[base.path()], local.path(), dest.path()).is_err());
    }

    #[test]
    fn apply_layer_unparseable_doc_falls_through_to_copy() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("m.yaml"), ": not : valid : yaml : [").unwrap();
        let local = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("m.yaml"), "a: 1\n").unwrap();
        let dest = tempfile::tempdir().unwrap();
        overlay(&[base.path()], local.path(), dest.path()).unwrap();
        assert!(dest.path().join("m.yaml").is_file());
    }
}
