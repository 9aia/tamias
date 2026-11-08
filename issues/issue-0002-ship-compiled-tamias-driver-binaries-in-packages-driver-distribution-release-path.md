+++
id = "I-002"
title = "Ship compiled tamias-driver-* binaries in packages (driver distribution release path)"
status = "open"
created = "2026-11-08"
priority = "medium"
+++

examples/board proves payload drivers/ -> <tamias-home>/drivers/ works for interpreted (python3) drivers. Compiled drivers (the Rust tamias-driver-* bins: tamias-driver-tamias, and product drivers in myco/kuda/cuora/sturn/mimus repos) can't ship as source — a package would need a prebuilt per-platform binary payload (dist-built, digest-pinned) or a cargo-install-style fetch. Documented as TODO in examples/README.md 'Shipping drivers in a package'.
