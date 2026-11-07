---
layout: home

hero:
  name: tamias
  text: package & content manager for the agent factory
  tagline: fetch, store, cache, dispense — peregrin bundles, myco kinds, kuda domains, mimus rules, one tool
  actions:
    - theme: brand
      text: Getting Started
      link: /getting-started
    - theme: alt
      text: Architecture
      link: /explanation/architecture

features:
  - icon: 🐿️
    title: One manager, many content types
    details: a ref + manifest engine; content types declare how a package installs — peregrin bundles, myco configs, kuda domains, mimus rules
    link: /explanation/content-types
    linkText: content types
  - icon: 📦
    title: Refs, not bespoke setup
    details: install from a path, a git repo (subdir + rev), a tarball over HTTP, or a Cloudflare registry — one grammar
    link: /reference/refs
    linkText: ref grammar
  - icon: 🗄️
    title: Local and Cloudflare stores
    details: a local larder + content-addressed fetch cache; a self-hosted Worker + R2 + D1/KV registry for distribution
    link: /explanation/architecture
    linkText: architecture
  - icon: ✅
    title: Verifiable provenance
    details: every install records its source ref, resolved sha, and payload digest; tamias verify recomputes it
    link: /reference/cli
    linkText: CLI reference
---
