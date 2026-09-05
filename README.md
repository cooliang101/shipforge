# ShipForge

ShipForge is a Rust TUI for building local project Components and deploying their versioned `tar.gz` Releases to Linux over SSH/SFTP. Components can share a connection or use separate Destinations, with independent versions and compensation boundaries.

## Development status

M1's safe deployment loop and M2's history/recovery management have passed their scoped acceptance, but this is not a production-ready release. Setup, deployment planning, live logs, cancellation, and rollback are implemented. Real two-Destination deployment and [WSL systemd lifecycle tests](docs/validation/systemd-acceptance.md) pass. M2 provides [detailed history](docs/validation/his-01.md), [remote inventory and audit](docs/validation/his-02.md), [read-only reconciliation](docs/validation/rec-01.md), [Release retention](docs/validation/ret-01.md), and [project/connection and history management](docs/validation/tui-mgt-01.md).

[TUI-01](docs/validation/tui-01.md) and [TUI-02](docs/validation/tui-02.md) have passed scoped automated acceptance for navigation, confirmations, searchable candidates, manual setup, per-Component remote directory/service selection and confirmed managed-section reinitialization. [TUI-03](docs/validation/tui-03.md) has passed scoped automated acceptance for step timing, retained-log search/filtering, recorded failed-command copying and confirmed local export. [TUI-04](docs/validation/tui-04.md) has passed scoped automated acceptance for management navigation, evidence, inspection and rollback consistency. Remaining M3 exit/performance work and M4 security, platform, packaging and final MVP acceptance remain on the [roadmap](docs/roadmap.md).

An early real SSH connection timeout and a separate unknown current observation remain unexplained despite passing subsequent tests; a common cause is unproven. Their evidence and diagnostics are retained for platform validation, not treated as resolved by the TUI changes.

## Run locally

Use a normal interactive terminal and Rust 1.96.1, validated on Windows and WSL Ubuntu-22.04. The [Linux client record](docs/validation/linux-client.md) includes native tests, a release build, and startup/exit smoke coverage. `Cargo.toml` declares Rust 1.88 as the minimum; that minimum, macOS, and the full platform matrix still require validation.

```sh
cargo run
```

All user operations are inside the TUI; there are no deployment subcommands. Select a project directory, confirm discovered Components or add one manually, and select an SSH connection and key. Use `F1` for help and `F4` to search supported candidate lists. Each Component can browse/select its own remote directory and optional service. Confirm the YAML preview to save `shipforge.yaml` in the project root; saving does not deploy. Never put secrets or private-key paths in that file. The target needs standard Linux SSH/SFTP and deployment tools, plus systemd or curl when configured; it does not need a ShipForge daemon.

During execution or from history details, `l` opens logs: `/` searches retained files, `f`/`t` filters Components/steps, `p` shows progress, and `e`/`s` previews log/summary export before confirmation. `y` requests copying recorded, redacted failed argv as diagnostic JSON; terminal clipboard permission is required and delivery is not acknowledged. See the [TUI guide](docs/tui-guide.md#步骤与日志) for scope and limits.

## Verify changes

```sh
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Run `cargo audit` separately with the cargo-audit tool installed. Default tests never use saved production Destinations; real Linux tests require explicit disposable fixtures. Instructions, test-only environment variables, and cleanup scope are in [tests/README.md](tests/README.md).

## Reference

- [Configuration guide](docs/configuration-guide.md) — canonical, TUI-managed configuration.
- [TUI guide](docs/tui-guide.md) — project editing, connections, history, inspection, and explicit rollback.
- [Requirements](docs/requirements.md) and [architecture](docs/architecture.md) — MVP scope and boundaries.
- [Contributor guide](AGENTS.md) — style, safety, testing, and per-work-package commits.
