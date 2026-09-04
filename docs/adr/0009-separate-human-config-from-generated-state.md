---
status: accepted
date: 2026-09-03
---

# Keep intent and generated identity in one Project file

The selected Project root contains one `shipforge.yaml`. Human-readable short-form intent and the TUI-maintained `_shipforge` identity section are logically separate but written together with atomic file replacement. `_shipforge` stores stable Project and Environment identities plus each Environment/Component generation, resolved default root, and normalization metadata; Destination IDs, revisions, endpoints, and credential references remain in the user-level registry. The file is the sole source of Project identity and Project-specific deployment intent: a missing file starts a new Project, while missing or invalid managed metadata is rejected and can be reinitialized with new identities only after explicit user confirmation. Local cache and remote deployment state never reconstruct this file. Users select SSH credentials during setup, but the Project file never stores private key material or user-specific key paths. This avoids a second generated file and keeps reopening deterministic, at the cost of requiring all MVP configuration changes to pass through the TUI so it can preserve the system-owned section.
