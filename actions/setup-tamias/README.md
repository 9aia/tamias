# setup-tamias

Composite GitHub Action that installs the `tamias` CLI on a GitHub
Actions runner. When the pinned `ref` matches a GitHub Release that
carries a prebuilt `tamias` binary for the runner's platform (built by
cargo-dist — see `dist-workspace.toml` and `.github/workflows/release.yml`
in this repo), the binary is downloaded, checksum-verified, and
unpacked. Otherwise the action falls back to compiling from source with
`cargo install`.

## Usage

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: 9aia/tamias/actions/setup-tamias@main
```

### Inputs

| input        | required | default | description                                                                     |
| ------------ | -------- | ------- | ------------------------------------------------------------------------------- |
| `ref`        | no       | `main`  | Git ref/tag of `9aia/tamias` to install                                         |
| `cargo-args` | no       | `""`    | Extra arguments appended to `cargo install` (source-build fallback only)        |

## Pinning

Pin a release tag for reproducible installs. Release tags on
`9aia/tamias` are named `tamias-vX.Y.Z` (e.g. `tamias-v0.1.5`):

```yaml
- uses: 9aia/tamias/actions/setup-tamias@<action-ref>
  with:
    ref: tamias-v0.1.5
```

Prebuilt binaries are currently published for
`x86_64-unknown-linux-gnu` only; other platforms (and refs without a
matching release asset) automatically use the `cargo install` fallback.
