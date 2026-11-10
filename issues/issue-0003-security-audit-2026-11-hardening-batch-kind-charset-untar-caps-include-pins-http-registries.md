+++
id = "I-003"
title = "security audit 2026-11: hardening batch (kind charset, untar caps, include pins, http registries)"
status = "done"
created = "2026-11-10"
owner = "devin"
priority = "medium"
labels = ["security"]
links = ["P-005"]
+++

Read-through audit of the install/fetch/store/serve/provider surface
(2026-11-10, main @ 1e9a03f). Overall posture is good — findings below
are hardening, not breaches. Ordered roughly by value.

## Verified safe (no action)

- **No auto-deploy on install.** `tamias-install` runs: fetch →
  manifest → `include:` compose → signature gate → bundled-driver
  consent check → driver `install` dispatch → store commit. It never
  touches systemd/launchd/schtasks, never enables units, never
  registers ingress hooks. `provider:` deploy is a separate explicit
  verb (`tamias deploy`), units land *disabled*, `tamias service
  enable`/`tamias run` are separate, hooks print a manual-review
  warning.
- Tar extraction rejects absolute/`..` paths, self-referential and
  escaping symlinks, and non-file/dir/symlink entry types.
- Fetch cache verifies payload digest against commit-time `.meta`
  before a hit; tampered/legacy entries are evicted and refetched.
- Store install is staged + atomic swap; record name/scope/version
  are charset-validated against path escape.
- Git/http argv boundaries are validated (`--` separators, rev/URL
  charset, hex-only shas from `ls-remote`); all subprocess calls are
  timeout-bounded; prompts disabled (`GIT_TERMINAL_PROMPT=0`,
  askpass emptied).
- `http://` unpinned tarballs warn on stderr; `signing.required`
  fails closed, and a signed package with no `allowed_signers` is a
  hard error even when not required.
- Telemetry is env-opt-in only (`TAMIAS_OTEL_*`/`OTEL_*`).
- `unsafe_code` forbidden workspace-wide; `unwrap_used` warned.

## Findings

1. **`type:`/`kind` is unvalidated** (medium). `Manifest::check`
   validates name/scope/version/provider but not `kind`; it lands in
   `tamias-driver-{kind}` joined onto PATH dirs and
   `<tamias-home>/drivers/`. `..`/separators in `kind` produce
   traversal-shaped lookups — exploitability is low (needs an existing
   `tamias-driver-*` directory component to resolve through), but the
   fix is one charset check. → covered by P-005 §D.1.
2. **No cap on *unpacked* tarball size** (medium). `max_download`
   bounds compressed bytes; `untar_gz` has no total-bytes or
   entry-count limit — a small `.tgz` can expand without bound
   (gzip bomb → disk fill). Add an unpacked cap + entry cap in
   `tamias-core::archive::unpack`.
3. **`include:` is a transitive remote-fetch vector** (medium,
   by-design-but-undocumented). A package manifest can name
   `git:`/`http(s)`/`cf:` refs that recurse through the full install
   pipeline under the caller's consent flags — one `tamias install`
   can pull an arbitrary dependency tree, and remote includes aren't
   required to be digest-pinned. Consider: warn on non-store include
   bases, or require `#sha256:`/`#<sha>` pins for remote includes.
4. **`http://` registry bases don't warn** (low). `cf:`/`http:`
   tarball fetches warn when cleartext+unpinned, but the *index*
   fetch (`HttpRegistry`) and `cf_base_url` accept `http://` silently;
   the payload digest check is then only as trustworthy as a
   cleartext index (signatures mitigate only when `allowed_signers`
   is configured). Warn on any `http://` registry base.
5. **`tamias serve` trusts its stdin completely** (low, document).
   Any writer can `install` arbitrary refs with `allow_drivers:true`,
   `publish`, and `registry.add`/`registry.remove` (which mutate the
   tamias config file). Fine for embedding, but the trust boundary
   should be documented; consider an op allowlist in config.
6. **`TAMIAS_ALLOW_DRIVERS=1` is a global bypass** (low). Honored by
   CLI, serve, and TUI for every install; a leaked/persisted env var
   silently consents bundled drivers everywhere. Document as
   dangerous; `doctor` should flag when set. → P-005 §D.4.
7. **Signature principal is the bare package name** (low).
   `allowed_signers` authorizes per-name, so `@acme/w` and `@other/w`
   share signer trust (the signature itself binds scope, so no
   replay). Extend principal to `@scope/name`. → P-005 §B.
8. **PATH drivers aren't consent-gated** (info, by design). Only
   *bundled* `drivers/` need `--allow-drivers`; any `tamias-driver-*`
   already on PATH or in `drivers/` runs its `install` verb on the
   untrusted payload with full user privileges. Correct trust model,
   but drivers must treat `--dir` as hostile — worth a line in
   CONTRIBUTING/driver docs.
9. **Nits**: `find_driver`/`lookup` check `is_file` but not the exec
   bit (spawn fails cleanly anyway); `tamias registry serve` without
   `TAMIAS_REGISTRY_SERVE_TOKEN` is an unauthenticated read/write blob
   store — it warns, but consider refusing non-loopback binds without
   a token; store digest doesn't cover file modes (consistent
   everywhere, just noted).

## Suggested order

kind-charset validation + untar caps (small, self-contained) →
include-pin warning → http-registry warning → P-005 lands the trust
tiers on top.
