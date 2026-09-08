"""Linux-only fault injection against the exact shipped publisher entry point.

Run as an ordinary WSL user. All writes stay in a TemporaryDirectory under /tmp.
No SSH configuration, production destination, or system service is used.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

from inplace_contract import InplaceContract


PUBLISHER = Path(__file__).resolve().parents[1] / "src/drivers/linux_ssh/inplace.py"
WRAPPER = r"""
import errno, os, runpy, signal, sys
from pathlib import Path
script, request, fault = sys.argv[1:]
replace = os.replace
def injected(source, destination):
    source, destination = Path(source), Path(destination)
    if source.name.startswith('.shipforge-write-') and destination.name == 'b':
        if fault == 'kill':
            os.kill(os.getpid(), signal.SIGKILL)
        if fault == 'write':
            raise OSError(errno.EIO, 'injected application replacement failure')
    if destination.name == 'state.json' and fault == 'state':
        raise OSError(errno.EIO, 'injected state replacement failure')
    return replace(source, destination)
os.replace = injected
sys.argv = [script, request]
runpy.run_path(script, run_name='__main__')
"""


@unittest.skipUnless(sys.platform == "linux" and os.geteuid() != 0,
                     "requires an unprivileged Linux user")
class LinuxFaults(InplaceContract):
    # Inherits and reruns the basic contract on the same Linux filesystem.
    def setUp(self):
        super().setUp()
        self.put("runtime/sentinel", b"do-not-touch")

    def tearDown(self):
        try:
            self.assertEqual((self.root / "runtime/sentinel").read_bytes(), b"do-not-touch")
        finally:
            super().tearDown()

    def child(self, operation, fault="none", **kwargs):
        request = dict(root=str(self.root), identity=self.identity,
                       deployment=self.deployment, operation=operation, **kwargs)
        return subprocess.run([sys.executable, "-B", "-c", WRAPPER,
                               str(PUBLISHER), json.dumps(request), fault],
                              capture_output=True, text=True, timeout=20)

    def pair(self):
        self.put("a", b"old-a")
        self.put("b", b"old-b")
        self.prepare("v1", {"a": b"new-a", "b": b"new-b"})
        self.runop("archive")

    def test_partial_write_error_is_recorded_and_recoverable(self):
        self.pair()
        result = self.child("publish", "write")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertTrue(json.loads(result.stdout)["recoverable"])
        self.assertEqual((self.root / "a").read_bytes(), b"new-a")
        self.assertEqual((self.root / "b").read_bytes(), b"old-b")
        self.assertEqual(self.runop("observe")["phase"], "publish-failed")
        self.runop("rollback", expected=None, desired=None)
        self.runop("phase", phase="stable")
        self.assertEqual((self.root / "a").read_bytes(), b"old-a")
        self.assertEqual((self.root / "b").read_bytes(), b"old-b")
        self.assertFalse(list(self.root.glob(".shipforge-write-*")))

    def test_sigkill_mid_publish_blocks_blind_recovery(self):
        self.pair()
        result = self.child("publish", "kill")
        self.assertEqual(result.returncode, -9, result.stderr)
        self.assertEqual((self.root / "a").read_bytes(), b"new-a")
        self.assertEqual((self.root / "b").read_bytes(), b"old-b")
        for op, args in [("observe", {}), ("rollback", dict(expected=None, desired=None)),
                         ("begin", dict(expected=None, manifest={"version": "v2"}))]:
            with self.assertRaises(ValueError):
                self.runop(op, **args)
        self.runop("discard")
        self.assertTrue((self.root / ".shipforge-deploy/incoming.tar.gz").exists())

    def test_state_write_failure_prevents_file_publish(self):
        self.pair()
        result = self.child("publish", "state")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual((self.root / "a").read_bytes(), b"old-a")
        self.assertEqual((self.root / "b").read_bytes(), b"old-b")
        self.assertEqual(self.runop("observe")["phase"], "archived")
        self.assertTrue((self.root / ".shipforge-deploy/state.json.new").exists())

    def test_archive_permission_failure_preserves_application(self):
        self.put("app", b"old")
        self.prepare("v1", {"app": b"new"})
        meta = self.root / ".shipforge-deploy"
        meta.chmod(0o500)
        try:
            result = self.child("archive")
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual((self.root / "app").read_bytes(), b"old")
            self.assertFalse((meta / "previous.tar.gz").exists())
        finally:
            meta.chmod(0o700)

    def test_restore_write_failure_keeps_unknown_state_and_archive(self):
        self.pair()
        self.runop("publish")
        result = self.child("rollback", "write", expected="v1", desired=None)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(json.loads(result.stdout)["recoverable"])
        self.assertTrue((self.root / ".shipforge-deploy/previous.tar.gz").exists())
        with self.assertRaises(ValueError):
            self.runop("observe")
        with self.assertRaises(ValueError):
            self.runop("rollback", expected="v1", desired=None)


if __name__ == "__main__":
    # Avoid unittest discovering the imported base class a second time.
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(LinuxFaults)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    sys.exit(not result.wasSuccessful() or bool(result.skipped))
