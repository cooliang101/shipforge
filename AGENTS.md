# Repository Guidelines

## Project Structure & Module Organization

`docs/requirements.md` is the product specification:

- `src/main.rs`: TUI entry point and runtime setup.
- `src/domain/`, `application/`: Destination/Component Release rules and Driver-neutral orchestration.
- `src/drivers/linux_ssh/`: the MVP Deployment Driver; provider APIs stay inside Drivers.
- `src/config/`, `adapters/`, `projects/`, `history/`, `telemetry/`: config, low-level I/O, discovery, persistence, and events.
- `src/tui/`: `ratatui` views, bounded UI state, input, and event projection.

Keep unit tests in `#[cfg(test)]` modules, integration tests in `tests/`, and fixtures in `tests/fixtures/`. Do not commit archives, logs, credentials, or local databases.

For schema work, follow `docs/configuration-guide.md`. Project configuration is TUI-managed in the MVP; do not add AI Agent editing, automation paths, alternate YAML forms, or user-facing Driver fields. Preserve `_shipforge`. `artifact` means the configured build-output path; `Release` means the sole generated versioned `tar.gz` deployment product.

## Build, Test, and Development Commands

```bash
cargo run                    # run the TUI locally
cargo build                  # compile a debug executable
cargo test                   # run unit and integration tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check   # verify formatting
```

## Coding Style & Naming Conventions

Accept `rustfmt` output. Use `snake_case` for modules, functions, and files; `PascalCase` for types and traits; and `SCREAMING_SNAKE_CASE` for constants. Keep platform behavior behind focused traits. Add operation, Component, and Destination context to errors without exposing secrets.

## Testing Guidelines

Use Rust's built-in harness and behavioral names such as `validate_config_rejects_missing_destination`. Test lifecycles, capabilities, path safety, rollback, and interruption. A Fake Driver tests orchestration; real Drivers pass shared contracts. Tests never contact production.

## TUI Architecture

Use `ratatui` with the `crossterm` backend. Build frames off-screen and flush changed cells through buffered output. Separate input, state, and rendering; use bounded event/log windows; cap redraw frequency; and restore raw mode, cursor, and alternate screen on every exit. Active deployments may only return to the UI or cancel safely—MVP has no detach mode. Add `mimalloc` only when benchmarks show a benefit.

## Commit & Pull Request Guidelines

Use imperative Conventional Commits, such as `feat: validate release paths`; the initial snapshot uses `chore:`. Finish each roadmap work package with code review, passing tests and quality gates, then a separate commit before starting the next. Do not label a WIP snapshot as completed work. Pull requests must describe changes, tests, risks, and rollback impact; link issues and include TUI screenshots when applicable. Highlight schema changes and update `docs/requirements.md` when behavior changes.

## Security & Deployment Safety

Never store secrets in `shipforge.yaml`. Validate SSH host keys, separate arguments from Shell text, redact Driver data, and preserve protected Component Releases.

History rules are in `docs/architecture.md`: distinguish unknown from absent and retain known outcomes on persistence failures. Keep `recovery_reports`/`recovery_report_components` and `deployment_revisions` separate from original outcomes; never complete old intents or reconstruct missing YAML/history. Inventory is not health or historical endpoint proof; auxiliary JSONL must not prevent compensation.
