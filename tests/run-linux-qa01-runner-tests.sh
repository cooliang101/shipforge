#!/usr/bin/env bash
set -Eeuo pipefail

# Focused runner regressions. These tests never start Docker or ssh-agent.

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
runner="$script_dir/run-linux-qa01-release-gate.sh"
harness_parent="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
harness_temp="$(mktemp -d "$harness_parent/shipforge-qa01-runner-test.XXXXXXXXXX")"

cleanup_harness() {
    local leaf
    leaf="$(basename "$harness_temp")"
    if [[ "$harness_temp" == "$harness_parent/$leaf" \
        && "$leaf" =~ ^shipforge-qa01-runner-test\.[A-Za-z0-9]{10}$ \
        && -d "$harness_temp" && ! -L "$harness_temp" ]]; then
        rm -rf -- "$harness_temp"
    fi
}
trap cleanup_harness EXIT

assert() {
    if ! "$@"; then
        printf 'QA-01 runner regression failed: %s\n' "$*" >&2
        exit 1
    fi
}

bash -n "$runner"
source "$runner"

registration_probe="$harness_temp/registration-completed"
set +e
(
    begin_resource_registration
    kill -TERM "$BASHPID"
    printf '%s\n' registered >"$registration_probe"
    finish_resource_registration
    exit 98
)
registration_status=$?
set -e
[[ "$registration_status" == '143' && "$(<"$registration_probe")" == 'registered' ]] \
    || fail "deferred registration signal returned $registration_status before ownership was retained"

registration_race_probe="$harness_temp/registration-race-survived"
set +e
(
    begin_resource_registration
    registration_race_injected=0
    set -T
    trap 'if [[ "$registration_race_injected" == 0 && "$BASH_COMMAND" == "trap '\''exit 130'\'' INT" ]]; then registration_race_injected=1; kill -TERM "$BASHPID"; fi' DEBUG
    finish_resource_registration
    printf '%s\n' survived >"$registration_race_probe"
    exit 98
)
registration_race_status=$?
set -e
[[ "$registration_race_status" == '143' && ! -e "$registration_race_probe" ]] \
    || fail "a signal during trap restoration was swallowed with status $registration_race_status"

assert grep -Fq "cargo_build_command test --locked --release --test linux_ssh_release_gate \"\$gate_case\"" "$runner"
assert grep -Fq "cargo_gate_command test --locked --release --test linux_ssh_release_gate \"\$gate_case\"" "$runner"
assert grep -Fq -- '-- --ignored --exact --nocapture --test-threads=1' "$runner"
assert grep -Fq 'docker_command cp ' "$runner"
if grep -Eq -- '--mount([=[:space:]]|$)|--volume([=[:space:]]|$)|docker_command .*([[:space:]])-v([[:space:]]|$)' "$runner"; then
    printf 'QA-01 runner regression failed: host/container mounts are forbidden\n' >&2
    exit 1
fi
if grep -Eq 'ssh-add[[:space:]]+-D([[:space:]]|$)' "$runner"; then
    printf 'QA-01 runner regression failed: clearing an SSH Agent is forbidden\n' >&2
    exit 1
fi
assert grep -Fq 'ssh-agent -D -a "$agent_socket"' "$runner"
assert grep -Fq 'ssh_add_command "$temp_dir/agent_ed25519"' "$runner"
assert grep -Fq '"$temp_dir/identity_ed25519.pub"' "$runner"
assert grep -Fq '"$temp_dir/agent_ed25519.pub"' "$runner"
if grep -Fq 'ssh_add_command "$temp_dir/identity_ed25519"' "$runner"; then
    printf 'QA-01 runner regression failed: IdentityFile key was added to the private Agent\n' >&2
    exit 1
fi
assert grep -Fq 'timeout --signal=TERM --kill-after=2s' "$runner"
assert grep -Fq 'timeout --signal=KILL "$duration"' "$runner"
assert grep -Fq "readonly docker_endpoint='unix:///var/run/docker.sock'" "$runner"
assert grep -Fq "readonly cargo_gate_timeout='3m'" "$runner"
assert grep -Fq 'endpoint_ready_budget_ms=30000' "$runner"
assert grep -Fq 'endpoint_probe_timeout_ms=2000' "$runner"
assert grep -Fq 'cleanup_deadline_ms=$((now + cleanup_budget_ms))' "$runner"
assert grep -Fq 'cleanup_absent_confirmations=3' "$runner"
assert grep -Fq 'cleanup_settle_ms=2000' "$runner"
assert grep -Fq 'cleanup_freshness_ms=500' "$runner"
assert grep -Fq 'container rm --force "$container_id"' "$runner"
assert grep -Fq 'image rm "$image_id"' "$runner"
assert grep -Fq 'environment=\"SHIPFORGE_QA01_AUTH=identity-file\"' "$runner"
assert grep -Fq 'environment=\"SHIPFORGE_QA01_AUTH=ssh-agent\"' "$runner"

mock_bin="$harness_temp/bin"
mock_endpoint_log="$harness_temp/docker-endpoint.log"
mkdir "$mock_bin"
cat >"$mock_bin/docker" <<'MOCK_DOCKER'
#!/usr/bin/env bash
printf 'host=%s context=%s tls=%s cert=%s\n' \
    "${DOCKER_HOST-unset}" "${DOCKER_CONTEXT-unset}" \
    "${DOCKER_TLS_VERIFY-unset}" "${DOCKER_CERT_PATH-unset}" \
    >"$SHIPFORGE_QA01_DOCKER_PROBE"
printf 'arg=%s\n' "$@" >>"$SHIPFORGE_QA01_DOCKER_PROBE"
MOCK_DOCKER
chmod 700 "$mock_bin/docker"
PATH="$mock_bin:$PATH" \
    DOCKER_HOST='tcp://production.invalid:2376' \
    DOCKER_CONTEXT='production' \
    DOCKER_TLS_VERIFY='1' \
    DOCKER_CERT_PATH='/production/certs' \
    SHIPFORGE_QA01_DOCKER_PROBE="$mock_endpoint_log" \
    docker_command info
assert grep -Fxq 'host=unset context=unset tls=unset cert=unset' "$mock_endpoint_log"
assert grep -Fxq 'arg=--host' "$mock_endpoint_log"
assert grep -Fxq 'arg=unix:///var/run/docker.sock' "$mock_endpoint_log"
assert grep -Fxq 'arg=info' "$mock_endpoint_log"

endpoint_readiness_state="$harness_temp/endpoint-readiness"
printf '0\n' >"$endpoint_readiness_state"
(
    docker_command() {
        local readiness_iteration
        if [[ "$1 $2" == 'container create' ]]; then
            printf '%s\n' 'ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'
            return 0
        fi
        if [[ "$1" == 'cp' || "$1 $2" == 'container start' ]]; then
            return 0
        fi
        if [[ "$1" == 'exec' && "$3" == '/bin/sh' ]]; then
            read -r readiness_iteration <"$endpoint_readiness_state"
            ((readiness_iteration += 1))
            printf '%s\n' "$readiness_iteration" >"$endpoint_readiness_state"
            # PID liveness succeeds in rounds one and three only.
            (( readiness_iteration != 2 ))
            return
        fi
        if [[ "$1" == 'exec' && "$3" == 'ssh-keygen' ]]; then
            read -r readiness_iteration <"$endpoint_readiness_state"
            # The fingerprint becomes readable in round two. Readiness must not
            # combine that with PID liveness retained from round one.
            if (( readiness_iteration >= 2 )); then
                printf '%s\n' '256 SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA fixture (ED25519)'
                return 0
            fi
            return 1
        fi
        if [[ "$1 $2" == 'container port' ]]; then
            printf '127.0.0.1:2222\n'
            return 0
        fi
        return 97
    }
    docker_readiness_command() {
        local deadline="$1"
        shift
        docker_command "$@"
    }
    run_id='0123456789abcdef0123456789abcdef'
    image_ref="shipforge-qa01-fixture:$run_id"
    temp_dir="$harness_temp"
    container_names=()
    start_endpoint T
    [[ "$endpoint_T_port" == '2222' ]]
    [[ "$endpoint_T_fingerprint" == \
        'SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' ]]
)
read -r endpoint_readiness_iterations <"$endpoint_readiness_state"
[[ "$endpoint_readiness_iterations" == 3 ]] \
    || fail "endpoint combined readiness evidence across iterations: $endpoint_readiness_iterations"

readiness_command_log="$harness_temp/docker-readiness-budget.log"
(
    monotonic_millis() {
        printf '2000\n'
    }
    deadline_bounded_command() {
        local duration="$1"
        shift
        printf 'duration=%s\n' "$duration" >"$readiness_command_log"
        printf 'arg=%s\n' "$@" >>"$readiness_command_log"
    }
    docker_readiness_command 2501 info
)
assert grep -Fxq 'duration=0.501s' "$readiness_command_log"
assert grep -Fxq 'arg=--host' "$readiness_command_log"
assert grep -Fxq 'arg=unix:///var/run/docker.sock' "$readiness_command_log"

cleanup_command_log="$harness_temp/docker-cleanup-budget.log"
(
    monotonic_millis() {
        printf '2000\n'
    }
    cleanup_deadline_ms=2501
    deadline_bounded_command() {
        local duration="$1"
        shift
        printf 'duration=%s\n' "$duration" >"$cleanup_command_log"
        printf 'arg=%s\n' "$@" >>"$cleanup_command_log"
    }
    docker_cleanup_command info
)
assert grep -Fxq 'duration=0.501s' "$cleanup_command_log"
assert grep -Fxq 'arg=--host' "$cleanup_command_log"
assert grep -Fxq 'arg=unix:///var/run/docker.sock' "$cleanup_command_log"

mock_log="$harness_temp/docker.log"
(
    mock_now_ms=10000
    mock_wrong_owner=''
    mock_timeout_target=''
    mock_late_target='fixture-late'
    mock_late_state="$harness_temp/late-inspections"
    printf '0\n' >"$mock_late_state"
    mock_late_image_state="$harness_temp/late-image-inspections"
    printf '0\n' >"$mock_late_image_state"
    mock_image_removed=0
    mock_wrong_owner_image=0
    declare -A mock_removed_containers=()

    monotonic_millis() {
        printf '%s\n' "$mock_now_ms"
    }

    sleep_within_deadline() {
        local deadline="$1" requested_ms="$2" remaining step
        remaining=$((deadline - mock_now_ms))
        (( remaining > 0 )) || return 1
        step="$requested_ms"
        (( step > remaining )) && step="$remaining"
        mock_now_ms=$((mock_now_ms + step))
    }

    docker_cleanup_command() {
        local target identity removed_target='' late_inspections=0
        printf '%s\n' "$*" >>"$mock_log"
        if [[ "$1 $2" == 'container inspect' && "${3:-}" == '--format' ]]; then
            target="${*: -1}"
            [[ "$target" == "$mock_timeout_target" ]] && return 124
            [[ "${mock_removed_containers[$target]:-0}" == 1 ]] && return 1
            if [[ "$target" == "$mock_late_target" ]]; then
                read -r late_inspections <"$mock_late_state"
                ((late_inspections += 1))
                printf '%s\n' "$late_inspections" >"$mock_late_state"
                # Simulate a timed-out create: the first reconciliation proves
                # stable absence for more than two seconds, then the owned
                # object appears while the shared deadline is still active.
                (( late_inspections <= 30 )) && return 1
                identity='dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd'
            elif [[ "$target" == 'fixture-a' ]]; then
                identity='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
            elif [[ "$target" == 'fixture-b' ]]; then
                identity='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
            else
                identity='cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc'
            fi
            if [[ "$target" == "$mock_wrong_owner" ]]; then
                printf '%s|another-run\n' "$identity"
            else
                printf '%s|%s\n' "$identity" "$run_id"
            fi
            return 0
        fi
        if [[ "$1 $2" == 'container ls' ]]; then
            [[ "$*" == *"$mock_timeout_target"* && -n "$mock_timeout_target" ]] && return 124
            return 0
        fi
        if [[ "$1 $2" == 'image inspect' && "${3:-}" == '--format' ]]; then
            (( mock_image_removed )) && return 1
            read -r late_inspections <"$mock_late_image_state"
            ((late_inspections += 1))
            printf '%s\n' "$late_inspections" >"$mock_late_image_state"
            (( late_inspections <= 30 )) && return 1
            if (( mock_wrong_owner_image )); then
                printf 'sha256:%s|another-run\n' \
                    'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'
            else
                printf 'sha256:%s|%s\n' \
                    'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee' "$run_id"
            fi
            return 0
        fi
        if [[ "$1 $2" == 'image ls' ]]; then
            return 0
        fi
        if [[ "$1 $2" == 'container rm' ]]; then
            case "${*: -1}" in
                aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)
                    removed_target='fixture-a'
                    ;;
                bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)
                    removed_target='fixture-b'
                    ;;
                cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc)
                    removed_target='fixture-c'
                    ;;
                dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd)
                    removed_target='fixture-late'
                    ;;
            esac
            [[ -n "$removed_target" ]] || return 97
            mock_removed_containers[$removed_target]=1
            return 0
        fi
        if [[ "$1 $2" == 'image rm' ]]; then
            [[ "${*: -1}" == \
                'sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee' ]] \
                || return 97
            mock_image_removed=1
            return 0
        fi
        return 97
    }

    run_id='0123456789abcdef0123456789abcdef'
    container_names=('fixture-a' 'fixture-b' 'fixture-late')
    image_ref="shipforge-qa01-fixture:$run_id"
    image_candidate=1
    agent_started=0
    temp_created=0
    cleanup_resources
    assert grep -Fq 'container rm --force aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' "$mock_log"
    assert grep -Fq 'container rm --force bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' "$mock_log"
    assert grep -Fq 'container rm --force dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd' "$mock_log"
    assert grep -Fq 'image rm sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee' "$mock_log"

    : >"$mock_log"
    mock_removed_containers=()
    container_names=('fixture-c')
    image_candidate=0
    mock_wrong_owner="${container_names[0]}"
    if cleanup_resources; then
        printf 'QA-01 runner regression failed: wrong container ownership was accepted\n' >&2
        exit 1
    fi
    if grep -Fq 'container rm --force' "$mock_log"; then
        printf 'QA-01 runner regression failed: wrong-owner container reached removal\n' >&2
        exit 1
    fi

    : >"$mock_log"
    container_names=()
    image_candidate=1
    mock_wrong_owner=''
    mock_wrong_owner_image=1
    mock_image_removed=0
    printf '30\n' >"$mock_late_image_state"
    if cleanup_resources; then
        printf 'QA-01 runner regression failed: wrong image ownership was accepted\n' >&2
        exit 1
    fi
    if grep -Fq 'image rm' "$mock_log"; then
        printf 'QA-01 runner regression failed: wrong-owner image reached removal\n' >&2
        exit 1
    fi

    mock_wrong_owner=''
    mock_wrong_owner_image=0
    mock_timeout_target='fixture-hung'
    mock_removed_containers=()
    : >"$mock_log"
    container_names=('fixture-hung' 'fixture-b')
    image_candidate=0
    if cleanup_resources; then
        printf 'QA-01 runner regression failed: a timed-out cleanup command was accepted\n' >&2
        exit 1
    fi
    assert grep -Fq 'container rm --force bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' "$mock_log"
)

# A later resource can consume the final probe budget after an earlier resource
# was observed absent. The incomplete sweep must invalidate both observations.
stale_sweep_log="$harness_temp/stale-sweep.log"
stale_time_state="$harness_temp/stale-time"
stale_probe_state="$harness_temp/stale-probes"
printf '10000\n' >"$stale_time_state"
printf '0\n' >"$stale_probe_state"
(
    monotonic_millis() {
        command cat "$stale_time_state"
    }
    sleep_within_deadline() {
        local deadline="$1" requested_ms="$2" current step
        read -r current <"$stale_time_state"
        (( current < deadline )) || return 1
        step="$requested_ms"
        (( current + step > deadline )) && step=$((deadline - current))
        printf '%s\n' "$((current + step))" >"$stale_time_state"
    }
    docker_cleanup_command() {
        local target count current
        read -r current <"$stale_time_state"
        (( current < cleanup_deadline_ms )) || return 124
        if [[ "$1 $2" == 'container inspect' ]]; then
            target="${*: -1}"
            if [[ "$target" == 'fixture-stale-b' ]]; then
                read -r count <"$stale_probe_state"
                ((count += 1))
                printf '%s\n' "$count" >"$stale_probe_state"
                if (( count == 30 )); then
                    printf '%s\n' "$cleanup_deadline_ms" >"$stale_time_state"
                    return 124
                fi
            fi
            return 1
        fi
        [[ "$1 $2" == 'container ls' ]] && return 0
        return 97
    }
    run_id='0123456789abcdef0123456789abcdef'
    container_names=('fixture-stale-a' 'fixture-stale-b')
    image_candidate=0
    agent_started=0
    temp_created=0
    if cleanup_resources 2>"$stale_sweep_log"; then
        exit 96
    fi
)
assert grep -Fq 'fixture-stale-a' "$stale_sweep_log"
assert grep -Fq 'fixture-stale-b' "$stale_sweep_log"

container_names=()
temp_parent="$harness_temp"
temp_dir="$(mktemp -d "$temp_parent/shipforge-qa01-run.XXXXXXXXXX")"
temp_created=1
cleanup_resources
assert test ! -e "$temp_dir"

temp_dir="$harness_temp/not-owned"
mkdir "$temp_dir"
if cleanup_resources; then
    printf 'QA-01 runner regression failed: unowned temporary directory was accepted\n' >&2
    exit 1
fi
assert test -d "$temp_dir"
rmdir "$temp_dir"
temp_created=0

agent_probe_log="$harness_temp/agent-probes.log"
cat >"$mock_bin/ssh-add" <<'MOCK_SSH_ADD'
#!/usr/bin/env bash
# Prove the hard outer deadline also bounds a client that ignores TERM.
trap '' TERM
exec tail -f /dev/null
MOCK_SSH_ADD
chmod 700 "$mock_bin/ssh-add"
agent_probe_started="$(monotonic_millis)"
if (
    export PATH="$mock_bin:$PATH"
    export SHIPFORGE_QA01_AGENT_PROBE_LOG="$agent_probe_log"
    agent_pid="$$"
    agent_socket="$harness_temp/mock-agent.sock"
    private_agent_is_owned() {
        return 0
    }
    fail() {
        return 1
    }
    deadline_bounded_command() {
        local duration="$1"
        shift
        printf '%s|%s|%s\n' "$duration" "$1" "${*:2}" \
            >>"$SHIPFORGE_QA01_AGENT_PROBE_LOG"
        command timeout --signal=KILL "$duration" "$@"
    }
    wait_for_private_agent
); then
    printf 'QA-01 runner regression failed: a hanging ssh-add probe was accepted\n' >&2
    exit 1
fi
agent_probe_finished="$(monotonic_millis)"
agent_probe_elapsed=$((agent_probe_finished - agent_probe_started))
if (( agent_probe_elapsed < 4900 || agent_probe_elapsed > 6500 )); then
    printf 'QA-01 runner regression failed: Agent deadline took %sms instead of about 5000ms\n' \
        "$agent_probe_elapsed" >&2
    exit 1
fi
agent_probe_count=0
while IFS='|' read -r duration executable arguments; do
    ((agent_probe_count += 1))
    if [[ ! "$duration" =~ ^([01])\.([0-9]{3})s$ \
        || ( "${BASH_REMATCH[1]}" == 1 && "${BASH_REMATCH[2]}" != 000 ) \
        || "$duration" == '0.000s' ]]; then
        printf 'QA-01 runner regression failed: Agent probe timeout exceeds one second: %s\n' \
            "$duration" >&2
        exit 1
    fi
    [[ "$executable" == 'ssh-add' && "$arguments" == '-l' ]] \
        || fail "unexpected Agent probe invocation: $executable $arguments"
done <"$agent_probe_log"
(( agent_probe_count >= 4 )) \
    || fail "expected repeated bounded Agent probes, found $agent_probe_count"

agent_pid="$$"
if private_agent_is_owned; then
    printf 'QA-01 runner regression failed: a non-child process was treated as the private Agent\n' >&2
    exit 1
fi

printf 'QA-01 Linux runner safety tests passed.\n'
