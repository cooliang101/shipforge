# SVC-01 — Remote Service Commands and systemd Preset

Date: 2026-09-07. Status: completed. Scope: Windows x64 GNU, existing Rust 1.96.1/MinGW, local gates only. Implementation is the commit containing this record, based on `14e12f0`. Final live reruns and their scoped cleanup passed.

## Delivered behavior

- Canonical project schema 2 uses per-Environment/Component `service.start`, `stop`, optional `update`, `restore` and `check`. Missing update reuses start; missing restore reuses effective update. systemd selection generates the same command plan plus its active/NRestarts check.
- Shared TUI setup/editing supports literal argv, cancellation, default reuse, long-value review and exact YAML confirmation. Service edits increment generation; equivalent schema 1 conversion preserves IDs, generation and root. Reads never rewrite configuration, and deployment rejects an unconfirmed schema 1 upgrade.
- Start/update use the new real version directory, restoration the historical directory, and stop the removed current's still-existing directory. Service actions are bounded and are not automatically retried. Command checks are optional, read-only and bounded; custom services do not require systemd.
- An unknown service exit result blocks competing recovery of that Component, even with a known current link or late cancellation. Earlier successful Components still compensate independently. Known failures retain existing compensation, durable-intent and manual-intervention rules.
- Frozen non-secret target settings protect historical recovery. Missing legacy service evidence is not reconstructed from live YAML; changed snapshots are rejected even with an unchanged generation. New optional history fields do not require a SQLite table migration; old records remain readable, but binary downgrade is not promised to read new records.
- Actual failed argv diagnostics now include cwd. Service stdout/stderr are emitted after a known result through the existing bounded, redacting log sink (64 KiB retained per stream, explicit truncation); internal probe output remains quiet. Interrupted commands do not promise complete output.

## Review and regression coverage

Local review covered schema conversion, opaque Driver boundaries, target drift, required recovery actions, unknown-result propagation, service/check timeouts, quoting, credential argument rejection, output bounds, TUI draft ownership and historical compatibility. No subagents or external code review were used.

New regressions cover custom lifecycle directories and ordering, known failure stopping later commands, unknown forward/restore outcomes, late cancellation, read-only probe retries, no implicit systemd dependency, missing executable preflight, legacy history refusal, changed frozen commands, schema/null/alternate-form rejection, command/cwd redaction and bounds, per-Component TUI editing/save/reload and long parameter navigation. Existing persistence-fault and shared protocol tests remain enabled.

## Gates

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check`, `git diff --check` | Passed |
| `cargo clippy --offline --locked --all-targets --all-features -- -D warnings` | Passed |
| `cargo test --offline --locked --all-targets --all-features --no-fail-fast` | 995 library + 2 entry-point + 14 SSH protocol + 7 release-gate helper + 7 platform + 1 recovery parent = **1,026 passed**; ignored cases excluded |
| `cargo test --offline --locked --doc` | Passed; no documentation tests defined |
| `cargo audit` | Passed after a transient fetch failure; 1,239 advisories / 363 dependencies, no reported vulnerability; existing `wnaf 0.14.0` yanked warning retained |
| Linux fixture runner safety regressions | Passed, including exact ServiceCommands selection and bounded lifecycle timeout |
| WSL systemd fixture runner safety regressions | Passed, 22 isolated cleanup scenarios plus native-command checks |
| `cargo build --offline --locked --release` | Passed; fixed `target/release/shipforge.exe`, 11,000,320 bytes |
| Exact release-binary Windows ConPTY | Both `q` and idle Ctrl+C-then-q tests passed |
| Isolated TUI stress gate | Passed: 80,000 logs / 400 inputs; p99 35.172 ms, max 36.306 ms, process RSS 13,504,512 bytes |
| Final PM2 live rerun | Passed in 328.77 seconds; scoped cleanup succeeded |
| Final systemd live rerun | Passed in 288.20 seconds; scoped cleanup succeeded |

Executable SHA-256: `36440e397e42d2e04130fc8f9fb8c3694f9c4c94cd1b28668f0108c857e75aad`. No toolchain was installed, output directory overridden, or GitHub workflow added.

## Live evidence and fixture corrections

`./tests/run-linux-acceptance.ps1 -Suite ServiceCommands -NpmRegistry https://registry.npmmirror.com` provisions isolated Node 24 / PM2 7.0.4 / OpenSSH fixtures. Two independently pinned loopback endpoints are attested; the service scenario deploys independent frontend/backend roots on endpoint A. It verifies PM2's absolute program path/cwd and the running worker's PID/ready evidence for first start, joint update, health failure, nonzero command exit, compensation, historical-version rollback, not-deployed restoration and first-deployment failure. Rollback here exercises the production orchestrator/Driver; frozen-history authorization is also covered by the application-service regression suite. The terminal keyboard tests use temporary YAML and fake adapters, not a production connection.

An earlier complete PM2 run passed in 323.16 seconds (`357a8df2ee0b45ae9a851a94f9a18a6b`) and cleaned up. Final output-logging rerun `dbc14447e4f34dbcac68795d8a109247` passed in 328.77 seconds with cleanup. Real systemd already passed twice, most recently in 283.87 seconds (`efce0bcf615942e6871b614d43322741`); final output-logging rerun `e6d30b19f965464b9eef8c89edc4d92d` passed in 288.20 seconds with cleanup. The systemd fixture uses temporary system-level units/root control SSH with unprivileged no-port payloads; this is not non-root authorization evidence.

The final live runs include service output logging. The only subsequent source change sanitizes control/bidirectional characters in the TUI service-directory preview, with a regression test; it does not change remote execution. Full default tests, strict Clippy, release build, exact-binary ConPTY and the isolated performance gate passed on that final source.

Corrections found during validation:

- Official npm registry TLS failed in this environment. An explicit HTTPS mirror option was added only to the disposable image build; TLS verification and host npm configuration were not changed.
- Cold PM2 startup writes bootstrap text before JSON. The fixture now starts the daemon with `ping` before parsing `jlist`; this fixes the fixture, not a claimed PM2 product integration defect.
- The long lifecycle test was killed by the runner's 300-second outer deadline after its successful forward/failure-recovery scenarios. Its own 600-second test deadline is now enclosed by an explicit 660-second process deadline, with a regression test. Product command deadlines are unchanged by this runner correction.
- A concurrent rebuild could not replace the running Windows systemd test executable. That build was not counted as passing; the fixture finished/cleaned up before the full rebuild and successful default-suite rerun. Never kill a live deployment to make compilation proceed.

All fixture containers, per-run image tags, temporary test identities, units and run directories are removed by their scoped runners. Normal Docker build cache and system authentication/journal records may remain. Nothing deploys to saved user Destinations or the example business project.

## Limits and next package

No remote dependency installation, PM2 discovery/Driver, database migration, locks, shared persistent-directory orchestration, CLI or AI Agent interface was added. Commands are trusted operator code, not a sandbox; scripts must manage only their Component, avoid secrets/ShipForge metadata and make required recovery safe. A zero command exit or matching current does not independently prove the intended process version. Optional probes provide point-in-time evidence, not ongoing monitoring.

Historical service records without frozen commands require manual recovery; old file-only history keeps its previous evidence rules. Early unexplained SSH timeouts and the pre-existing yanked dependency warning are not declared fixed. `QA-02`, `QA-03`, `REL-03` and `ACC-01` remain outstanding; this package does not declare the full MVP production-ready.
