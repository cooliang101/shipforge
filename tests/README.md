# Integration Tests

## Local history and query model

The default suite includes schema v1–v6 migration/reopen tests, frozen Component context and Release receipts, explicit unknown/absent observations, and fault-injected history writes during deployment and compensation. REC-01 adds immutable inspection reports, stale-source rejection, local-only startup attention and cache-only rebuilding after database loss; corrupt existing history is never replaced. Startup attention does not create a missing history database or parent directory, migrate schemas, or change history rows; SQLite may create/use WAL/SHM sidecars for an existing WAL database. The production service protocol fixture verifies metadata and timed steps, then exercises actual `RecoveryService` calls against pending local history and a fresh cache, asserting remote files/links and original history remain unchanged. These internal APIs do not add a user-facing CLI or complete the history management screens.

`cargo test --test recovery_crash` runs 15 abrupt child-exit/reopen scenarios around simulated build, prepare, activate, compensate and rollback effects, including gaps between completed stages. Only the parent launches the exact ignored helper with test-only `SHIPFORGE_CRASH_TEST_ROOT`/`SHIPFORGE_CRASH_TEST_TOKEN`; the fixture directory and token are generated per scenario. Local file-backed Driver facts are simulated, while SQLite commits, process exit without unwinding, reopening and the production recovery service are real. The helper never connects to SSH and is not a product CLI.

RET-01 adds bounded protection-history reads and automatic newest-five planning, including previous health, old endpoints/revisions, active/pending references and incomplete evidence. Application tests use a Fake Driver with the real orchestrator and SQLite to prove intent-before-delete, partial/unknown outcomes, result-write failures, safe deadline/cancellation boundaries, and no compensation of successful deployments. These tests do not execute remote deletion.

On Linux, `cargo test --locked --lib drivers::linux_ssh::retention::tests::native` executes the production Bash deletion script in owned temporary directories. Four cases cover exact removal, metadata byte changes (NUL/oversized newline tails), payload symlinks, and a 65,537-byte LF-terminated listing whose simulated `find` deliberately returns success. Only that listing uses a shim; other commands and the script are real. This needs GNU Bash/coreutils/find/awk/sed, not SSH or elevated privileges.

## Two disposable Linux deployment targets

HIS-02 extends the default tests with bounded inventory/manifest validation, typed audit records, torn/duplicate/unsupported JSONL handling and post-effect warning propagation. The protocol server checks exact supported command forms; it does not execute the audit Shell script. Actual Shell, descriptor, permission and link behavior is exercised by the disposable Linux suite below. See [HIS-02 evidence and limits](../docs/validation/his-02.md).

On Windows with PowerShell 7, WSL Docker, `ssh-keygen`, Git and the repository's Rust toolchain available, run:

```powershell
./tests/run-linux-acceptance.ps1 -Distribution Ubuntu-22.04
```

The runner builds `tests/fixtures/openssh`, starts two independent Debian/OpenSSH containers without `--privileged`, with random loopback-only ports and temporary keys, obtains Host Key fingerprints through Docker, and runs the explicitly ignored `linux_ssh_deployment.rs` cases. Default `-Suite All` runs `Deployment`, `Retention`, then `AutomaticRetention` sequentially; select one suite for focused verification. Each exact test name must first be found once with `--ignored --exact --list`, preventing a zero-test false success. SSH deployment uses the `deploy` account; this is not a rootless container setup. A foreground WSL input pipe keeps the Docker runtime available while Windows cargo runs. It does not modify WSL settings, existing containers, saved Destinations, or real project configurations. Cleanup checks per-run container labels and the exact temporary directory before deleting its containers, image tag, and test identity; ordinary Docker build cache can remain. Cleanup errors fail the runner, while still attempting to restore environment variables and release its WSL helper.

The test refuses to write without the explicit opt-in, two distinct ports/Host Keys, and a fixture marker checked over pinned SSH. It exercises single-Component deployment, joint deployment, explicit rollback to an earlier version/undeployed state, actual destination-side HTTP failure compensation, and cancellation after one activation. It also queries archive metadata and original audit attribution, rejects wrong manifests and links, distinguishes archive-only/directory-only entries, tolerates missing/torn audit, and preserves successful rollback when audit permissions deny append. Remote commands, SFTP, files, hashes, links, and HTTP are real; test payloads are precreated files and the build command is `rustc --version`. This suite does **not** run systemd or prove the full M1 service-stability gate.

Each async acceptance case has its own ten-minute deadline; `All` is three sequential cases, not one ten-minute run. Run `./tests/run-linux-acceptance-cleanup-tests.ps1` to check exact test discovery/execution and the runner's cleanup paths with in-memory doubles; it needs only PowerShell 7 and never starts WSL/Docker or deletes files. This separate regression checks native exit-code handling, aggregated cleanup failures, ownership/path refusal, environment restoration, and helper-process disposal.

The independent retention case deploys four production-created Releases to `/srv/shipforge-acceptance/retention-<UUID>`, then calls the Driver with an exact candidate and a test-only retain count of two. It verifies complete deletion, an unwritable archive directory causing partial removal, and a fresh archive-only retry after restoring permissions. Current, protected payloads, Marker and metadata/temporary sentinels stay unchanged. It proves actual Driver behavior, not an end-to-end run of the application's default-five policy. Permissions are restored before inspecting the result; panic or outer timeout relies on the runner's container teardown. A failed or ignored run is not acceptance evidence.

`AutomaticRetention` uses another fresh root and six complete production deployments, with no direct cleanup call or retention override. It checks the first version's archive and directory are absent, the remaining five packages match their original manifest/hash/size receipts, and current is the sixth version. SQLite must preserve all package history and contain exactly one successful cleanup intent in the sixth Deployment, with no pending work. The fifth version has a positive health record, but this fixture configures no HTTP/systemd probe; protection outside the newest five is covered by the separate planning tests. See [RET-01 evidence and limits](../docs/validation/ret-01.md).

REC-01 also checks the production recovery service on the real frontend target using separate local pending-intent and missing-cache fixtures. It identifies four canonical temporary-remnant kinds and compares file hashes, no-follow metadata and link targets before/after inspection. Reads may change access times, so atime is excluded. The source fixture represents surviving interruption evidence; it is not a claim that this Linux test kills the client at every SSH instruction. Full history/recovery management pages remain `TUI-MGT-01`. See [REC-01 evidence and limits](../docs/validation/rec-01.md).

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

`tests/support/deployment_service.rs` composes the production application service with that server: a temporary Git project is built with `rustc`, only the selected Component is deployed, and build/prepare/activate intents, logs, and manifest source revision are checked against one Deployment ID. Fixture Git initialization uses isolated configuration, no user hooks, and a ten-second deadline per command; the full async protocol fixture has a 180-second deadline. No registry or credentials from real Projects are read.

The default protocol suite uses real SSH/SFTP wire exchanges but simulated remote commands and filesystem state; the read-only external probe verifies basic OpenSSH compatibility only. The separate two-container and WSL systemd suites provide real deployment/compensation and service-stability evidence. The Unix-only inherited-process-pipe tests have passed in WSL Linux, requiring both bounded completion and non-truncated EOF; Windows cannot exercise those cases. See [Linux client validation](../docs/validation/linux-client.md), [TUI-DEP-01 validation](../docs/validation/tui-dep-01.md), [two-Destination validation](../docs/validation/linux-ssh-acceptance.md), and [systemd/M1 acceptance](../docs/validation/systemd-acceptance.md).
