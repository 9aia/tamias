# examples

Sample packages you can publish and install with tamias.

```sh
tamias publish examples/ci-gate
 tamias search ci-gate
tamias install git:9aia/tamias//examples/ci-gate
```

| Dir | `type` | What it is |
| --- | --- | --- |
| `ci-gate/` | `peregrin-bundle` | A push→verdict CI gate workflow (installs into peregrin's store). |
| `pm-pack/` | `myco-config` | A project-management kinds preset (installs a myco pack). |
| `board/` | `files` | Ships a **pavo view driver** under `drivers/` — `tamias install --allow-drivers ./examples/board` drops `tamias-driver-board` into `<tamias-home>/drivers/` and pavo discovers it (`pavo drivers`, `pavo open board:status`). |

## Shipping drivers in a package

A payload's `drivers/` dir is copied into `<tamias-home>/drivers/` on
install — but only with explicit consent, since bundled drivers are
arbitrary code: pass `--allow-drivers` or set `TAMIAS_ALLOW_DRIVERS=1`.
Without it the install is refused before anything is registered.

The payload must carry **ready-to-run executables**. Interpreted
drivers (the `#!/usr/bin/env python3` examples under
`pavo/examples/drivers/`, like `board/`'s) ship as source at
`drivers/<name>`. Compiled drivers (the Rust `tamias-driver-*` bins)
ship as **prebuilt per-platform binaries** under
`drivers/bin/<target-triple>/`:

```text
my-driver-payload/
├── tamias.yaml
└── drivers/
    ├── tamias-driver-board            # interpreted — any platform
    └── bin/
        ├── x86_64-unknown-linux-gnu/
        │   └── tamias-driver-foo       # compiled — this target only
        └── aarch64-apple-darwin/
            └── tamias-driver-foo
```

At install time tamias copies `drivers/bin/<triple>/` only when
`<triple>` equals the host's rustc target triple (e.g.
`x86_64-unknown-linux-gnu`); the binaries land flat and executable in
`<tamias-home>/drivers/`. A package that ships platform dirs but none
for this host fails the install with a clear error — it can't silently
drop the driver. Everything under `drivers/` is part of the payload
digest, so `tamias verify` covers the binaries like any other file.

Publishing recipe for a compiled driver (e.g. `tamias-driver-foo` in a
product repo): build per target —
`cargo build --release --target <triple>` (or `dist build`, which this
repo's `dist-workspace.toml` already plans) — then stage each binary at
`drivers/bin/<triple>/tamias-driver-foo` in the payload dir and
`tamias publish` / `bundle pack` it like any other package. The `.tgz`
is digest-pinned by the registry/index exactly like interpreted
payloads; no separate binary channel is needed. (A
cargo-install-style fetch/build recipe was considered and rejected:
tamias packages are content-addressed artifacts, not build recipes.)

A *pavo* driver (manifest `capabilities: ["pavo"]`) should answer
`capabilities` with `"types": []` so tamias's content-type lookup
(`find_driver(<type>)`) can never claim installs through it — see
`board/drivers/tamias-driver-board` and `crates/tamias-driver-tamias`.
