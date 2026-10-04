#!/usr/bin/env python3

import argparse
import bisect
import configparser
import hashlib
import json
import os
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
from collections import Counter, deque
from contextlib import contextmanager
from dataclasses import dataclass, field, replace
from enum import Enum
from pathlib import Path

from frametimes import RingError, nearest_rank, read_ring, summarize
from headless_session import (
    CLIENT_HIDDEN_VARIABLES,
    DEFAULT_OUTPUT,
    ECLIPSE_APP_ID,
    OUTPUT_PROFILES,
    RENDERER,
    SYSFS_DRM,
    SessionFailed,
    SessionUnavailable,
    find_sway,
    open_session,
    pick_render_node,
    render_nodes,
    without,
)

APP_ID = "io.github.kuenec.Eclipse"
PERF_DIR = Path(__file__).resolve().parents[2] / "build" / "perf"
PROC = Path("/proc")
USER_DATA = Path.home() / ".var" / "app" / APP_ID
CLOCK_TICKS_PER_S = os.sysconf("SC_CLK_TCK")
KIB_PER_MIB = 1024
NANOS_PER_MS = 1_000_000
BYTES_PER_BLOCK = 512
TICK_S = 1
EARLY_WINDOW_S = (0, 10)
FRAME_WINDOWS_S = (EARLY_WINDOW_S, (10, 70))
TMP_SAMPLE_S = 5
THREAD_SAMPLE_S = 5
THREADS_FROM_S = 10
PSS_SAMPLE_S = 60
OBSERVE_S = 70
CPU_WINDOW_S = 10
CLOSE_GRACE_S = 20
KILL_GRACE_S = 10
CLOSE_POLL_S = 0.1
READER_JOIN_S = 5
RUN_LOG_POLL_S = 0.05
INSTANCE_EXIT_S = 10
HIDE_RULE_GRACE_S = 3
HIDE_RULE_POLL_S = 0.25
COMMAND_TIMEOUT_S = 30
INSTALL_TIMEOUT_S = 600
FAILED_LOG_LINES = 200
TOP_THREADS = 8
FRAME_CAP_FPS = 60
SETTLED_IDLE_RATIO = 1.2
BOOTSTRAP_RESAMPLES = 2000
BOOTSTRAP_SEED = 0
INTERVAL_PERCENTS = (2.5, 97.5)
DIGITS = 3
TEXT_LIMIT = 64
SAFE_TEXT = re.compile(rf"[ -.0-~]{{0,{TEXT_LIMIT}}}")
LABEL = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,31}")
PORTAL_COMM = "flatpak-portal"
PORTAL_CLASS_PREFIX = "xdg-desktop-portal"
SPECIAL_WORKSPACE_PREFIX = "special:"
HYPRLAND_VARIABLES = ("HYPRLAND_INSTANCE_SIGNATURE", "WAYLAND_DISPLAY")
HIDE_RULE_PROBE = 'if not _G.eclipse_tests_hidden then error("missing") end'
BORROWED_SANDBOX = "--app-path runs borrow the installed app's permissions and runtime"
HIDE_RULE_MISSING = (
    "sway is not installed, and no window hider for test runs is active (the "
    "Hyprland Lua global eclipse_tests_hidden is unset); install sway (`sudo "
    "pacman -S sway` on Arch), or run a hider that moves only the windows of "
    "measure.py's processes, never every Eclipse window, to a special workspace "
    "and sets that global"
)
UPDATE_CHECK_MARKER = "Checking APKCombo"
FATAL_MARKERS = ("Roblox could not start", "eclipse run:", "panicked")
VERSION_CODE = re.compile(r"\(versionCode (\d+)\)")
GPU_NAME = re.compile(r'\[FLog::Graphics\] Vulkan Device: (.*?)(?: tag="[^"]*")?$')
VENDOR_ID = re.compile(r"\bvendorId=(\d+)\b")
EXTENT = re.compile(r"\bwidth=(\d+) height=(\d+)\b")
ECLIPSE_VERSION = re.compile(r"^eclipse (\S+)$", re.MULTILINE)
PSS_LINE = re.compile(r"^Pss:\s+(\d+) kB$", re.MULTILINE)
MAPPING_HEADER = re.compile(r"^[0-9a-f]+-[0-9a-f]+ \S+ [0-9a-f]+ \S+ \d+\s*(.*)$")
EXIT_FAILED = 1
EXIT_UNAVAILABLE = 2
INTERRUPTED_STATUS = 128 + signal.SIGINT
OBSERVE_DEFAULT_S = 600
OBSERVE_MAX_S = 14400
STARTUP_MARKER = "setting caps.videoMemory"
OUTPUT_CHUNK_BYTES = 1 << 16
APP_ID_SEGMENT = r"[A-Za-z_][A-Za-z0-9_-]*"
FLATPAK_APP_ID = re.compile(rf"{APP_ID_SEGMENT}(?:\.{APP_ID_SEGMENT}){{2,}}")
FLATPAK_APP_ID_LIMIT = 255
OBSERVED_RUNTIME = "observe reads the app's runtime from its installation"


class Launch(Enum):
    WARM = "warm"
    FIRST = "first"


FIRST_FRAME_TIMEOUT_S = {Launch.WARM: 120, Launch.FIRST: 300}


class Marker(Enum):
    LAUNCHING = "Launching the installed Roblox"
    ART_BOOTED = "ART VM booted"
    WINDOW = "host window created"
    HANDOFF = "present-loop handoff"
    FIRST_FRAME = "seam armed"


class Discard(Enum):
    UPDATE_CHECK = "update_check"
    FATAL = "fatal"
    FOREIGN_INSTANCE = "foreign_instance"
    DESKTOP_WINDOW = "desktop_window"
    USER_DATA = "user_data"
    WRONG_GPU = "wrong_gpu"
    WRONG_EXTENT = "wrong_extent"
    NO_FIRST_FRAME = "no_first_frame"
    CLIENT_EXITED = "client_exited"
    NO_FRAME_LOG = "no_frame_log"


ROW_FIELDS = (
    "series",
    "label",
    "launch",
    "run",
    "discard",
    "loadavg_start",
    "loadavg_end",
    "eclipse_version",
    "eclipse_sha256",
    "roblox_version_code",
    "gpu_name",
    "gpu_vendor",
    "gpu_pci",
    "render_node",
    "compositor",
    "compositor_version",
    "renderer",
    "mode",
    "extent",
    "audio",
    "desktop_check",
    "markers_s",
    "pss_mib",
    "pss_mib_by_comm",
    "frame_log_pss_mib",
    "tmp_peak_bytes",
    "tmp_peak_allocated_bytes",
    "tmp_at_pss_bytes",
    "tmp_at_pss_allocated_bytes",
    "cpu_percent_0_10",
    "cpu_percent_windows",
    "top_threads",
    "frames_0_10",
    "frames_10_70",
    "settled_idle",
    "profile_bytes",
    "close_s",
    "close_forced",
)
SERIES_FIELDS = (
    "gpu_name",
    "gpu_vendor",
    "gpu_pci",
    "render_node",
    "compositor",
    "compositor_version",
    "renderer",
    "mode",
    "audio",
)


class Refused(Exception):
    pass


class SeriesFailed(Exception):
    pass


def short_text(value):
    printable = "".join(char for char in value if " " <= char <= "~" and char != "/")
    return printable.strip()[:TEXT_LIMIT]


def checked(value):
    if value is None or isinstance(value, (bool, int, float)):
        return value
    if isinstance(value, str):
        if not SAFE_TEXT.fullmatch(value):
            raise ValueError(f"a perf result may not hold the text {value!r}")
        return value
    if isinstance(value, (list, tuple)):
        return [checked(item) for item in value]
    if isinstance(value, dict):
        return {checked(key): checked(item) for key, item in value.items()}
    raise TypeError(f"a perf result may not hold {type(value).__name__} values")


def serialize(record, fields):
    if tuple(record) != fields:
        raise ValueError(f"a perf record needs exactly the fields {fields}")
    return json.dumps(checked(record), separators=(",", ":"))


def run_checked(command, timeout=COMMAND_TIMEOUT_S, **options):
    return subprocess.run(
        command,
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        check=True,
        timeout=timeout,
        **options,
    )


@dataclass
class ClientOutput:
    markers: dict = field(default_factory=dict)
    version_code: int | None = None
    gpu_name: str | None = None
    vendor_id: int | None = None
    extent: tuple | None = None
    update_check: bool = False
    fatal: bool = False

    def feed(self, line, seconds):
        for marker in Marker:
            if marker.value in line and marker not in self.markers:
                self.markers[marker] = round(seconds, DIGITS)
                self.parse_marker_line(marker, line)
        self.update_check = self.update_check or UPDATE_CHECK_MARKER in line
        self.fatal = self.fatal or any(marker in line for marker in FATAL_MARKERS)
        if self.gpu_name is None and (name := GPU_NAME.search(line)):
            self.gpu_name = short_text(name.group(1))
        if self.vendor_id is None and (vendor := VENDOR_ID.search(line)):
            self.vendor_id = int(vendor.group(1))

    def parse_marker_line(self, marker, line):
        if marker is Marker.LAUNCHING and (code := VERSION_CODE.search(line)):
            self.version_code = int(code.group(1))
        if marker is Marker.FIRST_FRAME and (extent := EXTENT.search(line)):
            self.extent = (int(extent.group(1)), int(extent.group(2)))


def link_target(path):
    return os.readlink(path) if path.is_symlink() else None


@dataclass(frozen=True)
class RunLogLink:
    path: Path
    previous: str | None

    @classmethod
    def before_launch(cls, path):
        return cls(path, link_target(path))

    def new_run(self):
        target = link_target(self.path)
        if target is None or target == self.previous:
            return None
        return self.path.parent / target


class OutputReader:
    def __init__(self, stream, run_log, started):
        self.lock = threading.Lock()
        self.output = ClientOutput()
        self.tail = deque(maxlen=FAILED_LOG_LINES)
        self.started = started
        self.stopped = threading.Event()
        self.threads = [
            threading.Thread(target=self.read_stream, args=(stream,), daemon=True),
            threading.Thread(target=self.follow, args=(run_log,), daemon=True),
        ]
        for thread in self.threads:
            thread.start()

    def take(self, line):
        with self.lock:
            self.output.feed(line, time.monotonic() - self.started)
            self.tail.append(line)

    def read_stream(self, stream):
        with stream:
            for raw in stream:
                self.take(raw.decode("utf-8", "replace").rstrip("\n"))

    def follow(self, run_log):
        while True:
            stopping = self.stopped.is_set()
            path = run_log.new_run()
            if path is not None:
                break
            if stopping:
                return
            self.stopped.wait(RUN_LOG_POLL_S)
        with path.open("rb") as log:
            pending = b""
            while True:
                stopping = self.stopped.is_set()
                *lines, pending = (pending + log.read()).split(b"\n")
                for line in lines:
                    self.take(line.decode("utf-8", "replace"))
                if stopping:
                    return
                self.stopped.wait(RUN_LOG_POLL_S)

    def finish(self):
        self.stopped.set()
        for thread in self.threads:
            thread.join(READER_JOIN_S)

    def snapshot(self):
        with self.lock:
            return replace(self.output, markers=dict(self.output.markers))

    def lines(self):
        with self.lock:
            return list(self.tail)


@dataclass(frozen=True)
class ProcessStat:
    comm: str
    ppid: int
    cpu_ticks: int
    start_ticks: int


def parse_stat(text):
    head, _, tail = text.rpartition(")")
    fields = tail.split()
    return ProcessStat(
        comm=head.partition("(")[2],
        ppid=int(fields[1]),
        cpu_ticks=int(fields[11]) + int(fields[12]),
        start_ticks=int(fields[19]),
    )


def read_processes(proc=PROC):
    table = {}
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            text = (entry / "stat").read_text(encoding="utf-8", errors="replace")
        except (FileNotFoundError, ProcessLookupError):
            continue
        table[int(entry.name)] = parse_stat(text)
    return table


@dataclass(frozen=True)
class Instance:
    instance: str
    pid: int
    application: str
    runtime: str


def parse_flatpak_ps(text):
    instances = []
    for line in text.splitlines():
        if not line.strip():
            continue
        instance, pid, application, runtime = (line.split("\t") + ["", ""])[:4]
        instances.append(Instance(instance, int(pid), application, runtime))
    return instances


def app_runs(instances, app_id, runtime):
    return [
        instance
        for instance in instances
        if instance.application == app_id and instance.runtime == runtime
    ]


def flatpak_instances(app_id, runtime):
    output = run_checked(
        ["flatpak", "ps", "--columns=instance,pid,application,runtime"]
    )
    return app_runs(parse_flatpak_ps(output.stdout), app_id, runtime)


def describe_instances(instances):
    return ", ".join(f"instance {i.instance} (pid {i.pid})" for i in instances)


def ancestry(pid, table):
    chain = []
    while pid in table and pid not in chain:
        chain.append(pid)
        pid = table[pid].ppid
    return chain


def descendants(roots, table):
    children = {}
    for pid, stat in table.items():
        children.setdefault(stat.ppid, []).append(pid)
    found = set()
    pending = [pid for pid in roots if pid in table]
    while pending:
        pid = pending.pop()
        if pid not in found:
            found.add(pid)
            pending.extend(children.get(pid, ()))
    return found


class Origin(Enum):
    LAUNCHED = "launched"
    PORTAL = "portal"
    FOREIGN = "foreign"


def instance_origin(instance, table, launcher_pid):
    chain = ancestry(instance.pid, table)
    if launcher_pid in chain:
        return Origin.LAUNCHED
    if any(table[pid].comm == PORTAL_COMM for pid in chain):
        return Origin.PORTAL
    return Origin.FOREIGN


class ProcessTree:
    def __init__(self):
        self.pids = set()
        self.comms = {}
        self.cpu_ticks = {}

    def follow(self, roots, table):
        self.pids = descendants(roots, table)
        for pid in self.pids:
            stat = table[pid]
            self.comms[pid] = stat.comm
            self.cpu_ticks[(pid, stat.start_ticks)] = stat.cpu_ticks

    def total_cpu_ticks(self):
        return sum(self.cpu_ticks.values())


class ProcessSet:
    def __init__(self, launcher_pid):
        self.launcher_pid = launcher_pid
        self.launched = {}
        self.portal = {}
        self.foreign_seen = False
        self.tree = ProcessTree()

    def refresh(self, instances, table):
        for instance in instances:
            if instance.pid not in table:
                continue
            match instance_origin(instance, table, self.launcher_pid):
                case Origin.LAUNCHED:
                    self.launched[instance.instance] = instance
                case Origin.PORTAL:
                    self.portal[instance.instance] = instance
                case Origin.FOREIGN:
                    self.foreign_seen = True
        roots = {self.launcher_pid} | {i.pid for i in self.owned_instances()}
        self.tree.follow(roots, table)

    def owned_instances(self):
        if self.foreign_seen:
            return list(self.launched.values())
        return [*self.launched.values(), *self.portal.values()]


def sample_seconds(sample):
    return sample[0]


def cpu_windows(samples, start_s, end_s, width_s, ticks_per_s):
    windows = []
    for window_start in range(start_s, end_s, width_s):
        first = bisect.bisect_left(samples, window_start, key=sample_seconds)
        last = bisect.bisect_left(samples, window_start + width_s, key=sample_seconds)
        if last == len(samples) or samples[last][0] <= samples[first][0]:
            break
        busy_s = (samples[last][1] - samples[first][1]) / ticks_per_s
        elapsed_s = samples[last][0] - samples[first][0]
        windows.append(round(100 * busy_s / elapsed_s, 1))
    return windows


def window_cpu_percent(samples, start_s, end_s):
    windows = cpu_windows(samples, start_s, end_s, end_s - start_s, CLOCK_TICKS_PER_S)
    return windows[0] if windows else None


def tree_usage(root):
    apparent = allocated = 0
    seen = set()
    for directory, directories, files in os.walk(root):
        for name in directories + files:
            try:
                status = os.lstat(os.path.join(directory, name))
            except FileNotFoundError:
                continue
            if (status.st_dev, status.st_ino) in seen:
                continue
            seen.add((status.st_dev, status.st_ino))
            apparent += status.st_size
            allocated += status.st_blocks * BYTES_PER_BLOCK
    return apparent, allocated


def rollup_pss_kib(text):
    match = PSS_LINE.search(text)
    return int(match.group(1)) if match else 0


def mapping_paths(maps_text):
    paths = []
    for line in maps_text.splitlines():
        header = MAPPING_HEADER.match(line)
        if header and header.group(1):
            paths.append(header.group(1))
    return paths


def mapping_pss_kib(smaps_text, path):
    total = 0
    inside = False
    for line in smaps_text.splitlines():
        header = MAPPING_HEADER.match(line)
        if header:
            inside = header.group(1) == path
        elif inside and line.startswith("Pss:"):
            total += int(line.split()[1])
    return total


def is_under(path, root):
    return path == str(root) or path.startswith(f"{root}/")


def open_paths(pid, proc=PROC):
    base = proc / str(pid)
    paths = [os.readlink(base / "cwd")]
    for fd in os.listdir(base / "fd"):
        try:
            paths.append(os.readlink(base / "fd" / fd))
        except FileNotFoundError:
            continue
    return paths


@dataclass(frozen=True)
class MemorySample:
    pss_kib: int
    by_comm: dict
    frame_log_kib: int
    user_data_paths: int


def sample_memory(pids, comms, ring_path, user_data=USER_DATA, proc=PROC):
    total = frame_log = user_data_paths = 0
    by_comm = Counter()
    for pid in sorted(pids):
        base = proc / str(pid)
        try:
            pss = rollup_pss_kib((base / "smaps_rollup").read_text(encoding="utf-8"))
            maps = mapping_paths((base / "maps").read_text(encoding="utf-8"))
            ring = 0
            if ring_path is not None and str(ring_path) in maps:
                smaps = (base / "smaps").read_text(encoding="utf-8")
                ring = mapping_pss_kib(smaps, str(ring_path))
            paths = maps + open_paths(pid, proc)
        except (FileNotFoundError, ProcessLookupError):
            continue
        total += pss
        frame_log += ring
        by_comm[short_text(comms.get(pid, ""))] += pss
        user_data_paths += sum(is_under(path, user_data) for path in paths)
    return MemorySample(total, dict(by_comm), frame_log, user_data_paths)


def thread_times(pids, proc=PROC):
    times = {}
    for pid in pids:
        try:
            tasks = list((proc / str(pid) / "task").iterdir())
        except (FileNotFoundError, ProcessLookupError):
            continue
        for task in tasks:
            try:
                nanos = int((task / "schedstat").read_text().split()[0])
                comm = (task / "comm").read_text(errors="replace").strip()
            except (FileNotFoundError, ProcessLookupError):
                continue
            times[(pid, int(task.name))] = (comm, nanos)
    return times


def read_wchan(pid, tid, proc=PROC):
    try:
        return (proc / str(pid) / "task" / str(tid) / "wchan").read_text()
    except (FileNotFoundError, ProcessLookupError):
        return ""


def top_threads(start, end, comms, proc=PROC):
    busiest = sorted(
        (
            (nanos - start.get(key, ("", 0))[1], key, comm)
            for key, (comm, nanos) in end.items()
        ),
        reverse=True,
    )[:TOP_THREADS]
    return [
        {
            "thread": short_text(comm),
            "process": short_text(comms.get(pid, "")),
            "cpu_ms": round(delta / NANOS_PER_MS),
            "wchan": short_text(read_wchan(pid, tid, proc)),
        }
        for delta, (pid, tid), comm in busiest
    ]


def hyprctl(*arguments):
    return run_checked(["hyprctl", *arguments]).stdout.strip()


def hide_rule_probe():
    probe = subprocess.run(
        ["hyprctl", "eval", HIDE_RULE_PROBE],
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=COMMAND_TIMEOUT_S,
    )
    return probe.returncode == 0 and probe.stdout.strip() == "ok"


def hide_rule_active():
    deadline = time.monotonic() + HIDE_RULE_GRACE_S
    while not hide_rule_probe():
        if time.monotonic() >= deadline:
            return False
        time.sleep(HIDE_RULE_POLL_S)
    return True


def hyprctl_json(*arguments):
    return json.loads(hyprctl("-j", *arguments))


def is_hidden(client, monitors):
    shown = {monitor["activeWorkspace"]["id"] for monitor in monitors}
    shown |= {monitor["specialWorkspace"]["id"] for monitor in monitors}
    workspace = client["workspace"]
    return (
        workspace["name"].startswith(SPECIAL_WORKSPACE_PREFIX)
        and workspace["id"] not in shown
    )


def is_watched(client, pids):
    return (
        client["pid"] in pids
        or APP_ID in (client["class"], client["initialClass"])
        or client["class"].startswith(PORTAL_CLASS_PREFIX)
    )


def desktop_windows(clients, monitors, pids, hidden_allowed):
    return [
        client
        for client in clients
        if is_watched(client, pids)
        and not (hidden_allowed and is_hidden(client, monitors))
    ]


@dataclass(frozen=True)
class HiddenHyprland:
    version: str
    expected_extent = None
    hidden_windows_allowed = True

    def describe(self):
        return {
            "compositor": "hyprland-hidden",
            "compositor_version": self.version,
            "renderer": None,
            "mode": None,
        }

    def client_environment(self, base):
        return without(base, CLIENT_HIDDEN_VARIABLES)

    def check_ready(self):
        if not hide_rule_active():
            raise SeriesFailed(
                "the window hider for test runs was gone for "
                f"{HIDE_RULE_GRACE_S} s, so the series stopped before the next boot"
            )

    def close_windows(self, pids):
        for client in hyprctl_json("clients"):
            if client["pid"] in pids:
                hyprctl(
                    "eval",
                    "hl.dispatch(hl.dsp.window.close({ window = "
                    f'"address:{client["address"]}" }}))',
                )


@dataclass(frozen=True)
class SwayHeadless:
    session: object
    hidden_windows_allowed = False

    @property
    def expected_extent(self):
        return (self.session.output.width, self.session.output.height)

    def describe(self):
        return {
            "compositor": "sway-headless",
            "compositor_version": short_text(self.session.sway_version),
            "renderer": RENDERER,
            "mode": str(self.session.output),
        }

    def client_environment(self, base):
        return self.session.client_environment(base)

    def check_ready(self):
        pass

    def close_windows(self, pids):
        subprocess.run(
            ["swaymsg", f'[app_id="{ECLIPSE_APP_ID}"] kill'],
            env=self.session.client_environment(os.environ),
            capture_output=True,
            timeout=COMMAND_TIMEOUT_S,
        )


def open_hidden_hyprland():
    if not all(os.environ.get(name) for name in HYPRLAND_VARIABLES):
        raise SessionUnavailable(
            "sway is required for headless runs, and without it runs use hidden "
            "windows on Hyprland, which needs HYPRLAND_INSTANCE_SIGNATURE and "
            "WAYLAND_DISPLAY; install sway (`sudo pacman -S sway` on Arch)"
        )
    if not hide_rule_active():
        raise SessionUnavailable(HIDE_RULE_MISSING)
    return HiddenHyprland(short_text(f"hyprland {hyprctl_json('version')['tag']}"))


@contextmanager
def open_compositor(output, gpu):
    try:
        sway = find_sway()
    except SessionUnavailable:
        sway = None
    if sway is not None:
        with open_session(sway, output or DEFAULT_OUTPUT, gpu) as session:
            yield SwayHeadless(session)
        return
    if output is not None:
        raise SessionUnavailable(
            "--output needs the headless sway session, and sway is not installed; "
            "hidden Hyprland windows keep the size Hyprland gives them"
        )
    yield open_hidden_hyprland()


@dataclass(frozen=True)
class App:
    label: str
    build_dir: Path | None

    def flatpak_run(self, *options):
        command = ["flatpak", "run"]
        if self.build_dir is not None:
            command.append(f"--app-path={self.build_dir / 'files'}")
        return [*command, *options]

    def binary(self):
        if self.build_dir is not None:
            return self.build_dir / "files" / "bin" / "eclipse"
        location = run_checked(["flatpak", "info", "--show-location", APP_ID])
        return Path(location.stdout.strip()) / "files" / "bin" / "eclipse"


@dataclass(frozen=True)
class Profile:
    root: Path

    def app_data(self):
        return self.root / "app-data"

    def environment(self):
        return {
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "ECLIPSE_APP_DATA_DIR": str(self.app_data()),
        }

    def run_log(self):
        return self.app_data() / "logs" / "eclipse.log"

    def create(self):
        for directory in self.environment().values():
            Path(directory).mkdir(parents=True)

    def update_check(self):
        return self.root / "data" / "eclipse" / "roblox" / "last-update-check.json"


def seed_update_check(profile, now):
    path = profile.update_check()
    path.parent.mkdir(parents=True, exist_ok=True)
    record = {"checked_at_unix": int(now), "rejected": None, "outcome": "completed"}
    path.write_text(json.dumps(record), encoding="utf-8")


def eclipse_command(app, profile, apk_dir, arguments, environment, frame_dir=None):
    options = [
        "--command=env",
        f"--filesystem={profile.root}",
        f"--filesystem={apk_dir}:ro",
        "--nosocket=pulseaudio",
    ]
    if frame_dir is not None:
        options.append(f"--filesystem=xdg-run/{frame_dir}")
    assignments = [
        f"{name}={value}"
        for name, value in (profile.environment() | environment).items()
    ]
    return [*app.flatpak_run(*options), APP_ID, *assignments, "eclipse", *arguments]


def boot_environment(ring_path):
    environment = {"ECLIPSE_HIDDEN_PACING": "off"}
    if ring_path is not None:
        environment["ECLIPSE_FRAMETIME_LOG"] = str(ring_path)
    return environment


def sandbox_contract(metadata):
    parser = configparser.ConfigParser(interpolation=None)
    parser.optionxform = str
    parser.read_string(metadata)
    context = dict(parser["Context"]) if parser.has_section("Context") else {}
    return {"runtime": parser.get("Application", "runtime"), "context": context}


def installed_metadata(app_id, purpose):
    try:
        return run_checked(["flatpak", "info", "--show-metadata", app_id]).stdout
    except subprocess.CalledProcessError as error:
        raise Refused(
            f"{app_id} is not installed; {purpose} ({error.stderr.strip()})"
        ) from error


def check_sandbox_contract(app):
    if app.build_dir is None:
        return
    build = sandbox_contract((app.build_dir / "metadata").read_text(encoding="utf-8"))
    installed = sandbox_contract(installed_metadata(APP_ID, BORROWED_SANDBOX))
    if build != installed:
        raise Refused(
            f"{app.label}: the build's runtime or [Context] permissions differ from "
            f"the installed {APP_ID}, and flatpak run --app-path uses the installed "
            f"ones; install a release with the same permissions and runtime first "
            f"(build {build}, installed {installed})"
        )


def binary_sha256(app):
    with app.binary().open("rb") as binary:
        return hashlib.file_digest(binary, "sha256").hexdigest()


def app_runtime(app_id, purpose):
    metadata = installed_metadata(app_id, purpose)
    return sandbox_contract(metadata)["runtime"].partition("/")[0]


def running_instances_message(runtime):
    instances = flatpak_instances(APP_ID, runtime)
    if not instances:
        return None
    return (
        f"{APP_ID} is already running ({describe_instances(instances)}); its /tmp "
        "is shared with the runs, so measure once it has exited"
    )


def median_difference(a, b, resamples=BOOTSTRAP_RESAMPLES, seed=BOOTSTRAP_SEED):
    rng = random.Random(seed)
    differences = [
        statistics.median(rng.choices(b, k=len(b)))
        - statistics.median(rng.choices(a, k=len(a)))
        for _ in range(resamples)
    ]
    low, high = (nearest_rank(differences, percent) for percent in INTERVAL_PERCENTS)
    return {
        "median_b_minus_a": round(statistics.median(b) - statistics.median(a), DIGITS),
        "interval": [round(low, DIGITS), round(high, DIGITS)],
        "beats_noise": not low <= 0 <= high,
    }


def percentile_summary(values):
    if not values:
        return None
    return {
        "p50": round(nearest_rank(values, 50), DIGITS),
        "p95": round(nearest_rank(values, 95), DIGITS),
    }


def early_frames(row, *keys):
    value = row["frames_0_10"]
    for key in keys:
        value = None if value is None else value[key]
    return [] if value is None else [value]


SETTLED_CPU = "cpu_percent_10_70"
METRICS = {
    "first_frame_s": lambda row: [row["markers_s"]["first_frame"]],
    "pss_mib": lambda row: [row["pss_mib"]],
    "cpu_percent_0_10": lambda row: [row["cpu_percent_0_10"]],
    "mean_fps_0_10": lambda row: early_frames(row, "mean_fps"),
    "interval_p99_ms_0_10": lambda row: early_frames(row, "interval_ms", "p99"),
    "seam_p99_ms_0_10": lambda row: early_frames(row, "seam_ms", "p99"),
    SETTLED_CPU: lambda row: row["cpu_percent_windows"],
    "close_s": lambda row: [row["close_s"]],
}
COMPARED_METRICS = (
    "first_frame_s",
    "pss_mib",
    "cpu_percent_0_10",
    "mean_fps_0_10",
    "interval_p99_ms_0_10",
    "seam_p99_ms_0_10",
)


def metric_values(rows, metric):
    return [value for row in rows for value in METRICS[metric](row)]


def compared_metrics(rows):
    compared = {metric: metric for metric in COMPARED_METRICS}
    phases = {row["settled_idle"] for row in rows}
    if phases == {True}:
        compared[f"idle_{SETTLED_CPU}"] = SETTLED_CPU
    elif phases == {False}:
        compared[SETTLED_CPU] = SETTLED_CPU
    return compared


def summarize_series(series_id, launch, labels, rows):
    valid = [row for row in rows if row["discard"] is None]
    summary = {"series": series_id, "launch": launch.value}
    summary |= {name: valid[0][name] if valid else None for name in SERIES_FIELDS}
    summary["labels"] = {}
    measured = {}
    for label in labels:
        label_rows = [row for row in rows if row["label"] == label]
        measured[label] = [
            row
            for row in valid
            if row["label"] == label and row["launch"] == launch.value
        ]
        first = [
            row["markers_s"]["first_frame"]
            for row in valid
            if row["label"] == label and row["launch"] == Launch.FIRST.value
        ]
        entry = {
            "valid_runs": len(measured[label]),
            "discarded": dict(
                Counter(row["discard"] for row in label_rows if row["discard"])
            ),
            "eclipse_version": label_rows[0]["eclipse_version"],
            "eclipse_sha256": label_rows[0]["eclipse_sha256"],
            "roblox_version_code": next(
                (row["roblox_version_code"] for row in measured[label]), None
            ),
            "first_launch_s": first[0] if launch is Launch.WARM and first else None,
            "settled_idle_runs": sum(
                bool(row["settled_idle"]) for row in measured[label]
            ),
        }
        for metric in METRICS:
            entry[metric] = percentile_summary(metric_values(measured[label], metric))
        summary["labels"][label] = entry
    summary["difference"] = None
    if len(labels) == 2:
        a, b = (measured[label] for label in labels)
        summary["difference"] = {}
        for name, metric in compared_metrics(a + b).items():
            values_a, values_b = metric_values(a, metric), metric_values(b, metric)
            if values_a and values_b:
                summary["difference"][name] = median_difference(values_a, values_b)
    return checked(summary)


def interleave(apps, runs, attempt):
    valid = Counter()
    attempts = Counter()
    while True:
        pending = [
            app for app in apps if valid[app] < runs and attempts[app] < 2 * runs
        ]
        if not pending:
            return all(valid[app] == runs for app in apps)
        for app in pending:
            attempts[app] += 1
            if attempt(app, attempts[app]):
                valid[app] += 1


@dataclass(frozen=True)
class Frames:
    early: dict | None = None
    late: dict | None = None
    settled_idle: bool | None = None


def summarize_frames(ring_path):
    ring = read_ring(ring_path)
    early, late = (summarize(ring, start, end) for start, end in FRAME_WINDOWS_S)
    if late["mean_fps"] is None:
        return Frames(early, late)
    return Frames(early, late, late["mean_fps"] < FRAME_CAP_FPS / SETTLED_IDLE_RATIO)


@dataclass
class RunState:
    processes: ProcessSet
    started: float
    loadavg_start: float
    discard: Discard | None = None
    cpu_samples: list = field(default_factory=list)
    tmp_peak: tuple = (0, 0)
    tmp_at_pss: tuple | None = None
    next_tmp_sample: float = 0.0
    thread_start: dict | None = None
    threads: list | None = None
    memory: MemorySample | None = None
    desktop_check: str = "unavailable"
    close_s: float | None = None
    close_forced: bool | None = None
    frames: Frames = Frames()
    loadavg_end: float | None = None
    profile_bytes: int | None = None


class Series:
    def __init__(self, series_id, runtime, apk_dir, frame_log, compositor, gpu, apps):
        self.id = series_id
        self.runtime = runtime
        self.root = PERF_DIR / "series" / series_id
        self.results = PERF_DIR / "results" / f"{series_id}.jsonl"
        self.apk_dir = apk_dir
        self.frame_log = frame_log
        self.frame_dir = f"eclipse-perf-{series_id}"
        self.runtime_dir = Path(os.environ["XDG_RUNTIME_DIR"])
        self.tmp_dir = self.runtime_dir / ".flatpak" / APP_ID / "tmp"
        self.compositor = compositor
        self.gpu = gpu
        self.apps = apps
        self.versions = {}
        self.hashes = {}
        self.profiles = Counter()
        self.rows = []

    def environment(self):
        return self.compositor.client_environment(os.environ)

    def eclipse(self, app, profile, arguments, environment, frame_dir=None):
        return eclipse_command(
            app, profile, self.apk_dir, arguments, environment, frame_dir
        )

    def instances(self):
        return flatpak_instances(APP_ID, self.runtime)

    @contextmanager
    def instances_exit(self, command):
        before = {instance.instance for instance in self.instances()}
        yield
        deadline = time.monotonic() + INSTANCE_EXIT_S
        while started := [i for i in self.instances() if i.instance not in before]:
            if time.monotonic() >= deadline:
                raise SeriesFailed(
                    f"{APP_ID} ({describe_instances(started)}) appeared while "
                    f"measure.py ran eclipse {command} and still runs "
                    f"{INSTANCE_EXIT_S} s after it returned"
                )
            time.sleep(CLOSE_POLL_S)

    def identify(self, app):
        profile = Profile(self.root / app.label / "identity")
        profile.create()
        with self.instances_exit("--version"):
            printed = run_checked(
                self.eclipse(app, profile, ["--version"], {}), env=self.environment()
            ).stdout
        match = ECLIPSE_VERSION.search(printed)
        if match is None:
            raise SeriesFailed(f"{app.label}: eclipse --version printed {printed!r}")
        self.versions[app] = short_text(match.group(1))
        self.hashes[app] = binary_sha256(app)
        shutil.rmtree(profile.root)

    def new_profile(self, app):
        self.profiles[app] += 1
        profile = Profile(self.root / app.label / f"profile-{self.profiles[app]}")
        profile.create()
        with self.instances_exit("install"):
            installed = subprocess.run(
                self.eclipse(app, profile, ["install", str(self.apk_dir)], {}),
                env=self.environment(),
                cwd=profile.root,
                capture_output=True,
                encoding="utf-8",
                errors="replace",
                timeout=INSTALL_TIMEOUT_S,
            )
        if installed.returncode != 0:
            tail = "\n".join((installed.stdout + installed.stderr).splitlines()[-20:])
            raise SeriesFailed(
                f"{app.label}: eclipse install failed with status "
                f"{installed.returncode}:\n{tail}"
            )
        seed_update_check(profile, time.time())
        return profile

    def boot(self, app, launch, run, profile):
        self.compositor.check_ready()
        if message := running_instances_message(self.runtime):
            raise SeriesFailed(message)
        name = f"{app.label}-{launch.value}-{run}"
        frame_dir = self.runtime_dir / self.frame_dir
        ring_path = frame_dir / f"{name}.bin" if self.frame_log else None
        environment = boot_environment(ring_path)
        if ring_path is not None:
            frame_dir.mkdir(mode=0o700)
        run_log = RunLogLink.before_launch(profile.run_log())
        loadavg_start = load_average()
        started = time.monotonic()
        launcher = subprocess.Popen(
            self.eclipse(
                app,
                profile,
                ["run"],
                environment,
                self.frame_dir if ring_path is not None else None,
            ),
            env=self.environment(),
            cwd=profile.root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        reader = OutputReader(launcher.stdout, run_log, started)
        state = RunState(ProcessSet(launcher.pid), started, loadavg_start)
        try:
            state.discard = self.observe(
                FIRST_FRAME_TIMEOUT_S[launch], launcher, reader, state, ring_path
            )
            self.close(launcher, state)
            reader.finish()
            if state.discard is None and ring_path is not None:
                if ring_path.exists():
                    state.frames = summarize_frames(ring_path)
                else:
                    state.discard = Discard.NO_FRAME_LOG
        except BaseException:
            self.kill(launcher, state.processes)
            raise
        finally:
            if frame_dir.exists():
                shutil.rmtree(frame_dir)
        state.loadavg_end = load_average()
        state.profile_bytes = tree_usage(profile.root)[0]
        row = self.row(app, launch, run, reader.snapshot(), state)
        self.record(name, row, reader.lines())
        if state.discard is Discard.DESKTOP_WINDOW:
            raise SeriesFailed(
                "an Eclipse or portal window appeared on the desktop, so the series "
                "stopped; check the window hider for test runs"
            )
        return row

    def observe(self, timeout, launcher, reader, state, ring_path):
        tick = state.started
        while True:
            discard = self.sample(timeout, launcher, reader, state, ring_path)
            if discard is not None or state.threads is not None:
                return discard
            tick += TICK_S
            time.sleep(max(0.0, tick - time.monotonic()))

    def sample(self, timeout, launcher, reader, state, ring_path):
        processes = state.processes
        processes.refresh(self.instances(), read_processes())
        tree = processes.tree
        now = time.monotonic()
        elapsed = now - state.started
        state.cpu_samples.append((elapsed, tree.total_cpu_ticks()))
        if now >= state.next_tmp_sample:
            state.tmp_peak = tuple(map(max, state.tmp_peak, tree_usage(self.tmp_dir)))
            state.next_tmp_sample = now + TMP_SAMPLE_S
        if self.desktop_window_shown(state):
            return Discard.DESKTOP_WINDOW
        if processes.foreign_seen:
            return Discard.FOREIGN_INSTANCE
        output = reader.snapshot()
        if output.update_check:
            return Discard.UPDATE_CHECK
        if output.fatal:
            return Discard.FATAL
        if output.vendor_id not in (None, int(self.gpu.vendor, 16)):
            return Discard.WRONG_GPU
        first_frame = output.markers.get(Marker.FIRST_FRAME)
        if first_frame is None:
            if launcher.poll() is not None or elapsed > timeout:
                return Discard.NO_FIRST_FRAME
            return None
        expected = self.compositor.expected_extent
        if expected is not None and output.extent != expected:
            return Discard.WRONG_EXTENT
        if launcher.poll() is not None:
            return Discard.CLIENT_EXITED
        since = elapsed - first_frame
        if since >= THREADS_FROM_S and state.thread_start is None:
            state.thread_start = thread_times(tree.pids)
        if since >= PSS_SAMPLE_S and state.memory is None:
            state.memory = sample_memory(tree.pids, tree.comms, ring_path)
            state.tmp_at_pss = tree_usage(self.tmp_dir)
            if state.memory.user_data_paths:
                return Discard.USER_DATA
        if since >= OBSERVE_S:
            end = thread_times(tree.pids)
            state.threads = top_threads(state.thread_start, end, tree.comms)
            if output.vendor_id is None:
                return Discard.WRONG_GPU
        return None

    def desktop_window_shown(self, state):
        if not os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
            return False
        state.desktop_check = "hyprland"
        windows = desktop_windows(
            hyprctl_json("clients"),
            hyprctl_json("monitors"),
            state.processes.tree.pids,
            self.compositor.hidden_windows_allowed,
        )
        return bool(windows)

    def close(self, launcher, state):
        requested = time.monotonic()
        self.compositor.close_windows(state.processes.tree.pids)
        state.close_forced = not self.wait_for_exit(
            launcher, state.processes, requested + CLOSE_GRACE_S
        )
        if state.close_forced:
            self.kill(launcher, state.processes)
        state.close_s = round(time.monotonic() - requested, DIGITS)

    def wait_for_exit(self, launcher, processes, deadline):
        owned = {instance.instance for instance in processes.owned_instances()}
        while time.monotonic() < deadline:
            running = {i.instance for i in self.instances()}
            if launcher.poll() is not None and not owned & running:
                return True
            time.sleep(CLOSE_POLL_S)
        return False

    def kill(self, launcher, processes):
        for instance in processes.owned_instances():
            subprocess.run(
                ["flatpak", "kill", instance.instance],
                capture_output=True,
                timeout=COMMAND_TIMEOUT_S,
            )
        if launcher.poll() is None:
            launcher.kill()
        deadline = time.monotonic() + KILL_GRACE_S
        if not self.wait_for_exit(launcher, processes, deadline):
            raise SeriesFailed(
                f"{APP_ID} instances started by this run still run after flatpak kill"
            )

    def row(self, app, launch, run, output, state):
        memory = state.memory
        first_frame = output.markers.get(Marker.FIRST_FRAME)
        since_first_frame = []
        if first_frame is not None:
            since_first_frame = [
                (elapsed - first_frame, ticks) for elapsed, ticks in state.cpu_samples
            ]
        tmp_at_pss = state.tmp_at_pss or (None, None)
        return {
            "series": self.id,
            "label": app.label,
            "launch": launch.value,
            "run": run,
            "discard": state.discard.value if state.discard else None,
            "loadavg_start": state.loadavg_start,
            "loadavg_end": state.loadavg_end,
            "eclipse_version": self.versions[app],
            "eclipse_sha256": self.hashes[app],
            "roblox_version_code": output.version_code,
            "gpu_name": output.gpu_name,
            "gpu_vendor": self.gpu.vendor,
            "gpu_pci": self.gpu.pci_address,
            "render_node": self.gpu.name,
            **self.compositor.describe(),
            "extent": list(output.extent) if output.extent else None,
            "audio": "none",
            "desktop_check": state.desktop_check,
            "markers_s": {
                marker.name.lower(): output.markers.get(marker) for marker in Marker
            },
            "pss_mib": mib(memory.pss_kib - memory.frame_log_kib) if memory else None,
            "pss_mib_by_comm": (
                {comm: mib(kib) for comm, kib in memory.by_comm.items()}
                if memory
                else None
            ),
            "frame_log_pss_mib": mib(memory.frame_log_kib) if memory else None,
            "tmp_peak_bytes": state.tmp_peak[0],
            "tmp_peak_allocated_bytes": state.tmp_peak[1],
            "tmp_at_pss_bytes": tmp_at_pss[0],
            "tmp_at_pss_allocated_bytes": tmp_at_pss[1],
            "cpu_percent_0_10": window_cpu_percent(since_first_frame, *EARLY_WINDOW_S),
            "cpu_percent_windows": cpu_windows(
                since_first_frame,
                THREADS_FROM_S,
                OBSERVE_S,
                CPU_WINDOW_S,
                CLOCK_TICKS_PER_S,
            ),
            "top_threads": state.threads,
            "frames_0_10": state.frames.early,
            "frames_10_70": state.frames.late,
            "settled_idle": state.frames.settled_idle,
            "profile_bytes": state.profile_bytes,
            "close_s": state.close_s,
            "close_forced": state.close_forced,
        }

    def record(self, name, row, lines):
        self.results.parent.mkdir(parents=True, exist_ok=True)
        with self.results.open("a", encoding="utf-8") as results:
            results.write(serialize(row, ROW_FIELDS) + "\n")
        self.rows.append(row)
        outcome = row["discard"] or f"first frame {row['markers_s']['first_frame']} s"
        print(f"measure.py: {name}: {outcome}", file=sys.stderr)
        if row["discard"] is not None:
            self.root.mkdir(parents=True, exist_ok=True)
            log = self.root / f"failed-{name}.log"
            log.write_text("".join(f"{line}\n" for line in lines), encoding="utf-8")
            print(f"measure.py: the end of its output is in {log}", file=sys.stderr)


def mib(kib):
    return round(kib / KIB_PER_MIB, 1)


def load_average():
    return round(os.getloadavg()[0], 2)


def run_launches(series, launch, runs):
    warm_profiles = {}

    def first_launch(app, attempt):
        profile = series.new_profile(app)
        row = series.boot(app, Launch.FIRST, attempt, profile)
        if row["discard"] is None and launch is Launch.WARM:
            warm_profiles[app] = profile
        else:
            shutil.rmtree(profile.root)
        return row["discard"] is None

    def warm_launch(app, attempt):
        row = series.boot(app, Launch.WARM, attempt, warm_profiles[app])
        return row["discard"] is None

    if launch is Launch.FIRST:
        return interleave(series.apps, runs, first_launch)
    return interleave(series.apps, 1, first_launch) and interleave(
        series.apps, runs, warm_launch
    )


def check_builds(series):
    for app in series.apps:
        check_sandbox_contract(app)
        if binary_sha256(app) != series.hashes[app]:
            raise SeriesFailed(
                f"{app.label}: bin/eclipse changed during the series, so its rows "
                "are discarded"
            )


def boot_command(arguments):
    apk_dir = arguments.apk_dir.resolve()
    if not apk_dir.is_dir():
        raise Refused(f"--apk-dir {apk_dir} is not a directory")
    if is_under(str(apk_dir), USER_DATA.resolve()):
        raise Refused(f"--apk-dir may not lie in {USER_DATA}, which holds your login")
    if "XDG_RUNTIME_DIR" not in os.environ:
        raise Refused("XDG_RUNTIME_DIR is unset; Flatpak runs need a user session")
    apps = arguments.app or [App("installed", None)]
    for app in apps:
        check_sandbox_contract(app)
    runtime = app_runtime(APP_ID, BORROWED_SANDBOX)
    if message := running_instances_message(runtime):
        raise Refused(message)
    gpu = pick_render_node(render_nodes(SYSFS_DRM), arguments.gpu)
    output = OUTPUT_PROFILES[arguments.output] if arguments.output else None
    launch = Launch(arguments.launch)
    series_id = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    with open_compositor(output, gpu) as compositor:
        series = Series(
            series_id,
            runtime,
            apk_dir,
            not arguments.no_frame_log,
            compositor,
            gpu,
            apps,
        )
        try:
            for app in apps:
                series.identify(app)
            complete = run_launches(series, launch, arguments.runs)
            check_builds(series)
        except SeriesFailed as error:
            raise SeriesFailed(
                f"{error}; the profiles and failure logs are in {series.root}"
            ) from error
    labels = [app.label for app in apps]
    summary = summarize_series(series.id, launch, labels, series.rows)
    summary_path = PERF_DIR / "results" / f"{series.id}-summary.json"
    summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print(f"measure.py: rows in {series.results}", file=sys.stderr)
    print(f"measure.py: summary in {summary_path}", file=sys.stderr)
    if not complete:
        raise SeriesFailed(
            f"fewer than {arguments.runs} valid runs per build after "
            f"{2 * arguments.runs} attempts; the profiles and failure logs are "
            f"in {series.root}"
        )
    shutil.rmtree(series.root)
    return 0


class End(Enum):
    SECONDS = "seconds"
    EXITED = "exited"
    INTERRUPTED = "interrupted"


OBSERVATION_FIELDS = (
    "app",
    "start",
    "end",
    "observed_s",
    "startup_s",
    "command_status",
    "loadavg_start",
    "loadavg_end",
    "cpu_percent",
    "cpu_percent_windows",
    "pss_mib",
    "pss_samples",
    "tmp_peak_bytes",
    "tmp_peak_allocated_bytes",
    "top_threads",
)


class MarkerWatch:
    def __init__(self, marker):
        self.marker = marker
        self.carried = b""
        self.found_s = None

    def feed(self, chunk, seconds):
        if self.found_s is not None:
            return
        window = self.carried + chunk
        if self.marker in window:
            self.found_s = round(seconds, DIGITS)
        kept = max(0, len(window) - len(self.marker) + 1)
        self.carried = window[kept:]

    def read(self, stream, started):
        with stream:
            while chunk := stream.read1(OUTPUT_CHUNK_BYTES):
                self.feed(chunk, time.monotonic() - started)


@dataclass(frozen=True)
class Launched:
    process: subprocess.Popen
    watch: MarkerWatch
    reader: threading.Thread


def launch(command, marker, started):
    try:
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
    except OSError as error:
        raise Refused(f"could not start {command[0]}: {error}") from error
    watch = MarkerWatch(marker.encode("utf-8"))
    reader = threading.Thread(
        target=watch.read, args=(process.stdout, started), daemon=True
    )
    reader.start()
    return Launched(process, watch, reader)


def next_multiple(elapsed_s, interval_s):
    return (elapsed_s // interval_s + 1) * interval_s


class Observation:
    def __init__(self, tmp_dir, started, steady_s, clock=time.monotonic, proc=PROC):
        self.tmp_dir = tmp_dir
        self.started = started
        self.steady_s = steady_s
        self.clock = clock
        self.proc = proc
        self.tree = ProcessTree()
        self.elapsed = 0.0
        self.cpu_samples = []
        self.pss_samples = []
        self.tmp_peak = (0, 0)
        self.thread_start = None
        self.threads = {}
        self.next_tmp_s = 0
        self.next_threads_s = 0
        self.next_pss_s = PSS_SAMPLE_S

    def sample(self, roots):
        self.tree.follow(roots, read_processes(self.proc))
        self.elapsed = self.clock() - self.started
        self.cpu_samples.append((self.elapsed, self.tree.total_cpu_ticks()))
        if self.elapsed >= self.next_tmp_s:
            self.tmp_peak = tuple(map(max, self.tmp_peak, tree_usage(self.tmp_dir)))
            self.next_tmp_s = next_multiple(self.elapsed, TMP_SAMPLE_S)
        if self.steady_s is None:
            return
        steady = self.elapsed - self.steady_s
        if steady >= self.next_threads_s:
            self.sample_threads()
            self.next_threads_s = next_multiple(steady, THREAD_SAMPLE_S)
        if steady >= self.next_pss_s:
            self.pss_samples.append(self.pss_sample())
            self.next_pss_s = next_multiple(steady, PSS_SAMPLE_S)

    def sample_threads(self):
        threads = thread_times(self.tree.pids, self.proc)
        if self.thread_start is None:
            self.thread_start = threads
        self.threads.update(threads)

    def pss_sample(self):
        by_comm = Counter()
        for pid in self.tree.pids:
            rollup = self.proc / str(pid) / "smaps_rollup"
            try:
                kib = rollup_pss_kib(rollup.read_text(encoding="utf-8"))
            except (FileNotFoundError, ProcessLookupError):
                continue
            by_comm[short_text(self.tree.comms[pid])] += kib
        return {
            "at_s": round(self.elapsed),
            "pss_mib": mib(by_comm.total()),
            "by_comm": {comm: mib(kib) for comm, kib in by_comm.most_common()},
        }

    def steady_cpu_windows(self):
        if self.steady_s is None:
            return []
        samples = [
            (seconds - self.steady_s, ticks) for seconds, ticks in self.cpu_samples
        ]
        return cpu_windows(samples, 0, OBSERVE_MAX_S, CPU_WINDOW_S, CLOCK_TICKS_PER_S)

    def steady_top_threads(self):
        if self.thread_start is None:
            return []
        return top_threads(self.thread_start, self.threads, self.tree.comms, self.proc)


def observe_until(observation, list_instances, launched, seconds):
    tick = observation.started
    try:
        while True:
            roots = {instance.pid for instance in list_instances()}
            if launched is not None:
                observation.steady_s = launched.watch.found_s
                if launched.process.poll() is None:
                    roots.add(launched.process.pid)
            observation.sample(roots)
            if not observation.tree.pids:
                return End.EXITED
            if observation.elapsed >= seconds:
                return End.SECONDS
            tick += TICK_S
            time.sleep(max(0.0, tick - time.monotonic()))
    except KeyboardInterrupt:
        return End.INTERRUPTED


def observation_record(app_id, end, observation, launched, loadavg_start):
    windows = observation.steady_cpu_windows()
    totals = [sample["pss_mib"] for sample in observation.pss_samples]
    return {
        "app": short_text(app_id),
        "start": "attach" if launched is None else "command",
        "end": end.value,
        "observed_s": round(observation.elapsed, DIGITS),
        "startup_s": None if launched is None else launched.watch.found_s,
        "command_status": None if launched is None else launched.process.poll(),
        "loadavg_start": loadavg_start,
        "loadavg_end": load_average(),
        "cpu_percent": percentile_summary(windows),
        "cpu_percent_windows": windows,
        "pss_mib": percentile_summary(totals),
        "pss_samples": observation.pss_samples,
        "tmp_peak_bytes": observation.tmp_peak[0],
        "tmp_peak_allocated_bytes": observation.tmp_peak[1],
        "top_threads": observation.steady_top_threads(),
    }


def wait_for_launch(launched, marker):
    if launched.watch.found_s is None:
        print(
            f"measure.py: no output of the command held {marker!r}, so it took no "
            "CPU, memory or thread numbers",
            file=sys.stderr,
        )
    if launched.reader.is_alive() or launched.process.poll() is None:
        print(
            "measure.py: waiting for the command to exit, so the app never writes "
            "to a closed pipe",
            file=sys.stderr,
        )
    try:
        launched.reader.join()
        launched.process.wait()
    except KeyboardInterrupt:
        return INTERRUPTED_STATUS
    return 0


def observe_command(arguments):
    if "XDG_RUNTIME_DIR" not in os.environ:
        raise Refused("XDG_RUNTIME_DIR is unset; Flatpak apps need a user session")
    app_id = arguments.app
    runtime = app_runtime(app_id, OBSERVED_RUNTIME)
    running = flatpak_instances(app_id, runtime)
    if arguments.cmd and running:
        raise Refused(
            f"{app_id} is already running ({describe_instances(running)}); close it "
            "to time a launch, or leave out CMD to attach to it"
        )
    if not arguments.cmd and not running:
        raise Refused(
            f"{app_id} is not running; start it first, or pass the command that "
            "starts it after --"
        )
    tmp_dir = Path(os.environ["XDG_RUNTIME_DIR"]) / ".flatpak" / app_id / "tmp"
    loadavg_start = load_average()
    started = time.monotonic()
    launched = None
    if arguments.cmd:
        launched = launch(arguments.cmd, arguments.marker, started)
    print(
        f"measure.py: observing {app_id} for up to {arguments.seconds} s; "
        "Ctrl+C stops early",
        file=sys.stderr,
    )
    observation = Observation(tmp_dir, started, None if launched else 0.0)
    end = observe_until(
        observation,
        lambda: flatpak_instances(app_id, runtime),
        launched,
        arguments.seconds,
    )
    if observation.steady_s is not None:
        observation.sample_threads()
    if launched is not None and end is End.EXITED:
        launched.reader.join(READER_JOIN_S)
    record = observation_record(app_id, end, observation, launched, loadavg_start)
    print(serialize(record, OBSERVATION_FIELDS), flush=True)
    if launched is None:
        return 0
    return wait_for_launch(launched, arguments.marker)


def parse_app(value):
    label, separator, build_dir = value.partition("=")
    if not separator or not LABEL.fullmatch(label):
        raise argparse.ArgumentTypeError(
            f"{value!r} is not LABEL=BUILD_DIR with a label of letters, digits, "
            "'.', '_' or '-'"
        )
    build = Path(build_dir).resolve()
    if not (build / "files" / "bin" / "eclipse").is_file():
        raise argparse.ArgumentTypeError(f"{build} has no files/bin/eclipse")
    if not (build / "metadata").is_file():
        raise argparse.ArgumentTypeError(f"{build} has no metadata file")
    return App(label, build)


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError(f"{value} is not a positive number of runs")
    return number


def flatpak_app_id(value):
    if len(value) > FLATPAK_APP_ID_LIMIT or not FLATPAK_APP_ID.fullmatch(value):
        raise argparse.ArgumentTypeError(
            f"{value!r} is not a Flatpak app ID such as org.vinegarhq.Sober"
        )
    return value


def observe_seconds(value):
    seconds = int(value)
    if not 1 <= seconds <= OBSERVE_MAX_S:
        raise argparse.ArgumentTypeError(
            f"{value} is not a number of seconds from 1 to {OBSERVE_MAX_S}"
        )
    return seconds


def marker_text(value):
    if not value:
        raise argparse.ArgumentTypeError("the marker may not be empty")
    return value


def parse_arguments(argv):
    parser = argparse.ArgumentParser(
        description="Measure Eclipse startup, memory, CPU and frame pacing."
    )
    commands = parser.add_subparsers(dest="command", required=True)
    boot = commands.add_parser(
        "boot",
        help="boot builds at the logged-out login screen and measure each run",
        description=(
            "Boot each build without installing it, in headless sway when it is "
            "installed and otherwise in hidden Hyprland windows."
        ),
    )
    boot.add_argument(
        "--apk-dir",
        type=Path,
        required=True,
        metavar="DIR",
        help="directory with Roblox's base.apk and split_config.x86_64.apk",
    )
    boot.add_argument(
        "--app",
        type=parse_app,
        action="append",
        metavar="LABEL=BUILD_DIR",
        help="flatpak-builder build directory or app checkout (default: installed)",
    )
    boot.add_argument("--runs", type=positive, default=10, help="valid runs per build")
    boot.add_argument(
        "--launch",
        choices=[launch.value for launch in Launch],
        default=Launch.WARM.value,
        help="warm: one first launch, then warm runs; first: a fresh profile per run",
    )
    boot.add_argument(
        "--output",
        choices=OUTPUT_PROFILES,
        help=f"headless sway output mode (default: {DEFAULT_OUTPUT})",
    )
    boot.add_argument("--gpu", metavar="PCI", help="PCI address of the GPU to use")
    boot.add_argument(
        "--no-frame-log",
        action="store_true",
        help="run without ECLIPSE_FRAMETIME_LOG",
    )
    boot.set_defaults(handler=boot_command)
    observe = commands.add_parser(
        "observe",
        help="measure a Flatpak app you run, such as Eclipse or Sober",
        description=(
            "Sample the CPU, memory and tmp use of every instance of a Flatpak app, "
            "its sandbox, D-Bus proxy and portal-spawned processes included, and "
            "print one JSON object of numbers. With CMD, time startup from the "
            "spawn to the first output line holding the marker, and take CPU, "
            "memory and thread numbers from then on; without CMD, attach to the "
            "running instances. Eclipse prints Roblox's log lines, which hold the "
            "default marker, only when run with --env=RUST_LOG=info."
        ),
    )
    observe.add_argument(
        "--app",
        type=flatpak_app_id,
        required=True,
        metavar="APP_ID",
        help="such as io.github.kuenec.Eclipse or org.vinegarhq.Sober",
    )
    observe.add_argument(
        "--seconds",
        type=observe_seconds,
        default=OBSERVE_DEFAULT_S,
        help=f"stop after this long, at most {OBSERVE_MAX_S} (default: %(default)s)",
    )
    observe.add_argument(
        "--marker",
        type=marker_text,
        default=STARTUP_MARKER,
        metavar="TEXT",
        help="output text that ends startup (default: %(default)r)",
    )
    observe.add_argument(
        "cmd", nargs="*", metavar="CMD", help="command that starts the app, after --"
    )
    observe.set_defaults(handler=observe_command)
    arguments = parser.parse_args(argv)
    if arguments.command == "boot":
        labels = [app.label for app in arguments.app or []]
        if len(set(labels)) != len(labels):
            parser.error("each --app needs its own label")
    return arguments


def exit_on_signal(signum, frame):
    sys.exit(128 + signum)


def main(argv):
    signal.signal(signal.SIGTERM, exit_on_signal)
    arguments = parse_arguments(argv)
    try:
        return arguments.handler(arguments)
    except (Refused, SessionUnavailable) as error:
        print(f"measure.py: {error}", file=sys.stderr)
        return EXIT_UNAVAILABLE
    except (SeriesFailed, SessionFailed, RingError) as error:
        print(f"measure.py: {error}", file=sys.stderr)
        return EXIT_FAILED


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
