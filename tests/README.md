# Integration Tests

`linux_ssh_disposable.rs` is deliberately ignored by the default test suite. It never reads the ShipForge Destination registry and refuses non-loopback hosts.

Run it only after starting a disposable Linux SSH server bound to localhost and independently obtaining its SHA-256 Host Key fingerprint:

```bash
SHIPFORGE_DISPOSABLE_SSH=1 \
SHIPFORGE_TEST_SSH_HOST=127.0.0.1 \
SHIPFORGE_TEST_SSH_PORT=2222 \
SHIPFORGE_TEST_SSH_USER=deploy \
SHIPFORGE_TEST_SSH_HOST_KEY='SHA256:...' \
SHIPFORGE_TEST_SSH_IDENTITY_FILE=/absolute/path/to/test_ed25519 \
cargo test --test linux_ssh_disposable -- --ignored --nocapture
```

The test performs only handshake, public-key authentication, a quoted `printf`, `test` probes, and `systemctl list-unit-files`. Destroy the server and test key after the run.

The default `linux_ssh_protocol.rs` test runs an in-process SSH/SFTP server and exercises command quoting, upload progress, retry cleanup, no-clobber behavior, cancellation cleanup, SHA-256 verification, Release Prepare, atomic `current` activation and observation, Destination-side HTTP health execution, and setup probing without contacting an external host. It also checks static marker conflicts, manifest identity, historical/undeployed restoration, rollback source matching, and refusal to write when `current` changes after planning.

`tests/support/deployment_service.rs` composes the production application service with that server: a temporary Git project is built with `rustc`, only the selected Component is deployed, and build/prepare/activate intents, logs, and manifest source revision are checked against one Deployment ID. No registry or credentials from real Projects are read.

These tests use real SSH/SFTP wire exchanges but simulated remote commands and filesystem state. The ignored test verifies only basic disposable OpenSSH compatibility, not full deployment acceptance. M1 still requires actual single- and multi-Component deployment, explicit rollback, and failure compensation on two disposable Linux Destinations. The Unix-only inherited-process-pipe tests also need Linux CI execution; a Windows run cannot validate them. See [TUI-DEP-01 validation](../docs/validation/tui-dep-01.md).
