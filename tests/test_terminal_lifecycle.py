"""PTY checks using harmless test workers; never invokes sudo or DNF."""
import array
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


class TerminalLifecycleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        supplied = os.environ.get("DEB2RPM_TEST_BINARY")
        candidates = [Path(supplied)] if supplied else [
            p for p in (ROOT / "target/debug/deps").glob("deb2rpm-*")
            if p.is_file() and not p.suffix and os.access(p, os.X_OK)]
        if not candidates:
            raise RuntimeError("Run blaze test --no-run first")
        cls.binary = max(candidates, key=lambda p: p.stat().st_mtime_ns)
        listed = subprocess.check_output([str(cls.binary), "--list"], text=True)
        if "tests::terminal_lifecycle_worker" not in listed:
            raise RuntimeError("Rebuild the tests with blaze test --no-run")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.log = Path(self.temp.name) / "interactive.log"
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        self.before = termios.tcgetattr(self.slave)
        self.process = None
        self.output = bytearray()

    def tearDown(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        os.close(self.master)
        os.close(self.slave)
        self.temp.cleanup()

    def start(self, scenario):
        env = os.environ.copy()
        env.update(TERM="xterm-kitty", DEB2RPM_TERMINAL_SCENARIO=scenario,
                   DEB2RPM_TERMINAL_LOG=str(self.log))
        self.process = subprocess.Popen([
            str(self.binary), "--ignored", "--exact", "tests::terminal_lifecycle_worker", "--nocapture"
        ], stdin=self.slave, stdout=self.slave, stderr=self.slave, env=env, start_new_session=True)

    def read_for(self, duration):
        deadline = time.monotonic() + duration
        received = bytearray()
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], min(0.05, max(0, deadline - time.monotonic())))
            if ready:
                try:
                    data = os.read(self.master, 65536)
                except OSError:
                    break
                if not data:
                    break
                received.extend(data)
                self.output.extend(data)
        return bytes(received)

    def until(self, marker):
        deadline = time.monotonic() + 8
        while marker not in self.output and time.monotonic() < deadline:
            self.read_for(0.05)
        self.assertIn(marker, self.output, self.output.decode(errors="replace"))

    def finish(self):
        status = self.process.wait(timeout=3)
        self.read_for(0.1)
        diagnostic = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", self.output.decode(errors="replace"))[-1600:]
        self.assertEqual(status, 0, diagnostic)

    def test_pause_ignores_typed_and_pasted_commands_and_restores_settings(self):
        self.start("pause")
        self.until(b"Press Enter to close")
        during = termios.tcgetattr(self.slave)
        self.assertFalse(during[3] & (termios.ECHO | termios.ICANON))
        self.assertTrue(during[3] & termios.ISIG)
        os.write(self.master, b"ls -la")
        self.assertNotIn(b"ls -la", self.read_for(0.1))
        os.write(self.master, b"\x1b[200")
        os.write(self.master, b"~echo pasted\nmore commands\n\x1b[201~")
        self.read_for(0.15)
        self.assertIsNone(self.process.poll(), "pasted Enter must not close")
        os.write(self.master, b"\rTRAILING_INPUT_MUST_NOT_REACH_SHELL")
        self.finish()
        self.assertEqual(termios.tcgetattr(self.slave), self.before)
        pending = array.array("i", [0])
        fcntl.ioctl(self.slave, termios.FIONREAD, pending, True)
        self.assertEqual(pending[0], 0)
        self.assertIn(b"\x1b[?2004h", self.output)
        self.assertIn(b"\x1b[?2004l", self.output)

    def test_signal_during_pause_restores_terminal_settings(self):
        self.start("pause-signal")
        self.until(b"Press Enter to close")
        self.process.send_signal(signal.SIGTERM)
        self.finish()
        self.assertEqual(termios.tcgetattr(self.slave), self.before)

    def check_interactive(self, scenario):
        self.start(scenario)
        self.until(b"Confirm probe: ")
        self.assertIn(b"\x1b[?1049h", self.output)
        os.write(self.master, b"yes\n")
        self.finish()
        enter = self.output.index(b"\x1b[?1049h")
        leave = self.output.index(b"\x1b[?1049l")
        self.assertIn(b"PROBE_STDOUT", self.output[enter:leave])
        self.assertIn(b"PROBE_STDERR", self.output[enter:leave])
        self.assertNotIn(b"Installing probe", self.output[enter:leave])
        self.assertEqual(self.output.count(b"\x1b[?1049h"), 1)
        self.assertEqual(self.output.count(b"\x1b[?1049l"), 1)
        log = self.log.read_text()
        self.assertIn("PROBE_STDOUT", log)
        self.assertIn("PROBE_STDERR", log)
        self.assertNotIn("\nyes\n", log)
        return log

    def test_interactive_handoff_preserves_input_and_restores_progress_screen(self):
        self.check_interactive("success")

    def test_failed_interactive_command_keeps_diagnostics_in_log(self):
        log = self.check_interactive("failure")
        self.assertIn("PROBE_FAILURE_DETAIL", log)
        self.assertIn(b"Failed", self.output)

    def test_interactive_signal_is_forwarded_and_screen_restored(self):
        self.start("interactive-signal")
        self.until(b"READY_FOR_SIGNAL")
        self.process.send_signal(signal.SIGTERM)
        self.finish()
        self.assertIn(b"\x1b[?1049l", self.output)

    def test_descendant_held_output_does_not_hang_interactive_wait(self):
        started = time.monotonic()
        self.start("descendant")
        self.finish()
        self.assertLess(time.monotonic() - started, 1.5)

    def test_native_child_stdio_remain_ttys_with_the_real_window_size(self):
        self.start("native-tty")
        self.finish()
        self.assertIn(b"NATIVE_TTY_OK", self.output)

    def test_terminal_resize_reaches_the_child_output_terminal(self):
        self.start("resize-tty")
        self.until(b"READY_TO_RESIZE")
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        self.read_for(0.15)
        os.write(self.master, b"continue\n")
        self.finish()
        self.assertIn(b"WINDOW_SIZE=30 100", self.output)

    def test_lines_reset_the_column_even_while_outer_output_processing_is_off(self):
        self.start("raw-lines")
        self.finish()
        self.assertIn(b"LAYOUT_FIRST\r\nLAYOUT_SECOND\r\n", self.output)
        self.assertIn(b"\x1b[A", self.output)
        self.assertNotIn(b"^[[A", self.output)
        self.assertEqual(termios.tcgetattr(self.slave), self.before)


if __name__ == "__main__":
    unittest.main()
