#!/usr/bin/env python3
"""Run one POSIX command with a deadline, bounded output, and tree cleanup."""

import argparse
import errno
import os
import re
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Callable, Optional


TIMEOUT_EXIT = 124
RUNNER_ERROR_EXIT = 125
START_ERROR_EXIT = 127
OWNER_TOKEN = re.compile(r"^[0-9a-f]{32}$")


class Collector(threading.Thread):
    def __init__(self, stream: object, limit: int) -> None:
        super().__init__(daemon=True)
        self._stream = stream
        self._limit = limit
        self.retained = bytearray()
        self.observed = 0
        self.failed = False

    def run(self) -> None:
        try:
            while True:
                chunk = self._stream.read(65536)
                if not chunk:
                    return
                self.observed += len(chunk)
                available = self._limit - len(self.retained)
                if available > 0:
                    self.retained.extend(chunk[:available])
        except OSError:
            self.failed = True

    def output(self) -> bytes:
        data = bytes(self.retained)
        if self.observed <= len(self.retained):
            return data
        notice = (
            "\n[shipforge bounded output truncated; "
            f"observed={self.observed} retained={len(self.retained)}]\n"
        ).encode("ascii")
        return data + notice


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def process_id(value: str) -> int:
    parsed = int(value)
    if parsed <= 1:
        raise argparse.ArgumentTypeError("must identify a non-system process")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--timeout-ms", required=True, type=positive_int)
    parser.add_argument("--term-grace-ms", default=2000, type=positive_int)
    parser.add_argument("--max-output-bytes", default=65536, type=positive_int)
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--stdout-path")
    parser.add_argument("--stderr-path")
    parser.add_argument("--pid-path")
    parser.add_argument("--status-path")
    parser.add_argument("--owner-token")
    parser.add_argument("--parent-pid", type=process_id)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.command[:1] == ["--"]:
        args.command = args.command[1:]
    if not args.command:
        parser.error("one command is required after --")
    if args.owner_token is not None and not OWNER_TOKEN.fullmatch(args.owner_token):
        parser.error("--owner-token must be one lowercase 32-character hex token")
    paths = [
        path
        for path in (args.stdout_path, args.stderr_path, args.pid_path, args.status_path)
        if path is not None
    ]
    if len(paths) != len(set(paths)):
        parser.error("output, pid, and status paths must be distinct")
    return args


def write_exclusive(path: Optional[str], payload: bytes) -> None:
    if path is None:
        return
    target = Path(path)
    with target.open("xb") as handle:
        handle.write(payload)
        handle.flush()


def signal_group(process: subprocess.Popen, requested: int) -> None:
    try:
        os.killpg(process.pid, requested)
    except ProcessLookupError:
        return


def group_is_alive(process: subprocess.Popen) -> bool:
    # Reap the group leader as soon as possible so a zombie cannot make the
    # owned process group look live after its last process has exited.
    process.poll()
    try:
        os.killpg(process.pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def wait_until(
    process: subprocess.Popen,
    deadline: float,
    interrupted: Optional[Callable[[], bool]] = None,
) -> bool:
    while process.poll() is None:
        if interrupted is not None and interrupted():
            return False
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return False
        try:
            process.wait(timeout=min(remaining, 0.1))
        except subprocess.TimeoutExpired:
            pass
    return True


def wait_group_until_gone(process: subprocess.Popen, deadline: float) -> bool:
    while group_is_alive(process):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return False
        time.sleep(min(remaining, 0.05))
    return True


def terminate_group(process: subprocess.Popen, grace_seconds: float) -> bool:
    signal_group(process, signal.SIGTERM)
    if wait_group_until_gone(process, time.monotonic() + grace_seconds):
        return True
    signal_group(process, signal.SIGKILL)
    return wait_group_until_gone(process, time.monotonic() + grace_seconds)


def normalized_exit(returncode: int) -> int:
    if returncode < 0:
        return 128 + min(-returncode, 127)
    return min(returncode, 255)


def emit(stream: object, payload: bytes) -> None:
    stream.write(payload)
    stream.flush()


def main() -> int:
    args = parse_args()
    process = None
    received_signal = 0
    parent_lost = False

    # Install handlers before spawning. Otherwise a TERM delivered between
    # Popen and handler registration could kill the wrapper but orphan the
    # newly created session leader.
    def handle_signal(number: int, _frame: object) -> None:
        nonlocal received_signal
        received_signal = received_signal or number
        if process is not None:
            signal_group(process, signal.SIGTERM)

    for handled in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(handled, handle_signal)

    try:
        if args.parent_pid is not None and os.getppid() != args.parent_pid:
            return RUNNER_ERROR_EXIT
        process = subprocess.Popen(
            args.command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        assert process.stdout is not None
        assert process.stderr is not None
        if received_signal:
            signal_group(process, signal.SIGTERM)
        try:
            write_exclusive(args.pid_path, f"{process.pid}\n".encode("ascii"))
        except OSError:
            terminate_group(process, args.term_grace_ms / 1000)
            return RUNNER_ERROR_EXIT

        stdout = Collector(process.stdout, args.max_output_bytes)
        stderr = Collector(process.stderr, args.max_output_bytes)
        stdout.start()
        stderr.start()

        deadline = time.monotonic() + args.timeout_ms / 1000
        def interrupted() -> bool:
            nonlocal parent_lost
            if args.parent_pid is not None and os.getppid() != args.parent_pid:
                parent_lost = True
            return received_signal != 0 or parent_lost

        completed = wait_until(process, deadline, interrupted)
        timed_out = not completed and received_signal == 0 and not parent_lost
        if not completed:
            completed = terminate_group(process, args.term_grace_ms / 1000)

        # A root command can exit while an inherited descendant remains in its
        # process group. Give normal group exit and pipe EOF a short grace,
        # then reap the whole owned group rather than leaking or hanging.
        group_leak = not wait_group_until_gone(process, time.monotonic() + 0.25)
        stdout.join(timeout=0.25)
        stderr.join(timeout=0.25)
        descendant_leak = group_leak or stdout.is_alive() or stderr.is_alive()
        if descendant_leak:
            terminate_group(process, args.term_grace_ms / 1000)
            stdout.join(timeout=args.term_grace_ms / 1000)
            stderr.join(timeout=args.term_grace_ms / 1000)
        if stdout.is_alive() or stderr.is_alive():
            signal_group(process, signal.SIGKILL)
            stdout.join(timeout=args.term_grace_ms / 1000)
            stderr.join(timeout=args.term_grace_ms / 1000)

        if (
            not completed
            or stdout.is_alive()
            or stderr.is_alive()
            or stdout.failed
            or stderr.failed
        ):
            result = RUNNER_ERROR_EXIT
        elif received_signal:
            result = 128 + received_signal
        elif parent_lost:
            result = RUNNER_ERROR_EXIT
        elif timed_out:
            result = TIMEOUT_EXIT
        elif descendant_leak:
            result = RUNNER_ERROR_EXIT
        else:
            assert process.returncode is not None
            result = normalized_exit(process.returncode)

        stdout_payload = stdout.output()
        stderr_payload = stderr.output()
        try:
            write_exclusive(args.stdout_path, stdout_payload)
            write_exclusive(args.stderr_path, stderr_payload)
            write_exclusive(args.status_path, f"{result}\n".encode("ascii"))
        except OSError:
            result = RUNNER_ERROR_EXIT

        if not args.quiet:
            emit(sys.stdout.buffer, stdout_payload)
            emit(sys.stderr.buffer, stderr_payload)
        return result
    except FileNotFoundError:
        return START_ERROR_EXIT
    except OSError as error:
        if error.errno not in (errno.ENOENT, errno.EACCES, errno.ENOEXEC):
            return RUNNER_ERROR_EXIT
        return START_ERROR_EXIT
    except Exception:
        # The caller receives only a fixed status. Command arguments and raw
        # diagnostics can include user names or temporary credential paths.
        return RUNNER_ERROR_EXIT
    finally:
        if process is not None and group_is_alive(process):
            terminate_group(process, args.term_grace_ms / 1000)


if __name__ == "__main__":
    raise SystemExit(main())
