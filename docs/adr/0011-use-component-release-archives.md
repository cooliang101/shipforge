---
status: accepted
---

# Use versioned Component Release archives

2026-09-07 update: the single Release archive decision remains accepted; the version-directory extraction and `current` switch below were replaced by [in-place application publishing](../deployment-contract.md). The current Driver verifies and applies the uploaded Release, removes the incoming package after success and keeps only the previous application archive.

A configured `artifact` is only one path to a Component's file or directory build output; its type is detected after the build and is not another configuration field. The core Packager converts that output exactly once into the sole deployable product: one immutable `<version>.tar.gz` Release for one Component, with a SHA-256 digest and a minimal manifest inside. The `linux-ssh` Driver consumes the finished Release, stores it unchanged, extracts it into that Component's version directory, and atomically switches that Component's `current` link. A future managed-platform Driver may unpack or translate the same Release at its boundary; this does not create another core output format. A Deployment may operate on several Releases, with a separate result for each Component and no environment-wide atomicity guarantee.
