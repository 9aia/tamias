# tamias docs

VitePress + Diátaxis site — served from the `gh-pages` branch at
`9aia.com/tamias`. Pages live on the `docs` branch as a worktree of
`main`. From the repo root, `cargo xtask site` runs `docs:gen` +
`vitepress build`. Generated pages (`reference/cli.md` via
`tamias gen-cli-docs`, `reference/config.md` and the crate map via
`gen-*.py`) are never hand-edited — keep `lib.rs` module docs accurate.
