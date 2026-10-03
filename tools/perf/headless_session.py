#!/usr/bin/env python3

import argparse
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections import deque
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

SYSFS_DRM = Path("/sys/class/drm")
DEV_DRI = Path("/dev/dri")
OUTPUT_NAME = "HEADLESS-1"
RENDERER = "gles2"
ECLIPSE_APP_ID = r"^io\.github\.kuenec\.Eclipse$"
READY_VARIABLE = "ECLIPSE_HEADLESS_READY"
DESKTOP_VARIABLES = ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "SWAYSOCK")
CLIENT_HIDDEN_VARIABLES = ("WAYLAND_SOCKET", "DISPLAY")
READY_TIMEOUT_S = 10
STOP_GRACE_S = 5
POLL_INTERVAL_S = 0.05
STDERR_TAIL_LINES = 20
EXIT_FAILED = 1
EXIT_UNAVAILABLE = 2
SWAY_MISSING = (
    "sway is required for headless runs; install it (`sudo pacman -S sway` on "
    "Arch); the harness never uses your desktop"
)


@dataclass(frozen=True)
class OutputMode:
    width: int
    height: int
    refresh_hz: int

    def __str__(self):
        return f"{self.width}x{self.height}@{self.refresh_hz}"


OUTPUT_PROFILES = {
    str(mode): mode
    for mode in (OutputMode(1920, 1080, 144), OutputMode(3840, 2160, 60))
}
DEFAULT_OUTPUT = OUTPUT_PROFILES["1920x1080@144"]


@dataclass(frozen=True)
class RenderNode:
    name: str
    pci_address: str
    vendor: str
    driver: str

    def __str__(self):
        return (
            f"{self.name}: PCI {self.pci_address}, vendor {self.vendor}, "
            f"driver {self.driver}"
        )


@dataclass(frozen=True)
class HeadlessSession:
    sway_version: str
    output: OutputMode
    gpu: RenderNode
    wayland_display: str
    swaysock: str

    def client_environment(self, base):
        environment = without(base, CLIENT_HIDDEN_VARIABLES)
        environment["WAYLAND_DISPLAY"] = self.wayland_display
        environment["SWAYSOCK"] = self.swaysock
        return environment


class SessionUnavailable(Exception):
    pass


class SessionFailed(Exception):
    pass


def without(environment, names):
    return {name: value for name, value in environment.items() if name not in names}


def find_sway():
    sway = shutil.which("sway")
    if sway is None:
        raise SessionUnavailable(SWAY_MISSING)
    return sway


def render_nodes(sysfs_drm):
    nodes = []
    for entry in sorted(sysfs_drm.glob("renderD*")):
        device = (entry / "device").resolve()
        if (device / "subsystem").resolve().name != "pci":
            continue
        nodes.append(
            RenderNode(
                name=entry.name,
                pci_address=device.name,
                vendor=(device / "vendor").read_text(encoding="ascii").strip(),
                driver=(device / "driver").resolve().name,
            )
        )
    return nodes


def pick_render_node(nodes, pci_address):
    if not nodes:
        raise SessionUnavailable("no PCI GPU has a DRM render node")
    if pci_address is None and len(nodes) == 1:
        return nodes[0]
    for node in nodes:
        if node.pci_address == pci_address:
            return node
    listing = "".join(f"\n  {node}" for node in nodes)
    if pci_address is None:
        raise SessionUnavailable(
            f"several GPUs; pick one with --gpu PCI_ADDRESS:{listing}"
        )
    raise SessionUnavailable(
        f"no render node belongs to PCI device {pci_address}; "
        f"render nodes:{listing}"
    )


def sway_config(output):
    partial = f'"${READY_VARIABLE}.part"'
    ready = f'"${READY_VARIABLE}"'
    lines = (
        "xwayland disable",
        "default_border none",
        f"output {OUTPUT_NAME} mode --custom {output}Hz",
        f'for_window [app_id="{ECLIPSE_APP_ID}"] fullscreen enable',
        f'exec printf "%s\\n" "$WAYLAND_DISPLAY" "$SWAYSOCK" > {partial} '
        f"&& mv {partial} {ready}",
    )
    return "".join(f"{line}\n" for line in lines)


def collect_lines(stream, tail):
    with stream:
        tail.extend(stream)


class SwayProcess:
    def __init__(self, sway, config, environment):
        self.process = subprocess.Popen(
            [sway, "--unsupported-gpu", "-c", str(config)],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            errors="replace",
        )
        self.stderr_tail = deque(maxlen=STDERR_TAIL_LINES)
        self.reader = threading.Thread(
            target=collect_lines,
            args=(self.process.stderr, self.stderr_tail),
            daemon=True,
        )
        self.reader.start()

    def wait_until_ready(self, ready):
        deadline = time.monotonic() + READY_TIMEOUT_S
        while not ready.exists():
            if self.process.poll() is not None:
                raise self.failure(
                    f"sway exited with status {self.process.returncode} "
                    "before its session was ready"
                )
            if time.monotonic() > deadline:
                raise self.failure(
                    f"sway's session was not ready within {READY_TIMEOUT_S} s"
                )
            time.sleep(POLL_INTERVAL_S)
        endpoints = ready.read_text(encoding="utf-8").splitlines()
        if len(endpoints) != 2 or not all(endpoints):
            raise self.failure(
                f"sway reported {endpoints} instead of its Wayland display "
                "and IPC socket"
            )
        return endpoints

    def failure(self, reason):
        self.stop()
        tail = "".join(self.stderr_tail).rstrip()
        if not tail:
            return SessionFailed(f"{reason}; sway wrote nothing to stderr")
        return SessionFailed(f"{reason}; the end of sway's stderr:\n{tail}")

    def stop(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=STOP_GRACE_S)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.reader.join(STOP_GRACE_S)


@contextmanager
def open_session(sway, output, gpu):
    version = subprocess.run(
        [sway, "--version"],
        capture_output=True,
        encoding="utf-8",
        check=True,
        timeout=READY_TIMEOUT_S,
    ).stdout.strip()
    with tempfile.TemporaryDirectory(prefix="eclipse-headless-") as run_dir:
        config = Path(run_dir) / "sway.conf"
        config.write_text(sway_config(output), encoding="utf-8")
        ready = Path(run_dir) / "ready"
        environment = without(os.environ, DESKTOP_VARIABLES)
        environment.update(
            WLR_BACKENDS="headless",
            WLR_HEADLESS_OUTPUTS="1",
            WLR_RENDERER=RENDERER,
            WLR_RENDER_DRM_DEVICE=str(DEV_DRI / gpu.name),
        )
        environment[READY_VARIABLE] = str(ready)
        sway_process = SwayProcess(sway, config, environment)
        try:
            wayland_display, swaysock = sway_process.wait_until_ready(ready)
            yield HeadlessSession(version, output, gpu, wayland_display, swaysock)
        finally:
            sway_process.stop()


def run_command(session, command):
    completed = subprocess.run(command, env=session.client_environment(os.environ))
    if completed.returncode < 0:
        return 128 - completed.returncode
    return completed.returncode


def exit_on_signal(signum, frame):
    sys.exit(128 + signum)


def parse_arguments(argv):
    parser = argparse.ArgumentParser(
        description="Run CMD in a headless sway session that never uses your desktop."
    )
    parser.add_argument(
        "--gpu",
        metavar="PCI",
        help="PCI address of the GPU that renders, such as 0000:01:00.0",
    )
    parser.add_argument(
        "--output",
        choices=OUTPUT_PROFILES,
        default=str(DEFAULT_OUTPUT),
        help="output mode (default: %(default)s)",
    )
    parser.add_argument(
        "command", nargs="+", metavar="CMD", help="command to run, after --"
    )
    return parser.parse_args(argv)


def main(argv):
    signal.signal(signal.SIGTERM, exit_on_signal)
    arguments = parse_arguments(argv)
    try:
        sway = find_sway()
        gpu = pick_render_node(render_nodes(SYSFS_DRM), arguments.gpu)
        output = OUTPUT_PROFILES[arguments.output]
        with open_session(sway, output, gpu) as session:
            return run_command(session, arguments.command)
    except SessionUnavailable as error:
        print(f"headless_session.py: {error}", file=sys.stderr)
        return EXIT_UNAVAILABLE
    except SessionFailed as error:
        print(f"headless_session.py: {error}", file=sys.stderr)
        return EXIT_FAILED


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
