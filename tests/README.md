# Integration Tests

## Windows-only local release gates

The Windows default suite includes SSH password DPAPI persistence, masked TUI setup/edit/save/cancel, and a password-authenticated loopback SSH/SFTP Release lifecycle. Its fixture verifies host-key rejection before any password is sent, wrong-password rejection, registry reload, cancellation and timeout. See [SSH-PWD-01](../docs/validation/ssh-password.md). These fixtures never use the user's saved server credentials.

MVP acceptance uses Windows x64 GNU with the existing Rust/MinGW toolchain (verified: Rust 1.96.1). Linux is the deployment target; WSL/Docker below only hosts disposable servers. Linux/macOS clients and a separate minimum-Rust matrix are outside MVP acceptance. No tests run on GitHub, including after pushes.

From the repository root, run each command locally and stop on any nonzero exit code. `--offline` reuses cached dependencies; it does not install a toolchain.

```powershell
rustc -Vv # Confirm host: x86_64-pc-windows-gnu
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo test --offline --locked --all-targets --all-features --no-fail-fast
cargo build --offline --locked --release
cargo audit
./tests/run-linux-acceptance-cleanup-tests.ps1
./tests/run-systemd-acceptance-cleanup-tests.ps1
cargo test --offline --locked --lib tui::app::performance_tests::isolated_tui_stress_stays_responsive_and_memory_bounded -- --ignored --exact --nocapture --test-threads=1
```

Run the performance gate alone. The existing `cargo-audit` command updates advisory data, not the Rust toolchain; `--no-fetch` checks cached advisories only and cannot establish freshness. Run these two release-binary ConPTY smokes after the release build:

```powershell
$shipforgePreviousSmokeBinary = $env:SHIPFORGE_RELEASE_SMOKE_BINARY
try {
    $env:SHIPFORGE_RELEASE_SMOKE_BINARY = (Resolve-Path -LiteralPath target/release/shipforge.exe).Path
    cargo test --offline --locked --test platform_smoke release_binary_q_exits_and_restores_terminal -- --ignored --exact --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Release q smoke failed' }
    cargo test --offline --locked --test platform_smoke release_binary_recovers_after_idle_ctrl_c_bytes_then_q -- --ignored --exact --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Release idle Ctrl+C smoke failed' }
} finally { $env:SHIPFORGE_RELEASE_SMOKE_BINARY = $shipforgePreviousSmokeBinary }
```

Run the explicit Windows `-Suite ReleaseGate` below separately. Default ignored tests are not counted as passed. Record the tested source commit and limits in [QA-01](../docs/validation/qa-01.md); only documentation changes and removal of GitHub workflows may reuse unchanged execution-code evidence.

## Local history and query model

The default suite includes schema v1–v7 migration/reopen tests, frozen Component context and Release receipts, explicit unknown/absent observations, and fault-injected history writes during deployment and compensation. REC-01 adds immutable inspection reports, stale-source rejection, local-only startup attention and cache-only rebuilding after database loss; corrupt existing history is never replaced. Startup attention does not create a missing history database or parent directory, migrate schemas, or change history rows; SQLite may create/use WAL/SHM sidecars for an existing WAL database. The production service protocol fixture verifies metadata and timed steps, then exercises actual `RecoveryService` calls against pending local history and a fresh cache, asserting remote files/links and original history remain unchanged. These internal APIs do not add a user-facing CLI. The TUI reaches bounded local history, explicit rollback and read-only inspections through management application services.

`cargo test --test recovery_crash` runs 15 abrupt child-exit/reopen scenarios around simulated build, prepare, activate, compensate and rollback effects, including gaps between completed stages. Only the parent launches the exact ignored helper with test-only `SHIPFORGE_CRASH_TEST_ROOT`/`SHIPFORGE_CRASH_TEST_TOKEN`; the fixture directory and token are generated per scenario. Local file-backed Driver facts are simulated, while SQLite commits, process exit without unwinding, reopening and the production recovery service are real. The helper never connects to SSH and is not a product CLI.

RET-01 adds bounded protection-history reads and automatic newest-five planning, including previous health, old endpoints/revisions, active/pending references and incomplete evidence. Application tests use a Fake Driver with the real orchestrator and SQLite to prove intent-before-delete, partial/unknown outcomes, result-write failures, safe deadline/cancellation boundaries, and no compensation of successful deployments. These tests do not execute remote deletion.

On Linux, `cargo test --locked --lib drivers::linux_ssh::retention::tests::native` executes the production Bash deletion script in owned temporary directories. Four cases cover exact removal, metadata byte changes (NUL/oversized newline tails), payload symlinks, and a 65,537-byte LF-terminated listing whose simulated `find` deliberately returns success. Only that listing uses a shim; other commands and the script are real. This needs GNU Bash/coreutils/find/awk/sed, not SSH or elevated privileges.

## Two disposable Linux deployment targets

HIS-02 extends the default tests with bounded inventory/manifest validation, typed audit records, torn/duplicate/unsupported JSONL handling and post-effect warning propagation. The protocol server checks exact supported command forms; it does not execute the audit Shell script. Actual Shell, descriptor, permission and link behavior is exercised by the disposable Linux suite below. See [HIS-02 evidence and limits](../docs/validation/his-02.md).

On Windows with PowerShell 7, WSL Docker, `ssh-keygen`, Git and the repository's Rust toolchain available, run:

```powershell
./tests/run-linux-acceptance.ps1 -Distribution Ubuntu-22.04
```

The runner builds `tests/fixtures/openssh`, starts two independent Debian/OpenSSH containers without `--privileged`, with random loopback-only ports and temporary keys, obtains Host Key fingerprints through Docker, and runs the explicitly ignored `linux_ssh_deployment.rs` cases. Default `-Suite All` runs `Deployment`, `Retention`, `AutomaticRetention`, then `Management` sequentially; select one suite for focused verification. Each exact test name must first be found once with `--ignored --exact --list`, preventing a zero-test false success. SSH deployment uses the `deploy` account; this is not a rootless container setup. A foreground WSL input pipe keeps the Docker runtime available while Windows cargo runs. It does not modify WSL settings, existing containers, saved Destinations, or real project configurations. Cleanup checks per-run container labels and the exact temporary directory before deleting its containers, image tag, and test identity; ordinary Docker build cache can remain. Cleanup errors fail the runner, while still attempting to restore environment variables and release its WSL helper.

For the Windows GNU QA-01 release gate, first ensure the existing Windows OpenSSH Authentication Agent service is running, then run `./tests/run-linux-acceptance.ps1 -Suite ReleaseGate`. The runner validates the native Windows GNU compiler with a bounded `rustc -vV` check and rejects Cargo target/output-directory environment overrides; it never adds `--target`. This reuses `target/release` rather than producing another platform tree or accepting MSVC evidence. It never changes the service startup mode or clears the Agent. It checks that the Agent is reachable, adds one distinct test key, and removes only that exact public key during cleanup; failure to remove it retains exact recovery material and fails the run. Other Agent identities are not enumerated as an acceptance snapshot. The test itself uses the release profile and the same isolated Debian/OpenSSH targets described below. Run `./tests/run-linux-acceptance-cleanup-tests.ps1` for Job Object, deadline, Agent-key, delayed-resource and two-phase cleanup regressions without starting Docker.

For optional Linux-client experiments outside the Windows-only MVP, run `bash ./tests/run-linux-qa01-release-gate.sh`. It builds the exact ignored test in release mode, then exercises separate IdentityFile and SSH Agent keys, strict Host Key rotation rejection, SFTP success/cancellation, and remote-command cancellation against two disposable loopback OpenSSH containers. The runner starts its own foreground `ssh-agent` on an owned socket, adds only the Agent key, and never reads, adds to, clears, or stops an inherited Agent. Both public keys are copied into stopped containers instead of mounting a host or repository directory. Random names, ports, labels, an owner-marked remote root, and a mode-700 temporary directory isolate each run. Setup, Cargo, Docker, Agent, and cleanup commands have outer deadlines; one cleanup timeout does not skip later resources. The exit trap verifies ownership before removal and fails if it cannot prove that containers, the image tag, Agent process, and temporary directory are gone. Docker build cache may remain. Run `bash ./tests/run-linux-qa01-runner-tests.sh` for syntax, exact-selection, no-mount, key-isolation, timeout-continuation, ownership-refusal, and cleanup regressions without starting Docker or an Agent.

For optional, unsupported macOS-client experiments, run `bash ./tests/run-macos-qa01-release-gate.sh` as a non-root user. It requires the system `/usr/sbin/sshd`, OpenSSH client tools, Python 3 and the existing Rust toolchain, but never installs software, invokes `sudo`, or enables Remote Login. It starts one loopback-only per-run sshd and a private Agent under a mode-700 directory, then verifies exact process ownership and removal. Run `bash ./tests/run-macos-qa01-runner-tests.sh` for the no-service safety suite. Neither these runners nor a minimum-Rust matrix is an MVP gate. No test workflow runs on GitHub.

The test refuses to write without the explicit opt-in, two distinct ports/Host Keys, and a fixture marker checked over pinned SSH. It exercises single-Component deployment, joint deployment, explicit rollback to an earlier version/undeployed state, actual destination-side HTTP failure compensation, and cancellation after one activation. It also queries archive metadata and original audit attribution, rejects wrong manifests and links, distinguishes archive-only/directory-only entries, tolerates missing/torn audit, and preserves successful rollback when audit permissions deny append. Remote commands, SFTP, files, hashes, links, and HTTP are real; test payloads are precreated files and the build command is `rustc --version`. This suite does **not** run systemd or prove the full M1 service-stability gate.

Each async acceptance case has its own ten-minute deadline; `All` is four sequential cases, not one ten-minute run. Run `./tests/run-linux-acceptance-cleanup-tests.ps1` to check exact test discovery/execution and the runner's cleanup paths with in-memory doubles; it needs only PowerShell 7 and never starts WSL/Docker or deletes files. This separate regression checks native exit-code handling, aggregated cleanup failures, ownership/path refusal, environment restoration, and helper-process disposal.

Failure-only diagnostics verify the exact run name/label, then read by immutable container ID: an 80-line timestamped OpenSSH log tail, selected container state and whitelisted effective SSH limits. Each diagnostic command is capped at 5 seconds and 64 KiB retained output; each container has a 20-second budget. Fixture-only `VERBOSE` logging changes observability, not authentication policy. Diagnostic failures preserve the original test failure and still run cleanup. The same regression script tests these scope, output and timeout boundaries.

The independent retention case deploys four production-created Releases to `/srv/shipforge-acceptance/retention-<UUID>`, then calls the Driver with an exact candidate and a test-only retain count of two. It verifies complete deletion, an unwritable archive directory causing partial removal, and a fresh archive-only retry after restoring permissions. Current, protected payloads, Marker and metadata/temporary sentinels stay unchanged. It proves actual Driver behavior, not an end-to-end run of the application's default-five policy. Permissions are restored before inspecting the result; panic or outer timeout relies on the runner's container teardown. A failed or ignored run is not acceptance evidence.

`AutomaticRetention` uses another fresh root and six complete production deployments, with no direct cleanup call or retention override. It checks the first version's archive and directory are absent, the remaining five packages match their original manifest/hash/size receipts, and current is the sixth version. SQLite must preserve all package history and contain exactly one successful cleanup intent in the sixth Deployment, with no pending work. The fifth version has a positive health record, but this fixture configures no HTTP/systemd probe; protection outside the newest five is covered by the separate planning tests. See [RET-01 evidence and limits](../docs/validation/ret-01.md).

REC-01 also checks the production recovery service on the real frontend target using separate local pending-intent and missing-cache fixtures. It identifies four canonical temporary-remnant kinds and compares file hashes, no-follow metadata and link targets before/after inspection. Reads may change access times, so atime is excluded. The source fixture represents surviving interruption evidence; it is not a claim that this Linux test kills the client at every SSH instruction. TUI history/recovery pages call the production services; their keyboard tests are separate from this real-SSH case. See [REC-01 evidence and limits](../docs/validation/rec-01.md).

## Custom remote service acceptance

Run `./tests/run-linux-acceptance.ps1 -Suite ServiceCommands` separately from `All`. It builds the test-only `Dockerfile.pm2` (Node 24, PM2 7.0.4) and uses the same attested, disposable loopback SSH endpoints, exact test discovery and cleanup. It installs runtime dependencies only inside its test image, never on the host or a production Destination. If official npm downloads fail, `-NpmRegistry https://registry.npmmirror.com` is an explicit HTTPS fixture-only alternative; host npm configuration and TLS validation remain unchanged.

The production deployment service publishes two independent Components on one SSH endpoint, checks selected-only isolation, first start, update, failed read-only checks, nonzero service exits, compensation, historical rollback and restoration to not-deployed. PM2's recorded program path/cwd and the actual worker PID/ready file must agree with each expected version. The first PM2 call uses `ping` before parsing `jlist`, so daemon bootstrap text is not mistaken for JSON. Readiness scripts are side-effect-free; service scripts perform only fixture-scoped process management.

This full lifecycle case retains a ten-minute internal deadline and an eleven-minute outer native-process deadline (other suites retain their existing limits). Pure tests separately inject service timeout/cancellation/unknown exit outcomes, argv injection, missing historical snapshots, configuration drift, persistence faults and TUI keyboard edits. Failed or truncated fixture runs are not acceptance. See [SVC-01 evidence](../docs/validation/svc-01.md).

Finish code changes and compile before starting long live fixtures. Do not rebuild an integration-test executable while that fixture is still running: Windows refuses replacement of a running executable. Wait for its result and cleanup, then rebuild; never force-terminate a live deployment just to unblock compilation.

## Management acceptance

Run `./tests/run-linux-acceptance.ps1 -Suite Management` for the production connection/history/rollback/recovery services. Each run uses isolated `/srv/shipforge-acceptance/management-<UUID>/<component>` roots and exact UUID-scoped health routes. It creates a temporary connection after pinned authentication, revises it to the independently attested second endpoint, checks retained revisions and removes only that unreferenced registration. A frontend-only deployment followed by a joint deployment supplies real package and health evidence; rolling back only frontend must leave backend's joint-deployment version unchanged. Local logs, linked rollback history, immutable source outcomes and saved inspection reports are checked.

This case uses the same application services as the TUI, not terminal input automation. The unit suite separately drives the rollback keyboard path through a real sealed plan and SQLite with an in-memory Driver, including exact component/version propagation and rejection of modified confirmation keys. Project editing tests use temporary YAML and registry files; connection UI tests use a fake setup adapter. None contact production. `python3 -B tests/fixtures/openssh/test_health.py` verifies namespace isolation and rejection of traversal/noncanonical routes without starting a server.

Implementation, review evidence and unresolved real-test failures are tracked in [TUI-MGT-01 acceptance](../docs/validation/tui-mgt-01.md). A successful individual run does not establish that intermittent connection timeouts are resolved.

## TUI navigation and control diagnostics

`cargo test --locked --lib tui::` runs keyboard/state tests and Ratatui `TestBackend` rendering regressions without a real interactive terminal. TUI-01 covers stable-ID Environment selection across views and renames, fixed production context while scrolling, long target lists, empty selections, historical read-only scope, identical-endpoint connection IDs, IPv6 display, and actual session cancellation help. A boundary-arrow regression ensures the current Component subset is not silently reselected.

Control-error tests exercise the actual local planning gateway with malformed temporary registry/credential files. Separate typed diagnostic-projection tests inject real SQLite failures and build/Driver errors. Secret sentinels must not appear in displayed diagnostics; Deployment IDs, known results and persistence/log warnings must survive. These use isolated fixtures, not saved user connections. Metadata-label tests leave user names, versions and log bodies untouched. This is automated rendering/behavior coverage, not manual usability, terminal recovery or high-throughput performance acceptance. See [TUI-01 evidence and limits](../docs/validation/tui-01.md).

TUI-02 adds shared candidate filtering, stale-list rejection and focus-only selection; help/confirmation isolation; long and empty lists; startup registry errors; manual setup without discovered Components; and independent per-Component root/service drafts. Full keyboard paths verify that configuration is written only after its YAML preview, not by search, browsing or applying a nested form. Fake setup adapters drive actual tracked workers through cancellation, late success and explicit retry. Temporary-file tests cover ambiguous multi-file registration failures without claiming a filesystem transaction.

Managed-section reinitialization uses real temporary YAML and connection files through the production application service. Tests cover fresh identities, preserved reliable roots, invalid/changed input, explicit confirmation, cancellation and successful YAML persistence despite recent-registry failure. `TestBackend` checks horizontal scrolling of long lines and complete preview navigation beyond 65,535 logical lines; the service still saves the exact confirmed bytes. These tests neither reconstruct history nor contact remote targets. See [TUI-02 evidence and limits](../docs/validation/tui-02.md).

TUI-04 management tests restore exact page/scope/selection snapshots, retain focus by stable ID on refresh, and reject cancelled late rollback plans. Controlled real workers preserve late inspection and execution outcomes; actual application-service rollbacks with an in-memory Driver prove that returning or pressing confirmation again cannot reuse a consumed plan. Evidence tests distinguish unknown, historical observations and phase receipts; complete App `TestBackend` regressions cover 80×10 lists, warnings and long candidate details. The logical-line viewport reaches beyond 65,535 rows and tests Unicode/fine panning without terminal I/O. See [TUI-04 evidence and limits](../docs/validation/tui-04.md).

TUI-05 lifecycle tests drive real finite workers through `q` confirmation, resume, cancellation, matching/stale completion and shutdown. Deployment planning/execution, management, connections, project editing, reinitialization, remote target selection, SSH setup, local attention and log/export tasks retain cancellation and join ownership. Fake terminal cleanup verifies partial setup, all-step best-effort restoration and combined errors; fixed diagnostics cannot expose panic payloads. `TestBackend` covers confirmation/waiting overlays down to empty and 20×3 terminals. The separate Linux PTY smoke sends `INT`, `TERM` and `HUP` directly to the release process while draining output, then checks exit status, termios and terminal-control pairs. This does not cover `SIGKILL`, host-enforced close deadlines, Windows ConPTY control-event delivery or TUI-06 whole-process benchmarks. See [TUI-05 evidence and limits](../docs/validation/tui-05.md).

TUI-06 keeps both whole-process stress entry points ignored in the default suite. Run the parent alone, without competing tests:

```sh
cargo test --locked --lib tui::app::performance_tests::isolated_tui_stress_stays_responsive_and_memory_bounded -- --ignored --exact --nocapture --test-threads=1
```

The parent starts one exact child with a nonce and a hard timeout. The child drives the production frame scheduler with `App` and Ratatui `TestBackend`, 80,000 concurrent log events, deterministic view eviction and 400 bounded synthetic inputs. It checks UI-drain-independent producer eviction, view eviction, producer-attempt-to-completed-frame latency and whole-process RSS after the timed workload. Windows/Linux report a process high-water value; macOS reports current RSS after the workload. This does not read a real `crossterm` event, flush a PTY or physical terminal, measure every producer stall, exercise every event channel, or replace the persistent log-writer saturation tests. See [TUI-06 evidence and limits](../docs/validation/tui-06.md).

## Structured execution logs and local export

TUI-03 tests cover explicit schema-v7 log format indexing without relabeling legacy files, complete JSONL rotation, durable-phase event scope, real failed-command snapshots, frozen elapsed clocks, bounded queues/windows and independent step-state retention. Historical-reader tests use raw temporary files to verify all-generation search, exact scope/filter cursors, same-size content drift, cross-file private-key/fragment boundaries, malformed/unsupported records, explicit limits and no history creation or migration.

Log UI tests drive keyboard handlers, tracked worker cancellation/join, multiline record/preview scrolling, failed-rollback identity and exact preview confirmation through `TestBackend`. Clipboard tests inject a writer: they verify one OSC 52 request, refusal and partial-output behavior, not the user's actual clipboard. Local-export tests use owned temporary directories to check no-clobber publication, changed paths, exact bytes and known publication despite late cancellation. Windows fixtures use a workspace-owned temporary directory when the sandbox cannot inspect user-profile ancestors; Unix fixtures use the native temporary filesystem so permission assertions do not depend on WSL drive mappings.

Remote diagnostic tests use in-memory SSH protocol streams and simulated command results, alongside the default loopback integration suite. They do not replace external OpenSSH/systemd acceptance. TUI-06 now covers the bounded synthetic App/`TestBackend` performance gate described above; terminal/multiplexer clipboard permission, physical-terminal latency and manual usability remain platform validation.

## Read-only connection troubleshooting

`./tests/run-linux-acceptance.ps1 -Suite ConnectionStability` selects one separate ignored diagnostic; it is **not included in `All`**. After the same two pinned, disposable endpoint attestations, it opens 100 fresh connections alternating A/B. Each connection retains the production 15-second timeout; a structured `cat` must return the exact fixture marker within a separate 15-second command timeout. Each session is dropped before the next connection, without explicit disconnect, matching the Driver's normal lifetime. No package build, deployment or remote write is performed.

The total diagnostic deadline is 180 seconds, including setup/attestation. The first failure stops immediately without retry. Fixed iteration, endpoint label, phase and elapsed-time fields omit raw SSH errors, credentials and remote output. Runner failure diagnostics and cleanup still apply. Success proves only those repeated connections and marker reads; it does not prove absence of resource accumulation or replace deployment/management acceptance.

## WSL systemd deployment acceptance

With explicit permission to create temporary **system-level services and a root SSH test endpoint** in a development WSL distro, run:

```powershell
./tests/run-systemd-acceptance.ps1 -Distribution Ubuntu-22.04
```

Requires PowerShell 7, Windows Rust/Git/`ssh-keygen`, and WSL Python 3, systemd, OpenSSH server, and `nobody:nogroup` (UID/GID 65534). The runner does not install packages or unlock accounts. It does not change default SSH settings, existing authorized keys, PAM/polkit policy, saved ShipForge connections, or business services. Do not use a production distro.

Each run has an exclusive `/var/tmp/shipforge-systemd-<run-id>/` directory, two runtime-only Worker units, and a separate SSH unit listening on one random IPv4 loopback port. Authentication requires a temporary Ed25519 identity and an independently obtained Host Key fingerprint. The dedicated authorized public-key file is under `/run`, so OpenSSH StrictModes remains enabled. The control connection is root because the production Driver calls system-level `systemctl` directly; the test payloads run as `nobody` without network sockets. This does not validate non-root service-management authorization.

The ignored `linux_ssh_systemd.rs` test exercises real `DeploymentService`/Driver packaging, SFTP, activation, default ten-second systemd stability, historical/undeployed rollback, unstable-update compensation, and failed first deployment. It verifies actual payload startup/exit evidence, restored versions, running/stopped processes, and SQLite terminal records. No HTTP health endpoint is configured. Payload scripts are test-generated; the build command is `rustc --version`.

The test deadline is ten minutes. The SSH unit has a separate fifteen-minute runtime cap; Worker units stop with it. Cleanup verifies the run marker, runtime unit files, process/session ownership and listener closure before removal. Ambiguous resources are preserved and cause failure. Windows temporary keys and process environment are cleaned independently of WSL cleanup failures. OpenSSH's shared `/run/sshd` runtime prerequisite and normal system authentication/journal records may remain; they are not application configuration. An abruptly terminated runner may need explicit cleanup of its reported run ID after inspecting the retained evidence.

The runner sets only process-local test variables: `SHIPFORGE_SYSTEMD_ACCEPTANCE`, `SHIPFORGE_SYSTEMD_RUN_ID`, `SHIPFORGE_TEST_SSH_PORT`, `SHIPFORGE_TEST_SSH_HOST_KEY`, and `SHIPFORGE_TEST_SSH_IDENTITY_FILE`. These are fixture inputs, not ShipForge user configuration or a deployment CLI.

Safety regressions mock system operations and do not provision services; Python cases also use their own temporary files:

```powershell
./tests/run-systemd-acceptance-cleanup-tests.ps1
```

```sh
python3 -B tests/fixtures/systemd/test_fixture.py
```

## Read-only external OpenSSH probe

`linux_ssh_disposable.rs` is deliberately ignored by the default test suite. It never reads the ShipForge Destination registry and refuses non-loopback hosts.

Run it only after starting a disposable Linux SSH server bound to localhost and independently obtaining its SHA-256 Host Key fingerprint:

```bash
SHIPFORGE_DISPOSABLE_SSH=1 \
SHIPFORGE_TEST_SSH_HOST=127.0.0.1 \
SHIPFORGE_TEST_SSH_PORT=2222 \
SHIPFORGE_TEST_SSH_USER=deploy \
SHIPFORGE_TEST_SSH_HOST_KEY='SHA256:...' \
SHIPFORGE_TEST_SSH_IDENTITY_FILE=/absolute/path/to/test_ed25519 \
cargo test --test linux_ssh_disposable -- --ignored --nocapture
```

The test performs only handshake, public-key authentication, a quoted `printf`, `test` probes, and `systemctl list-unit-files`. Destroy the server and test key after the run.

The default `linux_ssh_protocol.rs` test runs an in-process SSH/SFTP server and exercises command quoting, upload progress, retry cleanup, no-clobber behavior, cancellation cleanup, SHA-256 verification, Release Prepare, atomic `current` activation and observation, Destination-side HTTP health execution, and setup probing without contacting an external host. It also checks static marker conflicts, manifest identity, historical/undeployed restoration, rollback source matching, and refusal to write when `current` changes after planning. Each connection owns its channel map while log/file facts remain shared; a separate no-network test checks that isolation. Fixed phase names, elapsed times and command counts aid timeout diagnosis without logging command contents.

`cargo test --locked --test linux_ssh_protocol remote_directory_protocol` runs six additional loopback SSH cases for directory browsing: pinned Host Key and signed public-key authentication, exact read-only command arguments, hostile/oversized responses, cancellation, timeout and client connection closure before server teardown. The fixture retains its temporary identity until both client and cleanup finish. Remote commands are simulated, not an external OpenSSH acceptance. Linux-only unit tests separately execute the production directory command sequence against owned temporary directories, including quoted paths and symlink rejection, without SSH or elevated privileges.

`tests/support/deployment_service.rs` composes the production application service with that server: a temporary Git project is built with `rustc`, only the selected Component is deployed, and build/prepare/activate intents, logs, and manifest source revision are checked against one Deployment ID. Fixture Git initialization uses isolated configuration, no user hooks, and a ten-second deadline per command; the full async protocol fixture has a 180-second deadline. No registry or credentials from real Projects are read.

The default protocol suite uses real SSH/SFTP wire exchanges but simulated remote commands and filesystem state; the read-only external probe verifies basic OpenSSH compatibility only. The separate two-container and WSL systemd suites provide real deployment/compensation and service-stability evidence. The Unix-only inherited-process-pipe tests have passed in WSL Linux, requiring both bounded completion and non-truncated EOF; Windows cannot exercise those cases. See [Linux client validation](../docs/validation/linux-client.md), [TUI-DEP-01 validation](../docs/validation/tui-dep-01.md), [two-Destination validation](../docs/validation/linux-ssh-acceptance.md), and [systemd/M1 acceptance](../docs/validation/systemd-acceptance.md).
