# fuzz

Coverage-guided fuzzing for tamias' untrusted-input surfaces. Detached
from the main workspace — it needs nightly + `cargo-fuzz`.

```sh
rustup toolchain install nightly
cargo +nightly install cargo-fuzz
cargo +nightly fuzz run parse_ref
cargo +nightly fuzz run untar
```

Targets:

- `parse_ref` — every `RefKind` (`git:`/`http`/`cf:`/path) plus the
  registry-ref grammar, over arbitrary UTF-8.
- `untar` — `untar`/`untar_gz` over arbitrary bytes (tar-slip, zip-bomb,
  malformed headers).

The same invariants run deterministically in the normal suite as
`proptest` properties (`crates/*/tests/props.rs`).
