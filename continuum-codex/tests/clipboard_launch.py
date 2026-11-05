#!/usr/bin/env python3
"""Offline process-boundary controls: python3 clipboard_launch.py PATH_TO_WRAPPER."""
import json
import os
from pathlib import Path
import pty
import subprocess
import sys
import tempfile
import unittest

BINARY = str(Path(sys.argv.pop(1)).resolve())
KEYS = ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "ZELLIJ", "CONTINUUM_CODEX_DEPTH")


class ClipboardLaunch(unittest.TestCase):
    def launch(self, *, zellij="0", terminal_in=True, terminal_out=True, native=None):
        with tempfile.TemporaryDirectory(prefix="continuum-clipboard-") as directory:
            root = Path(directory)
            child = root / "fake-codex"
            result = root / "result.json"
            child.write_text(
                "#!/usr/bin/env python3\nimport json, os, sys\n"
                + f"keys = {KEYS!r}\n"
                + f"with open({str(result)!r}, 'w') as out:\n"
                + "    json.dump({'env': {k: os.environ.get(k) for k in keys}, 'args': sys.argv[1:]}, out)\n"
                + "sys.exit(37)\n"
            )
            child.chmod(0o700)
            env = dict(os.environ)
            for key in (*KEYS, "CONTINUUM_CODEX_NATIVE_CLIPBOARD"):
                env.pop(key, None)
            env.update(HOME=directory, CONTINUUM_CODEX_BIN=str(child),
                       DISPLAY=":clipboard-fixture", WAYLAND_DISPLAY="wayland-fixture",
                       WAYLAND_SOCKET="12345")
            if zellij is not None:
                env["ZELLIJ"] = zellij
            if native is not None:
                env["CONTINUUM_CODEX_NATIVE_CLIPBOARD"] = native
            parent_before = dict(os.environ)
            master, slave = pty.openpty()
            try:
                args = ["resume", "synthetic-session", "--no-alt-screen"]
                proc = subprocess.run([BINARY, *args], env=env,
                                      stdin=slave if terminal_in else subprocess.DEVNULL,
                                      stdout=slave if terminal_out else subprocess.DEVNULL,
                                      stderr=subprocess.PIPE, timeout=10)
                self.assertEqual(proc.returncode, 37, proc.stderr.decode())
                found = json.loads(result.read_text())
                self.assertEqual(found["args"], args)
                self.assertEqual(found["env"]["CONTINUUM_CODEX_DEPTH"], "1")
                self.assertEqual(dict(os.environ), parent_before)
                return found["env"]
            finally:
                os.close(slave)
                os.close(master)

    def assert_native(self, found):
        self.assertEqual(found["DISPLAY"], ":clipboard-fixture")
        self.assertEqual(found["WAYLAND_DISPLAY"], "wayland-fixture")
        self.assertEqual(found["WAYLAND_SOCKET"], "12345")

    @unittest.skipUnless(sys.platform == "linux", "Linux workaround")
    def test_zellij_terminal_routes_copy_without_native_display(self):
        found = self.launch()
        self.assertEqual(found["DISPLAY"], "")
        self.assertEqual(found["WAYLAND_DISPLAY"], "")
        self.assertIsNone(found["WAYLAND_SOCKET"])
        self.assertEqual(found["ZELLIJ"], "0")

    def test_non_zellij_retains_native_display(self):
        self.assert_native(self.launch(zellij=None))

    def test_empty_zellij_retains_native_display(self):
        self.assert_native(self.launch(zellij=""))

    def test_redirected_input_retains_native_display(self):
        self.assert_native(self.launch(terminal_in=False))

    def test_redirected_output_retains_native_display(self):
        self.assert_native(self.launch(terminal_out=False))

    def test_native_opt_out_retains_display(self):
        self.assert_native(self.launch(native="1"))

    @unittest.skipIf(sys.platform == "linux", "Other platforms keep native clipboard")
    def test_other_platform_retains_native_display(self):
        self.assert_native(self.launch())


if __name__ == "__main__":
    unittest.main()
