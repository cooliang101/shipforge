---
status: accepted
---

# Configure deployment destinations per Component

Each Environment configures every deployable Component directly with a system-generated immutable Destination ID and supported target settings. The TUI derives Destination display text from its endpoint, so users do not name connections. For `linux-ssh`, Component settings include remote root, service, and health checks. Components may reuse the same Destination while retaining independent generations, Releases, activation, and rollback. Reusing a server repeats its Destination ID in each applicable Component entry, keeping the execution and rollback boundary explicit. Driver selection remains an internal property of the user-level Destination record and is not serialized into project YAML.
