import configparser
import json
import os
from pathlib import Path
import subprocess
import shlex
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class TerminalLauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.config = self.root / "config"
        self.config.mkdir()
        self.record = self.root / "args.json"
        self.env = os.environ.copy()
        self.env.update(PATH=str(self.bin), HOME=str(self.root),
                        XDG_CONFIG_HOME=str(self.config), XDG_CONFIG_DIRS=str(self.root / "system"),
                        XDG_CURRENT_DESKTOP="Test", RECORD=str(self.record))
        self.env.pop("TERMINAL", None)

    def tearDown(self):
        self.temp.cleanup()

    def fake(self, name):
        path = self.bin / name
        path.write_text("#!/usr/bin/python3\nimport os,sys,json\nopen(os.environ['RECORD'],'w').write(json.dumps(sys.argv))\n")
        path.chmod(0o755)
        return path

    def launch(self, *args):
        return subprocess.run(["/bin/bash", str(ROOT / "terminal-launch.sh"), *args],
                              env=self.env, capture_output=True, text=True)

    def test_xdg_launcher_receives_arguments_without_shell_interpretation(self):
        self.fake("xdg-terminal-exec")
        result = self.launch("--pause", "/tmp/a $(touch nope) 'quoted'.deb")
        self.assertEqual(result.returncode, 0, result.stderr)
        args = json.loads(self.record.read_text())
        self.assertEqual(args[1:], [str(ROOT / "deb2rpm"), "--pause", "/tmp/a $(touch nope) 'quoted'.deb"])

    def test_default_terminal_list_selects_kitty_not_desktop_fallback(self):
        self.fake("kitty")
        self.fake("ptyxis")
        (self.config / "xdg-terminals.list").write_text("# preferred terminal\nkitty.desktop\n")
        result = self.launch("--pause", "/tmp/app with spaces.deb")
        self.assertEqual(result.returncode, 0, result.stderr)
        args = json.loads(self.record.read_text())
        self.assertEqual(Path(args[0]).name, "kitty")
        self.assertEqual(args[1:], ["--", str(ROOT / "deb2rpm"), "--pause", "/tmp/app with spaces.deb"])

    def test_terminal_environment_override(self):
        self.fake("alacritty")
        self.env["TERMINAL"] = "alacritty"
        result = self.launch("file.deb")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.record.read_text())[1:3], ["-e", str(ROOT / "deb2rpm")])

    def test_desktop_specific_list_precedes_generic_list(self):
        self.fake("kitty")
        self.fake("foot")
        (self.config / "test-xdg-terminals.list").write_text("foot.desktop\n")
        (self.config / "xdg-terminals.list").write_text("kitty.desktop\n")
        result = self.launch("file.deb")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(json.loads(self.record.read_text())[0]).name, "foot")

    def test_no_default_does_not_choose_an_arbitrary_terminal(self):
        self.fake("ptyxis")
        result = self.launch("file.deb")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.record.exists())
        self.assertIn("default terminal", result.stderr)

    def test_excluded_terminal_is_not_selected(self):
        self.fake("kitty")
        self.fake("foot")
        (self.config / "xdg-terminals.list").write_text("-kitty.desktop\nkitty.desktop\nfoot.desktop\n")
        result = self.launch("file.deb")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(json.loads(self.record.read_text())[0]).name, "foot")

    def test_explicit_unexclude_restores_preferred_terminal(self):
        self.fake("kitty")
        (self.config / "xdg-terminals.list").write_text("-kitty.desktop\n+kitty.desktop\nkitty.desktop\n")
        result = self.launch("file.deb")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(json.loads(self.record.read_text())[0]).name, "kitty")

    def test_installer_stages_explicit_launcher_without_terminal_true(self):
        self.fake("blaze")
        self.env["PATH"] += ":/usr/bin:/bin"
        bin_path = self.root / "installed bin"
        data_path = self.root / "data"
        self.env.update(XDG_BIN_HOME=str(bin_path), XDG_DATA_HOME=str(data_path))
        result = subprocess.run(["/bin/bash", str(ROOT / "install.sh")], env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        entry = (data_path / "applications/deb2rpm.desktop").read_text()
        build = json.loads(self.record.read_text())
        self.assertIn("--message-format=short", build)
        self.assertEqual(build[build.index("--target-dir") + 1], str(ROOT / "target"))
        self.assertIn(f'Exec="{bin_path}/deb2rpm-terminal" --pause %f', entry)
        self.assertIn("Terminal=false", entry)
        self.assertNotIn("Terminal=true", entry)
        launcher = bin_path / "deb2rpm-terminal"
        self.assertTrue(os.access(launcher, os.X_OK))
        self.assertEqual(launcher.read_text(), (ROOT / "terminal-launch.sh").read_text())

        service = data_path / "kio/servicemenus/deb2rpm.desktop"
        self.assertTrue(service.is_file(), "installer must register the Dolphin action")
        self.assertTrue(os.access(service, os.X_OK))
        entry = configparser.ConfigParser(interpolation=None)
        entry.read(service)
        metadata = entry["Desktop Entry"]
        self.assertEqual(metadata["Type"], "Service")
        self.assertIn("application/vnd.debian.binary-package;", metadata["MimeType"])
        self.assertEqual(metadata["X-KDE-Protocols"], "file")
        self.assertEqual(metadata["X-KDE-RequiredNumberOfUrls"], "1")
        action = entry["Desktop Action convertInstall"]
        self.assertEqual(action["Name"], "Convert & Install")
        # Exercise the installed action's command through the real launcher,
        # replacing Dolphin's file field with a filename containing shell syntax.
        command = shlex.split(action["Exec"])
        package = str(self.root / "app $(touch nope) 'quoted'.deb")
        command = [package if arg == "%f" else arg for arg in command]
        self.fake("kitty")
        self.env["TERMINAL"] = "kitty"
        result = subprocess.run(command, env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.record.read_text())[1:],
                         ["--", str(bin_path / "deb2rpm"), "--pause", package])


if __name__ == "__main__":
    unittest.main()
