import io
import json
import os
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from frametimes import NANOS_PER_SECOND, Entry, Ring, summarize
from headless_session import RenderNode, SessionUnavailable
from measure import (
    APP_ID,
    CLOCK_TICKS_PER_S,
    EARLY_WINDOW_S,
    OBSERVATION_FIELDS,
    ROW_FIELDS,
    STARTUP_MARKER,
    App,
    ClientOutput,
    Frames,
    HiddenHyprland,
    Instance,
    Launch,
    MarkerWatch,
    Marker,
    MemorySample,
    Observation,
    OutputReader,
    ProcessSet,
    ProcessStat,
    Profile,
    RunLogLink,
    RunState,
    Series,
    SeriesFailed,
    app_runs,
    cpu_windows,
    desktop_windows,
    eclipse_command,
    interleave,
    median_difference,
    open_compositor,
    parse_flatpak_ps,
    parse_stat,
    rollup_pss_kib,
    running_instances_message,
    sample_memory,
    sandbox_contract,
    seed_update_check,
    serialize,
    summarize_frames,
    summarize_series,
    thread_times,
    top_threads,
)

SCRIPT = Path(__file__).resolve().with_name("measure.py")
NVIDIA = RenderNode("renderD128", "0000:01:00.0", "0x10de", "nvidia")
FAKE_HYPRCTL = """\
#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_HYPRCTL_LOG"
case "$*" in
"eval if not _G.eclipse_tests_hidden then error(\\"missing\\") end")
    read -r misses < "$FAKE_MISSES"
    if [ "$FAKE_HIDE_RULE" = on ] && [ "$misses" -le 0 ]; then
        echo ok
    else
        echo $((misses - 1)) > "$FAKE_MISSES"
        echo 'error: missing'
        exit 7
    fi
    ;;
"-j version") echo '{"tag": "v0.56.2"}' ;;
"-j clients") while IFS= read -r line; do echo "$line"; done < "$FAKE_CLIENTS" ;;
*) echo ok ;;
esac
"""
FAKE_FLATPAK = """\
#!/bin/sh
case "$1 $2" in
"info --show-metadata")
    if [ "$3" != "$FAKE_APP" ]; then
        echo "error: $3 is not installed" >&2
        exit 1
    fi
    printf '[Application]\\nname=%s\\nruntime=org.example.Platform/x86_64/1\\n' "$3"
    ;;
"ps --columns=instance,pid,application,runtime") cat "$FAKE_PS" ;;
*)
    echo "unexpected: flatpak $*" >&2
    exit 9
    ;;
esac
"""
FAKE_LINGERING_FLATPAK = """\
#!/bin/sh
case "$1" in
run) echo "$FAKE_LINGER_POLLS" > "$FAKE_POLLS" ;;
ps)
    polls=$(cat "$FAKE_POLLS")
    if [ "$polls" -gt 0 ]; then
        printf '77\\t4242\\tio.github.kuenec.Eclipse\\torg.example.Platform\\n'
        echo $((polls - 1)) > "$FAKE_POLLS"
    fi
    ;;
*)
    echo "unexpected: flatpak $*" >&2
    exit 9
    ;;
esac
"""
GAME = "org.example.Game"
LAUNCH_LINE = "# Launching the installed Roblox 2.740.931 (versionCode 3170)"
WINDOW_RECORD = (
    "2026-10-03T09:20:06.187965Z  INFO eclipse::graphics: host window created "
    "(winit, no GTK) title=Eclipse — Roblox"
)
FIRST_FRAME_RECORD = (
    "2026-10-03T09:20:06.910230Z  INFO eclipse::loader::vk_overlay: vk-overlay: "
    "present seam armed (engine present interposed) device_set=true "
    "swapchain_set=true format=37 width={width} height={height} images=3"
)
BOOT_LINES = (
    (0.2, "# Verifying the Roblox client's signature…"),
    (0.9, LAUNCH_LINE),
    (
        0.9,
        "2026-10-03T09:20:05.700731Z  INFO stdout: # Launching the installed Roblox "
        "2.740.931 (versionCode 3171)",
    ),
    (3.0, "ART VM booted with Roblox's Java on the classpath ✓"),
    (4.0, WINDOW_RECORD),
    (
        5.0,
        "2026-10-03T09:20:06.756943Z  INFO liblog: 2026-10-03T09:20:06.727Z,0.727500,"
        "005e,6,Warning [FLog::Graphics] 'HTC unknown:NVIDIA GeForce RTX "
        "5070:2580660800' - PowerVR caps: vendorId=4318 supportsTextureMSAA=true "
        'tag="Roblox"',
    ),
    (
        5.0,
        "2026-10-03T09:20:06.756969Z  INFO liblog: 2026-10-03T09:20:06.727Z,0.727528,"
        '005e,6 [FLog::Graphics] Vulkan Device: NVIDIA GeForce RTX 5070 tag="Roblox"',
    ),
    (
        5.0,
        "2026-10-03T09:20:06.756972Z  INFO liblog: 2026-10-03T09:20:06.727Z,0.727530,"
        '005e,6 [FLog::Graphics] Vulkan Device: Vendor 10de Device 2f04 tag="Roblox"',
    ),
    (
        5.1,
        "2026-10-03T09:20:06.796681Z  INFO eclipse::graphics: Eclipse released its "
        "Vulkan renderer then dispatched the SurfaceView lifecycle (surfaceCreated + "
        "surfaceChanged); present-loop handoff (drop-before-dispatch) width=800 "
        "height=600",
    ),
    (5.5, FIRST_FRAME_RECORD.format(width=1920, height=1080)),
    (9.0, FIRST_FRAME_RECORD.format(width=640, height=480)),
    (
        9.5,
        "2026-10-03T09:20:15.000000Z  INFO stderr: opened "
        "/home/zzuserzz/.var/app/io.github.kuenec.Eclipse on zzhostzz",
    ),
)


def default_sigint():
    signal.signal(signal.SIGINT, signal.SIG_DFL)


def client_output(lines=BOOT_LINES):
    output = ClientOutput()
    for seconds, line in lines:
        output.feed(line, seconds)
    return output


def stat_line(pid, comm, ppid, cpu_ticks, start_ticks=7):
    fields = ["S", ppid, 1, 1, 0, -1, 0, 0, 0, 0, 0, cpu_ticks, 0]
    fields += [0, 0, 20, 0, 1, 0, start_ticks]
    return f"{pid} ({comm}) " + " ".join(str(value) for value in fields)


def table(processes):
    return {
        pid: ProcessStat(comm, ppid, ticks, 7)
        for pid, (comm, ppid, ticks) in processes.items()
    }


class ClientOutputTest(unittest.TestCase):
    def test_markers_version_code_gpu_and_extent_come_from_the_boot_lines(self):
        output = client_output()
        self.assertEqual(
            {marker.name: seconds for marker, seconds in output.markers.items()},
            {
                "LAUNCHING": 0.9,
                "ART_BOOTED": 3.0,
                "WINDOW": 4.0,
                "HANDOFF": 5.1,
                "FIRST_FRAME": 5.5,
            },
        )
        self.assertEqual(output.version_code, 3170)
        self.assertEqual(output.gpu_name, "NVIDIA GeForce RTX 5070")
        self.assertEqual(output.vendor_id, 0x10DE)
        self.assertEqual(output.extent, (1920, 1080))
        self.assertFalse(output.update_check)
        self.assertFalse(output.fatal)

    def test_update_checks_and_fatal_lines_are_flagged(self):
        output = client_output(
            [
                (1.0, "# Checking APKCombo for the newest Roblox client…"),
                (2.0, "thread 'main' panicked at src/main.rs:1:1"),
            ]
        )
        self.assertTrue(output.update_check)
        self.assertTrue(output.fatal)


class RunLogTest(unittest.TestCase):
    OLD_RUN = "eclipse-20261003T091434.289Z.log"
    NEW_RUN = "eclipse-20261003T092005.695Z.log"
    WAIT_S = 5

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.profile = Profile(Path(temp.name))
        self.logs = self.profile.run_log().parent
        self.logs.mkdir(parents=True)

    def start_run(self, name, text):
        (self.logs / name).write_text(text, encoding="utf-8")
        staging = self.logs / "eclipse.log.next"
        staging.symlink_to(name)
        staging.rename(self.profile.run_log())

    def reader(self, pipe_output=b""):
        run_log = RunLogLink.before_launch(self.profile.run_log())
        reader = OutputReader(io.BytesIO(pipe_output), run_log, time.monotonic())
        self.addCleanup(reader.finish)
        return reader

    def wait_for(self, reader, marker):
        deadline = time.monotonic() + self.WAIT_S
        while marker not in reader.snapshot().markers:
            self.assertLess(time.monotonic(), deadline, f"{marker} never arrived")
            time.sleep(0.01)

    def test_markers_come_from_the_pipe_and_the_new_run_log_only(self):
        old_frame = FIRST_FRAME_RECORD.format(width=1920, height=1080)
        self.start_run(self.OLD_RUN, f"{WINDOW_RECORD}\n{old_frame}\n")
        reader = self.reader(f"{LAUNCH_LINE}\n".encode())
        time.sleep(0.2)
        new_frame = FIRST_FRAME_RECORD.format(width=800, height=600)
        split = new_frame.index(" height=")
        self.start_run(self.NEW_RUN, f"{WINDOW_RECORD}\n{new_frame[:split]}")
        self.wait_for(reader, Marker.WINDOW)
        with (self.logs / self.NEW_RUN).open("a", encoding="utf-8") as log:
            log.write(f"{new_frame[split:]}\n")
        reader.finish()
        output = reader.snapshot()
        self.assertEqual(
            set(output.markers), {Marker.LAUNCHING, Marker.WINDOW, Marker.FIRST_FRAME}
        )
        self.assertEqual(output.version_code, 3170)
        self.assertEqual(
            output.extent,
            (800, 600),
            "the previous run's log is never read, and a line written in two parts "
            "is read whole",
        )

    def test_a_run_that_writes_no_new_log_stops_its_reader(self):
        self.start_run(self.OLD_RUN, f"{WINDOW_RECORD}\n")
        reader = self.reader()
        time.sleep(0.2)
        reader.finish()
        self.assertEqual(reader.snapshot().markers, {})
        self.assertFalse(any(thread.is_alive() for thread in reader.threads))


class ProcParsingTest(unittest.TestCase):
    def test_stat_parsing_survives_spaces_and_parentheses_in_comm(self):
        stat = parse_stat(stat_line(42, "Jit (pool) 1", 7, 30, 99))
        self.assertEqual(stat, ProcessStat("Jit (pool) 1", 7, 30, 99))

    def test_smaps_rollup_pss_is_read_in_kib(self):
        rollup = (
            "55d0c0a1b000-7ffd5c3f2000 ---p 00000000 00:00 0    [rollup]\n"
            "Rss:              123456 kB\n"
            "Pss:               98765 kB\n"
            "Pss_Dirty:          1234 kB\n"
        )
        self.assertEqual(rollup_pss_kib(rollup), 98765)

    def test_flatpak_ps_rows_with_and_without_an_application(self):
        listing = (
            "672099535\t2299\t\torg.gnome.Sdk\n"
            "1747885476\t4092128\tio.github.kuenec.Eclipse\torg.gnome.Platform\n"
            "1912789344\t4053937\torg.flatpak.Builder\torg.freedesktop.Sdk\n"
        )
        self.assertEqual(
            parse_flatpak_ps(listing),
            [
                Instance("672099535", 2299, "", "org.gnome.Sdk"),
                Instance("1747885476", 4092128, APP_ID, "org.gnome.Platform"),
                Instance(
                    "1912789344", 4053937, "org.flatpak.Builder", "org.freedesktop.Sdk"
                ),
            ],
        )

    def test_builds_and_sdk_shells_of_the_app_are_not_app_runs(self):
        run = Instance("1", 100, APP_ID, "org.gnome.Platform")
        build_step = Instance("2", 200, APP_ID, "org.gnome.Sdk")
        builder = Instance("3", 300, "org.flatpak.Builder", "org.freedesktop.Sdk")
        self.assertEqual(
            app_runs([run, build_step, builder], APP_ID, "org.gnome.Platform"), [run]
        )


class ProcessSetTest(unittest.TestCase):
    PROCESSES = {
        1: ("systemd", 0, 0),
        50: ("flatpak-portal", 1, 0),
        99: ("python3", 1, 0),
        100: ("bwrap", 99, 1),
        101: ("bwrap", 100, 1),
        102: ("eclipse", 101, 500),
        103: ("xdg-dbus-proxy", 100, 2),
        200: ("bwrap", 50, 1),
        201: ("WebKitWebProces", 200, 40),
        299: ("flatpak-builder", 1, 0),
        300: ("bwrap", 299, 1),
        301: ("make", 300, 9),
    }
    LAUNCHED = Instance("1", 100, APP_ID, "org.gnome.Platform")
    PORTAL = Instance("2", 200, APP_ID, "org.gnome.Platform")
    FOREIGN = Instance("3", 300, APP_ID, "org.gnome.Platform")

    def test_descendants_and_portal_spawned_instances_belong_to_the_run(self):
        processes = ProcessSet(100)
        processes.refresh([self.LAUNCHED, self.PORTAL], table(self.PROCESSES))
        self.assertEqual(processes.tree.pids, {100, 101, 102, 103, 200, 201})
        self.assertFalse(processes.foreign_seen)
        self.assertEqual(processes.owned_instances(), [self.LAUNCHED, self.PORTAL])

    def test_an_instance_outside_the_run_is_foreign_and_never_owned(self):
        processes = ProcessSet(100)
        processes.refresh(
            [self.LAUNCHED, self.PORTAL, self.FOREIGN], table(self.PROCESSES)
        )
        self.assertTrue(processes.foreign_seen)
        self.assertEqual(processes.owned_instances(), [self.LAUNCHED])
        self.assertEqual(processes.tree.pids, {100, 101, 102, 103})

    def test_an_instance_that_already_exited_is_not_foreign(self):
        processes = ProcessSet(100)
        gone = Instance("4", 400, APP_ID, "org.gnome.Platform")
        processes.refresh([self.LAUNCHED, gone], table({}))
        self.assertFalse(processes.foreign_seen)

    def test_cpu_windows_keep_the_ticks_of_a_process_that_exited(self):
        processes = ProcessSet(100)
        samples = []
        for second in range(0, 81):
            alive = {100: ("eclipse", 99, 50 * second)}
            if second < 35:
                alive[101] = ("helper", 100, 100 * second)
            processes.refresh([], table(alive))
            samples.append((second, processes.tree.total_cpu_ticks()))
        self.assertEqual(
            cpu_windows(samples, 10, 70, 10, 100),
            [150.0, 150.0, 90.0, 50.0, 50.0, 50.0],
        )

    def test_cpu_windows_stop_where_the_samples_end(self):
        samples = [(second, 10 * second) for second in range(0, 35)]
        self.assertEqual(cpu_windows(samples, 10, 70, 10, 100), [10.0, 10.0])


class ProcTreeTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.proc = Path(temp.name)
        self.user_data = Path("/home/u/.var/app") / APP_ID

    def process(self, pid, pss, maps="", smaps="", fds=()):
        base = self.proc / str(pid)
        (base / "fd").mkdir(parents=True)
        (base / "smaps_rollup").write_text(f"Pss: {pss} kB\n", encoding="utf-8")
        (base / "maps").write_text(maps, encoding="utf-8")
        (base / "smaps").write_text(smaps, encoding="utf-8")
        (base / "cwd").symlink_to("/home/u/perf/profile")
        for number, target in enumerate(fds):
            (base / "fd" / str(number)).symlink_to(target)

    def test_memory_sums_pss_finds_the_ring_and_counts_user_data_paths(self):
        ring = "/run/user/1000/eclipse-perf-1/main-warm-1.bin"
        mapping = f"7f0000000000-7f0000400000 rw-s 00000000 00:2a 77 {ring}"
        library = "7f1000000000-7f1000001000 r--p 00000000 08:01 5 /app/lib/x.so"
        self.process(
            100,
            9000,
            maps=f"{mapping}\n{library}\n",
            smaps=f"{mapping}\nSize: 4096 kB\nPss: 4096 kB\n{library}\nPss: 4 kB\n",
            fds=("/dev/null", f"{self.user_data}/data/cookies"),
        )
        self.process(101, 500)
        sample = sample_memory(
            {100, 101, 102},
            {100: "eclipse", 101: "bwrap"},
            Path(ring),
            self.user_data,
            self.proc,
        )
        self.assertEqual(
            sample,
            MemorySample(9500, {"eclipse": 9000, "bwrap": 500}, 4096, 1),
        )

    def test_top_threads_rank_cpu_time_and_count_new_threads_from_zero(self):
        threads = ((100, "eclipse", 9_000_000_000), (105, "Render", 3_000_000_000))
        for tid, comm, nanos in threads:
            task = self.proc / "100" / "task" / str(tid)
            task.mkdir(parents=True)
            (task / "schedstat").write_text(f"{nanos} 0 0\n", encoding="utf-8")
            (task / "comm").write_text(f"{comm}\n", encoding="utf-8")
            (task / "wchan").write_text("futex_wait_queue", encoding="utf-8")
        end = thread_times({100, 102}, self.proc)
        start = {(100, 100): ("eclipse", 8_000_000_000)}
        self.assertEqual(
            top_threads(start, end, {100: "eclipse"}, self.proc),
            [
                {
                    "thread": "Render",
                    "process": "eclipse",
                    "cpu_ms": 3000,
                    "wchan": "futex_wait_queue",
                },
                {
                    "thread": "eclipse",
                    "process": "eclipse",
                    "cpu_ms": 1000,
                    "wchan": "futex_wait_queue",
                },
            ],
        )


class DesktopWindowsTest(unittest.TestCase):
    MONITORS = [
        {
            "activeWorkspace": {"id": 1, "name": "1"},
            "specialWorkspace": {"id": 0, "name": ""},
        },
        {
            "activeWorkspace": {"id": 11, "name": "11"},
            "specialWorkspace": {"id": -98, "name": "special:scratch"},
        },
    ]

    def client(self, pid, klass, workspace_id, workspace_name):
        return {
            "pid": pid,
            "class": klass,
            "initialClass": klass,
            "workspace": {"id": workspace_id, "name": workspace_name},
        }

    def test_hidden_mode_allows_only_windows_on_an_unshown_special_workspace(self):
        hidden = self.client(102, APP_ID, -97, "special:eclipse-tests")
        other_workspace = self.client(102, APP_ID, 3, "3")
        shown_special = self.client(102, APP_ID, -98, "special:scratch")
        helper = self.client(201, "eclipse-webview", 1, "1")
        portal = self.client(77, "xdg-desktop-portal-gtk", 1, "1")
        unrelated = self.client(999, "firefox", 1, "1")
        clients = [hidden, other_workspace, shown_special, helper, portal, unrelated]
        self.assertEqual(
            desktop_windows(clients, self.MONITORS, {102, 201}, True),
            [other_workspace, shown_special, helper, portal],
        )

    def test_headless_mode_flags_every_eclipse_window_on_the_desktop(self):
        hidden = self.client(102, APP_ID, -97, "special:eclipse-tests")
        self.assertEqual(
            desktop_windows([hidden], self.MONITORS, {102}, False), [hidden]
        )


class FakeHyprlandTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.temp = Path(temp.name)
        bin_dir = self.temp / "bin"
        bin_dir.mkdir()
        hyprctl = bin_dir / "hyprctl"
        hyprctl.write_text(FAKE_HYPRCTL, encoding="utf-8")
        hyprctl.chmod(0o755)
        self.log = self.temp / "hyprctl.log"
        self.clients = self.temp / "clients.json"
        self.misses = self.temp / "misses"
        self.misses.write_text("0\n", encoding="utf-8")
        self.environment = {
            "PATH": str(bin_dir),
            "HYPRLAND_INSTANCE_SIGNATURE": "fake",
            "WAYLAND_DISPLAY": "wayland-1",
            "DISPLAY": ":0",
            "WAYLAND_SOCKET": "7",
            "FAKE_HIDE_RULE": "on",
            "FAKE_HYPRCTL_LOG": str(self.log),
            "FAKE_CLIENTS": str(self.clients),
            "FAKE_MISSES": str(self.misses),
        }
        grace = mock.patch("measure.HIDE_RULE_GRACE_S", 0.5)
        grace.start()
        self.addCleanup(grace.stop)

    def compositor(self, output=None, **overrides):
        with mock.patch.dict(os.environ, self.environment | overrides):
            with open_compositor(output, NVIDIA) as compositor:
                return compositor

    def test_without_sway_runs_use_hidden_hyprland_windows(self):
        compositor = self.compositor()
        self.assertEqual(compositor, HiddenHyprland("hyprland v0.56.2"))
        self.assertEqual(
            compositor.describe(),
            {
                "compositor": "hyprland-hidden",
                "compositor_version": "hyprland v0.56.2",
                "renderer": None,
                "mode": None,
            },
        )
        environment = compositor.client_environment(self.environment)
        self.assertEqual(environment["WAYLAND_DISPLAY"], "wayland-1")
        self.assertNotIn("DISPLAY", environment)
        self.assertNotIn("WAYLAND_SOCKET", environment)

    def test_a_missing_hide_rule_starts_nothing(self):
        with self.assertRaises(SessionUnavailable) as raised:
            self.compositor(FAKE_HIDE_RULE="off")
        self.assertIn("eclipse_tests_hidden", str(raised.exception))
        self.assertIn("install sway", str(raised.exception))
        self.assertIn("only the windows of measure.py's", str(raised.exception))
        self.assertNotIn(APP_ID, str(raised.exception))

    def test_a_desktop_without_hyprland_needs_sway(self):
        with self.assertRaises(SessionUnavailable) as raised:
            self.compositor(HYPRLAND_INSTANCE_SIGNATURE="")
        self.assertIn("sway is required for headless runs", str(raised.exception))

    def test_an_output_mode_needs_sway(self):
        with self.assertRaises(SessionUnavailable) as raised:
            self.compositor(output=object())
        self.assertIn("--output needs the headless sway session", str(raised.exception))

    def probes(self):
        return [line for line in self.log.read_text().splitlines() if "_G." in line]

    def test_a_hide_rule_that_a_reload_wiped_briefly_keeps_the_series(self):
        self.misses.write_text("2\n", encoding="utf-8")
        with mock.patch.dict(os.environ, self.environment):
            HiddenHyprland("hyprland v0.56.2").check_ready()
        self.assertEqual(len(self.probes()), 3)

    def test_a_hide_rule_that_stays_gone_stops_the_series(self):
        with mock.patch.dict(os.environ, self.environment | {"FAKE_HIDE_RULE": "off"}):
            with self.assertRaises(SeriesFailed) as raised:
                HiddenHyprland("hyprland v0.56.2").check_ready()
        self.assertIn("was gone for 0.5 s", str(raised.exception))
        self.assertGreater(len(self.probes()), 1)

    def test_closing_touches_only_windows_of_the_run(self):
        clients = [
            {"pid": 102, "address": "0x5501"},
            {"pid": 999, "address": "0x5502"},
        ]
        self.clients.write_text(json.dumps(clients) + "\n", encoding="utf-8")
        with mock.patch.dict(os.environ, self.environment):
            HiddenHyprland("hyprland v0.56.2").close_windows({102, 201})
        closes = [line for line in self.log.read_text().splitlines() if "close" in line]
        self.assertEqual(
            closes,
            [
                'eval hl.dispatch(hl.dsp.window.close({ window = "address:0x5501" }))',
            ],
        )


class LaunchTest(unittest.TestCase):
    def test_build_runs_borrow_the_installed_sandbox_with_an_isolated_profile(self):
        command = eclipse_command(
            App("main", Path("/b/main")),
            Profile(Path("/perf/series/1/main/profile-1")),
            Path("/apk"),
            ["run"],
            {"ECLIPSE_FRAMETIME_LOG": "/run/user/1000/eclipse-perf-1/main-warm-1.bin"},
            "eclipse-perf-1",
        )
        self.assertEqual(
            command,
            [
                "flatpak",
                "run",
                "--app-path=/b/main/files",
                "--command=env",
                "--filesystem=/perf/series/1/main/profile-1",
                "--filesystem=/apk:ro",
                "--nosocket=pulseaudio",
                "--filesystem=xdg-run/eclipse-perf-1",
                APP_ID,
                "XDG_DATA_HOME=/perf/series/1/main/profile-1/data",
                "XDG_CACHE_HOME=/perf/series/1/main/profile-1/cache",
                "XDG_CONFIG_HOME=/perf/series/1/main/profile-1/config",
                "ECLIPSE_APP_DATA_DIR=/perf/series/1/main/profile-1/app-data",
                "ECLIPSE_FRAMETIME_LOG=/run/user/1000/eclipse-perf-1/main-warm-1.bin",
                "eclipse",
                "run",
            ],
        )

    def test_the_installed_build_runs_without_an_app_path(self):
        command = eclipse_command(
            App("installed", None), Profile(Path("/p")), Path("/apk"), ["install"], {}
        )
        self.assertEqual(command[:3], ["flatpak", "run", "--command=env"])

    def test_the_seeded_update_check_reads_as_a_completed_check(self):
        with tempfile.TemporaryDirectory() as temp:
            profile = Profile(Path(temp))
            seed_update_check(profile, 1_790_000_000.9)
            path = Path(temp) / "data/eclipse/roblox/last-update-check.json"
            self.assertEqual(
                json.loads(path.read_text(encoding="utf-8")),
                {
                    "checked_at_unix": 1_790_000_000,
                    "rejected": None,
                    "outcome": "completed",
                },
            )

    def test_sandbox_contracts_compare_runtime_and_context_only(self):
        installed = (
            "[Application]\nname=io.github.kuenec.Eclipse\n"
            "runtime=org.gnome.Platform/x86_64/51\n\n"
            "[Context]\nshared=ipc;network;\nsockets=wayland;pulseaudio;\n"
        )
        build = (
            installed + "\n[Build]\nbuilt-extensions=io.github.kuenec.Eclipse.Debug;\n"
        )
        wider = installed.replace("sockets=wayland;", "sockets=wayland;x11;")
        self.assertEqual(sandbox_contract(build), sandbox_contract(installed))
        self.assertNotEqual(sandbox_contract(wider), sandbox_contract(installed))

    def test_launches_interleave_and_retry_up_to_twice_the_runs(self):
        calls = []
        outcomes = {"A": [False, True, True], "B": [True, True]}

        def attempt(app, number):
            calls.append(f"{app}{number}")
            return outcomes[app][number - 1]

        self.assertTrue(interleave(["A", "B"], 2, attempt))
        self.assertEqual(calls, ["A1", "B1", "A2", "B2", "A3"])

    def test_launches_give_up_after_twice_the_runs(self):
        calls = []

        def attempt(app, number):
            calls.append(f"{app}{number}")
            return False

        self.assertFalse(interleave(["A", "B"], 2, attempt))
        self.assertEqual(calls, ["A1", "B1", "A2", "B2", "A3", "B3", "A4", "B4"])


class InstanceExitTest(unittest.TestCase):
    RUNTIME = "org.example.Platform"

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.temp = Path(temp.name)
        bin_dir = self.temp / "bin"
        bin_dir.mkdir()
        flatpak = bin_dir / "flatpak"
        flatpak.write_text(FAKE_LINGERING_FLATPAK, encoding="utf-8")
        flatpak.chmod(0o755)
        self.polls = self.temp / "polls"
        self.polls.write_text("0\n", encoding="utf-8")
        environment = {
            "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', os.defpath)}",
            "XDG_RUNTIME_DIR": str(self.temp),
            "FAKE_POLLS": str(self.polls),
        }
        patches = (
            mock.patch.dict(os.environ, environment),
            mock.patch("measure.PERF_DIR", self.temp / "perf"),
        )
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)
        self.series = Series(
            "20261003-120000",
            self.RUNTIME,
            self.temp / "apk",
            True,
            HiddenHyprland("hyprland v0.56.2"),
            NVIDIA,
            [],
        )

    def install(self, linger_polls):
        os.environ["FAKE_LINGER_POLLS"] = str(linger_polls)
        return self.series.new_profile(App("main", None))

    def test_an_install_instance_that_flatpak_still_lists_is_waited_out(self):
        profile = self.install(3)
        self.assertTrue(profile.update_check().is_file())
        self.assertEqual(self.polls.read_text(encoding="utf-8").strip(), "0")
        self.assertIsNone(running_instances_message(self.RUNTIME))

    def test_an_instance_that_stays_after_its_command_stops_the_series(self):
        with mock.patch("measure.INSTANCE_EXIT_S", 0.3):
            with self.assertRaises(SeriesFailed) as raised:
                self.install(1000)
        self.assertIn("instance 77 (pid 4242)", str(raised.exception))
        self.assertIn("eclipse install", str(raised.exception))


class FramesTest(unittest.TestCase):
    MS = 1_000_000

    def ring(self, entered):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        path = Path(temp.name) / "ring.bin"
        header = struct.pack("<8sQQ40x", b"ECLFRAME", len(entered), len(entered))
        entries = b"".join(struct.pack("<QII", at, 1000, 1000) for at in entered)
        path.write_bytes(header + entries)
        return path

    def steady(self, start_ms, end_ms):
        return [ms * self.MS for ms in range(start_ms, end_ms, 16)]

    def test_bursts_once_a_second_count_as_settled_idle(self):
        entered = self.steady(1000, 11000)
        for second in range(11, 71):
            entered += [(second * 1000 + offset) * self.MS for offset in (0, 16, 32)]
        frames = summarize_frames(self.ring(entered))
        self.assertLess(frames.late["interval_ms"]["p50"], 20)
        self.assertTrue(frames.settled_idle)

    def test_a_steady_frame_rate_is_not_settled_idle(self):
        frames = summarize_frames(self.ring(self.steady(1000, 72000)))
        self.assertFalse(frames.settled_idle)


class SummaryTest(unittest.TestCase):
    LABELS = ("main", "branch")
    LAUNCHES = ((Launch.FIRST, 1), (Launch.WARM, 1), (Launch.WARM, 2))

    def test_bootstrap_is_deterministic_and_separates_a_shift_from_noise(self):
        a = [1.0, 1.1, 0.9, 1.05, 0.95, 1.02, 0.98, 1.0, 1.01, 0.99]
        same = median_difference(a, list(a))
        self.assertEqual(same, median_difference(a, list(a)))
        self.assertLessEqual(same["interval"][0], 0)
        self.assertGreaterEqual(same["interval"][1], 0)
        self.assertFalse(same["beats_noise"])
        shifted = median_difference(a, [value * 1.2 for value in a])
        self.assertGreater(shifted["interval"][0], 0)
        self.assertTrue(shifted["beats_noise"])
        self.assertEqual(shifted["median_b_minus_a"], 0.2)

    def series(self):
        environment = {
            "HOME": "/home/zzuserzz",
            "USER": "zzuserzz",
            "XDG_RUNTIME_DIR": "/run/user/1000",
        }
        with mock.patch.dict(os.environ, environment):
            series = Series(
                "20261002-120000",
                "org.gnome.Platform",
                Path("/home/zzuserzz/apk"),
                True,
                HiddenHyprland("hyprland v0.56.2"),
                NVIDIA,
                [],
            )
        for label in self.LABELS:
            series.versions[App(label, None)] = "0.1.4"
            series.hashes[App(label, None)] = "ab" * 32
        return series

    def cpu_samples(self, early_percent, late_percent=20):
        rendered_until_s = 16
        samples = []
        for second in range(90):
            early_s = min(second, rendered_until_s)
            late_s = max(0, second - rendered_until_s)
            busy = early_percent * early_s + late_percent * late_s
            samples.append((second, CLOCK_TICKS_PER_S * busy // 100))
        return samples

    def frames(self, interval_ns, seam_ns, settled_idle):
        presents = range(EARLY_WINDOW_S[1] * NANOS_PER_SECOND // interval_ns)
        entries = (Entry(n * interval_ns + 1, seam_ns, 1000) for n in presents)
        early = summarize(Ring(tuple(entries), 0), *EARLY_WINDOW_S)
        return Frames(early, None, settled_idle)

    def row(self, series, label, launch, run, cpu_percent=20, frames=Frames()):
        state = RunState(ProcessSet(100), 0.0, 1.5)
        state.cpu_samples = self.cpu_samples(cpu_percent)
        state.memory = MemorySample(900_096, {"eclipse": 900_096}, 4096, 0)
        state.threads = []
        state.close_s = 1.25
        state.close_forced = False
        state.frames = frames
        state.loadavg_end = 1.7
        state.profile_bytes = 512_000_000
        return series.row(App(label, None), launch, run, client_output(), state)

    def test_rows_and_summary_hold_no_path_home_host_or_user(self):
        series = self.series()
        rows = [
            self.row(series, label, launch, run)
            for label in self.LABELS
            for launch, run in self.LAUNCHES
        ]
        lines = [serialize(row, ROW_FIELDS) for row in rows]
        summary = json.dumps(
            summarize_series(series.id, Launch.WARM, ["main", "branch"], rows)
        )
        for text in [*lines, summary]:
            self.assertNotIn("/", text)
            self.assertNotIn("zzuserzz", text)
            self.assertNotIn("zzhostzz", text)
        self.assertEqual(tuple(json.loads(lines[0])), ROW_FIELDS)
        parsed = json.loads(summary)
        self.assertEqual(parsed["labels"]["main"]["valid_runs"], 2)
        self.assertEqual(parsed["labels"]["main"]["first_launch_s"], 5.5)
        self.assertEqual(
            parsed["labels"]["main"]["pss_mib"], {"p50": 875.0, "p95": 875.0}
        )
        self.assertEqual(parsed["labels"]["main"]["cpu_percent_10_70"]["p50"], 20.0)
        self.assertEqual(
            parsed["difference"]["first_frame_s"]["median_b_minus_a"], 0.0
        )
        self.assertEqual(parsed["compositor"], "hyprland-hidden")

    def test_the_difference_covers_cpu_and_frame_pacing_while_rendering(self):
        series = self.series()
        builds = {
            "main": (20, self.frames(16_666_667, 1_000, True)),
            "branch": (30, self.frames(20_000_000, 50_000, True)),
        }
        rows = [
            self.row(series, label, launch, run, *builds[label])
            for label in self.LABELS
            for launch, run in self.LAUNCHES
        ]

        summary = summarize_series(series.id, Launch.WARM, list(self.LABELS), rows)

        self.assertEqual(rows[0]["cpu_percent_0_10"], 20.0)
        self.assertEqual(rows[0]["cpu_percent_windows"], [20.0] * 6)
        difference = {
            metric: (shift["median_b_minus_a"], shift["beats_noise"])
            for metric, shift in summary["difference"].items()
        }
        self.assertEqual(
            difference,
            {
                "first_frame_s": (0.0, False),
                "pss_mib": (0.0, False),
                "cpu_percent_0_10": (10.0, True),
                "mean_fps_0_10": (-10.0, True),
                "interval_p99_ms_0_10": (3.333, True),
                "seam_p99_ms_0_10": (0.049, True),
                "idle_cpu_percent_10_70": (0.0, False),
            },
        )
        self.assertEqual(summary["labels"]["branch"]["mean_fps_0_10"]["p50"], 50.0)
        self.assertEqual(summary["labels"]["branch"]["settled_idle_runs"], 2)

    def test_cpu_after_10_s_is_compared_only_between_runs_in_the_same_phase(self):
        series = self.series()
        cases = {
            "every run rendering": ((False, False), {"cpu_percent_10_70"}),
            "every run idle": ((True, True), {"idle_cpu_percent_10_70"}),
            "mixed": ((False, True), set()),
            "no frame log": ((None, None), set()),
        }
        for case, (phases, compared) in cases.items():
            runs = [Frames(settled_idle=idle) for idle in phases]
            rows = [
                self.row(series, label, Launch.WARM, run, frames=frames)
                for label in self.LABELS
                for run, frames in enumerate(runs, 1)
            ]
            summary = summarize_series(series.id, Launch.WARM, list(self.LABELS), rows)
            with self.subTest(case):
                self.assertEqual(
                    {metric for metric in summary["difference"] if "10_70" in metric},
                    compared,
                )

    def test_a_row_rejects_text_that_could_hold_a_path(self):
        row = dict.fromkeys(ROW_FIELDS)
        row["gpu_name"] = "/home/zzuserzz"
        with self.assertRaises(ValueError):
            serialize(row, ROW_FIELDS)


class CommandLineTest(unittest.TestCase):
    def test_an_apk_dir_in_the_users_eclipse_data_is_refused(self):
        with tempfile.TemporaryDirectory() as temp:
            apk_dir = Path(temp) / ".var" / "app" / APP_ID / "apk"
            apk_dir.mkdir(parents=True)
            completed = subprocess.run(
                [sys.executable, SCRIPT, "boot", "--apk-dir", apk_dir],
                env={"HOME": temp, "PATH": temp, "XDG_RUNTIME_DIR": temp},
                capture_output=True,
                encoding="utf-8",
                timeout=30,
            )
        self.assertEqual(completed.returncode, 2)
        self.assertIn("which holds your login", completed.stderr)

    def test_a_label_without_a_build_dir_is_a_usage_error(self):
        completed = subprocess.run(
            [sys.executable, SCRIPT, "boot", "--apk-dir", ".", "--app", "main"],
            capture_output=True,
            encoding="utf-8",
            timeout=30,
        )
        self.assertEqual(completed.returncode, 2)
        self.assertIn("LABEL=BUILD_DIR", completed.stderr)


class MarkerWatchTest(unittest.TestCase):
    MARKER = STARTUP_MARKER.encode()

    def test_a_marker_split_across_reads_counts_when_its_end_arrives(self):
        watch = MarkerWatch(self.MARKER)
        watch.feed(b"[FLog::Graphics] VULKAN unifiedMemory = false, set", 0.5)
        self.assertIsNone(watch.found_s)
        watch.feed(b"ting caps.videoMemory = 67108864\n", 0.75)
        self.assertEqual(watch.found_s, 0.75)

    def test_only_the_first_marker_counts(self):
        watch = MarkerWatch(self.MARKER)
        watch.feed(b"setting caps.videoMemory = 1\n", 1.0)
        watch.feed(b"setting caps.videoMemory = 2\n", 9.0)
        self.assertEqual(watch.found_s, 1.0)

    def test_output_without_the_marker_keeps_less_than_a_marker(self):
        watch = MarkerWatch(self.MARKER)
        for _ in range(64):
            watch.feed(b"x" * 65536, 1.0)
        self.assertIsNone(watch.found_s)
        self.assertLess(len(watch.carried), len(self.MARKER))


class ObservationTest(unittest.TestCase):
    def test_tmp_threads_and_pss_keep_their_own_cadence(self):
        with tempfile.TemporaryDirectory() as temp:
            tmp_dir = Path(temp)
            seconds = iter([0.0, 5.0, 59.0, 60.0, 61.0, 120.2])
            observation = Observation(tmp_dir, 0.0, 0.0, lambda: next(seconds))
            for size in (100, 200, 300, 400, 500, 50):
                (tmp_dir / "cache").write_bytes(b"x" * size)
                observation.sample({os.getpid()})
        self.assertEqual(len(observation.cpu_samples), 6)
        self.assertEqual(observation.tmp_peak[0], 400)
        self.assertEqual([s["at_s"] for s in observation.pss_samples], [60, 120])
        for sample in observation.pss_samples:
            self.assertGreater(sample["pss_mib"], 0)
            self.assertAlmostEqual(
                sum(sample["by_comm"].values()), sample["pss_mib"], delta=1
            )
        main_thread = (os.getpid(), os.getpid())
        self.assertIn(main_thread, observation.thread_start)
        start_ns = observation.thread_start[main_thread][1]
        self.assertGreaterEqual(observation.threads[main_thread][1], start_ns)

    def test_memory_and_threads_of_a_launch_start_at_its_marker(self):
        with tempfile.TemporaryDirectory() as temp:
            seconds = iter([0.0, 5.0, 30.0, 31.0, 61.0, 91.0])
            observation = Observation(Path(temp), 0.0, None, lambda: next(seconds))
            for _ in range(3):
                observation.sample({os.getpid()})
            before_marker = observation.thread_start
            observation.steady_s = 30.5
            for _ in range(3):
                observation.sample({os.getpid()})
        self.assertIsNone(before_marker)
        self.assertIsNotNone(observation.thread_start)
        self.assertEqual([s["at_s"] for s in observation.pss_samples], [91])

    def test_cpu_windows_of_a_launch_leave_out_its_startup(self):
        observation = Observation(Path("/nonexistent"), 0.0, None)
        startup_s = 25

        def ticks(second):
            busy_tenths = 9 * min(second, startup_s) + max(0, second - startup_s)
            return CLOCK_TICKS_PER_S * busy_tenths / 10

        observation.cpu_samples = [
            (second, ticks(second)) for second in range(0, 60, 5)
        ]
        self.assertEqual(observation.steady_cpu_windows(), [])
        observation.steady_s = startup_s
        self.assertEqual(observation.steady_cpu_windows(), [10.0, 10.0, 10.0])


class ObserveCommandTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.temp = Path(temp.name)
        bin_dir = self.temp / "bin"
        bin_dir.mkdir()
        flatpak = bin_dir / "flatpak"
        flatpak.write_text(FAKE_FLATPAK, encoding="utf-8")
        flatpak.chmod(0o755)
        self.instances = self.temp / "instances"
        self.instances.write_text("", encoding="utf-8")
        self.runtime_dir = self.temp / "run"
        self.runtime_dir.mkdir()
        self.environment = {
            "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', os.defpath)}",
            "HOME": str(self.temp),
            "XDG_RUNTIME_DIR": str(self.runtime_dir),
            "FAKE_APP": GAME,
            "FAKE_PS": str(self.instances),
        }

    def observe(self, *arguments):
        return subprocess.run(
            [sys.executable, SCRIPT, "observe", *arguments],
            env=self.environment,
            capture_output=True,
            encoding="utf-8",
            timeout=60,
        )

    def record(self, completed):
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertNotIn("/", completed.stdout)
        record = json.loads(completed.stdout)
        self.assertEqual(tuple(record), OBSERVATION_FIELDS)
        return record

    def test_startup_runs_from_the_spawn_to_the_marker_line(self):
        script = f"echo booting; sleep 0.2; echo 'x {STARTUP_MARKER} = 1'"
        completed = self.observe("--app", GAME, "--", "sh", "-c", script)
        record = self.record(completed)
        self.assertAlmostEqual(record["startup_s"], 0.2, delta=0.05)
        self.assertEqual(record["start"], "command")
        self.assertEqual(record["end"], "exited")
        self.assertEqual(record["command_status"], 0)
        self.assertNotIn("booting", completed.stdout + completed.stderr)

    def test_attaching_stops_when_the_instances_exit(self):
        sleeper = subprocess.Popen(["sleep", "1.5"])
        reaper = threading.Thread(target=sleeper.wait)
        reaper.start()
        self.addCleanup(reaper.join)
        self.instances.write_text(
            f"1\t{sleeper.pid}\t{GAME}\torg.example.Platform\n"
            f"2\t{os.getpid()}\t{GAME}\torg.example.Sdk\n",
            encoding="utf-8",
        )
        tmp_dir = self.runtime_dir / ".flatpak" / GAME / "tmp"
        tmp_dir.mkdir(parents=True)
        (tmp_dir / "cache").write_bytes(b"x" * 10_000)
        record = self.record(self.observe("--app", GAME, "--seconds", "30"))
        self.assertEqual(record["start"], "attach")
        self.assertEqual(record["end"], "exited")
        self.assertGreaterEqual(record["observed_s"], 1.0)
        self.assertLess(record["observed_s"], 10)
        self.assertIsNone(record["startup_s"])
        self.assertIsNone(record["command_status"])
        self.assertEqual(record["tmp_peak_bytes"], 10_000)
        self.assertEqual([t["thread"] for t in record["top_threads"]], ["sleep"])

    def test_the_seconds_limit_ends_the_observation_but_not_the_command(self):
        started = time.monotonic()
        completed = self.observe(
            "--app", GAME, "--seconds", "1", "--", "sh", "-c", "sleep 2.5"
        )
        record = self.record(completed)
        self.assertGreaterEqual(time.monotonic() - started, 2.5)
        self.assertEqual(record["end"], "seconds")
        self.assertGreaterEqual(record["observed_s"], 1.0)
        self.assertIsNone(record["command_status"])
        self.assertIn("waiting for the command to exit", completed.stderr)

    def test_an_interrupt_ends_the_observation_and_still_prints_it(self):
        sleeper = subprocess.Popen(["sleep", "30"])
        self.addCleanup(sleeper.wait)
        self.addCleanup(sleeper.kill)
        self.instances.write_text(
            f"1\t{sleeper.pid}\t{GAME}\torg.example.Platform\n", encoding="utf-8"
        )
        observer = subprocess.Popen(
            [sys.executable, SCRIPT, "observe", "--app", GAME, "--seconds", "30"],
            env=self.environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            preexec_fn=default_sigint,
        )
        self.assertIn("observing", observer.stderr.readline())
        time.sleep(0.5)
        observer.send_signal(signal.SIGINT)
        stdout, stderr = observer.communicate(timeout=30)
        self.assertEqual(observer.returncode, 0, stderr)
        record = json.loads(stdout)
        self.assertEqual(record["end"], "interrupted")
        self.assertLess(record["observed_s"], 5)

    def test_a_command_that_never_prints_the_marker_reports_its_status(self):
        completed = self.observe("--app", GAME, "--", "sh", "-c", "echo hi; exit 3")
        record = self.record(completed)
        self.assertIsNone(record["startup_s"])
        self.assertEqual(record["command_status"], 3)
        self.assertEqual(record["end"], "exited")
        self.assertIn("no output of the command held", completed.stderr)

    def test_a_launch_is_refused_while_the_app_runs(self):
        self.instances.write_text(
            f"7\t{os.getpid()}\t{GAME}\torg.example.Platform\n", encoding="utf-8"
        )
        started = self.temp / "started"
        completed = self.observe("--app", GAME, "--", "touch", str(started))
        self.assertEqual(completed.returncode, 2)
        self.assertIn(f"{GAME} is already running (instance 7", completed.stderr)
        self.assertFalse(started.exists())
        self.assertEqual(completed.stdout, "")

    def test_attaching_needs_a_running_instance(self):
        completed = self.observe("--app", GAME)
        self.assertEqual(completed.returncode, 2)
        self.assertIn(f"{GAME} is not running", completed.stderr)

    def test_an_app_that_is_not_installed_is_refused(self):
        completed = self.observe("--app", "org.example.Missing")
        self.assertEqual(completed.returncode, 2)
        self.assertIn("org.example.Missing is not installed", completed.stderr)

    def test_bad_app_ids_durations_and_markers_are_usage_errors(self):
        for arguments in (
            ("--app", "../../etc"),
            ("--app", "org.example"),
            ("--app", GAME, "--seconds", "0"),
            ("--app", GAME, "--seconds", "14401"),
            ("--app", GAME, "--marker", ""),
        ):
            with self.subTest(arguments=arguments):
                completed = self.observe(*arguments)
                self.assertEqual(completed.returncode, 2)
                self.assertIn("usage:", completed.stderr)


if __name__ == "__main__":
    unittest.main()
