# ShipForge

ShipForge is a Windows Rust TUI for building local project Components and deploying their versioned `tar.gz` Releases to Linux over SSH/SFTP. Components can share a connection or use separate Destinations, with independent versions and compensation boundaries. The MVP client targets Windows x64 GNU only; Linux remains the deployment server, not a supported client platform.

Built for personal deployment to servers you manage, not an enterprise delivery pipeline. Priorities are reusable configuration, straightforward deployment, understandable failures and a runnable exe. Existing safeguards remain; approvals, multi-user governance and exhaustive failure-matrix certification are out of scope. See the [personal-use roadmap](docs/roadmap.md).

## Deployment behavior

ShipForge publishes application files into the configured existing directory. It preserves existing service/Nginx configuration and runtime data, and keeps one `previous.tar.gz` containing only the previous application files for failed-publish recovery. All service commands run in the configured root. There is no required directory migration, current symlink, remote version tree, database/attachment backup, or permission-policy change.

The replacement SSH driver uses Python 3 standard library over pinned SSH and SFTP. Known service failures restore the previous application and execute the configured recovery commands. Unknown command outcomes block competing recovery. Publishing is per-file replacement, not an atomic directory switch. Build artifacts must contain only application files; see [deployment contract](docs/deployment-contract.md) and [configuration](docs/configuration-guide.md).

History, TUI configuration, structured logs, password SSH and saved-password sudo remain available. Acceptance reports for the retired releases/current engine are historical evidence only; current checks are listed in [tests](tests/README.md).

## Run locally

SSH connections support password, private-key and SSH Agent authentication. In either connection form, press `F5` for masked password entry, then confirm the host-key fingerprint before authentication. Passwords are saved with Windows current-user DPAPI protection in the local credential registry, never in project YAML or logs. See [password configuration](docs/configuration-guide.md#ssh-密码登录).

Use an interactive Windows terminal with ConPTY support and the existing Rust/MinGW toolchain. The verified setup is Rust 1.96.1 with target `x86_64-pc-windows-gnu`; no additional local toolchain is installed. `Cargo.toml` retains its Rust 1.88 declaration, but that minimum has not been independently tested. Earlier [Linux client tests](docs/validation/linux-client.md) are historical evidence, not an MVP support commitment.

```sh
cargo run
```

All user operations are inside the TUI; there are no deployment subcommands. Select a project directory, confirm discovered Components or add one manually, and select an SSH connection and key. Use `F1` for help and `F4` to search supported candidate lists. Each Component can browse/select its own remote directory and optional service. Confirm the YAML preview to save `shipforge.yaml` in the project root; saving does not deploy. Never put secrets or private-key paths in that file. The target needs standard Linux SSH/SFTP and deployment tools, plus each configured service runtime/tool (such as Node/PM2), systemd or curl; it does not need a ShipForge daemon.

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
- [AI Agent configuration guide](docs/ai-agent-configuration.md) — inspect existing release scripts, prepare configuration inputs, and save through the TUI.
- [TUI guide](docs/tui-guide.md) — project editing, connections, history, inspection, and explicit rollback.
- [Requirements](docs/requirements.md) and [architecture](docs/architecture.md) — MVP scope and boundaries.
- [Contributor guide](AGENTS.md) — style, safety, testing, and per-work-package commits.
