# setup-tamias

Composite GitHub Action that installs the `tamias` CLI on a GitHub
Actions runner by compiling it from source with `cargo install`.

## Usage

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: 9aia/tamias/actions/setup-tamias@main
```

### Inputs

| input        | required | default | description                                                  |
| ------------ | -------- | ------- | ------------------------------------------------------------ |
| `ref`        | no       | `main`  | Git ref/tag of `9aia/tamias` to install                      |
| `cargo-args` | no       | `""`    | Extra arguments appended to `cargo install`                  |

## Pinning

Pin a release tag for reproducible installs. Release tags on
`9aia/tamias` are named `tamias-vX.Y.Z` (e.g. `tamias-v0.1.5`):

```yaml
- uses: 9aia/tamias/actions/setup-tamias@<action-ref>
  with:
    ref: tamias-v0.1.5
```

## TODO

Release binaries (via cargo-dist — `dist-workspace.toml` already exists
but has `ci = []` / no installers configured yet) would replace the
source build once published, making installs fast and
Rust-toolchain-free.
