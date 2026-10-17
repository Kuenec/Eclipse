import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

SESSION = Path(__file__).resolve().parent / "session.sh"

STUBS = {
    "dbus-run-session": '[ "$1" = -- ] && shift\nexec "$@"\n',
    "pipewire": 'echo "$$" >>"$STUB_DAEMONS"\nexec sleep 30\n',
    "pipewire-pulse": 'echo "$$" >>"$STUB_DAEMONS"\nexec sleep 30\n',
    "wireplumber": (
        'if [ "$1" = --version ]; then\n'
        '    echo "Linked with libwireplumber 0.5.18"\n'
        "    exit 0\n"
        "fi\n"
        'echo "$*" >"$STUB_DAEMONS.wireplumber"\n'
        'echo "$$" >>"$STUB_DAEMONS"\n'
        "exec sleep 30\n"
    ),
    "pactl": (
        'case "$1" in\n'
        'info) echo "Server String: ${STUB_SERVER_STRING:-$PULSE_SERVER}" ;;\n'
        "load-module) echo 7 ;;\n"
        "list)\n"
        "    for name in $STUB_SINKS; do\n"
        "        printf '1\\t%s\\tstub\\tfloat32le 2ch 48000Hz\\tIDLE\\n' \"$name\"\n"
        "    done\n"
        "    ;;\n"
        'set-default-sink) echo "$2" >"$STUB_DAEMONS.default" ;;\n'
        "esac\n"
    ),
}

COMMAND = (
    'printf "%s\\n" "$PULSE_SERVER" "$ECLIPSE_AUDIO_HARNESS" "$XDG_RUNTIME_DIR" '
    '"$XDG_CONFIG_HOME" >"$STUB_DAEMONS.seen"\n'
    'cp "$XDG_CONFIG_HOME/alsa/asoundrc" "$STUB_DAEMONS.asoundrc"\n'
    "exit 3\n"
)

HARNESS_SINKS = "harness_a harness_b harness_hfp"


class SessionScriptTest(unittest.TestCase):
    def setUp(self):
        self.work = tempfile.TemporaryDirectory()
        self.root = Path(self.work.name)
        self.bin = self.root / "bin"
        self.runtime = self.root / "runtime"
        self.bin.mkdir()
        self.runtime.mkdir()
        for name, body in {**STUBS, "command": COMMAND}.items():
            stub = self.bin / name
            stub.write_text(f"#!/usr/bin/env bash\n{body}", encoding="utf-8")
            stub.chmod(0o755)
        self.daemons = self.root / "daemons"

    def tearDown(self):
        self.work.cleanup()

    def session(self, **stub):
        environment = {
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "XDG_RUNTIME_DIR": str(self.runtime),
            "PULSE_SERVER": "unix:/run/user/1000/pulse/native",
            "STUB_DAEMONS": str(self.daemons),
            "STUB_SINKS": HARNESS_SINKS,
            **stub,
        }
        return subprocess.run(
            [str(SESSION), "--server", "pipewire", "--", str(self.bin / "command")],
            capture_output=True,
            text=True,
            env=environment,
            timeout=30,
        )

    def assert_cleaned_up(self):
        self.assertEqual(list(self.runtime.iterdir()), [])
        for pid in self.daemons.read_text(encoding="utf-8").split():
            deadline = time.monotonic() + 5
            while Path(f"/proc/{pid}").exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertFalse(Path(f"/proc/{pid}").exists(), f"daemon {pid} still runs")

    def test_the_command_runs_on_the_private_server_and_everything_is_removed(self):
        done = self.session()
        self.assertEqual(done.returncode, 3, done.stderr)
        server, harness, runtime, config = (
            (self.root / "daemons.seen").read_text(encoding="utf-8").split("\n")[:4]
        )
        self.assertTrue(harness.startswith(f"{self.runtime}/eclipse-audio."))
        self.assertEqual(runtime, f"{harness}/runtime")
        self.assertEqual(server, f"unix:{harness}/runtime/pulse/native")
        self.assertEqual(config, f"{harness}/config")
        self.assertEqual(
            (self.root / "daemons.asoundrc").read_text(encoding="utf-8"),
            "pcm.!default { type pulse }\nctl.!default { type pulse }\n",
        )
        self.assertEqual(
            (self.root / "daemons.wireplumber").read_text(encoding="utf-8"),
            "--profile policy\n",
        )
        self.assertEqual(
            (self.root / "daemons.default").read_text(encoding="utf-8"), "harness_a\n"
        )
        self.assertEqual(len(self.daemons.read_text(encoding="utf-8").split()), 3)
        self.assert_cleaned_up()

    def test_a_server_answering_elsewhere_is_refused(self):
        done = self.session(STUB_SERVER_STRING="unix:/run/user/1000/pulse/native")
        self.assertEqual(done.returncode, 1)
        self.assertIn("refusing to run: pactl reached", done.stderr)
        self.assertFalse((self.root / "daemons.seen").exists())
        self.assert_cleaned_up()

    def test_a_server_with_real_devices_is_refused(self):
        done = self.session(
            STUB_SINKS=f"{HARNESS_SINKS} alsa_output.pci-0000_00_1f.3.analog-stereo"
        )
        self.assertEqual(done.returncode, 1)
        self.assertIn(
            "refusing to run: the private server has other sinks: "
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            done.stderr,
        )
        self.assertFalse((self.root / "daemons.seen").exists())
        self.assert_cleaned_up()

    def test_usage_errors_start_nothing(self):
        for arguments in (
            [],
            ["--server", "jack", "--", "true"],
            ["--server", "pipewire", "true"],
            ["--server", "pipewire", "--"],
        ):
            done = subprocess.run(
                [str(SESSION), *arguments],
                capture_output=True,
                text=True,
                env={**os.environ, "PATH": f"{self.bin}:/usr/bin:/bin"},
                timeout=30,
            )
            self.assertEqual(done.returncode, 1, arguments)
            self.assertIn("usage: session.sh", done.stderr)
        self.assertFalse(self.daemons.exists())


if __name__ == "__main__":
    unittest.main()
