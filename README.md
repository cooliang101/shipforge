# ShipForge

ShipForge is a Rust TUI for building local project Components and deploying their versioned `tar.gz` Releases to Linux over SSH/SFTP. Components can share a connection or use separate Destinations, with independent versions and compensation boundaries.

## Development status

M1's safe deployment loop has passed acceptance, but this is not a production-ready release. Setup, deployment planning, live logs, cancellation, and the core rollback service are implemented. Real two-Destination deployment and [WSL systemd lifecycle tests](docs/validation/systemd-acceptance.md) pass. M2 now provides [local detailed history](docs/validation/his-01.md), [remote Release inventory and audit](docs/validation/his-02.md), [read-only reconciliation with startup attention](docs/validation/rec-01.md), and [automatic Release retention](docs/validation/ret-01.md). History/recovery management screens, final TUI hardening, and the complete platform matrix remain on the [roadmap](docs/roadmap.md).

## Run locally

Use a normal interactive terminal and Rust 1.96.1, validated on Windows and WSL Ubuntu-22.04. The [Linux client record](docs/validation/linux-client.md) includes native tests, a release build, and startup/exit smoke coverage. `Cargo.toml` declares Rust 1.88 as the minimum; that minimum, macOS, and the full platform matrix still require validation.

```sh
cargo run
```

All user operations are inside the TUI; there are no deployment subcommands. Select a project directory, confirm discovered Components, and select an SSH connection and key. The TUI writes `shipforge.yaml` in the selected project root. Never put secrets or private-key paths in that file. The target needs standard Linux SSH/SFTP and deployment tools, plus systemd or curl when configured; it does not need a ShipForge daemon.

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
- [Requirements](docs/requirements.md) and [architecture](docs/architecture.md) — MVP scope and boundaries.
- [Contributor guide](AGENTS.md) — style, safety, testing, and per-work-package commits.
