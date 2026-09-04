---
status: accepted
---

# Use capability-based deployment drivers

Deployment orchestration will depend on an internal Deployment Driver SPI instead of SSH/SFTP or vendor APIs. Built-in Drivers receive a Component's resolved Destination and deployment settings, then expose plans, capabilities, Release references, observations, activation, rollback, logs, and cleanup; the MVP includes only `linux-ssh`, while managed-platform and external-process Drivers come later. This prevents Linux release mechanics from becoming universal abstractions, at the cost of explicit capability negotiation and Driver contract testing.
