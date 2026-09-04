---
status: accepted
---

# Use versioned Component Release archives

A configured `artifact` is only one path to a Component's file or directory build output; its type is detected after the build and is not another configuration field. The core Packager converts that output exactly once into the sole deployable product: one immutable `<version>.tar.gz` Release for one Component, with a SHA-256 digest and a minimal manifest inside. The `linux-ssh` Driver consumes the finished Release, stores it unchanged, extracts it into that Component's version directory, and atomically switches that Component's `current` link. A future managed-platform Driver may unpack or translate the same Release at its boundary; this does not create another core output format. A Deployment may operate on several Releases, with a separate result for each Component and no environment-wide atomicity guarantee.
