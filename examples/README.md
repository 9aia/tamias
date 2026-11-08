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
`pavo/examples/drivers/`, like `board/`'s) ship as source. Compiled
drivers (the Rust `tamias-driver-*` bins) would need a prebuilt
per-platform binary in the payload — the binary-distribution path is
a release TODO; today those install via `cargo install`/`cargo xtask
install` onto `PATH` instead.

A *pavo* driver (manifest `capabilities: ["pavo"]`) should answer
`capabilities` with `"types": []` so tamias's content-type lookup
(`find_driver(<type>)`) can never claim installs through it — see
`board/drivers/tamias-driver-board` and `crates/tamias-driver-tamias`.
