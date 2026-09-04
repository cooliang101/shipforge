#!/usr/bin/env python3
"""Opt-in WSL fixture; never installs packages or changes default SSH settings."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import socket
import stat
import subprocess
import sys
import time


SETTLE_SECONDS = 5
UNIT_DIRECTORY = Path("/run/systemd/system")
AUTHORIZED_KEY_DIRECTORY = Path("/run")


def run(*args, check=True):
    return subprocess.run(args, check=check, text=True, capture_output=True, timeout=30)


def validate_run_id(value):
    if not re.fullmatch(r"[0-9a-f]{32}", value):
        raise ValueError("run ID must be exactly 32 lowercase hexadecimal characters")
    return value


def paths(run_id):
    validate_run_id(run_id)
    return Path(f"/var/tmp/shipforge-systemd-{run_id}")


def authorized_key_path(run_id):
    return AUTHORIZED_KEY_DIRECTORY / f"shipforge-systemd-{validate_run_id(run_id)}.authorized_keys"


def unit_name(run_id, component):
    if component not in ("ssh", "worker", "first"):
        raise ValueError("unknown fixture unit")
    return f"shipforge-{validate_run_id(run_id)}-{component}.service"


def unit_property(unit, name):
    return run("systemctl", "show", unit, f"--property={name}", "--value").stdout.strip()


def write_new(path, value, mode=0o600):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        output.write(value)


def owned_directory(root, run_id):
    expected = paths(run_id)
    if root != expected or root.is_symlink() or root.resolve() != expected:
        raise ValueError("refusing cleanup outside the exact fixture directory")
    if root.stat().st_uid != 0 or not root.is_dir() or root.stat().st_mode & 0o022:
        raise ValueError("fixture directory is not root-owned")
    marker = root / "marker"
    if not owned_file(marker) or marker.read_text() != f"shipforge-systemd-v1:{run_id}\n":
        raise ValueError("fixture marker mismatch; refusing cleanup")
    control = root / "control"
    if control.exists() or control.is_symlink():
        if control.is_symlink() or not control.is_dir() or control.stat().st_uid != 0 or control.stat().st_mode & 0o077:
            raise ValueError("unsafe fixture control directory")


def owned_file(path):
    try:
        info = path.lstat()
    except FileNotFoundError:
        return False
    return stat.S_ISREG(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o022


def create(run_id, public_key):
    root = paths(run_id)
    if root.exists() or root.is_symlink():
        raise ValueError("fixture directory already exists")
    authorized_key = authorized_key_path(run_id)
    if authorized_key.exists() or authorized_key.is_symlink():
        raise ValueError("fixture authorized key already exists")
    key = Path(public_key).read_text().strip()
    if not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\r\n]*)?", key):
        raise ValueError("expected exactly one generated Ed25519 public key")
    for name in ("ssh", "worker", "first"):
        unit = unit_name(run_id, name)
        unit_file = UNIT_DIRECTORY / unit
        if unit_file.exists() or unit_file.is_symlink() or unit_property(unit, "LoadState") != "not-found":
            raise ValueError(f"fixture unit already exists: {unit}")
    if not Path("/usr/sbin/sshd").is_file() or Path("/proc/1/comm").read_text().strip() != "systemd":
        raise ValueError("requires an installed SSH server and running systemd")
    root.mkdir(mode=0o755)
    try:
        write_new(root / "marker", f"shipforge-systemd-v1:{run_id}\n", 0o644)
    except Exception:
        # Only remove the still-empty directory created by this invocation.
        # If even the marker is partially written, preserve it for inspection.
        if not root.is_symlink() and root.stat().st_uid == 0:
            try:
                root.rmdir()
            except OSError:
                pass
        raise
    try:
        endpoint = populate(root, run_id, key)
    except Exception as original:
        try:
            cleanup(run_id, announce=False)
        except Exception as cleanup_error:
            raise RuntimeError(f"fixture setup failed ({original}); cleanup failed ({cleanup_error})") from original
        raise
    print(json.dumps(endpoint))


def populate(root, run_id, key):
    control = root / "control"
    control.mkdir(mode=0o700)
    write_new(control / "baseline-sessions.json", json.dumps(root_ssh_sessions()))
    evidence = root / "evidence"
    evidence.mkdir(mode=0o755)
    os.chown(evidence, 65534, 65534)
    for name in ("worker", "first"):
        (root / name).mkdir(mode=0o755)
    authorized_key = authorized_key_path(run_id)
    key_contents = f'from="127.0.0.1",restrict {key}\n'
    write_new(control / "authorized_keys", key_contents)
    # StrictModes checks every ancestor of AuthorizedKeysFile. /var/tmp is
    # intentionally world-writable, so keep only the actual public-key file
    # under /run; Release roots and the evidence backup remain unchanged.
    write_new(authorized_key, key_contents)
    run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(control / "host_ed25519"))
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    config = f"""Port {port}
AddressFamily inet
ListenAddress 127.0.0.1
HostKey {control}/host_ed25519
PidFile {control}/sshd.pid
AuthorizedKeysFile {authorized_key}
AllowUsers root
PermitRootLogin prohibit-password
PasswordAuthentication no
KbdInteractiveAuthentication no
AuthenticationMethods publickey
UsePAM yes
StrictModes yes
DisableForwarding yes
PermitTTY no
PermitUserRC no
PermitUserEnvironment no
X11Forwarding no
PermitTunnel no
UseDNS no
PrintMotd no
PrintLastLog no
LoginGraceTime 20
MaxAuthTries 2
Subsystem sftp internal-sftp
LogLevel VERBOSE
"""
    write_new(control / "sshd_config", config)
    # OpenSSH's standard runtime prerequisite, not a custom configuration.
    ssh_runtime = Path("/run/sshd")
    if not ssh_runtime.exists():
        ssh_runtime.mkdir(mode=0o755)
        write_new(control / "created-sshd-runtime", "yes\n")
    if ssh_runtime.is_symlink() or ssh_runtime.stat().st_uid != 0 or stat.S_IMODE(ssh_runtime.stat().st_mode) != 0o755:
        raise ValueError("unsafe OpenSSH privilege separation directory")
    run("/usr/sbin/sshd", "-t", "-f", str(control / "sshd_config"))
    fingerprint = run("ssh-keygen", "-lf", str(control / "host_ed25519.pub"), "-E", "sha256").stdout.split()[1]
    endpoint = {"port": port, "host_key": fingerprint}
    # Persist the intended endpoint before any unit can start, including when
    # systemctl start subsequently fails or its result is uncertain.
    write_new(control / "endpoint.json", json.dumps(endpoint))
    ssh_unit = unit_name(run_id, "ssh")
    for name in ("ssh", "worker", "first"):
        unit = unit_name(run_id, name)
        if name == "ssh":
            content = f"""[Unit]
Description=ShipForge disposable systemd fixture {run_id}
[Service]
Type=exec
ExecStart=/usr/sbin/sshd -D -e -f {control}/sshd_config
Restart=no
RuntimeMaxSec=900
TimeoutStopSec=5
KillMode=control-group
ProtectHome=yes
"""
        else:
            content = f"""[Unit]
Description=ShipForge disposable worker {run_id} {name}
BindsTo={ssh_unit}
After={ssh_unit}
StartLimitIntervalSec=0
[Service]
Type=exec
User=nobody
Group=nogroup
WorkingDirectory={root}/{name}/current
ExecStart=/bin/sh {root}/{name}/current/worker.sh
Restart=on-failure
RestartSec=50ms
RuntimeMaxSec=900
TimeoutStopSec=5
KillMode=control-group
NoNewPrivileges=yes
# Releases and evidence intentionally live in /var/tmp. A private /var/tmp
# would hide both; ProtectSystem keeps other paths read-only instead.
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths={evidence}
RestrictAddressFamilies=AF_UNIX
"""
        write_new(control / unit, content, 0o644)
        write_new(UNIT_DIRECTORY / unit, content, 0o644)
    run("systemctl", "daemon-reload")
    for name in ("ssh", "worker", "first"):
        verify_unit(root, unit_name(run_id, name))
    run("systemctl", "start", ssh_unit)
    wait_for_listener(ssh_unit, port)
    return endpoint


def wait_for_listener(ssh_unit, port):
    deadline = time.monotonic() + SETTLE_SECONDS
    while True:
        listener = run("ss", "-H", "-ltnp", f"sport = :{port}").stdout.splitlines()
        if listener:
            leader = unit_property(ssh_unit, "MainPID")
            if len(listener) != 1 or len(listener[0].split()) < 4 or listener[0].split()[3] != f"127.0.0.1:{port}" or not re.search(rf"\bpid={re.escape(leader)},", listener[0]) or leader == "0":
                raise RuntimeError("fixture SSH listener is not uniquely owned on IPv4 loopback")
            return
        if time.monotonic() >= deadline:
            raise RuntimeError("fixture SSH listener did not become ready")
        time.sleep(0.1)


def descendants(parent):
    links = {}
    for process in Path("/proc").iterdir():
        if process.name.isdigit():
            try:
                # The command in parentheses can contain spaces or parentheses.
                fields = (process / "stat").read_text().rsplit(")", 1)[1].split()
                links[int(process.name)] = int(fields[1])
            except (OSError, ValueError, IndexError):
                continue
    result = {parent}
    while True:
        expanded = result | {pid for pid, owner in links.items() if owner in result}
        if expanded == result:
            return result
        result = expanded


def session_ids():
    sessions = []
    for line in run("loginctl", "list-sessions", "--no-legend", "--no-pager").stdout.splitlines():
        session = line.split()[0]
        if not re.fullmatch(r"[A-Za-z0-9_-]+", session):
            raise ValueError("unexpected session ID")
        sessions.append(session)
    return sessions


def session_properties(session):
    result = run(
        "loginctl", "show-session", session, "--all",
        "--property=Leader", "--property=Service", "--property=Name",
        "--property=Scope", "--property=TimestampMonotonic", check=False,
    )
    if result.returncode:
        if session not in session_ids():
            return None
        raise RuntimeError(f"could not inspect existing session {session}")
    fields = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
    if set(fields) != {"Leader", "Service", "Name", "Scope", "TimestampMonotonic"}:
        raise ValueError(f"incomplete session identity: {session}")
    if not fields["Leader"].isdigit() or not fields["TimestampMonotonic"].isdigit() or not fields["Scope"]:
        raise ValueError(f"invalid session identity: {session}")
    return {"id": session, **fields}


def root_ssh_sessions():
    result = []
    for session in session_ids():
        fields = session_properties(session)
        if fields and fields["Service"] == "sshd" and fields["Name"] == "root":
            result.append(fields)
    return result


def read_control_json(root, name):
    path = root / "control" / name
    if not owned_file(path):
        raise ValueError(f"missing or unsafe fixture evidence: {name}")
    return json.loads(path.read_text())


def fixture_sessions(root, ssh_unit):
    baseline = read_control_json(root, "baseline-sessions.json")
    record = root / "control" / "cleanup-sessions.json"
    recorded = read_control_json(root, record.name) if record.exists() or record.is_symlink() else []
    leader = int(unit_property(ssh_unit, "MainPID") or "0")
    children = descendants(leader) if leader else set()
    current = root_ssh_sessions()
    matches = [session for session in current if int(session["Leader"]) in children or session in recorded]
    unknown = [session for session in current if session not in baseline and session not in matches]
    if unknown:
        # Once the SSH leader has exited, new PAM scopes cannot be attributed
        # from ancestry. Never silently ignore or terminate an unproven scope.
        raise RuntimeError("new root SSH sessions cannot be attributed to this fixture")
    if record.exists() or record.is_symlink():
        if any(session not in recorded for session in matches):
            raise RuntimeError("new SSH sessions appeared after cleanup began")
    else:
        write_new(record, json.dumps(matches))
    return matches


def terminate_session(expected):
    session = expected["id"]
    current = session_properties(session)
    if current is None:
        return
    if current != expected:
        raise RuntimeError(f"session identity changed: {session}")
    result = run("loginctl", "terminate-session", session, check=False)
    deadline = time.monotonic() + SETTLE_SECONDS
    while True:
        current = session_properties(session)
        if current is None:
            return
        if current != expected:
            raise RuntimeError(f"session identity changed while stopping: {session}")
        if result.returncode or time.monotonic() >= deadline:
            raise RuntimeError(f"owned SSH session did not disappear: {session}")
        time.sleep(0.1)


def ensure_listener_gone(root):
    endpoint = read_control_json(root, "endpoint.json")
    port = endpoint.get("port")
    if type(port) is not int or not 1 <= port <= 65535:
        raise ValueError("invalid fixture endpoint port")
    deadline = time.monotonic() + SETTLE_SECONDS
    while run("ss", "-H", "-ltn", f"sport = :{port}").stdout.strip():
        if time.monotonic() >= deadline:
            raise RuntimeError("fixture SSH port still has a listener; preserving evidence")
        time.sleep(0.1)


def verify_unit(root, unit):
    target = UNIT_DIRECTORY / unit
    if not target.exists() and not target.is_symlink():
        if unit_property(unit, "LoadState") != "not-found":
            raise ValueError(f"unit exists outside the owned runtime file: {unit}")
        return False
    backup = root / "control" / unit
    if not owned_file(target) or not owned_file(backup) or target.read_bytes() != backup.read_bytes():
        raise ValueError(f"unit contents changed: {unit}")
    fragment = unit_property(unit, "FragmentPath")
    if fragment and fragment != str(target):
        raise ValueError(f"unit loaded from another location: {unit}")
    if unit_property(unit, "DropInPaths"):
        raise ValueError(f"unit has unexpected drop-ins: {unit}")
    return True


def verify_authorized_key(root, run_id):
    target = authorized_key_path(run_id)
    if not target.exists() and not target.is_symlink():
        return False
    backup = root / "control" / "authorized_keys"
    if not owned_file(target) or not owned_file(backup) or target.read_bytes() != backup.read_bytes():
        raise ValueError("fixture authorized key ownership or contents changed")
    return True


def cleanup(run_id, announce=True):
    root = paths(run_id)
    if not root.exists() and not root.is_symlink():
        authorized_key = authorized_key_path(run_id)
        if authorized_key.exists() or authorized_key.is_symlink():
            raise ValueError("authorized key remains without fixture marker; manual inspection required")
        for name in ("ssh", "worker", "first"):
            if unit_property(unit_name(run_id, name), "LoadState") != "not-found":
                raise ValueError("unit remains without fixture marker; manual inspection required")
        return
    owned_directory(root, run_id)
    failures = []
    try:
        verify_authorized_key(root, run_id)
    except Exception as error:
        failures.append(str(error))
    verified = []
    for name in ("worker", "first", "ssh"):
        unit = unit_name(run_id, name)
        try:
            if verify_unit(root, unit):
                verified.append(unit)
        except Exception as error:
            failures.append(str(error))
    ssh_unit = unit_name(run_id, "ssh")
    sessions = []
    if ssh_unit in verified:
        try:
            sessions = fixture_sessions(root, ssh_unit)
        except Exception as error:
            failures.append(f"session inspection: {error}")
    for unit in verified:
        try:
            verify_unit(root, unit)
            run("systemctl", "stop", unit)
            if unit_property(unit, "ActiveState") not in ("inactive", "failed") or unit_property(unit, "MainPID") != "0":
                raise ValueError(f"unit did not stop: {unit}")
            run("systemctl", "reset-failed", unit, check=False)
        except Exception as error:
            failures.append(f"stopping {unit}: {error}")
    # PAM may have placed authenticated children in separate session scopes.
    for session in sessions:
        try:
            terminate_session(session)
        except Exception as error:
            failures.append(f"session cleanup: {error}")
    endpoint = root / "control" / "endpoint.json"
    if endpoint.exists() or endpoint.is_symlink() or ssh_unit in verified:
        try:
            ensure_listener_gone(root)
        except Exception as error:
            failures.append(f"listener cleanup: {error}")
    if failures:
        raise RuntimeError("cleanup incomplete; preserving fixture evidence: " + "; ".join(failures))
    # Recheck immediately before deletion, only after all unit, session and
    # listener checks succeeded. Never remove an unproven replacement file.
    if verify_authorized_key(root, run_id):
        authorized_key_path(run_id).unlink()
    for unit in verified:
        if verify_unit(root, unit):
            (UNIT_DIRECTORY / unit).unlink()
    run("systemctl", "daemon-reload")
    for name in ("worker", "first", "ssh"):
        if unit_property(unit_name(run_id, name), "LoadState") != "not-found":
            raise RuntimeError("fixture unit still loaded; preserving evidence")
    if (root / "control" / "created-sshd-runtime").exists():
        # This standard shared directory is normally empty even while another
        # SSH instance uses it. Emptiness is not proof that removal is safe.
        print("Retained shared OpenSSH runtime prerequisite /run/sshd", file=sys.stderr)
    owned_directory(root, run_id)
    if not shutil.rmtree.avoids_symlink_attacks:
        raise RuntimeError("platform lacks symlink-safe fixture deletion")
    shutil.rmtree(root)
    if announce:
        print(f"Cleaned disposable systemd fixture {run_id}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("create", "cleanup", "diagnostics"))
    parser.add_argument("run_id", type=validate_run_id)
    parser.add_argument("public_key", nargs="?")
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise PermissionError("fixture setup requires explicitly authorized WSL root execution")
    if args.operation == "create":
        if not args.public_key:
            raise ValueError("a newly generated public key is required")
        create(args.run_id, args.public_key)
    elif args.operation == "cleanup":
        cleanup(args.run_id)
    else:
        owned_directory(paths(args.run_id), args.run_id)
        for name in ("worker", "first", "ssh"):
            unit = unit_name(args.run_id, name)
            if verify_unit(paths(args.run_id), unit):
                print(run("systemctl", "status", unit, "--no-pager", check=False).stdout)
                print(run("journalctl", "-u", unit, "-n", "30", "--no-pager", check=False).stdout)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(str(error), file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError):
            print(error.stderr, file=sys.stderr)
        sys.exit(1)
