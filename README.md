# ShipForge

ShipForge is a Rust TUI for building local project Components and deploying their versioned `tar.gz` Releases to Linux over SSH/SFTP. Components can share a connection or use separate Destinations, with independent versions and compensation boundaries.

## Development status

The project is in M1 acceptance, not a production-ready release. Setup, deployment planning, live logs, cancellation, and the core rollback service are implemented. History/recovery/retention management and final TUI hardening remain on the [roadmap](docs/roadmap.md). Real two-Destination deployment tests pass; live systemd acceptance and the platform matrix are still incomplete.

## Run locally

Use a normal interactive terminal and the validated Windows Rust 1.96.1 toolchain. `Cargo.toml` declares Rust 1.88 as the minimum; that minimum and the other client platforms still require validation.

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
