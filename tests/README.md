# Tests

Current Linux SSH deployment uses the in-place application contract in [deployment-contract.md](../docs/deployment-contract.md). Tests never use saved production credentials or destinations.

## Local gates (Windows x64 GNU)

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
```

The executable is `target/release/shipforge.exe`; do not override Cargo target or output directories.

- `tests/inplace_contract.py`, invoked by its Rust harness, executes the shipped Python publisher in disposable directories. It covers existing application files, runtime preservation, previous-only archive, stale frontend resources, rollback, corrupted uploads/archives, identity/version mismatch, hard links, drift, interruption and scoped upload discard.
- `tests/linux_ssh_protocol.rs` drives production SSH/SFTP and the new Driver against a loopback russh server. The server runs the exact embedded Python script in an isolated temporary directory. Service replies simulate success, known failure and missing exit status; they do not run a real systemd service. Password tests retain DPAPI reload, host pin, auth cancellation, transfer retries, and explicit sudo prompt/echo protection.
- `tests/remote_directory_protocol.rs` verifies read-only remote browsing and setup.
- Rust unit tests cover orchestration, history persistence, cancellation, configuration and TUI behavior. Generic legacy-capability contracts remain to read existing history; linux-ssh no longer advertises remote retention.
- Release ConPTY smokes remain in `tests/platform_smoke.rs`; run their ignored Windows release tests with `SHIPFORGE_RELEASE_SMOKE_BINARY` set to the absolute release executable.

The prior releases/current Linux deployment and systemd fixtures were retired with that engine. Their validation documents are historical evidence and do not validate the new publisher. Real Linux deployment validation is recorded separately; no test is permitted to discover or mutate production destinations automatically.
