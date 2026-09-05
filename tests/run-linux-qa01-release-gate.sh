#!/usr/bin/env bash
set -Eeuo pipefail

# Native-Linux QA-01 gate. All credentials, Agent state, containers, ports, and
# image tags belong to this process and are removed by the EXIT trap.

gate_case='qa01_release_gate_validates_host_key_rotation_agent_sftp_and_cancellation'
script_path="${BASH_SOURCE[0]}"
if [[ "$script_path" == */* ]]; then
    script_parent="${script_path%/*}"
else
    script_parent='.'
fi
script_dir="$(cd "$script_parent" && pwd -P)"
repository="$(cd "$script_dir/.." && pwd -P)"
fixture_context="$script_dir/fixtures/openssh"
readonly setup_timeout='15s'
readonly docker_timeout='20s'
readonly docker_build_timeout='5m'
readonly cleanup_timeout='15s'
readonly cleanup_budget_ms=15000
readonly cleanup_probe_timeout_ms=1000
readonly cleanup_settle_ms=2000
readonly cleanup_freshness_ms=500
readonly cleanup_absent_confirmations=3
readonly cleanup_retry_delay_ms=100
readonly agent_ready_budget_ms=5000
readonly agent_probe_timeout_ms=1000
readonly agent_retry_delay_ms=100
readonly endpoint_ready_budget_ms=30000
readonly endpoint_probe_timeout_ms=2000
readonly cargo_build_timeout='10m'
readonly cargo_gate_timeout='3m'
readonly docker_endpoint='unix:///var/run/docker.sock'

run_id=''
image_ref=''
image_candidate=0
temp_parent=''
temp_dir=''
temp_created=0
agent_pid=''
agent_socket=''
agent_started=0
container_names=()
cleanup_deadline_ms=0
deferred_signal_status=0

monotonic_millis() {
    local uptime whole fraction
    IFS=' ' read -r uptime _ </proc/uptime || return 1
    whole="${uptime%%.*}"
    fraction="${uptime#*.}000"
    fraction="${fraction:0:3}"
    [[ "$whole" =~ ^[0-9]+$ && "$fraction" =~ ^[0-9]{3}$ ]] || return 1
    printf '%s\n' "$((10#$whole * 1000 + 10#$fraction))"
}

remaining_millis() {
    local deadline="$1" now
    now="$(monotonic_millis)" || return 1
    if (( now >= deadline )); then
        printf '0\n'
    else
        printf '%s\n' "$((deadline - now))"
    fi
}

milliseconds_duration() {
    local duration_ms="$1"
    (( duration_ms > 0 )) || return 1
    printf '%d.%03ds\n' "$((duration_ms / 1000))" "$((duration_ms % 1000))"
}

sleep_within_deadline() {
    local deadline="$1" requested_ms="$2" remaining sleep_ms sleep_duration
    remaining="$(remaining_millis "$deadline")" || return 1
    (( remaining > 0 )) || return 1
    sleep_ms="$requested_ms"
    (( sleep_ms > remaining )) && sleep_ms="$remaining"
    printf -v sleep_duration '%d.%03d' "$((sleep_ms / 1000))" "$((sleep_ms % 1000))"
    sleep "$sleep_duration"
}

bounded_command() {
    local duration="$1"
    shift
    command timeout --signal=TERM --kill-after=2s "$duration" "$@"
}

deadline_bounded_command() {
    local duration="$1"
    shift
    # Cleanup reconciliation and readiness probing have one outer deadline.
    # A grace period per invocation would let a TERM-resistant client multiply
    # that budget, so these read/probe clients are hard-stopped at their slice.
    command timeout --signal=KILL "$duration" "$@"
}

docker_command() {
    bounded_command "$docker_timeout" env -u DOCKER_HOST -u DOCKER_CONTEXT \
        -u DOCKER_TLS_VERIFY -u DOCKER_CERT_PATH \
        docker --host "$docker_endpoint" "$@"
}

docker_build_command() {
    bounded_command "$docker_build_timeout" env -u DOCKER_HOST -u DOCKER_CONTEXT \
        -u DOCKER_TLS_VERIFY -u DOCKER_CERT_PATH \
        docker --host "$docker_endpoint" "$@"
}

docker_cleanup_command() {
    local command_timeout="$cleanup_timeout" remaining
    if (( cleanup_deadline_ms > 0 )); then
        remaining="$(remaining_millis "$cleanup_deadline_ms")" || return 125
        (( remaining > 0 )) || return 124
        (( remaining > cleanup_probe_timeout_ms )) && remaining="$cleanup_probe_timeout_ms"
        command_timeout="$(milliseconds_duration "$remaining")" || return 125
    fi
    deadline_bounded_command "$command_timeout" env -u DOCKER_HOST -u DOCKER_CONTEXT \
        -u DOCKER_TLS_VERIFY -u DOCKER_CERT_PATH \
        docker --host "$docker_endpoint" "$@"
}

docker_readiness_command() {
    local deadline="$1" remaining command_timeout
    shift
    remaining="$(remaining_millis "$deadline")" || return 125
    (( remaining > 0 )) || return 124
    (( remaining > endpoint_probe_timeout_ms )) && remaining="$endpoint_probe_timeout_ms"
    command_timeout="$(milliseconds_duration "$remaining")" || return 125
    deadline_bounded_command "$command_timeout" env -u DOCKER_HOST -u DOCKER_CONTEXT \
        -u DOCKER_TLS_VERIFY -u DOCKER_CERT_PATH \
        docker --host "$docker_endpoint" "$@"
}

setup_command() {
    bounded_command "$setup_timeout" "$@"
}

ssh_add_command() {
    bounded_command "$setup_timeout" ssh-add "$@"
}

ssh_add_probe_command() {
    local duration="$1"
    shift
    deadline_bounded_command "$duration" ssh-add "$@"
}

cargo_build_command() {
    bounded_command "$cargo_build_timeout" cargo "$@"
}

cargo_gate_command() {
    bounded_command "$cargo_gate_timeout" cargo "$@"
}

fail() {
    printf 'QA-01 Linux runner: %s\n' "$*" >&2
    exit 1
}

defer_registration_signal() {
    (( deferred_signal_status == 0 )) && deferred_signal_status="$1"
}

begin_resource_registration() {
    deferred_signal_status=0
    trap 'defer_registration_signal 130' INT
    trap 'defer_registration_signal 143' TERM
    trap 'defer_registration_signal 129' HUP
}

finish_resource_registration() {
    trap 'exit 130' INT
    trap 'exit 143' TERM
    trap 'exit 129' HUP
    local pending_status="$deferred_signal_status"
    (( pending_status == 0 )) || exit "$pending_status"
}

cleanup_problem() {
    printf 'QA-01 cleanup: %s\n' "$*" >&2
}

private_agent_is_owned() {
    local observed_pid observed_command observed_state observed_parent remainder
    [[ "$agent_pid" =~ ^[1-9][0-9]*$ && -d "/proc/$agent_pid" ]] || return 1
    read -r observed_pid observed_command observed_state observed_parent remainder \
        <"/proc/$agent_pid/stat" || return 1
    [[ "$observed_pid" == "$agent_pid" \
        && "$observed_parent" == "$$" \
        && "$observed_command" == '(ssh-agent)' ]]
}

private_agent_state() {
    local observed_pid observed_command observed_state observed_parent remainder
    [[ -d "/proc/$agent_pid" ]] || return 1
    read -r observed_pid observed_command observed_state observed_parent remainder \
        <"/proc/$agent_pid/stat" || return 1
    printf '%s' "$observed_state"
}

stop_private_agent() {
    local state=''
    if (( ! agent_started )); then
        return 0
    fi
    if [[ -d "/proc/$agent_pid" ]]; then
        if ! private_agent_is_owned; then
            cleanup_problem "refused to signal unowned process $agent_pid"
            return 1
        fi
        kill -TERM "$agent_pid" 2>/dev/null || true
        for _ in {1..50}; do
            [[ -d "/proc/$agent_pid" ]] || break
            state="$(private_agent_state)" || state=''
            [[ "$state" == Z* ]] && break
            sleep 0.1
        done
        if [[ -d "/proc/$agent_pid" && "$state" != Z* ]]; then
            if ! private_agent_is_owned; then
                cleanup_problem "lost ownership of Agent process $agent_pid"
                return 1
            fi
            if ! kill -KILL "$agent_pid" 2>/dev/null; then
                cleanup_problem "could not terminate private Agent process $agent_pid"
                return 1
            fi
            for _ in {1..20}; do
                [[ -d "/proc/$agent_pid" ]] || break
                state="$(private_agent_state)" || state=''
                [[ "$state" == Z* ]] && break
                sleep 0.1
            done
            if [[ -d "/proc/$agent_pid" && "$state" != Z* ]]; then
                cleanup_problem "private Agent process $agent_pid resisted termination"
                return 1
            fi
        fi
        wait "$agent_pid" 2>/dev/null || true
    else
        wait "$agent_pid" 2>/dev/null || true
    fi
    if [[ -d "/proc/$agent_pid" ]]; then
        cleanup_problem "private SSH Agent process $agent_pid is still present"
        return 1
    fi
    return 0
}

temp_dir_is_owned() {
    local leaf
    [[ -n "$temp_parent" && -n "$temp_dir" && -d "$temp_dir" && ! -L "$temp_dir" ]] || return 1
    leaf="${temp_dir##*/}"
    [[ "$temp_dir" == "$temp_parent/$leaf" \
        && "$leaf" =~ ^shipforge-qa01-run\.[A-Za-z0-9]{10}$ \
        && -O "$temp_dir" ]]
}

container_name_is_absent() {
    local expected="$1" names name
    names="$(docker_cleanup_command container ls --all --filter "name=$expected" --format '{{.Names}}' 2>/dev/null)" \
        || return 1
    while IFS= read -r name; do
        [[ "$name" == "$expected" ]] && return 1
    done <<<"$names"
    return 0
}

image_ref_is_absent() {
    local images
    images="$(docker_cleanup_command image ls --quiet --filter "reference=$image_ref" 2>/dev/null)" \
        || return 1
    [[ -z "$images" ]]
}

probe_container_cleanup() {
    local name="$1" observation container_id label
    cleanup_probe_state='unknown'
    observation="$(docker_cleanup_command container inspect \
        --format '{{.Id}}|{{ index .Config.Labels "shipforge.test.run" }}' "$name" 2>/dev/null)"
    if [[ $? -ne 0 ]]; then
        if container_name_is_absent "$name"; then
            cleanup_probe_state='absent'
        fi
        return 0
    fi

    container_id="${observation%%|*}"
    label="${observation#*|}"
    if [[ ! "$container_id" =~ ^[0-9a-f]{64}$ || "$label" != "$run_id" ]]; then
        cleanup_problem "container $name has invalid identity or ownership; it was preserved"
        cleanup_probe_state='failed'
        return 0
    fi
    # Always remove by the immutable identity returned by the owned inspection.
    if docker_cleanup_command container rm --force "$container_id" >/dev/null; then
        cleanup_probe_state='present'
    fi
}

probe_image_cleanup() {
    local observation image_id label
    cleanup_probe_state='unknown'
    observation="$(docker_cleanup_command image inspect \
        --format '{{.Id}}|{{ index .Config.Labels "shipforge.test.run" }}' "$image_ref" 2>/dev/null)"
    if [[ $? -ne 0 ]]; then
        if image_ref_is_absent; then
            cleanup_probe_state='absent'
        fi
        return 0
    fi

    image_id="${observation%%|*}"
    label="${observation#*|}"
    if [[ ! "$image_id" =~ ^sha256:[0-9a-f]{64}$ || "$label" != "$run_id" ]]; then
        cleanup_problem "image $image_ref has invalid identity or ownership; it was preserved"
        cleanup_probe_state='failed'
        return 0
    fi
    # A timed-out build can publish the tag late, so remove the verified image ID.
    if docker_cleanup_command image rm "$image_id" >/dev/null; then
        cleanup_probe_state='present'
    fi
}

cleanup_resources() {
    local had_errexit=0 failures=0 now active=0 index name sweep_complete=1 remaining=0
    local container_count="${#container_names[@]}" image_active=0 image_failed=0
    local image_absent_since=0 image_last_absent=0 image_absent_count=0 image_last_state='unknown'
    local -a container_failed=() container_absent_since=() container_last_absent=()
    local -a container_absent_count=() container_last_state=()
    [[ $- == *e* ]] && had_errexit=1
    set +e

    now="$(monotonic_millis)"
    if [[ $? -ne 0 ]]; then
        cleanup_problem 'could not establish the Docker cleanup deadline'
        failures=1
        now=0
        cleanup_deadline_ms=0
    else
        cleanup_deadline_ms=$((now + cleanup_budget_ms))
    fi

    for ((index = 0; index < container_count; index += 1)); do
        container_failed[index]=0
        container_absent_since[index]=0
        container_last_absent[index]=0
        container_absent_count[index]=0
        container_last_state[index]='unknown'
    done
    (( image_candidate )) && image_active=1

    if (( cleanup_deadline_ms > 0 )); then
        while :; do
            now="$(monotonic_millis)" || now="$cleanup_deadline_ms"
            (( now < cleanup_deadline_ms )) || break
            active=0
            sweep_complete=1
            for ((index = 0; index < container_count; index += 1)); do
                (( container_failed[index] )) && continue
                active=1
                name="${container_names[index]}"
                remaining="$(remaining_millis "$cleanup_deadline_ms")" || remaining=0
                if (( remaining <= 0 )); then
                    sweep_complete=0
                    break
                fi
                probe_container_cleanup "$name"
                now="$(monotonic_millis)" || now="$cleanup_deadline_ms"
                case "$cleanup_probe_state" in
                    absent)
                        if (( container_absent_since[index] == 0 )); then
                            container_absent_since[index]="$now"
                            container_absent_count[index]=1
                        else
                            ((container_absent_count[index] += 1))
                        fi
                        container_last_absent[index]="$now"
                        container_last_state[index]='absent'
                        ;;
                    failed)
                        container_failed[index]=1
                        container_last_state[index]='failed'
                        failures=1
                        ;;
                    *)
                        # Present, removal-unknown, and observation failures all
                        # invalidate an earlier absence streak.
                        container_absent_since[index]=0
                        container_last_absent[index]=0
                        container_absent_count[index]=0
                        container_last_state[index]="$cleanup_probe_state"
                        ;;
                esac
                if (( now >= cleanup_deadline_ms )); then
                    sweep_complete=0
                    break
                fi
            done

            if (( sweep_complete && image_active && ! image_failed )); then
                active=1
                remaining="$(remaining_millis "$cleanup_deadline_ms")" || remaining=0
                if (( remaining <= 0 )); then
                    sweep_complete=0
                else
                    probe_image_cleanup
                    now="$(monotonic_millis)" || now="$cleanup_deadline_ms"
                    case "$cleanup_probe_state" in
                        absent)
                            if (( image_absent_since == 0 )); then
                                image_absent_since="$now"
                                image_absent_count=1
                            else
                                ((image_absent_count += 1))
                            fi
                            image_last_absent="$now"
                            image_last_state='absent'
                            ;;
                        failed)
                            image_failed=1
                            image_last_state='failed'
                            failures=1
                            ;;
                        *)
                            image_absent_since=0
                            image_last_absent=0
                            image_absent_count=0
                            image_last_state="$cleanup_probe_state"
                            ;;
                    esac
                    (( now < cleanup_deadline_ms )) || sweep_complete=0
                fi
            fi

            if (( ! sweep_complete )); then
                for ((index = 0; index < container_count; index += 1)); do
                    (( container_failed[index] )) || container_last_state[index]='incomplete-sweep'
                done
                (( image_active && ! image_failed )) && image_last_state='incomplete-sweep'
                break
            fi

            # Even a stable-looking absence remains under observation until the
            # one shared deadline: a killed Docker client can leave daemon work
            # that publishes its owned object after the initial settle window.
            (( active )) || break
            now="$(monotonic_millis)" || now="$cleanup_deadline_ms"
            (( now < cleanup_deadline_ms )) || break
            sleep_within_deadline "$cleanup_deadline_ms" "$cleanup_retry_delay_ms" || true
        done

        now="$(monotonic_millis)" || now="$cleanup_deadline_ms"
        for ((index = 0; index < container_count; index += 1)); do
            (( container_failed[index] )) && continue
            if [[ "${container_last_state[index]}" != 'absent' ]] \
                || (( container_absent_count[index] < cleanup_absent_confirmations \
                    || container_last_absent[index] - container_absent_since[index] < cleanup_settle_ms \
                    || now - container_last_absent[index] > cleanup_freshness_ms )); then
                cleanup_problem "could not prove disposable container ${container_names[index]} stably absent before the shared cleanup deadline"
                failures=1
            fi
        done
        if (( image_active && ! image_failed )) \
            && { [[ "$image_last_state" != 'absent' ]] \
                || (( image_absent_count < cleanup_absent_confirmations \
                    || image_last_absent - image_absent_since < cleanup_settle_ms \
                    || now - image_last_absent > cleanup_freshness_ms )); }; then
            cleanup_problem "could not prove disposable image $image_ref stably absent before the shared cleanup deadline"
            failures=1
        fi
    fi

    cleanup_deadline_ms=0

    if ! stop_private_agent; then
        failures=1
    fi

    if (( temp_created )); then
        if ! temp_dir_is_owned; then
            cleanup_problem "refused to remove an unverified temporary directory: $temp_dir"
            failures=1
        elif ! bounded_command "$cleanup_timeout" rm -rf -- "$temp_dir"; then
            cleanup_problem "could not remove disposable directory $temp_dir"
            failures=1
        elif [[ -e "$temp_dir" || -L "$temp_dir" ]]; then
            cleanup_problem "disposable directory still exists after removal: $temp_dir"
            failures=1
        fi
    fi

    (( had_errexit )) && set -e
    return "$failures"
}

on_exit() {
    local operation_status=$? cleanup_status=0
    trap - EXIT
    trap '' INT TERM HUP
    set +e
    cleanup_resources || cleanup_status=$?
    if (( cleanup_status == 0 )); then
        printf 'QA-01 disposable Linux resources removed.\n'
    elif (( operation_status == 0 )); then
        operation_status=1
    fi
    exit "$operation_status"
}

require_commands() {
    local executable
    for executable in cargo docker ssh-keygen ssh-agent ssh-add sha256sum timeout mktemp awk chmod rm env; do
        command -v "$executable" >/dev/null 2>&1 || fail "missing prerequisite: $executable"
    done
    docker_command info >/dev/null 2>&1 || fail 'Docker daemon is unavailable'
}

start_private_agent() {
    begin_resource_registration
    agent_socket="$temp_dir/agent.sock"
    ssh-agent -D -a "$agent_socket" >"$temp_dir/ssh-agent.log" 2>&1 &
    agent_pid=$!
    agent_started=1
    finish_resource_registration
    wait_for_private_agent
}

wait_for_private_agent() {
    local probe_status=0 now deadline remaining probe_timeout
    now="$(monotonic_millis)" || fail 'could not establish the SSH Agent readiness deadline'
    deadline=$((now + agent_ready_budget_ms))
    while :; do
        remaining="$(remaining_millis "$deadline")" \
            || fail 'could not read the SSH Agent readiness deadline'
        (( remaining > 0 )) || break
        (( remaining > agent_probe_timeout_ms )) && remaining="$agent_probe_timeout_ms"
        probe_timeout="$(milliseconds_duration "$remaining")" \
            || fail 'could not calculate the SSH Agent probe timeout'
        if SSH_AUTH_SOCK="$agent_socket" SSH_AGENT_PID="$agent_pid" \
            ssh_add_probe_command "$probe_timeout" -l >/dev/null 2>&1; then
            probe_status=0
        else
            probe_status=$?
        fi
        if (( probe_status == 0 || probe_status == 1 )); then
            export SSH_AUTH_SOCK="$agent_socket"
            export SSH_AGENT_PID="$agent_pid"
            return 0
        fi
        private_agent_is_owned || fail 'private SSH Agent exited before becoming ready'
        sleep_within_deadline "$deadline" "$agent_retry_delay_ms" || true
    done
    fail 'private SSH Agent did not become ready within five seconds'
}

start_endpoint() {
    local suffix="$1" name ref fingerprint='' candidate_fingerprint='' fingerprint_output=''
    local binding='' pid_live=0 readiness=0 deadline now remaining
    name="shipforge-qa01-$run_id-${suffix,,}"
    container_names+=("$name")
    ref="$(docker_command container create --name "$name" \
        --label "shipforge.test.run=$run_id" \
        --publish '127.0.0.1::22' "$image_ref")" \
        || fail "could not create endpoint $suffix"
    [[ "$ref" =~ ^[0-9a-f]{64}$ ]] || fail "endpoint $suffix returned an invalid container identity"
    docker_command cp "$temp_dir/authorized_keys" "$ref:/fixture/authorized_keys" >/dev/null \
        || fail "could not copy the disposable public key to endpoint $suffix"
    docker_command container start "$ref" >/dev/null || fail "could not start endpoint $suffix"

    now="$(monotonic_millis)" || fail "could not establish endpoint $suffix readiness deadline"
    deadline=$((now + endpoint_ready_budget_ms))
    while :; do
        remaining="$(remaining_millis "$deadline")" \
            || fail "could not read endpoint $suffix readiness deadline"
        (( remaining > 0 )) || break
        pid_live=0
        candidate_fingerprint=''
        fingerprint_output=''
        if docker_readiness_command "$deadline" exec "$ref" /bin/sh -c \
            'test -s /run/sshd/shipforge.pid && kill -0 "$(cat /run/sshd/shipforge.pid)"' \
            >/dev/null 2>&1; then
            pid_live=1
        fi
        fingerprint_output="$(docker_readiness_command "$deadline" exec "$ref" ssh-keygen -lf /fixture/host_ed25519.pub -E sha256 2>/dev/null)" || true
        if [[ "$fingerprint_output" =~ (SHA256:[A-Za-z0-9+/]{43}) ]]; then
            candidate_fingerprint="${BASH_REMATCH[1]}"
        fi
        if (( pid_live )) && [[ -n "$candidate_fingerprint" ]]; then
            readiness=1
            fingerprint="$candidate_fingerprint"
            break
        fi
        sleep_within_deadline "$deadline" 100 || true
    done
    [[ -n "$fingerprint" && $readiness -eq 1 ]] || fail "endpoint $suffix did not become ready"

    binding="$(docker_readiness_command "$deadline" container port "$ref" '22/tcp')" \
        || fail "could not read endpoint $suffix port within its readiness deadline"
    [[ "$binding" =~ ^127\.0\.0\.1:([1-9][0-9]*)$ ]] \
        || fail "endpoint $suffix is not bound to one IPv4 loopback port"
    printf -v "endpoint_${suffix}_port" '%s' "${BASH_REMATCH[1]}"
    printf -v "endpoint_${suffix}_fingerprint" '%s' "$fingerprint"
}

run_release_gate() {
    local listed line count=0
    cd "$repository"
    listed="$(cargo_build_command test --locked --release --test linux_ssh_release_gate "$gate_case" \
        -- --ignored --exact --list)" || fail 'could not build and discover the QA-01 release gate'
    while IFS= read -r line; do
        [[ "$line" == "$gate_case: test" ]] && ((count += 1))
    done <<<"$listed"
    [[ "$count" == '1' ]] || fail "expected exactly one release gate, found $count"
    cargo_gate_command test --locked --release --test linux_ssh_release_gate "$gate_case" \
        -- --ignored --exact --nocapture --test-threads=1
}

main() {
    local hash_output identity_output agent_output identity_fingerprint agent_fingerprint temp_setup_status=0
    require_commands
    trap on_exit EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    trap 'exit 129' HUP

    temp_parent="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
    begin_resource_registration
    temp_dir="$(setup_command mktemp -d "$temp_parent/shipforge-qa01-run.XXXXXXXXXX")" \
        || temp_setup_status=$?
    [[ -n "$temp_dir" ]] && temp_created=1
    finish_resource_registration
    (( temp_setup_status == 0 && temp_created == 1 )) \
        || fail 'could not create the disposable temporary directory'
    setup_command chmod 700 "$temp_dir"
    hash_output="$(printf '%s' "$temp_dir:$$" | setup_command sha256sum)"
    run_id="${hash_output%% *}"
    run_id="${run_id:0:32}"
    [[ "$run_id" =~ ^[0-9a-f]{32}$ ]] || fail 'could not create a safe run identifier'
    image_ref="shipforge-qa01-fixture:$run_id"
    image_candidate=1
    printf 'QA-01 disposable Linux run: %s\n' "$run_id"

    setup_command ssh-keygen -q -t ed25519 -N '' -f "$temp_dir/identity_ed25519" \
        || fail 'could not create the disposable IdentityFile key'
    setup_command ssh-keygen -q -t ed25519 -N '' -f "$temp_dir/agent_ed25519" \
        || fail 'could not create the disposable Agent key'
    identity_output="$(setup_command ssh-keygen -lf "$temp_dir/identity_ed25519.pub" -E sha256)" \
        || fail 'could not read the IdentityFile key fingerprint'
    agent_output="$(setup_command ssh-keygen -lf "$temp_dir/agent_ed25519.pub" -E sha256)" \
        || fail 'could not read the Agent key fingerprint'
    [[ "$identity_output" =~ (^|[[:space:]])(SHA256:[A-Za-z0-9+/]{43})([[:space:]]|$) ]] \
        || fail 'IdentityFile key fingerprint is malformed'
    identity_fingerprint="${BASH_REMATCH[2]}"
    [[ "$agent_output" =~ (^|[[:space:]])(SHA256:[A-Za-z0-9+/]{43})([[:space:]]|$) ]] \
        || fail 'Agent key fingerprint is malformed'
    agent_fingerprint="${BASH_REMATCH[2]}"
    [[ "$identity_fingerprint" =~ ^SHA256:[A-Za-z0-9+/]{43}$ ]] \
        || fail 'IdentityFile key fingerprint is malformed'
    [[ "$agent_fingerprint" =~ ^SHA256:[A-Za-z0-9+/]{43}$ \
        && "$agent_fingerprint" != "$identity_fingerprint" ]] \
        || fail 'IdentityFile and Agent keys must be distinct'
    {
        setup_command awk 'NF { print "environment=\"SHIPFORGE_QA01_AUTH=identity-file\" " $0 }' \
            "$temp_dir/identity_ed25519.pub"
        setup_command awk 'NF { print "environment=\"SHIPFORGE_QA01_AUTH=ssh-agent\" " $0 }' \
            "$temp_dir/agent_ed25519.pub"
    } >"$temp_dir/authorized_keys" \
        || fail 'could not assemble the two-key authorized_keys fixture'

    start_private_agent
    ssh_add_command "$temp_dir/agent_ed25519" >/dev/null \
        || fail 'could not add the Agent-only key to the private SSH Agent'

    docker_build_command build --label "shipforge.test.run=$run_id" \
        -t "$image_ref" "$fixture_context"
    start_endpoint A
    start_endpoint B
    [[ "$endpoint_A_fingerprint" != "$endpoint_B_fingerprint" ]] \
        || fail 'the independent endpoint Host Keys unexpectedly match'

    export SHIPFORGE_QA01_OPENSSH=1
    export SHIPFORGE_QA01_SSH_HOST=127.0.0.1
    export SHIPFORGE_QA01_SSH_PORT="$endpoint_A_port"
    export SHIPFORGE_QA01_SSH_USER=deploy
    export SHIPFORGE_QA01_SSH_CURRENT_HOST_KEY="$endpoint_A_fingerprint"
    export SHIPFORGE_QA01_SSH_PREVIOUS_HOST_KEY="$endpoint_B_fingerprint"
    export SHIPFORGE_QA01_SSH_IDENTITY_FILE="$temp_dir/identity_ed25519"
    export SHIPFORGE_QA01_SSH_AGENT_FINGERPRINT="$agent_fingerprint"

    run_release_gate
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
