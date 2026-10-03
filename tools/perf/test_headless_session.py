import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from headless_session import (
    DEFAULT_OUTPUT,
    OUTPUT_PROFILES,
    SYSFS_DRM,
    HeadlessSession,
    RenderNode,
    SessionFailed,
    SessionUnavailable,
    open_session,
    pick_render_node,
    render_nodes,
    run_command,
    sway_config,
)

SCRIPT = Path(__file__).resolve().with_name("headless_session.py")
NVIDIA = RenderNode("renderD128", "0000:01:00.0", "0x10de", "nvidia")
RECORDED_VARIABLES = (
    "WLR_BACKENDS",
    "WLR_HEADLESS_OUTPUTS",
    "WLR_RENDERER",
    "WLR_RENDER_DRM_DEVICE",
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "DISPLAY",
    "SWAYSOCK",
)
FAKE_SWAY = f"""\
import json
import os
import signal
import subprocess
import sys

if sys.argv[1:] == ["--version"]:
    print("sway version 0.0-fake")
    sys.exit(0)
record = os.environ["FAKE_SWAY_RECORD"]
config = sys.argv[sys.argv.index("-c") + 1]
with open(config, encoding="utf-8") as handle:
    lines = handle.read().splitlines()
started = {{
    "argv": sys.argv[1:],
    "environ": {{name: os.environ.get(name) for name in {RECORDED_VARIABLES!r}}},
}}
with open(os.path.join(record, "sway.json"), "w", encoding="utf-8") as handle:
    json.dump(started, handle)
if os.environ["FAKE_SWAY_MODE"] == "fail":
    print("fake sway: cannot open the render node", file=sys.stderr)
    sys.exit(1)


def stop(signum, frame):
    open(os.path.join(record, "terminated"), "w").close()
    sys.exit(0)


signal.signal(signal.SIGTERM, stop)
command = next(line for line in lines if line.startswith("exec "))
ipc = {{"WAYLAND_DISPLAY": "wayland-fake", "SWAYSOCK": os.path.join(record, "ipc")}}
subprocess.run(
    ["sh", "-c", command.removeprefix("exec ")], env={{**os.environ, **ipc}}, check=True
)
while True:
    signal.pause()
"""


def add_render_node(sysfs, name, address, vendor, driver, bus="pci"):
    device = sysfs / "devices" / address
    (device / "drm" / name).mkdir(parents=True)
    (device / "vendor").write_text(f"{vendor}\n", encoding="ascii")
    drivers = sysfs / "bus" / bus / "drivers" / driver
    drivers.mkdir(parents=True, exist_ok=True)
    (device / "driver").symlink_to(drivers)
    (device / "subsystem").symlink_to(sysfs / "bus" / bus)
    (device / "drm" / name / "device").symlink_to(device)
    drm = sysfs / "class" / "drm"
    drm.mkdir(parents=True, exist_ok=True)
    (drm / name).symlink_to(device / "drm" / name)
    return drm


class RenderNodeTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.sysfs = Path(temp.name)

    def hybrid_laptop(self):
        add_render_node(self.sysfs, "renderD128", "0000:01:00.0", "0x10de", "nvidia")
        return add_render_node(
            self.sysfs, "renderD129", "0000:0e:00.0", "0x1002", "amdgpu"
        )

    def test_picks_the_node_of_the_requested_pci_address(self):
        node = pick_render_node(render_nodes(self.hybrid_laptop()), "0000:0e:00.0")
        self.assertEqual(
            node, RenderNode("renderD129", "0000:0e:00.0", "0x1002", "amdgpu")
        )

    def test_several_nodes_without_an_address_list_each_node(self):
        with self.assertRaises(SessionUnavailable) as raised:
            pick_render_node(render_nodes(self.hybrid_laptop()), None)
        message = str(raised.exception)
        self.assertIn("--gpu", message)
        self.assertIn(
            "renderD128: PCI 0000:01:00.0, vendor 0x10de, driver nvidia", message
        )
        self.assertIn(
            "renderD129: PCI 0000:0e:00.0, vendor 0x1002, driver amdgpu", message
        )

    def test_an_unknown_address_lists_the_nodes(self):
        with self.assertRaises(SessionUnavailable) as raised:
            pick_render_node(render_nodes(self.hybrid_laptop()), "0000:02:00.0")
        message = str(raised.exception)
        self.assertIn("0000:02:00.0", message)
        self.assertIn("renderD128: PCI 0000:01:00.0", message)
        self.assertIn("renderD129: PCI 0000:0e:00.0", message)

    def test_a_single_node_needs_no_address(self):
        drm = add_render_node(
            self.sysfs, "renderD128", "0000:01:00.0", "0x8086", "i915"
        )
        self.assertEqual(pick_render_node(render_nodes(drm), None).driver, "i915")

    def test_non_pci_render_nodes_are_not_candidates(self):
        add_render_node(self.sysfs, "renderD128", "vgem", "0x0000", "vgem", "platform")
        drm = add_render_node(
            self.sysfs, "renderD129", "0000:0e:00.0", "0x1002", "amdgpu"
        )
        self.assertEqual([node.name for node in render_nodes(drm)], ["renderD129"])


class SwayConfigTest(unittest.TestCase):
    def test_config_sets_the_mode_fullscreens_eclipse_and_disables_xwayland(self):
        lines = sway_config(OUTPUT_PROFILES["3840x2160@60"]).splitlines()
        self.assertIn("output HEADLESS-1 mode --custom 3840x2160@60Hz", lines)
        self.assertIn(
            'for_window [app_id="^io\\.github\\.kuenec\\.Eclipse$"] fullscreen enable',
            lines,
        )
        self.assertIn("xwayland disable", lines)


class CommandLineTest(unittest.TestCase):
    def test_missing_sway_exits_with_status_2_and_starts_nothing(self):
        with tempfile.TemporaryDirectory() as temp:
            marker = Path(temp) / "command-ran"
            empty = Path(temp) / "bin"
            empty.mkdir()
            completed = subprocess.run(
                [sys.executable, SCRIPT, "--", "/usr/bin/touch", marker],
                env={**os.environ, "PATH": str(empty)},
                capture_output=True,
                encoding="utf-8",
                timeout=30,
            )
            self.assertEqual(completed.returncode, 2)
            self.assertIn("sway is required for headless runs", completed.stderr)
            self.assertIn("never uses your desktop", completed.stderr)
            self.assertFalse(marker.exists())

    def test_command_gets_the_headless_display_and_returns_its_status(self):
        session = HeadlessSession(
            "sway version 0.0", DEFAULT_OUTPUT, NVIDIA, "wayland-9", "/run/ipc.sock"
        )
        desktop = {
            "WAYLAND_DISPLAY": "wayland-1",
            "DISPLAY": ":0",
            "WAYLAND_SOCKET": "7",
        }
        with tempfile.TemporaryDirectory() as temp:
            seen = Path(temp) / "seen"
            report = (
                'printf "%s|%s|%s|%s" "$WAYLAND_DISPLAY" "$SWAYSOCK" '
                '"${DISPLAY-unset}" "${WAYLAND_SOCKET-unset}" > "$0"; exit 7'
            )
            with mock.patch.dict(os.environ, desktop):
                status = run_command(session, ["sh", "-c", report, seen])
            self.assertEqual(status, 7)
            self.assertEqual(
                seen.read_text(encoding="utf-8"),
                "wayland-9|/run/ipc.sock|unset|unset",
            )

    def test_a_command_killed_by_a_signal_returns_128_plus_the_signal(self):
        session = HeadlessSession(
            "sway version 0.0", DEFAULT_OUTPUT, NVIDIA, "wayland-9", "/run/ipc.sock"
        )
        self.assertEqual(run_command(session, ["sh", "-c", "kill -TERM $$"]), 143)


class FakeSwaySessionTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.record = Path(temp.name)
        self.sway = self.record / "sway"
        self.sway.write_text(f"#!{sys.executable}\n{FAKE_SWAY}", encoding="utf-8")
        self.sway.chmod(0o755)

    def fake_environment(self, mode):
        return mock.patch.dict(
            os.environ,
            {
                "FAKE_SWAY_RECORD": str(self.record),
                "FAKE_SWAY_MODE": mode,
                "WAYLAND_DISPLAY": "wayland-1",
                "WAYLAND_SOCKET": "7",
                "DISPLAY": ":0",
                "SWAYSOCK": "/run/user/desktop-sway.sock",
            },
        )

    def started(self):
        return json.loads((self.record / "sway.json").read_text(encoding="utf-8"))

    def test_sway_runs_headless_on_the_chosen_gpu_without_the_desktop(self):
        output = OUTPUT_PROFILES["3840x2160@60"]
        with self.fake_environment("ready"):
            with open_session(str(self.sway), output, NVIDIA) as session:
                pass
        self.assertEqual(self.started()["argv"][:2], ["--unsupported-gpu", "-c"])
        self.assertEqual(
            self.started()["environ"],
            {
                "WLR_BACKENDS": "headless",
                "WLR_HEADLESS_OUTPUTS": "1",
                "WLR_RENDERER": "gles2",
                "WLR_RENDER_DRM_DEVICE": "/dev/dri/renderD128",
                "WAYLAND_DISPLAY": None,
                "WAYLAND_SOCKET": None,
                "DISPLAY": None,
                "SWAYSOCK": None,
            },
        )
        self.assertEqual(
            session,
            HeadlessSession(
                "sway version 0.0-fake",
                output,
                NVIDIA,
                "wayland-fake",
                str(self.record / "ipc"),
            ),
        )

    def test_leaving_the_session_stops_sway_and_deletes_the_run_dir(self):
        with self.fake_environment("ready"):
            with open_session(str(self.sway), DEFAULT_OUTPUT, NVIDIA):
                run_dir = Path(self.started()["argv"][2]).parent
                self.assertTrue(run_dir.is_dir())
        self.assertTrue((self.record / "terminated").exists())
        self.assertFalse(run_dir.exists())

    def test_sway_failing_to_start_reports_its_stderr(self):
        with self.fake_environment("fail"):
            with self.assertRaises(SessionFailed) as raised:
                with open_session(str(self.sway), DEFAULT_OUTPUT, NVIDIA):
                    self.fail("the session started")
        message = str(raised.exception)
        self.assertIn("exited with status 1", message)
        self.assertIn("fake sway: cannot open the render node", message)
        self.assertFalse(Path(self.started()["argv"][2]).parent.exists())


@unittest.skipIf(shutil.which("sway") is None, "sway is not installed")
class RealSwayTest(unittest.TestCase):
    def test_headless_output_has_the_requested_mode(self):
        nodes = render_nodes(SYSFS_DRM)
        if not nodes:
            self.skipTest("no PCI GPU has a DRM render node")
        with open_session(shutil.which("sway"), DEFAULT_OUTPUT, nodes[0]) as session:
            outputs = json.loads(
                subprocess.run(
                    ["swaymsg", "-r", "-t", "get_outputs"],
                    env=session.client_environment(os.environ),
                    capture_output=True,
                    encoding="utf-8",
                    check=True,
                    timeout=10,
                ).stdout
            )
        [output] = outputs
        mode = output["current_mode"]
        self.assertEqual(output["name"], "HEADLESS-1")
        self.assertEqual(
            (mode["width"], mode["height"], mode["refresh"]), (1920, 1080, 144000)
        )
        self.assertFalse(Path(session.swaysock).exists())


if __name__ == "__main__":
    unittest.main()
