---
status: accepted
date: 2026-09-03
---

# Use russh for the built-in SSH transport

The built-in `linux-ssh` Driver will use `russh` with `russh-sftp`. It integrates directly with Tokio, requires the client to implement Host Key verification, supports Unix SSH Agent sockets plus Windows OpenSSH named pipes and Pageant, and exposes an SFTP subsystem over the same asynchronous session. ShipForge selects the `ring` crypto backend with compression support because the default AWS-LC backend requires an external NASM installation in the validated Windows GNU toolchain. The optional RSA implementation is disabled because its dependency has an unresolved timing-side-channel advisory; MVP users must select a modern non-RSA SSH identity. This fits ShipForge's cancellation and ordered-event model better than wrapping blocking `libssh2` calls. SSH config is only a source of editable candidates from concrete direct `Host` blocks; full `Include`, `Match`, wildcard, and `ProxyJump` resolution is deliberately outside the MVP.
