+++
id = "I-001"
title = "pavo-core paths.rs still falls back to ./tamias (tamias fixed to ./.tamias)"
status = "open"
created = "2026-11-08"
priority = "medium"
+++

tamias commit d263b18 changed tamias-driver::tamias_home() no-env fallback from ./tamias to ./.tamias to match the CLI's ./.tamias/* roots. pavo-core/src/paths.rs is a deliberate copy ('the layout is the shared contract') and still says ./tamias — needs the same one-line fix + its test updated. Out of scope for the tamias-side change (pavo is a sibling repo).
