# SSH Technology Validation

## Decision

ShipForge uses `russh` and `russh-sftp` for the built-in `linux-ssh` Driver. It enables the `ring` crypto backend and compression support; the default AWS-LC backend was rejected after requiring an external NASM installation on the Windows GNU toolchain. The optional RSA implementation is disabled because its dependency has an unresolved timing-side-channel advisory, so the MVP accepts modern non-RSA SSH identities only. The selected configuration exposes Tokio-native connection, command-channel, SSH Agent signing, and SFTP APIs.

## Verified in Code

- Host Keys are never accepted implicitly. Capture mode records a fingerprint while rejecting the first connection; strict mode accepts only the fingerprint confirmed through the TUI.
- Fingerprint capture and authenticated connections share a client configuration with TCP `nodelay` enabled; protocol, authentication and timeout defaults are otherwise unchanged. This avoids cumulative small-packet delays in sequential command exchanges. The Linux loopback regression retains its original 180-second deadline; RET-01's controlled client-only comparison completed the full case in 4.16 seconds after the baseline timed out.
- Unix Agent sockets, Windows OpenSSH named pipes, and Pageant have explicit discovery paths. Agent identities expose only fingerprint, comment, and certificate status to selection UI.
- Agent discovery observes a cancellation token and does not contact a deployment Destination.
- SSH config discovery reads only concrete direct `Host` blocks and treats the result as editable TUI candidates. `Include`, `Match`, wildcards, and `ProxyJump` are intentionally outside the MVP.
- `linux-ssh` target validation rejects non-normalized roots, unsafe systemd unit names, credential-bearing or ambiguous health URLs, unsupported health schemes, and unknown fields.
- The in-process SSH/SFTP protocol test covers authenticated command execution, bounded streaming upload progress, a partial-write failure followed by cleanup and retry, no-clobber conflicts, cancellation cleanup, remote SHA-256 success and mismatch, Release Prepare, Component-level atomic `current` activation, final observation, and Destination-side HTTP health execution over one real SSH session.
- The opt-in two-container acceptance suite uses real Debian/OpenSSH endpoints and the production deployment service for single/joint deployment, explicit rollback, destination-side HTTP failure compensation, and cancellation. See [live evidence and limits](validation/linux-ssh-acceptance.md); this does not establish systemd or cross-platform acceptance.
- A separate [WSL systemd acceptance suite](validation/systemd-acceptance.md) now verifies a no-port Worker, stability failures, compensation, and historical/undeployed rollback. Its temporary root control connection does not establish non-root service authorization or the complete platform matrix.

## Release-Gate Live Checks

Disposable OpenSSH environments must verify successful SSH Agent and IdentityFile authentication, SFTP upload/cancellation, command cancellation, Host Key rotation rejection, and behavior from Windows, macOS, and Linux clients. These checks belong to the `QA-01` release gate and are required before claiming platform support; they do not run in the default test suite. No production Environment may be used.
