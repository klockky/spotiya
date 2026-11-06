import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import tempfile
import termios
import time
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "soloist/set-key.sh"


class SoloistKeyTest(unittest.TestCase):
    def run_prompt(self, state, value):
        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)

        def terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        proc = subprocess.Popen(
            ["sh", str(SCRIPT)], stdin=slave, stdout=slave, stderr=slave,
            env={**os.environ, "SPOTIYA_STATE_DIR": str(state)}, preexec_fn=terminal,
        )
        output = bytearray()
        try:
            deadline = time.monotonic() + 5
            while b"(hidden): " not in output:
                self.assertLess(time.monotonic(), deadline, "key prompt did not appear")
                if select.select([master], [], [], 0.1)[0]:
                    output.extend(os.read(master, 4096))
            self.assertFalse(termios.tcgetattr(slave)[3] & termios.ECHO)
            os.write(master, value)
            proc.wait(timeout=5)
            while select.select([master], [], [], 0.05)[0]:
                output.extend(os.read(master, 4096))
            self.assertEqual(termios.tcgetattr(slave), original)
            return proc.returncode, bytes(output)
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
            os.close(master)
            os.close(slave)

    def test_key_is_hidden_and_private(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            code, output = self.run_prompt(state, b"synthetic-cli-key\n")
            self.assertEqual(code, 0)
            self.assertNotIn(b"synthetic-cli-key", output)
            key = state / "soloist/api-key"
            self.assertEqual(key.read_bytes(), b"synthetic-cli-key\n")
            self.assertEqual(key.stat().st_mode & 0o777, 0o600)
            self.assertEqual(list(key.parent.iterdir()), [key])

    def test_cancel_keeps_existing_key_and_restores_terminal(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            key = state / "soloist/api-key"
            key.parent.mkdir()
            key.write_bytes(b"existing-test-key\n")
            code, output = self.run_prompt(state, b"\x03")
            self.assertNotEqual(code, 0)
            self.assertNotIn(b"existing-test-key", output)
            self.assertEqual(key.read_bytes(), b"existing-test-key\n")
            self.assertEqual(list(key.parent.iterdir()), [key])
