#!/usr/bin/env bash
set -Eeuo pipefail

# Native macOS QA-01 gate. This runner creates only per-run resources owned by
# the current non-root user; it never enables Remote Login or invokes sudo.

script_source="${BASH_SOURCE[0]}"
if [[ "$script_source" == */* ]]; then
    script_parent="${script_source%/*}"
else
    script_parent='.'
fi
script_dir="$(cd "$script_parent" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"
bounded_helper="$script_dir/support/bounded-posix-command.py"

readonly gate_case='qa01_release_gate_validates_host_key_rotation_agent_sftp_and_cancellation'
readonly command_output_limit='131072'
readonly gate_output_limit='8388608'
readonly managed_output_limit='131072'
readonly managed_timeout_ms='180000'

python_path=''
cargo_path=''
chmod_path=''
id_path=''
mktemp_path=''
mkdir_path=''
ps_path=''
rm_path=''
sleep_path=''
ssh_add_path=''
ssh_agent_path=''
ssh_keygen_path=''
ssh_keyscan_path=''
sshd_path='/usr/sbin/sshd'

temp_parent=''
temp_dir=''
temp_created=0
run_id=''
current_user=''
port=''
identity_fingerprint=''
agent_fingerprint=''
current_host_fingerprint=''
previous_host_fingerprint=''

agent_started=0
agent_helper_pid=''
agent_child_pid=''
agent_socket=''
agent_pid_path=''
agent_stdout_path=''
agent_stderr_path=''
agent_status_path=''

sshd_started=0
sshd_helper_pid=''
sshd_child_pid=''
sshd_config=''
sshd_pid_path=''
sshd_stdout_path=''
sshd_stderr_path=''
sshd_status_path=''

fail() {
    printf 'QA-01 macOS gate failed: %s\n' "$1" >&2
    exit 1
}

find_executable() {
    local name="$1" result
    result="$(type -P "$name" 2>/dev/null || true)"
    [[ -n "$result" && "$result" == /* && -x "$result" ]] || return 1
    printf '%s\n' "$result"
}

bounded_command() {
    local timeout_ms="$1" max_output="$2"
    shift 2
    "$python_path" -B "$bounded_helper" \
        --timeout-ms "$timeout_ms" \
        --term-grace-ms 2000 \
        --max-output-bytes "$max_output" \
        -- "$@"
}

bounded_silent() {
    local timeout_ms="$1"
    shift
    bounded_command "$timeout_ms" "$command_output_limit" "$@" >/dev/null 2>&1
}

bounded_capture() {
    local timeout_ms="$1"
    shift
    bounded_command "$timeout_ms" "$command_output_limit" "$@" 2>/dev/null
}

pause_briefly() {
    bounded_silent 500 "$sleep_path" 0.10 || true
}

require_commands() {
    local kernel uid bash_version
    [[ -f "$bounded_helper" && ! -L "$bounded_helper" ]] \
        || fail 'the bounded POSIX command helper is missing or is a symbolic link'

    python_path="$(find_executable python3)" || fail 'python3 is required; this runner never installs tools'
    [[ -x /usr/bin/uname ]] || fail '/usr/bin/uname is required'
    kernel="$(bounded_capture 2000 /usr/bin/uname -s)" \
        || fail 'could not identify the runner operating system'
    [[ "$kernel" == 'Darwin' ]] || fail 'this live gate runs only on macOS'

    cargo_path="$(find_executable cargo)" || fail 'cargo is required; this runner never installs Rust'
    chmod_path="$(find_executable chmod)" || fail 'chmod is required'
    id_path="$(find_executable id)" || fail 'id is required'
    mktemp_path="$(find_executable mktemp)" || fail 'mktemp is required'
    mkdir_path="$(find_executable mkdir)" || fail 'mkdir is required'
    ps_path="$(find_executable ps)" || fail 'ps is required'
    rm_path="$(find_executable rm)" || fail 'rm is required'
    sleep_path="$(find_executable sleep)" || fail 'sleep is required'
    ssh_add_path="$(find_executable ssh-add)" || fail 'the system OpenSSH ssh-add client is required'
    ssh_agent_path="$(find_executable ssh-agent)" || fail 'the system OpenSSH ssh-agent is required'
    ssh_keygen_path="$(find_executable ssh-keygen)" || fail 'the system OpenSSH ssh-keygen client is required'
    ssh_keyscan_path="$(find_executable ssh-keyscan)" || fail 'the system OpenSSH ssh-keyscan client is required'
    [[ -x "$sshd_path" && ! -L "$sshd_path" ]] \
        || fail '/usr/sbin/sshd is required as a regular system executable'
    [[ -x /usr/bin/shasum && ! -L /usr/bin/shasum ]] \
        || fail '/usr/bin/shasum is required for the disposable sha256sum shim'

    bash_version="${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"
    [[ "${BASH_VERSINFO[0]}" -gt 3 \
        || ( "${BASH_VERSINFO[0]}" -eq 3 && "${BASH_VERSINFO[1]}" -ge 2 ) ]] \
        || fail "Bash 3.2 or newer is required (found $bash_version)"
    uid="$(bounded_capture 2000 "$id_path" -u)" || fail 'could not identify the current uid'
    [[ "$uid" =~ ^[0-9]+$ ]] || fail 'the current uid is malformed'
    [[ "$uid" != '0' ]] \
        || fail 'the macOS gate requires a non-root current user and never falls back to sudo'
    current_user="$(bounded_capture 2000 "$id_path" -un)" \
        || fail 'could not identify the current user'
    [[ "$current_user" =~ ^[A-Za-z0-9._-]+$ ]] \
        || fail 'the current user name is not safe for an isolated sshd configuration'
}

validate_owned_temp_dir() {
    local leaf marker=''
    validate_temp_path_shape || return 1
    [[ -f "$temp_dir/.shipforge-owner" \
        && ! -L "$temp_dir/.shipforge-owner" ]] || return 1
    IFS= read -r marker <"$temp_dir/.shipforge-owner" || return 1
    [[ -n "$run_id" && "$marker" == "$run_id" && "$run_id" =~ ^[0-9a-f]{32}$ ]]
}

validate_temp_path_shape() {
    local leaf
    [[ "$temp_created" == '1' && -n "$temp_parent" && -n "$temp_dir" ]] || return 1
    leaf="${temp_dir##*/}"
    [[ "$temp_dir" == "$temp_parent/$leaf" \
        && "$leaf" =~ ^shipforge-qa01-macos-run\.[A-Za-z0-9]{10}$ \
        && -d "$temp_dir" && ! -L "$temp_dir" ]]
}

create_owned_temp_dir() {
    local requested_parent seed hash_output leaf
    requested_parent="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
    [[ -d "$requested_parent" ]] || fail 'the temporary parent directory does not exist'
    temp_parent="$(cd "$requested_parent" && pwd -P)" \
        || fail 'could not resolve the temporary parent directory'
    [[ "$temp_parent" =~ ^/[A-Za-z0-9._/-]+$ ]] \
        || fail 'the temporary parent path cannot be represented safely in sshd_config'
    temp_dir="$(bounded_capture 3000 "$mktemp_path" -d \
        "$temp_parent/shipforge-qa01-macos-run.XXXXXXXXXX")" \
        || fail 'could not create the isolated temporary directory'
    leaf="${temp_dir##*/}"
    [[ "$temp_dir" == "$temp_parent/$leaf" \
        && "$leaf" =~ ^shipforge-qa01-macos-run\.[A-Za-z0-9]{10}$ \
        && -d "$temp_dir" && ! -L "$temp_dir" ]] \
        || fail 'mktemp returned an unsafe temporary directory'
    temp_created=1
    bounded_silent 3000 "$chmod_path" 700 "$temp_dir" \
        || fail 'could not restrict the temporary directory'
    seed="$temp_dir/.owner-seed"
    printf '%s\n' "$temp_dir:$$:${SECONDS}" >"$seed" \
        || fail 'could not create the temporary ownership seed'
    hash_output="$(bounded_capture 3000 /usr/bin/shasum -a 256 -- "$seed")" \
        || fail 'could not create the temporary ownership token'
    run_id="${hash_output%% *}"
    run_id="${run_id:0:32}"
    [[ "$run_id" =~ ^[0-9a-f]{32}$ ]] \
        || fail 'the temporary ownership token is malformed'
    printf '%s\n' "$run_id" >"$temp_dir/.shipforge-owner" \
        || fail 'could not record temporary directory ownership'
    bounded_silent 3000 "$rm_path" -f -- "$seed" \
        || fail 'could not remove the temporary ownership seed'
    validate_owned_temp_dir || fail 'temporary directory ownership could not be verified'
    printf 'QA-01 native macOS run: %s\n' "$run_id"
}

write_sha256sum_shim() {
    bounded_silent 3000 "$mkdir_path" "$temp_dir/bin" \
        || fail 'could not create the disposable command directory'
    {
        printf '%s\n' '#!/bin/sh'
        printf '%s\n' 'if [ "${1-}" = "--" ]; then shift; fi'
        printf '%s\n' 'exec /usr/bin/shasum -a 256 -- "$@"'
    } >"$temp_dir/bin/sha256sum" \
        || fail 'could not write the disposable sha256sum shim'
    bounded_silent 3000 "$chmod_path" 700 "$temp_dir/bin/sha256sum" \
        || fail 'could not restrict the disposable sha256sum shim'
}

fingerprint_for_public_key() {
    local public_key="$1" output
    output="$(bounded_capture 5000 "$ssh_keygen_path" -lf "$public_key" -E sha256)" \
        || return 1
    [[ "$output" =~ (^|[[:space:]])(SHA256:[A-Za-z0-9+/]{43})([[:space:]]|$) ]] \
        || return 1
    printf '%s\n' "${BASH_REMATCH[2]}"
}

generate_keys_and_authorization() {
    local identity_public agent_public
    bounded_silent 10000 "$ssh_keygen_path" -q -t ed25519 -N '' \
        -C shipforge-qa01-identity-file -f "$temp_dir/identity_ed25519" \
        || fail 'could not create the disposable IdentityFile key'
    bounded_silent 10000 "$ssh_keygen_path" -q -t ed25519 -N '' \
        -C shipforge-qa01-ssh-agent -f "$temp_dir/agent_ed25519" \
        || fail 'could not create the disposable Agent key'
    bounded_silent 10000 "$ssh_keygen_path" -q -t ed25519 -N '' \
        -C shipforge-qa01-current-host -f "$temp_dir/current_host_ed25519" \
        || fail 'could not create the current disposable Host Key'
    bounded_silent 10000 "$ssh_keygen_path" -q -t ed25519 -N '' \
        -C shipforge-qa01-previous-host -f "$temp_dir/previous_host_ed25519" \
        || fail 'could not create the previous disposable Host Key'

    identity_fingerprint="$(fingerprint_for_public_key "$temp_dir/identity_ed25519.pub")" \
        || fail 'the IdentityFile fingerprint is malformed'
    agent_fingerprint="$(fingerprint_for_public_key "$temp_dir/agent_ed25519.pub")" \
        || fail 'the Agent fingerprint is malformed'
    current_host_fingerprint="$(fingerprint_for_public_key "$temp_dir/current_host_ed25519.pub")" \
        || fail 'the current Host Key fingerprint is malformed'
    previous_host_fingerprint="$(fingerprint_for_public_key "$temp_dir/previous_host_ed25519.pub")" \
        || fail 'the previous Host Key fingerprint is malformed'
    [[ "$identity_fingerprint" != "$agent_fingerprint" \
        && "$current_host_fingerprint" != "$previous_host_fingerprint" ]] \
        || fail 'the disposable client and Host Keys must be distinct'

    IFS= read -r identity_public <"$temp_dir/identity_ed25519.pub" \
        || fail 'could not read the IdentityFile public key'
    IFS= read -r agent_public <"$temp_dir/agent_ed25519.pub" \
        || fail 'could not read the Agent public key'
    [[ "$identity_public" == ssh-ed25519\ * && "$agent_public" == ssh-ed25519\ * ]] \
        || fail 'the disposable public keys are malformed'
    {
        printf 'environment="SHIPFORGE_QA01_AUTH=identity-file" %s\n' "$identity_public"
        printf 'environment="SHIPFORGE_QA01_AUTH=ssh-agent" %s\n' "$agent_public"
    } >"$temp_dir/authorized_keys" \
        || fail 'could not write the isolated authorized_keys file'
    bounded_silent 3000 "$chmod_path" 600 \
        "$temp_dir/authorized_keys" \
        "$temp_dir/identity_ed25519" \
        "$temp_dir/agent_ed25519" \
        "$temp_dir/current_host_ed25519" \
        "$temp_dir/previous_host_ed25519" \
        || fail 'could not restrict the disposable key material'
}

pick_loopback_port() {
    local selected
    selected="$(bounded_capture 3000 "$python_path" -c \
        'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')" \
        || fail 'could not reserve a candidate loopback port'
    [[ "$selected" =~ ^[0-9]+$ && "$selected" -ge 1024 && "$selected" -le 65535 ]] \
        || fail 'the candidate loopback port is malformed'
    port="$selected"
}

write_sshd_config() {
    sshd_config="$temp_dir/sshd_config"
    {
        printf 'Port %s\n' "$port"
        printf '%s\n' 'ListenAddress 127.0.0.1'
        printf 'PidFile %s\n' "$temp_dir/sshd.daemon.pid"
        printf 'AuthorizedKeysFile %s\n' "$temp_dir/authorized_keys"
        printf 'AllowUsers %s\n' "$current_user"
        printf '%s\n' 'PubkeyAuthentication yes'
        printf '%s\n' 'AuthenticationMethods publickey'
        printf '%s\n' 'PermitRootLogin no'
        printf '%s\n' 'PasswordAuthentication no'
        printf '%s\n' 'KbdInteractiveAuthentication no'
        printf '%s\n' 'ChallengeResponseAuthentication no'
        printf '%s\n' 'PermitUserEnvironment SHIPFORGE_QA01_AUTH'
        printf 'SetEnv PATH=%s/bin:/usr/bin:/bin:/usr/sbin:/sbin\n' "$temp_dir"
        printf '%s\n' 'UsePAM no'
        # The absolute authorized_keys file is beneath a random mode-700 run
        # directory. StrictModes cannot reliably accept every hosted-runner
        # ancestor (RUNNER_TEMP and /private/tmp vary), so do not let unrelated
        # parent modes make this loopback-only fixture nondeterministic.
        printf '%s\n' 'StrictModes no'
        printf '%s\n' 'AllowTcpForwarding no'
        printf '%s\n' 'AllowAgentForwarding no'
        printf '%s\n' 'X11Forwarding no'
        printf '%s\n' 'PermitTunnel no'
        printf '%s\n' 'PermitUserRC no'
        printf '%s\n' 'UseDNS no'
        printf '%s\n' 'PrintMotd no'
        printf '%s\n' 'Subsystem sftp internal-sftp'
        printf '%s\n' 'LogLevel VERBOSE'
    } >"$sshd_config" || fail 'could not write the isolated sshd configuration'
    bounded_silent 10000 "$sshd_path" -t -f "$sshd_config" \
        -h "$temp_dir/current_host_ed25519" \
        || fail 'system sshd cannot validate an isolated non-root configuration; no sudo fallback is permitted'
}

start_managed_process() {
    local stdout_path="$1" stderr_path="$2" pid_path="$3" status_path="$4"
    shift 4
    "$python_path" -B "$bounded_helper" \
        --timeout-ms "$managed_timeout_ms" \
        --term-grace-ms 3000 \
        --max-output-bytes "$managed_output_limit" \
        --quiet \
        --stdout-path "$stdout_path" \
        --stderr-path "$stderr_path" \
        --pid-path "$pid_path" \
        --status-path "$status_path" \
        --owner-token "$run_id" \
        --parent-pid "$$" \
        -- "$@" &
    managed_helper_pid="$!"
}

wait_for_child_pid() {
    local helper_pid="$1" pid_path="$2" deadline child=''
    deadline=$((SECONDS + 5))
    while (( SECONDS <= deadline )); do
        if [[ -f "$pid_path" && ! -L "$pid_path" ]]; then
            IFS= read -r child <"$pid_path" || return 1
            [[ "$child" =~ ^[0-9]+$ && "$child" -gt 1 && "$child" != "$$" ]] || return 1
            printf '%s\n' "$child"
            return 0
        fi
        kill -0 "$helper_pid" 2>/dev/null || return 1
        pause_briefly
    done
    return 1
}

helper_is_owned() {
    local helper_pid="$1" pid_path="$2" line parent command_line
    validate_owned_temp_dir || return 1
    [[ "$helper_pid" =~ ^[0-9]+$ && "$pid_path" == "$temp_dir/"* ]] || return 1
    line="$(bounded_capture 1000 "$ps_path" -ww -o ppid= -o command= -p "$helper_pid")" \
        || return 1
    [[ "$line" =~ ^[[:space:]]*([0-9]+)[[:space:]]+(.*)$ ]] || return 1
    parent="${BASH_REMATCH[1]}"
    command_line="${BASH_REMATCH[2]}"
    [[ "$parent" == "$$" \
        && "$command_line" == *"$bounded_helper"* \
        && "$command_line" == *"--owner-token $run_id"* \
        && "$command_line" == *"--pid-path $pid_path"* ]]
}

child_is_owned() {
    local kind="$1" child_pid="$2" helper_pid="$3" marker="$4"
    local line parent group command_line
    validate_owned_temp_dir || return 1
    [[ "$child_pid" =~ ^[0-9]+$ && "$child_pid" -gt 1 && "$child_pid" != "$$" \
        && -n "$marker" && "$marker" == "$temp_dir/"* ]] || return 1
    line="$(bounded_capture 1000 "$ps_path" -ww -o ppid= -o pgid= -o command= -p "$child_pid")" \
        || return 1
    [[ "$line" =~ ^[[:space:]]*([0-9]+)[[:space:]]+([0-9]+)[[:space:]]+(.*)$ ]] \
        || return 1
    parent="${BASH_REMATCH[1]}"
    group="${BASH_REMATCH[2]}"
    command_line="${BASH_REMATCH[3]}"
    [[ "$group" == "$child_pid" \
        && ( "$parent" == "$helper_pid" || "$parent" == '1' || "$parent" == "$$" ) \
        && "$command_line" == *"$marker"* ]] || return 1
    if [[ "$kind" == 'agent' ]]; then
        [[ "$command_line" == *ssh-agent* && "$command_line" == *' -D '* ]]
    elif [[ "$kind" == 'sshd' ]]; then
        [[ "$command_line" == *sshd* && "$command_line" == *' -D '* \
            && "$command_line" == *"$temp_dir/current_host_ed25519"* ]]
    else
        return 1
    fi
}

start_private_agent() {
    agent_socket="$temp_dir/agent.sock"
    agent_pid_path="$temp_dir/agent.child.pid"
    agent_stdout_path="$temp_dir/agent.stdout"
    agent_stderr_path="$temp_dir/agent.stderr"
    agent_status_path="$temp_dir/agent.status"
    start_managed_process \
        "$agent_stdout_path" "$agent_stderr_path" "$agent_pid_path" "$agent_status_path" \
        "$ssh_agent_path" -D -a "$agent_socket"
    agent_helper_pid="$managed_helper_pid"
    agent_started=1
    agent_child_pid="$(wait_for_child_pid "$agent_helper_pid" "$agent_pid_path")" \
        || fail 'the private SSH Agent did not publish its owned child pid'
    helper_is_owned "$agent_helper_pid" "$agent_pid_path" \
        && child_is_owned agent "$agent_child_pid" "$agent_helper_pid" "$agent_socket" \
        || fail 'the private SSH Agent process ownership could not be verified'
}

wait_for_private_agent() {
    local deadline status
    deadline=$((SECONDS + 8))
    while (( SECONDS <= deadline )); do
        helper_is_owned "$agent_helper_pid" "$agent_pid_path" \
            && child_is_owned agent "$agent_child_pid" "$agent_helper_pid" "$agent_socket" \
            || return 1
        if [[ -S "$agent_socket" ]]; then
            if SSH_AUTH_SOCK="$agent_socket" SSH_AGENT_PID="$agent_child_pid" \
                bounded_silent 1000 "$ssh_add_path" -l; then
                return 0
            else
                status=$?
                [[ "$status" == '1' ]] && return 0
            fi
        fi
        pause_briefly
    done
    return 1
}

load_agent_key() {
    local inventory count=0 seen=''
    SSH_AUTH_SOCK="$agent_socket" SSH_AGENT_PID="$agent_child_pid" \
        bounded_silent 5000 "$ssh_add_path" "$temp_dir/agent_ed25519" \
        || fail 'could not load the Agent-only key into the private SSH Agent'
    inventory="$(SSH_AUTH_SOCK="$agent_socket" SSH_AGENT_PID="$agent_child_pid" \
        bounded_capture 5000 "$ssh_add_path" -l -E sha256)" \
        || fail 'could not inspect the private SSH Agent inventory'
    while IFS= read -r line; do
        [[ -n "$line" ]] || continue
        ((count += 1))
        if [[ "$line" =~ (^|[[:space:]])(SHA256:[A-Za-z0-9+/]{43})([[:space:]]|$) ]]; then
            seen="${BASH_REMATCH[2]}"
        else
            fail 'the private SSH Agent inventory is malformed'
        fi
    done <<<"$inventory"
    [[ "$count" == '1' && "$seen" == "$agent_fingerprint" \
        && "$seen" != "$identity_fingerprint" ]] \
        || fail 'the private SSH Agent does not contain exactly the Agent-only key'
}

start_private_sshd() {
    sshd_pid_path="$temp_dir/sshd.child.pid"
    sshd_stdout_path="$temp_dir/sshd.stdout"
    sshd_stderr_path="$temp_dir/sshd.stderr"
    sshd_status_path="$temp_dir/sshd.status"
    start_managed_process \
        "$sshd_stdout_path" "$sshd_stderr_path" "$sshd_pid_path" "$sshd_status_path" \
        "$sshd_path" -D -e -f "$sshd_config" -h "$temp_dir/current_host_ed25519"
    sshd_helper_pid="$managed_helper_pid"
    sshd_started=1
    sshd_child_pid="$(wait_for_child_pid "$sshd_helper_pid" "$sshd_pid_path")" \
        || fail 'system sshd could not start as the current non-root user; no sudo fallback is permitted'
    helper_is_owned "$sshd_helper_pid" "$sshd_pid_path" \
        && child_is_owned sshd "$sshd_child_pid" "$sshd_helper_pid" "$sshd_config" \
        || fail 'the isolated sshd process ownership could not be verified'
}

scan_current_host_key() {
    local scan_stdout="$1" scan_stderr="$2" scanned
    "$python_path" -B "$bounded_helper" \
        --timeout-ms 3000 \
        --term-grace-ms 1000 \
        --max-output-bytes "$command_output_limit" \
        --quiet \
        --stdout-path "$scan_stdout" \
        --stderr-path "$scan_stderr" \
        -- "$ssh_keyscan_path" -T 2 -p "$port" 127.0.0.1 \
        || return 1
    [[ -s "$scan_stdout" && ! -L "$scan_stdout" ]] || return 1
    scanned="$(fingerprint_for_public_key "$scan_stdout")" || return 1
    [[ "$scanned" == "$current_host_fingerprint" ]]
}

wait_for_private_sshd() {
    local deadline attempt=0 scan_stdout scan_stderr
    deadline=$((SECONDS + 10))
    while (( SECONDS <= deadline )); do
        helper_is_owned "$sshd_helper_pid" "$sshd_pid_path" \
            && child_is_owned sshd "$sshd_child_pid" "$sshd_helper_pid" "$sshd_config" \
            || return 1
        ((attempt += 1))
        scan_stdout="$temp_dir/scan.$attempt.stdout"
        scan_stderr="$temp_dir/scan.$attempt.stderr"
        if scan_current_host_key "$scan_stdout" "$scan_stderr"; then
            return 0
        fi
        pause_briefly
    done
    return 1
}

process_group_is_live() {
    local child_pid="$1"
    [[ "$child_pid" =~ ^[0-9]+$ && "$child_pid" -gt 1 ]] || return 1
    kill -0 -- "-$child_pid" 2>/dev/null
}

wait_for_processes_to_stop() {
    local helper_pid="$1" child_pid="$2" deadline
    deadline=$((SECONDS + 4))
    while (( SECONDS <= deadline )); do
        if ! kill -0 "$helper_pid" 2>/dev/null \
            && ! process_group_is_live "$child_pid"; then
            return 0
        fi
        pause_briefly
    done
    return 1
}

stop_managed_process() {
    local kind="$1" helper_pid="$2" child_pid="$3" pid_path="$4" marker="$5"
    local helper_owned=0 child_owned=0 cleanup_ok=1
    [[ "$helper_pid" =~ ^[0-9]+$ ]] || return 1
    if [[ ! "$child_pid" =~ ^[0-9]+$ \
        && -f "$pid_path" && ! -L "$pid_path" ]]; then
        IFS= read -r child_pid <"$pid_path" || child_pid=''
    fi

    if process_group_is_live "$child_pid"; then
        if child_is_owned "$kind" "$child_pid" "$helper_pid" "$marker"; then
            child_owned=1
        else
            cleanup_ok=0
        fi
    fi
    if kill -0 "$helper_pid" 2>/dev/null; then
        if helper_is_owned "$helper_pid" "$pid_path"; then
            helper_owned=1
        else
            cleanup_ok=0
        fi
    fi
    if [[ "$helper_owned" == '1' ]]; then
        if ! kill -TERM "$helper_pid" 2>/dev/null \
            && kill -0 "$helper_pid" 2>/dev/null; then
            cleanup_ok=0
        fi
    fi
    if [[ "$child_owned" == '1' ]]; then
        if ! kill -TERM -- "-$child_pid" 2>/dev/null \
            && process_group_is_live "$child_pid"; then
            cleanup_ok=0
        fi
    fi

    if ! wait_for_processes_to_stop "$helper_pid" "$child_pid"; then
        if [[ ! "$child_pid" =~ ^[0-9]+$ \
            && -f "$pid_path" && ! -L "$pid_path" ]]; then
            IFS= read -r child_pid <"$pid_path" || child_pid=''
            if process_group_is_live "$child_pid" \
                && child_is_owned "$kind" "$child_pid" "$helper_pid" "$marker"; then
                child_owned=1
                kill -TERM -- "-$child_pid" 2>/dev/null || true
            fi
        fi
        if [[ "$child_owned" == '1' ]] && process_group_is_live "$child_pid"; then
            kill -KILL -- "-$child_pid" 2>/dev/null || cleanup_ok=0
        fi
        if [[ "$helper_owned" == '1' ]] && kill -0 "$helper_pid" 2>/dev/null; then
            kill -KILL "$helper_pid" 2>/dev/null || cleanup_ok=0
        fi
        wait_for_processes_to_stop "$helper_pid" "$child_pid" || cleanup_ok=0
    fi
    if ! kill -0 "$helper_pid" 2>/dev/null; then
        wait "$helper_pid" 2>/dev/null || true
    else
        cleanup_ok=0
    fi
    process_group_is_live "$child_pid" && cleanup_ok=0
    [[ "$cleanup_ok" == '1' ]]
}

remove_private_material() {
    [[ -n "$rm_path" && -n "$temp_dir" ]] || return 1
    validate_temp_path_shape || return 1
    bounded_silent 5000 "$rm_path" -f -- \
        "$temp_dir/identity_ed25519" \
        "$temp_dir/identity_ed25519.pub" \
        "$temp_dir/agent_ed25519" \
        "$temp_dir/agent_ed25519.pub" \
        "$temp_dir/current_host_ed25519" \
        "$temp_dir/current_host_ed25519.pub" \
        "$temp_dir/previous_host_ed25519" \
        "$temp_dir/previous_host_ed25519.pub" \
        "$temp_dir/authorized_keys"
}

cleanup_resources() {
    local cleanup_ok=1
    if [[ "$sshd_started" == '1' ]]; then
        stop_managed_process sshd "$sshd_helper_pid" "$sshd_child_pid" \
            "$sshd_pid_path" "$sshd_config" || cleanup_ok=0
        sshd_started=0
    fi
    if [[ "$agent_started" == '1' ]]; then
        stop_managed_process agent "$agent_helper_pid" "$agent_child_pid" \
            "$agent_pid_path" "$agent_socket" || cleanup_ok=0
        agent_started=0
    fi

    if [[ "$temp_created" == '1' ]]; then
        if [[ "$cleanup_ok" == '1' ]] && validate_owned_temp_dir; then
            bounded_silent 10000 "$rm_path" -rf -- "$temp_dir" || cleanup_ok=0
            [[ ! -e "$temp_dir" && ! -L "$temp_dir" ]] || cleanup_ok=0
        else
            remove_private_material || cleanup_ok=0
            printf '%s\n' \
                'QA-01 macOS cleanup retained a private diagnostic directory after ownership verification failed; raw logs are suppressed.' >&2
            cleanup_ok=0
        fi
        temp_created=0
    fi
    [[ "$cleanup_ok" == '1' ]]
}

on_exit() {
    local original_status="$?" cleanup_status=0
    trap - EXIT
    trap '' INT TERM HUP
    cleanup_resources || cleanup_status=1
    if [[ "$original_status" == '0' && "$cleanup_status" != '0' ]]; then
        printf '%s\n' 'QA-01 macOS gate failed: owned resource cleanup could not be verified.' >&2
        exit 1
    fi
    exit "$original_status"
}

handle_signal() {
    exit "$1"
}

install_traps() {
    trap on_exit EXIT
    trap 'handle_signal 130' INT
    trap 'handle_signal 143' TERM
    trap 'handle_signal 129' HUP
}

prebuild_exact_gate() {
    local listed count=0 line
    listed="$(bounded_command 300000 "$gate_output_limit" \
        "$cargo_path" test --locked --release --test linux_ssh_release_gate "$gate_case" \
        -- --ignored --exact --list 2>/dev/null)" \
        || fail 'the exact release gate could not be built and enumerated'
    while IFS= read -r line; do
        [[ "$line" == "$gate_case: test" ]] && ((count += 1))
    done <<<"$listed"
    [[ "$count" == '1' ]] || fail "expected exactly one QA-01 release gate, found $count"
}

emit_sanitized_gate_output() {
    local file="$1" line
    [[ -f "$file" && ! -L "$file" ]] || return 0
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ -n "$temp_dir" ]] && line="${line//$temp_dir/<qa01-temp>}"
        [[ -n "$current_user" ]] && line="${line//$current_user/<qa01-user>}"
        [[ -n "$run_id" ]] && line="${line//$run_id/<qa01-run>}"
        [[ -n "$identity_fingerprint" ]] \
            && line="${line//$identity_fingerprint/<identity-fingerprint>}"
        [[ -n "$agent_fingerprint" ]] \
            && line="${line//$agent_fingerprint/<agent-fingerprint>}"
        [[ -n "$current_host_fingerprint" ]] \
            && line="${line//$current_host_fingerprint/<current-host-fingerprint>}"
        [[ -n "$previous_host_fingerprint" ]] \
            && line="${line//$previous_host_fingerprint/<previous-host-fingerprint>}"
        printf '%s\n' "$line"
    done <"$file"
}

run_exact_gate() {
    local gate_stdout="$temp_dir/gate.stdout" gate_stderr="$temp_dir/gate.stderr" status
    if "$python_path" -B "$bounded_helper" \
        --timeout-ms 130000 \
        --term-grace-ms 3000 \
        --max-output-bytes "$gate_output_limit" \
        --quiet \
        --stdout-path "$gate_stdout" \
        --stderr-path "$gate_stderr" \
        -- "$cargo_path" test --locked --release --test linux_ssh_release_gate "$gate_case" \
        -- --ignored --exact --nocapture --test-threads=1; then
        status=0
    else
        status=$?
    fi
    emit_sanitized_gate_output "$gate_stdout"
    emit_sanitized_gate_output "$gate_stderr" >&2
    [[ "$status" == '0' ]] || fail "the exact release gate returned status $status"
}

main() {
    cd "$repo_root" || fail 'could not enter the repository root'
    umask 077
    require_commands
    prebuild_exact_gate
    install_traps
    create_owned_temp_dir
    write_sha256sum_shim
    generate_keys_and_authorization
    pick_loopback_port
    write_sshd_config

    start_private_agent
    wait_for_private_agent \
        || fail 'the private SSH Agent did not become ready within its bounded deadline'
    load_agent_key
    start_private_sshd
    wait_for_private_sshd \
        || fail 'system sshd could not serve the isolated loopback fixture as the current non-root user; no sudo fallback is permitted'

    export SHIPFORGE_QA01_OPENSSH=1
    export SHIPFORGE_QA01_SSH_HOST=127.0.0.1
    export SHIPFORGE_QA01_SSH_PORT="$port"
    export SHIPFORGE_QA01_SSH_USER="$current_user"
    export SHIPFORGE_QA01_SSH_CURRENT_HOST_KEY="$current_host_fingerprint"
    export SHIPFORGE_QA01_SSH_PREVIOUS_HOST_KEY="$previous_host_fingerprint"
    export SHIPFORGE_QA01_SSH_IDENTITY_FILE="$temp_dir/identity_ed25519"
    export SHIPFORGE_QA01_SSH_AGENT_FINGERPRINT="$agent_fingerprint"
    export SSH_AUTH_SOCK="$agent_socket"
    export SSH_AGENT_PID="$agent_child_pid"

    run_exact_gate
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
