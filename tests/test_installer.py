import hashlib
import http.server
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
ASSET = "spotiya-armv7-linux-gnueabihf.tar.gz"


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)
        self.base = self.directory / "installed"
        self.log = self.directory / "calls"
        self.commands = self.directory / "commands"
        self.commands.mkdir()
        for name, body in {
            "id": "echo 0",
            "uname": 'echo "${TEST_ARCH:-aarch64}"',
            "od": 'case "$*" in *-j4*) echo "01 01";; *) echo "28 00";; esac',
        }.items():
            self.executable(self.commands / name, ("#!/bin/sh\n" + body + "\n").encode())
        self.binary = b'''#!/bin/sh
printf '%s\\n' "$*" >> "$TEST_LOG"
case "$1" in --help) [ "${TEST_LEGACY:-0}" = 1 ] || echo '--uninstall [--yes]';; --version) echo 'spotiya 0.9.3';; --update) [ "${TEST_UPDATE_FAILURE:-0}" = 0 ] || exit 1;; esac
'''
        archive = self.directory / ASSET
        with tarfile.open(archive, "w:gz") as output:
            for name, body in {
                "spotiya/spotiya": self.binary,
                "spotiya/scripts/install.sh": b'#!/bin/sh\necho install >> "$TEST_LOG"\n[ "${TEST_INSTALL_FAILURE:-0}" = 0 ] || exit 1\nmkdir -p "$TEST_BASE"\ncp "$(dirname "$0")/../spotiya" "$TEST_BASE/spotiya"\n',
            }.items():
                member = tarfile.TarInfo(name)
                member.mode = 0o755
                member.size = len(body)
                output.addfile(member, io.BytesIO(body))
        (self.directory / "SHA256SUMS").write_text(hashlib.sha256(archive.read_bytes()).hexdigest() + "  " + ASSET + "\n")
        handler = lambda *args, **kwargs: QuietHandler(*args, directory=str(self.directory), **kwargs)
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        maind = self.directory / "maind"
        self.executable(maind, br'''#!/usr/bin/env python3
import os, pathlib, shlex, sys, urllib.request, urllib.parse
assert sys.argv[1] == "--gst-launch"
parts = shlex.split(sys.argv[2])
assert "ssl-strict=true" in parts
assert any(p.startswith("ssl-ca-file=") for p in parts)
locations = [p.removeprefix("location=") for p in parts if p.startswith("location=")]
assert locations[0].startswith("https://github.com/klockky/spotiya/releases/latest/download/")
if os.environ.get("TEST_DOWNLOAD_ERROR") == "1":
    pathlib.Path(locations[1]).write_text('echo unexpected > "$TEST_MARKER"\n')
    print("ERROR from souphttpsrc0: TLS failure")
    sys.exit(0)
try:
    url = os.environ["TEST_SERVER"] + "/" + urllib.parse.urlparse(locations[0]).path.rsplit("/", 1)[1]
    with urllib.request.urlopen(url) as response:
        pathlib.Path(locations[1]).write_bytes(response.read())
    print("EOS: end of stream")
except Exception:
    print("ERROR from souphttpsrc0: download failed")
''')
        ca = self.directory / "ca.crt"
        ca.touch()
        script = (ROOT / "install.sh").read_text().replace("/data/opt/spotify-connect", str(self.base))
        script = script.replace("/system/vendor/quasar/maind", str(maind))
        script = script.replace("/etc/ssl/certs/ca-certificates.crt", str(ca))
        self.script = self.directory / "install.sh"
        self.script.write_text(script)
        (self.directory / "INSTALL_SHA256SUMS").write_text(hashlib.sha256(self.script.read_bytes()).hexdigest() + "  install.sh\n")
        self.env = dict(os.environ, PATH=str(self.commands) + os.pathsep + os.environ["PATH"], TEST_LOG=str(self.log), TEST_BASE=str(self.base), TEST_MARKER=str(self.directory / "unexpected-execution"), TEST_SERVER=f"http://127.0.0.1:{self.server.server_port}")

    def fast_command(self):
        readme = (ROOT / "README.md").read_text()
        section = readme.split("## Быстрая установка и обновление\n", 1)[1]
        command = section.split("```sh\n", 1)[1].split("\n```", 1)[0]
        self.assertEqual(len(command.splitlines()), 1)
        self.assertLess(len(command.encode()), 1024, "BusyBox terminals truncate longer commands")
        return (command.replace("/data/opt/spotify-connect", str(self.base))
                .replace("/system/vendor/quasar/maind", str(self.directory / "maind"))
                .replace("/etc/ssl/certs/ca-certificates.crt", str(self.directory / "ca.crt")))

    def run_fast(self, input_text="1\n", **environment):
        return subprocess.run(["sh", "-c", self.fast_command()], env=dict(self.env, **environment), input=input_text, capture_output=True, text=True)

    @staticmethod
    def executable(path, body):
        path.write_bytes(body)
        path.chmod(0o755)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.temporary.cleanup()

    def run_installer(self, *args, input_text=None, **environment):
        return subprocess.run(["sh", str(self.script), *args], env=dict(self.env, **environment), input=input_text, capture_output=True, text=True)

    def calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_readme_command_opens_menu_then_installs_or_updates(self):
        result = self.run_fast()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), ["--version", "install"])
        state = self.base / "receiver-state"
        state.mkdir()
        saved = {state / "credentials.json": b"synthetic login", state / "profile.json": b"saved profile", self.base / "youtube": b"installed add-on"}
        for path, data in saved.items():
            path.write_bytes(data)
        self.log.unlink()
        (self.directory / ASSET).unlink()
        result = self.run_fast()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), ["--update"])
        for path, data in saved.items():
            self.assertEqual(path.read_bytes(), data)

    def test_readme_command_does_not_execute_a_partial_installer(self):
        result = self.run_fast(TEST_DOWNLOAD_ERROR="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Installer download failed", result.stderr)
        self.assertFalse((self.directory / "unexpected-execution").exists())
        self.assertFalse(self.base.exists())
        self.assertEqual(self.calls(), [])

    def test_menu_cancellation_empty_choice_and_eof_do_not_install(self):
        for choice in ("0\n", "\n", ""):
            result = self.run_fast(input_text=choice)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Cancelled", result.stdout)
            self.assertEqual(self.calls(), [])
            self.assertFalse(self.base.exists())

    def test_menu_retries_invalid_selection_and_supports_check(self):
        result = self.run_installer(input_text="bad\n2\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Enter 0, 1, 2 or 3", result.stdout)
        self.assertEqual(self.calls(), ["--version"])
        self.assertFalse(self.base.exists())

    def test_uninstall_uses_installed_binary_without_downloading_or_updating(self):
        self.base.mkdir()
        self.executable(self.base / "spotiya", self.binary)
        (self.directory / ASSET).unlink()
        result = self.run_installer(input_text="3\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), ["--help", "--uninstall"])
        self.log.unlink()
        result = self.run_installer("--uninstall", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), ["--help", "--uninstall --yes"])

    def test_older_installation_uses_downloaded_uninstaller_without_installing(self):
        self.base.mkdir()
        self.executable(self.base / "spotiya", self.binary)
        result = self.run_installer("--uninstall", "--yes", TEST_LEGACY="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), ["--help", "--version", "--uninstall --yes"])
        self.assertNotIn("Spotiya is ready", result.stdout)

    def test_uninstall_missing_installation_and_invalid_yes_do_nothing(self):
        result = self.run_installer("--uninstall", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("not installed", result.stdout)
        result = self.run_installer("--install", "--yes")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--yes requires", result.stderr)
        self.assertFalse(self.base.exists())
        self.assertEqual(self.calls(), [])

    def test_readme_command_runs_only_the_installer_named_in_the_release_checksums(self):
        sums = self.directory / "INSTALL_SHA256SUMS"
        for text in ("0" * 64 + "  install.sh\n", sums.read_text() * 2, sums.read_text().replace("install.sh", "other.sh")):
            sums.write_text(text)
            result = self.run_fast()
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Installer checksum mismatch", result.stderr)
            self.assertEqual(self.calls(), [])
            self.assertFalse(self.base.exists())
        sums.unlink()
        result = self.run_fast()
        self.assertIn("Installer download failed", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_failed_update_does_not_fall_back_to_a_fresh_install(self):
        self.base.mkdir()
        self.executable(self.base / "spotiya", self.binary)
        result = self.run_fast(TEST_UPDATE_FAILURE="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), ["--update"])

    def test_dry_run_verifies_release_without_installing(self):
        result = self.run_installer("--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), ["--version"])
        self.assertFalse(self.base.exists())

    def test_fresh_install_uses_the_packaged_transactional_installer(self):
        result = self.run_installer("--install")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), ["--version", "install"])
        self.assertTrue((self.base / "spotiya").is_file())

    def test_existing_installation_updates_without_downloading(self):
        self.base.mkdir()
        self.executable(self.base / "spotiya", self.binary)
        (self.directory / ASSET).unlink()
        self.assertEqual(self.run_installer("--install").returncode, 0)
        self.assertEqual(self.calls(), ["--update"])

    def test_corruption_and_incompatible_architecture_cannot_execute_download(self):
        with (self.directory / ASSET).open("ab") as file:
            file.write(b"changed")
        result = self.run_installer("--install")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertEqual(self.calls(), [])
        result = self.run_installer("--install", TEST_ARCH="x86_64")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ARMv7", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_factory_tool_success_exit_after_tls_failure_does_not_install_partial_data(self):
        result = self.run_installer("--install", TEST_DOWNLOAD_ERROR="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HTTPS download did not complete", result.stderr)
        self.assertEqual(self.calls(), [])
        self.assertFalse(self.base.exists())

    def test_installer_failure_is_reported_as_failure(self):
        result = self.run_installer("--install", TEST_INSTALL_FAILURE="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("Spotiya is ready", result.stdout)
        self.assertEqual(self.calls(), ["--version", "install"])
        self.assertFalse(self.base.exists())

    def test_duplicate_or_foreign_checksum_entries_are_rejected(self):
        sums = self.directory / "SHA256SUMS"
        sums.write_text(sums.read_text() * 2)
        result = self.run_installer("--install")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid or duplicate", result.stderr)
        self.assertEqual(self.calls(), [])
        sums.write_text("0" * 64 + "  /etc/passwd\n")
        result = self.run_installer("--install")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid or duplicate", result.stderr)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
