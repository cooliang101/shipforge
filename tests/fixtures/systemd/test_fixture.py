"""Host-independent safety regressions; all service/network operations are mocked."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("systemd_fixture", Path(__file__).with_name("fixture.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
RUN_ID = "a" * 32
SESSION = {
    "id": "c42", "Leader": "321", "Service": "sshd", "Name": "root",
    "Scope": "session-c42.scope", "TimestampMonotonic": "123456",
}


def result(stdout="", returncode=0):
    return subprocess.CompletedProcess([], returncode, stdout, "")


class FixtureSafetyTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="shipforge-fixture-unit-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.root = self.base / "run"
        self.root.mkdir()
        self.control = self.root / "control"
        self.control.mkdir()
        (self.control / "baseline-sessions.json").write_text("[]")
        (self.control / "endpoint.json").write_text(json.dumps({"port": 12345}))
        self.units = self.base / "units"
        self.units.mkdir()
        self.keys = self.base / "runtime-keys"
        self.keys.mkdir()
        self.calls = []
        self.overrides = {}
        self.start_patch("paths", return_value=self.root)
        self.start_patch("UNIT_DIRECTORY", new=self.units)
        self.start_patch("AUTHORIZED_KEY_DIRECTORY", new=self.keys)
        self.start_patch("owned_directory")
        self.start_patch("owned_file", side_effect=lambda path: path.is_file() and not path.is_symlink())
        self.properties = self.start_patch("unit_property", side_effect=self.unit_property)
        self.run = self.start_patch("run", side_effect=self.command)
        self.start_patch("SETTLE_SECONDS", new=0)
        self.start_patch("time.sleep")
        # Never run recursive fixture deletion in this suite. TemporaryDirectory
        # owns and removes only the test's independently generated scratch tree.
        self.remove = self.start_patch("shutil.rmtree")
        self.remove.avoids_symlink_attacks = True

    def start_patch(self, target, **kwargs):
        if "." in target:
            owner, name = target.split(".", 1)
            operation = patch.object(getattr(fixture, owner), name, **kwargs)
        else:
            operation = patch.object(fixture, target, **kwargs)
        mocked = operation.start()
        self.addCleanup(operation.stop)
        return mocked

    def unit_property(self, unit, name):
        if (unit, name) in self.overrides:
            return self.overrides[(unit, name)]
        target = self.units / unit
        return {
            "LoadState": "loaded" if target.exists() else "not-found",
            "FragmentPath": str(target) if target.exists() else "",
            "DropInPaths": "", "ActiveState": "inactive", "MainPID": "0",
        }[name]

    def command(self, *args, **kwargs):
        self.calls.append(args)
        return result()

    def add_unit(self, name):
        unit = fixture.unit_name(RUN_ID, name)
        (self.units / unit).write_text("fixture-owned\n")
        (self.control / unit).write_text("fixture-owned\n")
        return unit

    def add_authorized_key(self):
        target = fixture.authorized_key_path(RUN_ID)
        target.write_text('from="127.0.0.1",restrict ssh-ed25519 QUJD test\n')
        (self.control / "authorized_keys").write_bytes(target.read_bytes())
        return target

    def test_cleanup_stops_only_owned_units_and_checks_listener_before_deletion(self):
        units = [self.add_unit(name) for name in ("worker", "first", "ssh")]
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            fixture.cleanup(RUN_ID)
        self.assertEqual(stdout.getvalue(), "")
        stops = [args[2] for args in self.calls if args[:2] == ("systemctl", "stop")]
        self.assertEqual(stops, units)
        self.assertIn(("ss", "-H", "-ltn", "sport = :12345"), self.calls)
        self.assertTrue(all(not (self.units / unit).exists() for unit in units))
        self.remove.assert_called_once_with(self.root)

    def test_cleanup_missing_root_is_idempotent_and_does_not_remove_anything(self):
        fixture.paths.return_value = self.base / "already-gone"
        fixture.cleanup(RUN_ID)
        fixture.cleanup(RUN_ID)
        self.assertEqual(self.calls, [])
        self.remove.assert_not_called()

    def test_partial_setup_owned_key_is_removed_without_any_started_unit(self):
        target = self.add_authorized_key()
        (self.control / "endpoint.json").unlink()
        fixture.cleanup(RUN_ID, announce=False)
        self.assertFalse(target.exists())
        self.remove.assert_called_once_with(self.root)

    def test_authorized_key_is_removed_only_after_listener_disappears(self):
        target = self.add_authorized_key()
        ssh = self.add_unit("ssh")

        def command(*args, **kwargs):
            if args[0] == "ss":
                self.assertTrue(target.exists())
                self.assertIn(("systemctl", "stop", ssh), self.calls)
            return self.command(*args, **kwargs)

        self.run.side_effect = command
        fixture.cleanup(RUN_ID, announce=False)
        self.assertFalse(target.exists())

    def test_authorized_key_changed_contents_preserves_evidence(self):
        target = self.add_authorized_key()
        target.write_text("foreign key")
        with self.assertRaisesRegex(RuntimeError, "authorized key ownership or contents changed"):
            fixture.cleanup(RUN_ID)
        self.assertEqual(target.read_text(), "foreign key")
        self.remove.assert_not_called()

    def test_authorized_key_unsafe_owner_or_mode_is_refused(self):
        target = self.add_authorized_key()
        fixture.owned_file.side_effect = lambda path: path != target and path.is_file()
        with self.assertRaisesRegex(ValueError, "authorized key ownership or contents changed"):
            fixture.verify_authorized_key(self.root, RUN_ID)
        self.assertTrue(target.exists())

    def test_authorized_key_without_root_marker_is_never_deleted(self):
        target = self.add_authorized_key()
        fixture.paths.return_value = self.base / "missing-root"
        with self.assertRaisesRegex(ValueError, "authorized key remains without fixture marker"):
            fixture.cleanup(RUN_ID)
        self.assertTrue(target.exists())
        self.remove.assert_not_called()

    def test_live_listener_prevents_authorized_key_removal(self):
        target = self.add_authorized_key()
        self.add_unit("ssh")
        self.run.side_effect = lambda *args, **kwargs: result("listener") if args[0] == "ss" else result()
        with self.assertRaisesRegex(RuntimeError, "still has a listener"):
            fixture.cleanup(RUN_ID)
        self.assertTrue(target.exists())

    def test_failed_session_cleanup_prevents_authorized_key_removal(self):
        target = self.add_authorized_key()
        self.add_unit("ssh")
        self.start_patch("fixture_sessions", return_value=[SESSION])
        self.start_patch("terminate_session", side_effect=RuntimeError("session remains"))
        with self.assertRaisesRegex(RuntimeError, "session remains"):
            fixture.cleanup(RUN_ID)
        self.assertTrue(target.exists())

    def test_partial_setup_with_only_worker_unit_can_be_cleaned(self):
        worker = self.add_unit("worker")
        (self.control / "endpoint.json").unlink()
        fixture.cleanup(RUN_ID, announce=False)
        self.assertIn(("systemctl", "stop", worker), self.calls)
        self.assertFalse(any(args[0] in ("ss", "loginctl") for args in self.calls))
        self.remove.assert_called_once_with(self.root)

    def test_changed_unit_is_not_stopped_or_deleted(self):
        worker = self.add_unit("worker")
        (self.units / worker).write_text("foreign replacement")
        with self.assertRaisesRegex(RuntimeError, "unit contents changed"):
            fixture.cleanup(RUN_ID)
        self.assertNotIn(("systemctl", "stop", worker), self.calls)
        self.assertTrue((self.units / worker).exists())
        self.remove.assert_not_called()

    def test_unexpected_drop_in_is_refused(self):
        worker = self.add_unit("worker")
        self.overrides[(worker, "DropInPaths")] = "/etc/systemd/system/foreign.conf"
        with self.assertRaisesRegex(ValueError, "unexpected drop-ins"):
            fixture.verify_unit(self.root, worker)

    def test_marker_failure_prevents_every_cleanup_effect(self):
        fixture.owned_directory.side_effect = ValueError("marker mismatch")
        with self.assertRaisesRegex(ValueError, "marker mismatch"):
            fixture.cleanup(RUN_ID)
        self.assertEqual(self.calls, [])
        self.remove.assert_not_called()

    def test_live_listener_preserves_units_and_evidence(self):
        ssh = self.add_unit("ssh")
        self.run.side_effect = lambda *args, **kwargs: result("listener") if args[0] == "ss" else result()
        with self.assertRaisesRegex(RuntimeError, "still has a listener"):
            fixture.cleanup(RUN_ID)
        self.assertTrue((self.units / ssh).exists())
        self.remove.assert_not_called()

    def test_unattributed_session_after_ssh_exit_is_not_terminated(self):
        self.add_unit("ssh")
        self.start_patch("root_ssh_sessions", return_value=[SESSION])
        with self.assertRaisesRegex(RuntimeError, "cannot be attributed"):
            fixture.cleanup(RUN_ID)
        self.assertFalse(any(args[:2] == ("loginctl", "terminate-session") for args in self.calls))
        self.remove.assert_not_called()

    def test_owned_session_identity_is_saved_before_ssh_is_stopped(self):
        ssh = self.add_unit("ssh")
        self.overrides[(ssh, "MainPID")] = "123"
        self.start_patch("descendants", return_value={123, 321})
        self.start_patch("root_ssh_sessions", return_value=[SESSION])
        self.assertEqual(fixture.fixture_sessions(self.root, ssh), [SESSION])
        self.assertEqual(json.loads((self.control / "cleanup-sessions.json").read_text()), [SESSION])

    def test_saved_session_identity_survives_a_cleanup_retry_after_ssh_exits(self):
        ssh = self.add_unit("ssh")
        (self.control / "cleanup-sessions.json").write_text(json.dumps([SESSION]))
        self.start_patch("root_ssh_sessions", return_value=[SESSION])
        self.assertEqual(fixture.fixture_sessions(self.root, ssh), [SESSION])

    def test_terminate_success_still_requires_exact_session_disappearance(self):
        self.start_patch("session_properties", return_value=SESSION)
        with self.assertRaisesRegex(RuntimeError, "did not disappear"):
            fixture.terminate_session(SESSION)
        self.assertIn(("loginctl", "terminate-session", "c42"), self.calls)

    def test_changed_session_identity_is_never_terminated(self):
        self.start_patch("session_properties", return_value={**SESSION, "Leader": "999"})
        with self.assertRaisesRegex(RuntimeError, "identity changed"):
            fixture.terminate_session(SESSION)
        self.assertEqual(self.calls, [])

    def test_session_already_absent_needs_no_termination(self):
        self.start_patch("session_properties", return_value=None)
        fixture.terminate_session(SESSION)
        self.assertEqual(self.calls, [])

    def test_owned_session_termination_waits_until_absence(self):
        self.start_patch("session_properties", side_effect=[SESSION, None])
        fixture.terminate_session(SESSION)
        self.assertEqual(self.calls, [("loginctl", "terminate-session", "c42")])

    def test_session_inspection_error_is_not_assumed_to_mean_absence(self):
        self.run.return_value = result(returncode=1)
        self.run.side_effect = None
        self.start_patch("session_ids", return_value=["c42"])
        with self.assertRaisesRegex(RuntimeError, "could not inspect"):
            fixture.session_properties("c42")

    def test_session_properties_uses_separate_flags_for_systemd_249(self):
        self.run.side_effect = None
        self.run.return_value = result("\n".join(f"{key}={value}" for key, value in SESSION.items() if key != "id"))
        self.assertEqual(fixture.session_properties("c42"), SESSION)
        self.run.assert_called_once_with(
            "loginctl", "show-session", "c42", "--all",
            "--property=Leader", "--property=Service", "--property=Name",
            "--property=Scope", "--property=TimestampMonotonic", check=False,
        )

    def test_baseline_excludes_existing_non_ssh_user_session(self):
        self.run.side_effect = [
            result("1 1000 wangliang\n"),
            result("Name=wangliang\nTimestampMonotonic=3565628\nService=login\nScope=session-1.scope\nLeader=613\n"),
        ]
        self.assertEqual(fixture.root_ssh_sessions(), [])

    def test_listener_readiness_waits_for_type_exec_startup(self):
        self.start_patch("SETTLE_SECONDS", new=5)
        self.overrides[("ssh", "MainPID")] = "123"
        self.run.side_effect = [result(), result('LISTEN 0 128 127.0.0.1:12345 0.0.0.0:* users:(("sshd",pid=123,fd=3))')]
        fixture.wait_for_listener("ssh", 12345)
        fixture.time.sleep.assert_called_once_with(0.1)

    def test_wrong_address_or_pid_is_refused_without_waiting(self):
        self.overrides[("ssh", "MainPID")] = "123"
        for address, pid in [("0.0.0.0", "123"), ("127.0.0.1", "999")]:
            with self.subTest(address=address, pid=pid):
                self.run.side_effect = None
                self.run.return_value = result(f'LISTEN 0 128 {address}:12345 0.0.0.0:* users:(("sshd",pid={pid},fd=3))')
                with self.assertRaisesRegex(RuntimeError, "not uniquely owned"):
                    fixture.wait_for_listener("ssh", 12345)
        fixture.time.sleep.assert_not_called()

    def test_listener_readiness_has_a_deadline(self):
        with self.assertRaisesRegex(RuntimeError, "did not become ready"):
            fixture.wait_for_listener("ssh", 12345)

    def prepare_create(self):
        new_root = self.base / "created"
        fixture.paths.return_value = new_root
        public = self.base / "generated.pub"
        public.write_text("ssh-ed25519 QUJD fixture-test\n")
        sshd = self.base / "sshd"
        sshd.write_text("test-only presence marker")
        comm = self.base / "comm"
        comm.write_text("systemd\n")
        self.start_patch("Path", side_effect=lambda value: {
            "/usr/sbin/sshd": sshd, "/proc/1/comm": comm,
        }.get(str(value), Path(value)))
        return new_root, public

    def test_create_failure_calls_cleanup_without_contaminating_endpoint_stdout(self):
        new_root, public = self.prepare_create()
        self.start_patch("populate", side_effect=RuntimeError("setup failed"))
        cleanup = self.start_patch("cleanup")
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            with self.assertRaisesRegex(RuntimeError, "setup failed"):
                fixture.create(RUN_ID, str(public))
        self.assertEqual(stdout.getvalue(), "")
        cleanup.assert_called_once_with(RUN_ID, announce=False)
        self.assertTrue((new_root / "marker").is_file())

    def test_create_retains_both_primary_and_cleanup_failure(self):
        _, public = self.prepare_create()
        self.start_patch("populate", side_effect=RuntimeError("primary"))
        self.start_patch("cleanup", side_effect=RuntimeError("secondary"))
        with self.assertRaisesRegex(RuntimeError, r"primary.*secondary"):
            fixture.create(RUN_ID, str(public))

    def test_create_collision_never_triggers_cleanup(self):
        cleanup = self.start_patch("cleanup")
        with self.assertRaisesRegex(ValueError, "already exists"):
            fixture.create(RUN_ID, "never-read.pub")
        cleanup.assert_not_called()

    def test_create_authorized_key_collision_never_triggers_cleanup(self):
        new_root, public = self.prepare_create()
        target = self.add_authorized_key()
        cleanup = self.start_patch("cleanup")
        with self.assertRaisesRegex(ValueError, "authorized key already exists"):
            fixture.create(RUN_ID, str(public))
        self.assertTrue(target.exists())
        self.assertFalse(new_root.exists())
        cleanup.assert_not_called()

    def test_create_success_emits_exactly_one_endpoint_json(self):
        _, public = self.prepare_create()
        endpoint = {"port": 12345, "host_key": "SHA256:fixture"}
        self.start_patch("populate", return_value=endpoint)
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            fixture.create(RUN_ID, str(public))
        self.assertEqual(stdout.getvalue(), json.dumps(endpoint) + "\n")


if __name__ == "__main__":
    unittest.main()
