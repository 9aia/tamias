+++
id = "I-002"
title = "Ship compiled tamias-driver-* binaries in packages (driver distribution release path)"
status = "done"
created = "2026-11-08"
priority = "med"
+++

examples/board proves payload drivers/ -> <tamias-home>/drivers/ works for interpreted (python3) drivers. Compiled drivers (the Rust tamias-driver-* bins: tamias-driver-tamias, and product drivers in myco/kuda/cuora/sturn/mimus repos) can't ship as source — a package would need a prebuilt per-platform binary payload (dist-built, digest-pinned) or a cargo-install-style fetch. Documented as TODO in examples/README.md 'Shipping drivers in a package'.

## Resolution

Done in b5459ae: payload convention drivers/bin/<rustc-target-triple>/<name> for compiled tamias-driver-* binaries alongside portable drivers/<name>. tamias-driver picks the host-triple dir (compile-time TAMIAS_TARGET_TRIPLE via build.rs), hard-errors when platform dirs exist but none match, consent gate (bundled_drivers) covers bin payloads, digest/verify unchanged (dir_digest already walks all payload files). Docs: examples/README.md 'Shipping drivers in a package' rewritten with layout + publish recipe (cargo build --release --target / dist build → stage → tamias publish). Tests: 3 unit + 2 CLI e2e in tamias-driver/tamias-cli; live proof: packaged tamias-driver-tamias debug bin, installed via --allow-drivers, pavo drivers discovers it with stripped PATH; foreign-triple-only package fails with clear error. Left: actual per-platform release publishing automation in product repos (recipe documented, not CI'd); kuda card regen still owed repo-wide (T-011).
