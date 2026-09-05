#!/usr/bin/env bash
set -Eeuo pipefail

# Focused macOS runner regressions. Nothing in this file starts sshd or an
# SSH Agent; the dynamic checks use only short-lived inert child processes.

script_source="${BASH_SOURCE[0]}"
if [[ "$script_source" == */* ]]; then
    script_parent="${script_source%/*}"
else
    script_parent='.'
fi
script_dir="$(cd "$script_parent" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"
runner="$script_dir/run-macos-qa01-release-gate.sh"
helper="$script_dir/support/bounded-posix-command.py"
ci_file="$repo_root/.github/workflows/ci.yml"
python_path="$(type -P python3 2>/dev/null || true)"
harness_parent="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
harness_temp="$(mktemp -d "$harness_parent/shipforge-qa01-macos-runner-test.XXXXXXXXXX")"
readonly owner_token='0123456789abcdef0123456789abcdef'

cleanup_harness() {
    local leaf
    leaf="${harness_temp##*/}"
    if [[ "$harness_temp" == "$harness_parent/$leaf" \
        && "$leaf" =~ ^shipforge-qa01-macos-runner-test\.[A-Za-z0-9]{10}$ \
        && -d "$harness_temp" && ! -L "$harness_temp" ]]; then
        rm -rf -- "$harness_temp"
    fi
}
trap cleanup_harness EXIT

fail_test() {
    printf 'QA-01 macOS runner safety test failed: %s\n' "$1" >&2
    exit 1
}

assert_file_contains() {
    grep -Fq -- "$2" "$1" || fail_test "expected $1 to contain: $2"
}

assert_file_excludes_regex() {
    if grep -Eq -- "$2" "$1"; then
        fail_test "forbidden pattern in $1: $2"
    fi
}

run_helper_expect() {
    local expected="$1" actual
    shift
    if "$python_path" -B "$helper" "$@"; then
        actual=0
    else
        actual=$?
    fi
    [[ "$actual" == "$expected" ]] \
        || fail_test "bounded helper returned $actual; expected $expected"
}

wait_for_file() {
    local path="$1" attempt=0
    while (( attempt < 60 )); do
        [[ -f "$path" && ! -L "$path" ]] && return 0
        sleep 0.05
        ((attempt += 1))
    done
    return 1
}

wait_for_pid_to_disappear() {
    local pid="$1" attempt=0
    while (( attempt < 60 )); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.05
        ((attempt += 1))
    done
    return 1
}

kill_verified_test_group() {
    local root_pid="$1" observed_pid="$2" expected_marker="$3"
    local process_info group command_line
    process_info="$(ps -ww -o pgid= -o command= -p "$observed_pid" 2>/dev/null || true)"
    [[ "$process_info" =~ ^[[:space:]]*([0-9]+)[[:space:]]+(.*)$ ]] || return 1
    group="${BASH_REMATCH[1]}"
    command_line="${BASH_REMATCH[2]}"
    [[ "$group" == "$root_pid" && "$command_line" == *"$expected_marker"* ]] || return 1
    kill -KILL -- "-$group" 2>/dev/null || true
}

[[ -n "$python_path" && "$python_path" == /* && -x "$python_path" ]] \
    || fail_test 'python3 is required; the safety test never installs it'
[[ -f "$runner" && ! -L "$runner" ]] || fail_test 'macOS runner is missing'
[[ -f "$helper" && ! -L "$helper" ]] || fail_test 'bounded POSIX helper is missing'

bash -n "$runner"
bash -n "$0"
run_helper_expect 0 \
    --timeout-ms 2000 --max-output-bytes 4096 -- \
    "$python_path" -B -c 'import pathlib, sys; compile(pathlib.Path(sys.argv[1]).read_bytes(), sys.argv[1], "exec")' \
    "$helper"

assert_file_contains "$runner" \
    "cargo_path\" test --locked --release --test linux_ssh_release_gate \"\$gate_case"
assert_file_contains "$runner" '-- --ignored --exact --nocapture --test-threads=1'
assert_file_contains "$runner" 'ListenAddress 127.0.0.1'
assert_file_contains "$runner" 'AllowUsers %s'
assert_file_contains "$runner" '"$sshd_path" -D -e -f "$sshd_config" -h "$temp_dir/current_host_ed25519"'
assert_file_contains "$runner" '"$ssh_agent_path" -D -a "$agent_socket"'
assert_file_contains "$runner" 'environment="SHIPFORGE_QA01_AUTH=identity-file"'
assert_file_contains "$runner" 'environment="SHIPFORGE_QA01_AUTH=ssh-agent"'
assert_file_contains "$runner" 'PermitUserEnvironment SHIPFORGE_QA01_AUTH'
assert_file_contains "$runner" 'Subsystem sftp internal-sftp'
assert_file_contains "$runner" 'StrictModes no'
assert_file_contains "$runner" 'exec /usr/bin/shasum -a 256 -- "$@"'
assert_file_contains "$runner" 'current_host_ed25519'
assert_file_contains "$runner" 'previous_host_ed25519'
assert_file_contains "$runner" 'identity_ed25519'
assert_file_contains "$runner" 'agent_ed25519'
assert_file_contains "$runner" '--owner-token "$run_id"'
assert_file_contains "$runner" '--parent-pid "$$"'
assert_file_contains "$runner" 'trap on_exit EXIT'
assert_file_contains "$runner" "trap 'handle_signal 130' INT"
assert_file_contains "$runner" "trap 'handle_signal 143' TERM"
assert_file_contains "$runner" "trap 'handle_signal 129' HUP"
assert_file_contains "$runner" 'validate_owned_temp_dir'
assert_file_contains "$runner" 'process_group_is_live'

assert_file_excludes_regex "$runner" 'ListenAddress[[:space:]]+(0\.0\.0\.0|::)'
assert_file_excludes_regex "$runner" '(^|[[:space:]])docker([[:space:]]|$)'
assert_file_excludes_regex "$runner" '^[[:space:]]*(command[[:space:]]+)?(sudo|brew|launchctl|systemsetup)([[:space:]]|$)'
assert_file_excludes_regex "$runner" 'ssh_add_path.*[[:space:]]-D([[:space:]]|$)'
assert_file_excludes_regex "$runner" 'ssh_add_path.*identity_ed25519'
assert_file_excludes_regex "$runner" 'declare[[:space:]]+-A|mapfile|readarray|wait[[:space:]]+-n'
assert_file_excludes_regex "$runner" '\$\{[^}]+,,\}'

ci_text="$(<"$ci_file")"
stable_ci="${ci_text%%  msrv-platform:*}"
msrv_ci="${ci_text#*  msrv-platform:}"
[[ "$stable_ci" == *'Run native macOS OpenSSH release gate'* \
    && "$stable_ci" == *'bash tests/run-macos-qa01-runner-tests.sh'* \
    && "$stable_ci" == *'bash tests/run-macos-qa01-release-gate.sh'* ]] \
    || fail_test 'stable macOS CI does not invoke both safety and live gates'
[[ "$msrv_ci" != *'run-macos-qa01-release-gate.sh'* \
    && "$msrv_ci" != *'Run native macOS OpenSSH release gate'* ]] \
    || fail_test 'the macOS live gate must not run in the MSRV job'

# Successful execution and exclusive metadata writes.
success_stdout="$harness_temp/success.stdout"
success_stderr="$harness_temp/success.stderr"
success_pid="$harness_temp/success.pid"
success_status="$harness_temp/success.status"
run_helper_expect 0 \
    --timeout-ms 2000 --term-grace-ms 300 \
    --max-output-bytes 1024 \
    --quiet \
    --stdout-path "$success_stdout" --stderr-path "$success_stderr" \
    --pid-path "$success_pid" --status-path "$success_status" \
    --owner-token "$owner_token" -- \
    /bin/sh -c 'printf shipforge-helper-ok'
[[ "$(<"$success_stdout")" == 'shipforge-helper-ok' \
    && ! -s "$success_stderr" && "$(<"$success_status")" == '0' ]] \
    || fail_test 'bounded helper did not persist the expected successful result'

# Both streams are drained without allowing retained output to grow without a bound.
large_stdout="$harness_temp/large.stdout"
large_stderr="$harness_temp/large.stderr"
run_helper_expect 0 \
    --timeout-ms 3000 --term-grace-ms 300 \
    --max-output-bytes 1024 \
    --quiet \
    --stdout-path "$large_stdout" --stderr-path "$large_stderr" \
    --owner-token "$owner_token" -- \
    "$python_path" -B -c \
    'import sys; sys.stdout.buffer.write(b"x" * 100000); sys.stderr.buffer.write(b"y" * 100000)'
large_stdout_data="$(<"$large_stdout")"
large_stderr_data="$(<"$large_stderr")"
[[ "${#large_stdout_data}" -le 1200 && "${#large_stderr_data}" -le 1200 \
    && "$large_stdout_data" == *'[shipforge bounded output truncated;'* \
    && "$large_stderr_data" == *'[shipforge bounded output truncated;'* ]] \
    || fail_test 'bounded helper retained unbounded output or omitted truncation evidence'

# A timed-out root and its ordinary descendant must leave no owned process group.
spawn_script="$harness_temp/spawn-descendant.py"
cat >"$spawn_script" <<'PY'
import pathlib
import subprocess
import sys
import time

child = subprocess.Popen(["/bin/sleep", "30"])
pathlib.Path(sys.argv[1]).write_text(str(child.pid) + "\n", encoding="ascii")
time.sleep(30)
PY
timeout_root_pid="$harness_temp/timeout-root.pid"
timeout_descendant_pid="$harness_temp/timeout-descendant.pid"
run_helper_expect 124 \
    --timeout-ms 800 --term-grace-ms 300 \
    --max-output-bytes 1024 --pid-path "$timeout_root_pid" \
    --owner-token "$owner_token" -- \
    "$python_path" -B "$spawn_script" "$timeout_descendant_pid"
wait_for_file "$timeout_descendant_pid" \
    || fail_test 'timeout fixture did not publish its descendant pid'
IFS= read -r timeout_root <"$timeout_root_pid"
IFS= read -r timeout_descendant <"$timeout_descendant_pid"
if ! wait_for_pid_to_disappear "$timeout_descendant"; then
    kill_verified_test_group "$timeout_root" "$timeout_descendant" '/bin/sleep 30' || true
    fail_test 'bounded helper leaked a descendant after timeout'
fi

# A successful leader that leaves an inherited group member is an error, and
# the helper must reap that group before its own finally block returns.
exited_parent_script="$harness_temp/exited-parent.py"
cat >"$exited_parent_script" <<'PY'
import pathlib
import subprocess
import sys

child = subprocess.Popen(["/bin/sleep", "30"])
pathlib.Path(sys.argv[1]).write_text(str(child.pid) + "\n", encoding="ascii")
PY
exited_root_pid="$harness_temp/exited-root.pid"
exited_descendant_pid="$harness_temp/exited-descendant.pid"
run_helper_expect 125 \
    --timeout-ms 3000 --term-grace-ms 300 \
    --max-output-bytes 1024 --pid-path "$exited_root_pid" \
    --owner-token "$owner_token" -- \
    "$python_path" -B "$exited_parent_script" "$exited_descendant_pid"
wait_for_file "$exited_descendant_pid" \
    || fail_test 'exited-parent fixture did not publish its descendant pid'
IFS= read -r exited_root <"$exited_root_pid"
IFS= read -r exited_descendant <"$exited_descendant_pid"
if ! wait_for_pid_to_disappear "$exited_descendant"; then
    kill_verified_test_group "$exited_root" "$exited_descendant" '/bin/sleep 30' || true
    fail_test 'bounded helper leaked a descendant whose parent exited normally'
fi

# If the spawning shell exits in the tiny interval before it can retain the
# helper pid, the helper's parent lease still tears down the child group.
parent_loss_script="$harness_temp/parent-loss.sh"
cat >"$parent_loss_script" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
python_path="$1"
helper="$2"
helper_pid_path="$3"
child_pid_path="$4"
status_path="$5"
owner_token="$6"
"$python_path" -B "$helper" \
    --timeout-ms 10000 --term-grace-ms 300 --max-output-bytes 1024 \
    --pid-path "$child_pid_path" --status-path "$status_path" \
    --owner-token "$owner_token" --parent-pid "$$" -- /bin/sleep 30 &
helper_pid="$!"
printf '%s\n' "$helper_pid" >"$helper_pid_path"
attempt=0
while (( attempt < 60 )); do
    [[ -f "$child_pid_path" && ! -L "$child_pid_path" ]] && exit 0
    sleep 0.05
    ((attempt += 1))
done
exit 94
SH
parent_loss_helper_pid="$harness_temp/parent-loss-helper.pid"
parent_loss_child_pid="$harness_temp/parent-loss-child.pid"
parent_loss_status="$harness_temp/parent-loss.status"
bash "$parent_loss_script" "$python_path" "$helper" \
    "$parent_loss_helper_pid" "$parent_loss_child_pid" "$parent_loss_status" "$owner_token"
wait_for_file "$parent_loss_helper_pid" || fail_test 'parent-loss fixture omitted its helper pid'
wait_for_file "$parent_loss_child_pid" || fail_test 'parent-loss fixture omitted its child pid'
IFS= read -r orphan_helper <"$parent_loss_helper_pid"
IFS= read -r orphan_child <"$parent_loss_child_pid"
wait_for_file "$parent_loss_status" || {
    orphan_command="$(ps -ww -o command= -p "$orphan_helper" 2>/dev/null || true)"
    if [[ "$orphan_command" == *"$helper"* && "$orphan_command" == *"$owner_token"* ]]; then
        kill -TERM "$orphan_helper" 2>/dev/null || true
    fi
    fail_test 'orphaned helper did not publish a bounded final status'
}
[[ "$(<"$parent_loss_status")" == '125' ]] \
    || fail_test 'orphaned helper did not report its lost-parent safety failure'
wait_for_pid_to_disappear "$orphan_helper" \
    || fail_test 'lost-parent helper remained alive'
if ! wait_for_pid_to_disappear "$orphan_child"; then
    kill_verified_test_group "$orphan_child" "$orphan_child" '/bin/sleep 30' || true
    fail_test 'lost-parent helper leaked its child group'
fi

# Cover the narrower launch edge: the parent exits immediately after `&`,
# before retaining `$!`. The lease may reject before spawn (no pid/status), or
# it may spawn and then must reap the entire child group with status 125.
immediate_launcher="$harness_temp/immediate-parent-loss.sh"
cat >"$immediate_launcher" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
"$1" -B "$2" \
    --timeout-ms 10000 --term-grace-ms 300 --max-output-bytes 1024 \
    --pid-path "$3" --status-path "$4" --owner-token "$5" \
    --parent-pid "$$" -- /bin/sleep 30 &
exit 0
SH
immediate_child_pid="$harness_temp/immediate-parent-loss.child.pid"
immediate_status="$harness_temp/immediate-parent-loss.status"
bash "$immediate_launcher" "$python_path" "$helper" \
    "$immediate_child_pid" "$immediate_status" "$owner_token"
attempt=0
while (( attempt < 100 )); do
    helper_live=0
    while IFS= read -r process_line; do
        if [[ "$process_line" == *"$helper"* \
            && "$process_line" == *"$immediate_child_pid"* ]]; then
            helper_live=1
            break
        fi
    done < <(ps -ax -ww -o pid= -o command=)
    (( helper_live == 0 )) && break
    sleep 0.05
    ((attempt += 1))
done
(( helper_live == 0 )) \
    || fail_test 'immediate parent-loss left an unregistered bounded helper alive'
if [[ -f "$immediate_status" ]]; then
    [[ ! -L "$immediate_status" && "$(<"$immediate_status")" == '125' ]] \
        || fail_test 'immediate parent-loss helper published an invalid status'
fi
if [[ -f "$immediate_child_pid" ]]; then
    [[ ! -L "$immediate_child_pid" ]] \
        || fail_test 'immediate parent-loss child pid path became a symbolic link'
    IFS= read -r immediate_child <"$immediate_child_pid"
    wait_for_pid_to_disappear "$immediate_child" \
        || fail_test 'immediate parent-loss leaked its child group'
fi

# TERM sent to the helper is converted to a bounded group shutdown and status 143.
term_pid_path="$harness_temp/term-child.pid"
"$python_path" -B "$helper" \
    --timeout-ms 10000 --term-grace-ms 300 \
    --max-output-bytes 1024 --pid-path "$term_pid_path" \
    --owner-token "$owner_token" -- /bin/sleep 30 &
term_helper_pid="$!"
wait_for_file "$term_pid_path" || {
    kill -TERM "$term_helper_pid" 2>/dev/null || true
    fail_test 'signal fixture did not publish its child pid'
}
IFS= read -r term_child_pid <"$term_pid_path"
kill -TERM "$term_helper_pid"
if wait "$term_helper_pid"; then
    term_status=0
else
    term_status=$?
fi
[[ "$term_status" == '143' ]] \
    || fail_test "bounded helper returned $term_status after TERM instead of 143"
if ! wait_for_pid_to_disappear "$term_child_pid"; then
    kill_verified_test_group "$term_child_pid" "$term_child_pid" '/bin/sleep 30' || true
    fail_test 'bounded helper leaked its child after TERM'
fi

# Existing and symbolic-link result paths must never be overwritten.
exclusive_target="$harness_temp/exclusive-target"
exclusive_link="$harness_temp/exclusive-link"
printf '%s\n' 'sentinel' >"$exclusive_target"
ln -s "$exclusive_target" "$exclusive_link"
run_helper_expect 125 \
    --timeout-ms 2000 --term-grace-ms 300 --max-output-bytes 1024 \
    --stdout-path "$exclusive_link" --owner-token "$owner_token" -- /bin/true
[[ "$(<"$exclusive_target")" == 'sentinel' ]] \
    || fail_test 'bounded helper followed and overwrote a symbolic-link result path'

# Cleanup attempts both managed resources even when the first stop fails.
cleanup_log="$harness_temp/cleanup-order.log"
(
    source "$runner"
    stop_managed_process() {
        printf '%s\n' "$1" >>"$cleanup_log"
        [[ "$1" != 'sshd' ]]
    }
    sshd_started=1
    sshd_helper_pid=101
    sshd_child_pid=102
    sshd_pid_path='/mock/sshd.pid'
    sshd_config='/mock/sshd.conf'
    agent_started=1
    agent_helper_pid=201
    agent_child_pid=202
    agent_pid_path='/mock/agent.pid'
    agent_socket='/mock/agent.sock'
    temp_created=0
    if cleanup_resources; then
        exit 91
    fi
)
[[ "$(<"$cleanup_log")" == $'sshd\nagent' ]] \
    || fail_test 'cleanup skipped the Agent after an sshd cleanup failure'

# Exercise the real ownership/stop/delete path with an inert script whose
# argv shape resembles the Agent. It never invokes the system ssh-agent.
managed_temp="$(mktemp -d "$harness_temp/shipforge-qa01-macos-run.XXXXXXXXXX")"
printf '%s\n' "$owner_token" >"$managed_temp/.shipforge-owner"
cat >"$managed_temp/inert-ssh-agent" <<'SH'
#!/usr/bin/env bash
while :; do
    /bin/sleep 30
done
SH
chmod 700 "$managed_temp/inert-ssh-agent"
managed_probe="$harness_temp/managed-probe.sh"
cat >"$managed_probe" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
source "$1"
python_path="$(type -P python3)"
ps_path="$(type -P ps)"
rm_path="$(type -P rm)"
sleep_path="$(type -P sleep)"
temp_parent="$2"
temp_dir="$3"
run_id="$4"
temp_created=1
agent_socket="$temp_dir/agent.sock"
agent_pid_path="$temp_dir/agent.child.pid"
agent_stdout_path="$temp_dir/agent.stdout"
agent_stderr_path="$temp_dir/agent.stderr"
agent_status_path="$temp_dir/agent.status"
start_managed_process \
    "$agent_stdout_path" "$agent_stderr_path" "$agent_pid_path" "$agent_status_path" \
    bash "$temp_dir/inert-ssh-agent" -D -a "$agent_socket"
agent_helper_pid="$managed_helper_pid"
agent_started=1
agent_child_pid="$(wait_for_child_pid "$agent_helper_pid" "$agent_pid_path")"
helper_is_owned "$agent_helper_pid" "$agent_pid_path"
child_is_owned agent "$agent_child_pid" "$agent_helper_pid" "$agent_socket"
cleanup_resources
SH
bash "$managed_probe" "$runner" "$harness_temp" "$managed_temp" "$owner_token"
[[ ! -e "$managed_temp" ]] \
    || fail_test 'real managed-process cleanup did not remove its exact owned temp directory'

# INT, TERM, and HUP all run the same owned EXIT cleanup without starting services.
trap_probe="$harness_temp/trap-probe.sh"
cat >"$trap_probe" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
source "$1"
python_path="$(type -P python3)"
rm_path="$(type -P rm)"
sleep_path="$(type -P sleep)"
temp_parent="$2"
temp_dir="$3"
run_id="$4"
temp_created=1
install_traps
kill -s "$5" "$$"
exit 98
SH
for signal_case in 'INT:130' 'TERM:143' 'HUP:129'; do
    signal_name="${signal_case%%:*}"
    expected_status="${signal_case##*:}"
    signal_temp="$(mktemp -d "$harness_temp/shipforge-qa01-macos-run.XXXXXXXXXX")"
    printf '%s\n' "$owner_token" >"$signal_temp/.shipforge-owner"
    if "$python_path" -B "$helper" \
        --timeout-ms 5000 --term-grace-ms 500 --max-output-bytes 4096 --quiet \
        --owner-token "$owner_token" -- \
        bash "$trap_probe" "$runner" "$harness_temp" "$signal_temp" \
        "$owner_token" "$signal_name"; then
        signal_status=0
    else
        signal_status=$?
    fi
    [[ "$signal_status" == "$expected_status" && ! -e "$signal_temp" ]] \
        || fail_test "$signal_name did not return $expected_status after verified cleanup"
done

# An invalid owner marker and a path outside the exact temp shape are retained.
(
    source "$runner"
    python_path="$(type -P python3)"
    rm_path="$(type -P rm)"
    sleep_path="$(type -P sleep)"
    temp_parent="$harness_temp"
    temp_dir="$(mktemp -d "$harness_temp/shipforge-qa01-macos-run.XXXXXXXXXX")"
    run_id="$owner_token"
    temp_created=1
    printf '%s\n' 'ffffffffffffffffffffffffffffffff' >"$temp_dir/.shipforge-owner"
    if cleanup_resources 2>/dev/null; then
        exit 92
    fi
    [[ -d "$temp_dir" ]]
)
outside_shape="$harness_temp/not-an-owned-run"
mkdir "$outside_shape"
printf '%s\n' 'must-remain' >"$outside_shape/identity_ed25519"
(
    source "$runner"
    python_path="$(type -P python3)"
    rm_path="$(type -P rm)"
    sleep_path="$(type -P sleep)"
    temp_parent="$harness_temp"
    temp_dir="$outside_shape"
    run_id="$owner_token"
    temp_created=1
    if cleanup_resources 2>/dev/null; then
        exit 93
    fi
)
[[ "$(<"$outside_shape/identity_ed25519")" == 'must-remain' ]] \
    || fail_test 'cleanup deleted material outside the exact owned temp shape'

printf '%s\n' 'QA-01 macOS runner safety tests passed without starting sshd or ssh-agent.'
