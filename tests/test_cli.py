import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class MaintenanceCLI(unittest.TestCase):
    def test_station_uninstall_wrapper_never_starts_receiver_and_rejects_extra_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            binary = directory / "spotiya"
            binary.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\"\n")
            binary.chmod(0o700)
            start = directory / "start.sh"
            start.write_text("#!/bin/sh\nexit 99\n")
            start.chmod(0o700)
            wrapper = directory / "cli.sh"
            wrapper.write_text((ROOT / "packaging/cli.sh").read_text().replace("/data/opt/spotify-connect", str(directory)))
            for values in ([], ["--yes"]):
                result = subprocess.run(["sh", str(wrapper), "uninstall"] + values, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines(), ["--uninstall"] + values)
            result = subprocess.run(["sh", str(wrapper), "uninstall", "unexpected"], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Usage", result.stderr)
            self.assertEqual(result.stdout, "")

    def test_youtube_toggles_or_plays_a_link_as_one_argument(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            adb = directory / "adb"
            # start.sh must report a running receiver before playback commands are sent.
            adb.write_text("#!/usr/bin/env python3\nimport json, os, sys\nwith open(os.environ['SPOTIYA_TEST_LOG'], 'a') as f: f.write(json.dumps(sys.argv[1:]) + '\\n')\nif sys.argv[-1].endswith('start.sh'): print('already running')\n")
            adb.chmod(0o700)
            log = directory / "calls.jsonl"
            env = dict(os.environ, PATH=str(directory) + os.pathsep + os.environ["PATH"], SPOTIYA_TEST_LOG=str(log))
            link = "https://www.youtube.com/watch?v=O5sjrLI5mSs&t=10"
            cases = [
                (["youtube", "on"], ["--youtube", "on"]),
                (["youtube", "off"], ["--youtube", "off"]),
                (["youtube", link], ["--youtube-url", link]),
            ]
            for command, _ in cases:
                result = subprocess.run([str(ROOT / "bin/spotiya"), "--device", "fixture:5555"] + command, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            commands = [c for c in calls if not c[-1].endswith("start.sh")]
            self.assertEqual(len(commands), len(cases))
            for call, (_, expected) in zip(commands, cases):
                self.assertEqual(shlex.split(call[3]), ["/data/opt/spotify-connect/spotiya"] + expected)

    def test_maintenance_never_starts_receiver_and_keeps_paths_as_single_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            adb = directory / "adb"
            adb.write_text("#!/usr/bin/env python3\nimport json, os, sys\nwith open(os.environ['SPOTIYA_TEST_LOG'], 'a') as f: f.write(json.dumps(sys.argv[1:]) + '\\n')\n")
            adb.chmod(0o700)
            log = directory / "calls.jsonl"
            env = dict(os.environ, PATH=str(directory) + os.pathsep + os.environ["PATH"], SPOTIYA_TEST_LOG=str(log))
            path = "/tmp/report with spaces; $(false).json"
            cases = [
                (["addon", "install", "airplay"], ["--addon-install", "airplay"]),
                (["addon", "install", "soloist"], ["--addon-install", "soloist"]),
                (["addon", "remove", "airplay"], ["--addon-remove", "airplay"]),
                (["addon", "file", "airplay", path, "/tmp/AIRPLAY_SHA256SUMS"], ["--addon-install-file", "airplay", path, "/tmp/AIRPLAY_SHA256SUMS"]),
                (["doctor"], ["--doctor"]),
                (["uninstall", "--yes"], ["--uninstall", "--yes"]),
                (["doctor", "report", path], ["--doctor-report", path]),
                (["update", "check"], ["--update-check"]),
                (["update", "v0.8.0"], ["--update-tag", "v0.8.0"]),
                (["update", "file", path, "/tmp/SHA256SUMS"], ["--update-file", path, "/tmp/SHA256SUMS"]),
            ]
            for command, expected in cases:
                result = subprocess.run([str(ROOT / "bin/spotiya"), "--device", "fixture:5555"] + command, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(len(calls), len(cases))
            for call, (_, expected) in zip(calls, cases):
                self.assertEqual(call[:3], ["-s", "fixture:5555", "shell"])
                self.assertEqual(shlex.split(call[3]), ["/data/opt/spotify-connect/spotiya"] + expected)
