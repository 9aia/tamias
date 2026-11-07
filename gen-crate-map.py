#!/usr/bin/env python3
"""Generate the crate-map table (into architecture.md) and
reference/crate-api.md from workspace Cargo.tomls — never hand-edit
either."""
import os, pathlib, re

# See gen-config-docs.py — TAMIAS_ROOT overrides the `..` worktree layout.
ROOT = pathlib.Path(
    os.environ.get("TAMIAS_ROOT") or pathlib.Path(__file__).parent / "..")
WST = (ROOT / "Cargo.toml").read_text()

LAYER = {
    "tamias": "binary",
    "tamias-core": "domain",
    "tamias-config": "port",
    "tamias-vault": "port",
    "tamias-deploy": "port",
    "tamias-ops": "ops",
}

# workspace members — match `"crates/…"` paths only (not dep paths)
members = re.findall(r'"(crates/[^"]+)"', WST)
seen, order_dedup = set(), []
for m in members:
    if m not in seen:
        seen.add(m)
        order_dedup.append(m)
# include the binary crate (path crates/tamias-cli, package name tamias)
if "crates/tamias-cli" not in order_dedup:
    order_dedup.append("crates/tamias-cli")

rows = []
for rel in order_dedup:
    t = (ROOT / rel / "Cargo.toml").read_text()
    name = re.search(r'^name = "([^"]+)"', t, re.M).group(1)
    desc = re.search(r'^description = "([^"]+)"', t, re.M).group(1)
    desc = desc.replace("<", "&lt;").replace(">", "&gt;")
    rows.append((name, LAYER.get(name, "adapter"), desc))

order = {"domain": 0, "port": 1, "ops": 2, "adapter": 3, "binary": 4}
rows.sort(key=lambda r: (order[r[1]], r[0]))
print(f"crate map: {len(rows)} crates")

# 1. crate map table inside explanation/architecture.md
arch = pathlib.Path("explanation/architecture.md")
s = arch.read_text()
table = "| crate | layer | purpose |\n|---|---|---|\n" + "\n".join(
    f"| `{n}` | {l} | {d} |" for n, l, d in rows)
# Consume every consecutive table row after the marker — the previous
# form (\n\|…\|\n?)* let a trailing \n? eat the newline the next
# iteration needed, so each run replaced only one row and duplicated.
s = re.sub(
    r"<!-- crate-map -->\n(?:\|[^\n]*\n?)*",
    "<!-- crate-map -->\n" + table + "\n", s, count=1)
arch.write_text(s)

# 2. crate-api page — one rustdoc link per crate
out = ["# Crate API", "",
       "Rustdoc for every workspace crate — public APIs are",
       "documented at source (doc comments).", ""]
for n, l, _ in rows:
    und = n.replace("-", "_")
    out.append(f"- [{n}](/tamias/api/{und}/index.html) — {l}")
pathlib.Path("reference/crate-api.md").write_text("\n".join(out) + "\n")
print("crate-api regenerated")
