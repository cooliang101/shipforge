# ShipForge

ShipForge is a Windows Rust TUI for building local project Components and deploying their versioned `tar.gz` Releases to Linux over SSH/SFTP. Components can share a connection or use separate Destinations, with independent versions and compensation boundaries. The MVP client targets Windows x64 GNU only; Linux remains the deployment server, not a supported client platform.

## Development status

M1's safe deployment loop and M2's history/recovery management have passed their scoped acceptance, but this is not a production-ready release. Setup, deployment planning, live logs, cancellation, and rollback are implemented. Real two-Destination deployment and [WSL systemd lifecycle tests](docs/validation/systemd-acceptance.md) pass. M2 provides [detailed history](docs/validation/his-01.md), [remote inventory and audit](docs/validation/his-02.md), [read-only reconciliation](docs/validation/rec-01.md), [Release retention](docs/validation/ret-01.md), and [project/connection and history management](docs/validation/tui-mgt-01.md).

[TUI-01](docs/validation/tui-01.md) and [TUI-02](docs/validation/tui-02.md) have passed scoped automated acceptance for navigation, confirmations, searchable candidates, manual setup, per-Component remote directory/service selection and confirmed managed-section reinitialization. [TUI-03](docs/validation/tui-03.md) covers structured progress, retained-log tools and confirmed local export; [TUI-04](docs/validation/tui-04.md) covers management navigation, evidence, inspection and rollback consistency. [TUI-05](docs/validation/tui-05.md) covers shutdown lifecycle and real Linux PTY restoration. [TUI-06](docs/validation/tui-06.md) bounds background redraws and validates UI-drain-independent log eviction, whole-process memory, and synthetic-submit-to-frame latency. M3 is complete. The Windows-only [QA-01 gate](docs/validation/qa-01.md) has passed local code review, tests, release ConPTY, performance, and real OpenSSH validation. GitHub tests and Linux/macOS client support are outside the MVP; M4 security, packaging, and final MVP acceptance remain on the [roadmap](docs/roadmap.md).

An early real SSH connection timeout and a separate unknown current observation remain unexplained despite passing subsequent tests; a common cause is unproven. Their evidence and diagnostics are retained for platform validation, not treated as resolved by the TUI changes.

The next development package is [SVC-01: remote Service Commands and the systemd preset](docs/roadmap.md#svc-01远端服务命令基础与-systemd-预设), before QA-02 and final release gates. Custom commands are required for the MVP; systemd must become a preset over the shared execution mechanism. The current binary still supports only dedicated systemd service operations, not configurable PM2/restart commands. Remote dependency installation is excluded. Existing acceptance evidence covers the earlier scope, not this pending change.

## Run locally

Use an interactive Windows terminal with ConPTY support and the existing Rust/MinGW toolchain. The verified setup is Rust 1.96.1 with target `x86_64-pc-windows-gnu`; no additional local toolchain is installed. `Cargo.toml` retains its Rust 1.88 declaration, but that minimum has not been independently tested. Earlier [Linux client tests](docs/validation/linux-client.md) are historical evidence, not an MVP support commitment.

```sh
cargo run
```

All user operations are inside the TUI; there are no deployment subcommands. Select a project directory, confirm discovered Components or add one manually, and select an SSH connection and key. Use `F1` for help and `F4` to search supported candidate lists. Each Component can browse/select its own remote directory and optional service. Confirm the YAML preview to save `shipforge.yaml` in the project root; saving does not deploy. Never put secrets or private-key paths in that file. The target needs standard Linux SSH/SFTP and deployment tools, plus systemd or curl when configured; it does not need a ShipForge daemon.

During execution or from history details, `l` opens logs: `/` searches retained files, `f`/`t` filters Components/steps, `p` shows progress, and `e`/`s` previews log/summary export before confirmation. `y` requests copying recorded, redacted failed argv as diagnostic JSON; terminal clipboard permission is required and delivery is not acknowledged. While work is active, `q` opens an exit confirmation: `Esc` or `r` returns, while `c` or `Ctrl+C` cancels and waits for a safe boundary before restoring the terminal. See the [TUI guide](docs/tui-guide.md) for scope and limits.

## Verify changes

Build with `cargo build --release`; the executable is always `target/release/shipforge.exe`. The project selects the installed Windows GNU toolchain through `rust-toolchain.toml` and fixes the cache root in `.cargo/config.toml`. Do not add `--target` or override Cargo output directories: explicit cross-compilation creates another platform subdirectory.

```powershell
cargo build --release
& .\target\release\shipforge.exe
```

Use `cargo clean --profile dev` to reclaim development caches while keeping the release build. A full `cargo clean` removes all project build output, including the executable; rebuild it afterward. Neither command removes downloaded crates or installed toolchains. See [build-layout verification](docs/validation/build-layout.md).

```sh
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

All gates run locally on Windows; there are no GitHub Actions test workflows, and pushing only synchronizes repository content. Run `cargo audit` separately using the existing cargo-audit tool. Default tests never use saved production Destinations; real Linux-server tests require explicit disposable fixtures. Commands for release ConPTY, performance, SSH and cleanup gates, test-only environment variables, and cleanup scope are in [tests/README.md](tests/README.md).

## Reference

- [Configuration guide](docs/configuration-guide.md) — canonical, TUI-managed configuration.
- [TUI guide](docs/tui-guide.md) — project editing, connections, history, inspection, and explicit rollback.
- [Requirements](docs/requirements.md) and [architecture](docs/architecture.md) — MVP scope and boundaries.
- [Contributor guide](AGENTS.md) — style, safety, testing, and per-work-package commits.
