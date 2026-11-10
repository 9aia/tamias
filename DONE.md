# Done

- T-011 kuda cards drifted repo-wide (18 modules, incl. deleted seed crates); run kuda sync separately — tamias-driver-tamias landed without card regen · created:2026-11-08
- T-017 Streaming dir\_digest follow-up if hit-path cost matters (already chunked per-file; dir-level framing unchanged) · pri:low · created:2026-11-08
- T-016 Fix flaky env-mutating CLI unit tests (cwd/PATH/env races; need process isolation or mutex coverage) · pri:low · created:2026-11-08
- T-014 Extract shared install pipeline: fetch->driver->store logic is copy-pasted across tamias-cli, tamias-serve, tamias-driver-tamias (3 divergent copies) · created:2026-11-08
