# Repository Guidelines

## Project Structure & Module Organization

`docs/requirements.md` is the product specification; `docs/architecture.md` defines safety invariants. Rust code is under `src/`: `main.rs` owns the TUI lifecycle; `domain/` and `application/` hold Driver-neutral rules; `drivers/linux_ssh/` owns SSH/SFTP behavior; `config/`, `projects/`, `history/`, `telemetry/`, and `adapters/` handle local boundaries; `tui/` contains input, state, and Ratatui views.

Keep unit tests beside code in `#[cfg(test)]` modules, integration tests in `tests/`, and fixtures in `tests/fixtures/`. Never commit generated Releases, logs, credentials, local databases, or private test keys.

## Build, Test, and Development Commands

```bash
cargo run
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

MVP supports Windows x64 GNU only. Reuse existing Rust/MinGW and test locally; never add GitHub test workflows. Release/SSH/systemd checks are in `tests/README.md`.

Use `cargo build --release`; the executable is always `target/release/shipforge.exe`. Respect `.cargo/config.toml` and `rust-toolchain.toml`; do not add `--target` or override Cargo output directories. `cargo clean --profile dev` clears development caches without removing the release build.

## Coding Style & Naming Conventions

Accept `rustfmt` output. Use `snake_case` for modules, files, and functions; `PascalCase` for types and traits; `SCREAMING_SNAKE_CASE` for constants. Keep platform behavior behind focused traits. Errors need operation, Component, and Destination context without secrets or raw parser/Driver diagnostics.

## Testing Guidelines

Use Rust’s built-in harness and behavioral names such as `validate_config_rejects_missing_destination`. Cover success, failure, cancellation, persistence faults, rollback, and interruption. Use a Fake Driver for orchestration and shared contracts for real Drivers; tests never contact production. Every bug fix needs a regression test.

## TUI & Configuration Rules

Use `ratatui` with the `crossterm` backend and buffered, off-screen rendering. Keep input, state, and rendering separate and memory windows bounded. App workers retain cancellation and join ownership; event-delivering workers also retain request IDs, while directly polled tasks are held exactly once. Join before navigation or exit. Cancelled reads/plans cannot open confirmation pages, while known writes and execution outcomes survive late cancellation. Every handled exit, error, or catchable panic must restore raw mode, alternate screen, and cursor. MVP has no detach mode.

Follow `docs/configuration-guide.md`. Configuration is TUI-managed: do not add Agent editing, alternate YAML forms, locks, or user-facing Driver fields. Preserve `_shipforge`; `artifact` is build output and `Release` is the sole versioned `tar.gz` product. Never store secrets in `shipforge.yaml`; validate Host Keys and keep argv separate from Shell text. Follow `docs/architecture.md` for history, evidence, retention, and unknown-result rules.

Remote service commands are an MVP requirement; systemd is a preset over the shared command mechanism, not a separate execution path. Keep commands per Environment/Component, not in shared SSH connections or local builds. Follow the roadmap's next-work-package priority; planned fields are not supported until configuration, TUI, execution, and recovery tests land together.

## Commit & Pull Request Guidelines

Use imperative Conventional Commits, for example `feat: harden TUI shutdown`. Finish each roadmap package with review, passing gates, synced docs, and a separate commit. Pull requests describe changes, tests, risks, rollback impact, schema changes, linked issues, and relevant TUI screenshots.
