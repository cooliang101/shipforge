---
status: accepted
date: 2026-09-03
---

# Use russh for the built-in SSH transport

RSA dependency tradeoff (2026-09-10): this enables russh's optional `rsa` feature, currently resolving to `rsa 0.10.0-rc.18`. [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) remains published without a patched version; this change does not claim to remediate that upstream timing-side-channel advisory. ShipForge uses RSA only for client authentication after verifying the pinned host, not as a remote RSA decryption service. Compatibility with existing personal deployment identities is the reason for enabling it; SHA-2 negotiation does not itself fix timing side channels. Ed25519/ECDSA remain available. No audit suppression is added.

The built-in `linux-ssh` Driver will use `russh` with `russh-sftp`. It integrates directly with Tokio, requires the client to implement Host Key verification, supports Unix SSH Agent sockets plus Windows OpenSSH named pipes and Pageant, and exposes an SFTP subsystem over the same asynchronous session. ShipForge selects the `ring` crypto backend with compression support because the default AWS-LC backend requires an external NASM installation in the validated Windows GNU toolchain. Updated 2026-09-10: enable RSA identity compatibility for existing deployment keys. RSA authentication negotiates SHA-512/SHA-256 inside the existing authentication deadline; it never falls back to SHA-1. Host Key pins and credential-loading bounds remain mandatory. This fits ShipForge's cancellation and ordered-event model better than wrapping blocking `libssh2` calls. SSH config is only a source of editable candidates from concrete direct `Host` blocks; full `Include`, `Match`, wildcard, and `ProxyJump` resolution is deliberately outside the MVP.
