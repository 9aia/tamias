# Security Policy

Report vulnerabilities privately to the maintainers via GitHub:
https://github.com/9aia/tamias/security/advisories/new

Do not open public issues for security reports.

## Scope

- tamias distributes content — refs, fetch, the install store + cache,
  and content-type dispatch. The threat surface is **supply chain**:
  a malicious or tampered package, a hijacked ref, a poisoned registry.
- Mitigations: SHA-256 payload digests recorded and re-checked by
  `tamias verify`; digest-pinned refs (`#sha256:…`, resolved commit
  shas); SSH-sign (sturn-style) as the growth path; the fetch cache is
  content-addressed.
- `tamias.yaml` / `tamias.lock` hold names, refs, and digests — never
  secret values. Publish tokens resolve via env var → keyring → arca
  (`arca://` refs), never inline.
- Refs and names are validated before they touch store keys — strict
  segment rules, bounded length, no path traversal.

## Notes

We respond to reports as quickly as we can; expect acknowledgement
within a few days.
