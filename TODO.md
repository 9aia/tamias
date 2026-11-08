# TODO — tamias

- [ ] T-011 kuda cards drifted repo-wide (18 modules, incl. deleted seed crates); run kuda sync separately — tamias-driver-tamias landed without card regen · created:2026-11-08
- [ ] T-012 CI gate: run tamias checks via bupha/peregrin, not GitHub Actions · created:2026-11-08
- [ ] T-013 Content signing: implement promised ssh-sign/cosign verification for git:/registry refs (AGENTS.md claims optional ssh-sign; no code exists) · pri:high · created:2026-11-08
- [ ] T-014 Extract shared install pipeline: fetch->driver->store logic is copy-pasted across tamias-cli, tamias-serve, tamias-driver-tamias (3 divergent copies) · created:2026-11-08
- [ ] T-015 Wire or remove dead machinery: Manifest.deps/tamias.lock resolution, include:/overlay(), tamias-config (parsed never applied), orphan tamias-shell, stub tamias-testkit/store-cf/cache-cf · pri:low · created:2026-11-08
- [ ] T-016 Fix flaky env-mutating CLI unit tests (cwd/PATH/env races; need process isolation or mutex coverage) · pri:low · created:2026-11-08
- [ ] T-017 Streaming dir\_digest follow-up if hit-path cost matters (already chunked per-file; dir-level framing unchanged) · pri:low · created:2026-11-08
