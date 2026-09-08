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

## WSL publisher fault checks

Run from Windows against the installed WSL distribution, as its ordinary non-root user:

```powershell
wsl -d Ubuntu-22.04 -- python3 -B /mnt/d/cdoe/shipforge/tests/inplace_faults.py
```

Adjust only the repository path when needed. The suite runs 16 inherited contracts plus five Linux fault checks in temporary directories: archive permission denial, partial application replacement failure, state replacement failure, SIGKILL during publishing, and failure during restore. Every case checks an out-of-scope runtime sentinel. Faults are injected only in child test processes around the exact shipped Python entry point; there are no product fault switches. Children have a 20-second deadline and temporary directories are cleaned on completion. Root/non-Linux invocation fails rather than counting skipped checks as success.

This suite does not connect through SSH, launch systemd/PM2, fill a disk, or test host power loss. The Windows protocol suite separately exercises the production Driver with simulated service replies. These are distinct evidence levels; see [WSL fault results](../docs/validation/wsl-inplace-faults.md).
