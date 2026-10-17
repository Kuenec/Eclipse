#!/usr/bin/env python3

import argparse
import json
import math
import os
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from array import array
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parents[2]
BASELINE = REPOSITORY / "build" / "perf" / "audio-baseline.jsonl"
MANIFEST = REPOSITORY / "packaging" / "flatpak" / "io.github.kuenec.Eclipse.yml"
FLATPAK_ENV = "- --env="
SESSION_VARIABLE = "ECLIPSE_AUDIO_HARNESS"
NOT_IN_SESSION = (
    "check.py only runs inside tools/audio-harness/session.sh, which starts a private "
    "sound server, so it never plays or records on your own devices"
)
SINK_A = "harness_a"
SINK_B = "harness_b"
SINK_HFP = "harness_hfp"
IDLE_SOURCE = f"{SINK_HFP}.monitor"
PINNED = "--property=node.dont-move=true"
TONE_HZ = 997
PITCH_TOLERANCE_HZ = 1.0
APP_RATE = 48000
APP_CHANNELS = 2
RECORD_RATE = 48000
SAMPLE_BYTES = 4
READ_BYTES = 65536
WINDOW_S = 0.005
GAP_SHARE = 0.25
TONE_FLOOR = 0.01
ONSET_LEVEL = 0.05
STEADY_S = 60
STALL = (60, 5)
STALL_GAP_TOLERANCE = 1
SHORT_S = 10
SWITCH_AT_S = 5
SWITCH_LIMIT_S = 0.5
CLICKS_NEEDED = 9
LATENCY_LIMIT_MS = 40
GATED_QUANTUM = 512
STRESS_RUNS = 10
LATENCY_MSEC = "60"
RT_PRIORITY = 5
RT_TIME_US = 200_000
RT_THREAD = "cpal_alsa_out"
RT_LIMIT_NAME = "Max realtime priority"
POLL_S = 0.05
SETTLE_LIMIT_S = 5
RECORDER_READY_S = 5
DRIVER_GRACE_S = 30
PACTL_LIMIT_S = 10
MIXER_IDENTITY = {
    "application.name": "Eclipse",
    "application.id": "io.github.kuenec.Eclipse",
    "application.icon_name": "io.github.kuenec.Eclipse",
}
SHARED_VOLUME_KEY = "media.role"


class HarnessError(Exception):
    pass


def require_session(environment):
    root = environment.get(SESSION_VARIABLE)
    server = environment.get("PULSE_SERVER", "")
    if not root or not server.startswith(f"unix:{root}/"):
        raise HarnessError(NOT_IN_SESSION)


def run(command):
    try:
        done = subprocess.run(
            command, capture_output=True, text=True, timeout=PACTL_LIMIT_S
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise HarnessError(f"`{' '.join(command)}` did not run: {error}") from error
    if done.returncode != 0:
        raise HarnessError(f"`{' '.join(command)}` failed: {done.stderr.strip()}")
    return done.stdout


def pactl_json(*arguments):
    return json.loads(run(["pactl", "--format=json", *arguments]))


def wait_for(condition, what):
    deadline = time.monotonic() + SETTLE_LIMIT_S
    while not (found := condition()):
        if time.monotonic() > deadline:
            raise HarnessError(f"{what} did not happen within {SETTLE_LIMIT_S} s")
        time.sleep(POLL_S)
    return found


def set_default(kind, name):
    run(["pactl", f"set-default-{kind}", name])
    wait_for(
        lambda: pactl_json("info").get(f"default_{kind}_name") == name,
        f"{name} becoming the default {kind}",
    )


def route(sink, source=IDLE_SOURCE):
    set_default("sink", sink)
    set_default("source", source)


def window_levels(samples, rate):
    size = max(1, round(rate * WINDOW_S))
    return [
        math.sqrt(
            sum(sample * sample for sample in samples[start : start + size]) / size
        )
        for start in range(0, len(samples) - size + 1, size)
    ]


def zero_crossing_hz(samples, rate):
    first = None
    last = 0.0
    crossings = 0
    for index in range(1, len(samples)):
        before, after = samples[index - 1], samples[index]
        if before < 0.0 <= after:
            last = index - 1 + before / (before - after)
            if first is None:
                first = last
            crossings += 1
    if crossings < 2 or last <= first:
        return None
    return (crossings - 1) * rate / (last - first)


@dataclass(frozen=True)
class Tone:
    seconds: float
    steady_rms: float
    gaps: int
    gap_ms: float
    hz: float


def analyse(samples, rate):
    levels = window_levels(samples, rate)
    loud = [level for level in levels if level > TONE_FLOOR]
    if not loud:
        return None
    steady = statistics.median(loud)
    on = [level >= steady * GAP_SHARE for level in levels]
    first = on.index(True)
    last = len(on) - 1 - on[::-1].index(True)
    runs = []
    start = first
    for index in range(first, last + 1):
        if not on[index] and start is not None:
            runs.append((start, index))
            start = None
        elif on[index] and start is None:
            start = index
    runs.append((start, last + 1))
    longest = max(runs, key=lambda run: run[1] - run[0])
    size = max(1, round(rate * WINDOW_S))
    return Tone(
        seconds=(last - first + 1) * size / rate,
        steady_rms=steady,
        gaps=len(runs) - 1,
        gap_ms=on[first : last + 1].count(False) * WINDOW_S * 1000,
        hz=zero_crossing_hz(samples[longest[0] * size : longest[1] * size], rate),
    )


def pitch_ok(hz, wanted=TONE_HZ):
    return hz is not None and abs(hz - wanted) <= PITCH_TOLERANCE_HZ


def tone_fields(tone, prefix=""):
    if tone is None:
        return {f"{prefix}tone_s": 0.0}
    return {
        f"{prefix}tone_s": round(tone.seconds, 3),
        f"{prefix}hz": None if tone.hz is None else round(tone.hz, 3),
        f"{prefix}gaps": tone.gaps,
        f"{prefix}gap_ms": tone.gap_ms,
    }


def steady_row(scenario, tone, report, gated):
    passed = None
    if gated:
        passed = (
            tone is not None
            and pitch_ok(tone.hz)
            and tone.gaps == 0
            and report["xruns"] == 0
        )
    return {
        "scenario": scenario,
        "passed": passed,
        **tone_fields(tone),
        "xruns": report["xruns"],
        "callbacks": report["callbacks"],
        "frames_per_burst": report["frames_per_burst"],
        "stream_errors": report["stream_errors"],
    }


def mixer_row(row, properties):
    shown = {key: properties.get(key) for key in (*MIXER_IDENTITY, SHARED_VOLUME_KEY)}
    named = all(properties.get(key) == value for key, value in MIXER_IDENTITY.items())
    return {
        **row,
        "passed": row["passed"] and named and SHARED_VOLUME_KEY not in properties,
        "mixer": shown,
    }


def stall_row(tone, report):
    return {
        "scenario": "S1-stall",
        "passed": tone is not None
        and report["stalls"] > 0
        and report["xruns"] == report["stalls"]
        and abs(tone.gaps - report["xruns"]) <= STALL_GAP_TOLERANCE,
        **tone_fields(tone),
        "xruns": report["xruns"],
        "stalls": report["stalls"],
        "callbacks": report["callbacks"],
        "frames_per_burst": report["frames_per_burst"],
        "stream_errors": report["stream_errors"],
    }


def switch_row(delay_s):
    return {
        "scenario": "S3",
        "passed": delay_s is not None and delay_s <= SWITCH_LIMIT_S,
        "moved_after_ms": None if delay_s is None else round(delay_s * 1000, 1),
    }


def hfp_row(report, tone):
    return {
        "scenario": "S4",
        "passed": report["sample_rate"] == APP_RATE
        and report["channels"] == APP_CHANNELS
        and tone is not None
        and pitch_ok(tone.hz),
        "sample_rate": report["sample_rate"],
        "channels": report["channels"],
        **tone_fields(tone),
    }


def rate_change_row(before, after):
    return {
        "scenario": "S5",
        "passed": before is not None
        and after is not None
        and pitch_ok(before.hz)
        and pitch_ok(after.hz),
        **tone_fields(before, "before_"),
        **tone_fields(after, "after_"),
    }


def loopback_rows(scenario, report, capture, quantum):
    passed = (
        report["detected"] >= CLICKS_NEEDED
        and pitch_ok(report["capture_hz"], report["tone_hz"])
        and capture.get("probe") == "none"
        and capture.get("started") == "running"
        and capture.get("closed") == "none"
    )
    rows = [
        {
            "scenario": scenario,
            "passed": passed,
            "detected": report["detected"],
            "clicks": report["clicks"],
            "capture_hz": report["capture_hz"],
            "input_rate": report["input_rate"],
            "input_channels": report["input_channels"],
            "capture_stream": capture,
            "stream_errors": report["stream_errors"],
        }
    ]
    if scenario == "S6":
        p95 = report["round_trip_ms_p95"]
        rows.append(
            {
                "scenario": "S7",
                "passed": (p95 is not None and p95 <= LATENCY_LIMIT_MS)
                if quantum == GATED_QUANTUM
                else None,
                "method": "loopback round trip through the sink monitor",
                "round_trip_ms_p50": report["round_trip_ms_p50"],
                "round_trip_ms_p95": p95,
            }
        )
    return rows


def own_sink_input(sink_inputs, pid):
    for entry in sink_inputs:
        if entry.get("properties", {}).get("application.process.id") == str(pid):
            return entry
    return None


def stream_sink(sinks, sink_inputs, pid):
    entry = own_sink_input(sink_inputs, pid)
    if entry is None:
        return None
    return {sink["index"]: sink["name"] for sink in sinks}.get(entry["sink"])


def flatpak_environment(manifest):
    environment = {}
    for line in manifest.splitlines():
        entry = line.strip()
        if entry.startswith(FLATPAK_ENV):
            name, _, value = entry[len(FLATPAK_ENV) :].partition("=")
            environment[name] = value
    return environment


def capture_state(source_outputs, pid):
    mine = [
        output
        for output in source_outputs
        if output.get("properties", {}).get("application.process.id") == str(pid)
    ]
    if not mine:
        return "none"
    return "corked" if all(output.get("corked") for output in mine) else "running"


def report_line(lines):
    for line in reversed(lines):
        parsed = json.loads(line)
        if "phase" not in parsed:
            return parsed
    return None


def realtime_priority_limit(limits):
    for line in limits.splitlines():
        if line.startswith(RT_LIMIT_NAME):
            soft = line[len(RT_LIMIT_NAME) :].split()[0]
            return math.inf if soft == "unlimited" else int(soft)
    return 0


class Recorder:
    def __init__(self, source, rate=RECORD_RATE):
        self.source = source
        self.rate = rate
        self.data = bytearray()
        self.checked = 0
        self.loud_at = None
        self.ready = threading.Event()
        self.process = subprocess.Popen(
            [
                "parecord",
                "--raw",
                PINNED,
                f"--device={source}",
                "--format=float32le",
                "--channels=1",
                f"--rate={rate}",
                "--latency-msec=10",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()
        if not self.ready.wait(RECORDER_READY_S):
            self.close()
            raise HarnessError(f"parecord delivered nothing from {source}")

    def _read(self):
        while chunk := self.process.stdout.read1(READ_BYTES):
            now = time.monotonic()
            self.data += chunk
            self.ready.set()
            complete = len(self.data) - len(self.data) % SAMPLE_BYTES
            if self.loud_at is None:
                fresh = array("f", bytes(self.data[self.checked : complete]))
                if any(abs(sample) > ONSET_LEVEL for sample in fresh):
                    self.loud_at = now
            self.checked = complete

    def close(self):
        self.process.terminate()
        self.process.wait()
        self.thread.join()

    def samples(self):
        complete = len(self.data) - len(self.data) % SAMPLE_BYTES
        return array("f", bytes(self.data[:complete]))

    def tone(self):
        return analyse(self.samples(), self.rate)


@contextmanager
def recording(*sources):
    recorders = []
    try:
        for source in sources:
            recorders.append(Recorder(source))
        yield recorders
    finally:
        for recorder in recorders:
            recorder.close()


class Driver:
    def __init__(self, command, environment):
        self.command = command
        self.started = time.monotonic()
        self.stderr = tempfile.TemporaryFile()
        self.process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=self.stderr,
            text=True,
            env=environment,
        )
        self.pid = self.process.pid

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if self.process.poll() is None:
            self.process.kill()
            self.process.communicate()
        self.stderr.close()

    def lines(self):
        for line in self.process.stdout:
            yield line.strip()

    def finish(self, earlier=()):
        shown = " ".join(self.command)
        try:
            stdout, _ = self.process.communicate(timeout=DRIVER_GRACE_S + STEADY_S)
        except subprocess.TimeoutExpired as error:
            raise HarnessError(f"`{shown}` did not finish") from error
        if self.process.returncode != 0:
            self.stderr.seek(0)
            said = self.stderr.read().decode("utf-8", "replace").strip()
            raise HarnessError(
                f"`{shown}` exited with {self.process.returncode}: {said}"
            )
        report = report_line([*earlier, *stdout.splitlines()])
        if report is None:
            raise HarnessError(f"`{shown}` printed no report")
        return report


class Harness:
    def __init__(self, eclipse, quantum, environment):
        self.eclipse = eclipse
        self.quantum = quantum
        self.environment = environment

    def sine(self, seconds, cpu=None, extra_environment=None, stall=()):
        command = [self.eclipse, "__audio-sine", str(seconds), str(TONE_HZ)]
        command.extend(str(value) for value in stall)
        if cpu is not None:
            command = ["taskset", "-c", str(cpu), *command]
        return Driver(command, {**self.environment, **(extra_environment or {})})

    def loopback(self):
        return Driver([self.eclipse, "__audio-loopback"], self.environment)


def steady(harness, scenario, gated, extra_environment=None, cpu=None, promote=False):
    route(SINK_A)
    with recording(f"{SINK_A}.monitor") as (recorder,), harness.sine(
        STEADY_S, cpu, extra_environment
    ) as driver:
        promoted = promote_thread(driver.pid) if promote else None
        try:
            report = driver.finish()
        except HarnessError:
            if driver.process.returncode == -signal.SIGXCPU:
                return {"scenario": scenario, "passed": None, "sigxcpu": True}
            raise
    row = steady_row(scenario, recorder.tone(), report, gated)
    if promote:
        row["promoted"] = promoted
    return row


def promote_thread(pid):
    with open("/proc/self/limits", encoding="utf-8") as limits:
        allowed = realtime_priority_limit(limits.read())
    if allowed < RT_PRIORITY:
        return f"skipped: RLIMIT_RTPRIO is {allowed}"
    tasks = Path(f"/proc/{pid}/task")
    found = []

    def named_thread():
        found.extend(
            task.name
            for task in tasks.iterdir()
            if (task / "comm").read_text(encoding="utf-8").strip() == RT_THREAD
        )
        return bool(found)

    wait_for(named_thread, f"the {RT_THREAD} thread starting")
    run(["prlimit", f"--pid={pid}", f"--rttime={RT_TIME_US}"])
    run(["chrt", "-f", "-p", str(RT_PRIORITY), found[0]])
    return f"SCHED_FIFO {RT_PRIORITY}"


def single_cpu():
    return min(os.sched_getaffinity(0))


def stress(harness, scenario, extra_environment=None, promote=False):
    return [
        {
            **steady(
                harness, scenario, False, extra_environment, single_cpu(), promote
            ),
            "run": run_index,
        }
        for run_index in range(1, STRESS_RUNS + 1)
    ]


def switch_midway(harness, first, second):
    route(first)
    with recording(f"{first}.monitor", f"{second}.monitor") as (
        before,
        after,
    ), harness.sine(SHORT_S) as driver:
        time.sleep(max(0.0, driver.started + SWITCH_AT_S - time.monotonic()))
        switched = time.monotonic()
        run(["pactl", "set-default-sink", second])
        wait_for(
            lambda: stream_sink(
                pactl_json("list", "sinks"),
                pactl_json("list", "sink-inputs"),
                driver.pid,
            )
            == second,
            f"Eclipse's stream moving to {second}",
        )
        driver.finish()
    return before, after, switched


def s1(harness):
    route(SINK_A)
    with recording(f"{SINK_A}.monitor") as (recorder,), harness.sine(
        STEADY_S
    ) as driver:
        stream = wait_for(
            lambda: own_sink_input(pactl_json("list", "sink-inputs"), driver.pid),
            "Eclipse's stream appearing",
        )
        report = driver.finish()
    row = steady_row("S1", recorder.tone(), report, True)
    return [mixer_row(row, stream["properties"])]


def s1_stall(harness):
    route(SINK_A)
    with recording(f"{SINK_A}.monitor") as (recorder,), harness.sine(
        STEADY_S, stall=STALL
    ) as driver:
        report = driver.finish()
    return [stall_row(recorder.tone(), report)]


def s2(harness):
    return stress(harness, "S2")


def s2_latency(harness):
    return stress(harness, "S2-latency60", {"PULSE_LATENCY_MSEC": LATENCY_MSEC})


def s2_rt(harness):
    return stress(harness, "S2-rt", promote=True)


def s3(harness):
    _, after, switched = switch_midway(harness, SINK_A, SINK_B)
    delay = None if after.loud_at is None else after.loud_at - switched
    return [switch_row(delay)]


def s4(harness):
    route(SINK_HFP)
    with recording(f"{SINK_HFP}.monitor") as (recorder,), harness.sine(
        SHORT_S
    ) as driver:
        report = driver.finish()
    return [hfp_row(report, recorder.tone())]


def s5(harness):
    before, after, _ = switch_midway(harness, SINK_A, SINK_HFP)
    return [rate_change_row(before.tone(), after.tone())]


def s6(harness):
    rows = []
    for scenario, sink in (("S6", SINK_A), ("S6-hfp", SINK_HFP)):
        route(sink, f"{sink}.monitor")
        capture = {}
        printed = []
        with harness.loopback() as driver:
            for line in driver.lines():
                printed.append(line)
                phase = json.loads(line).get("phase")
                if phase is not None:
                    outputs = pactl_json("list", "source-outputs")
                    capture[phase] = capture_state(outputs, driver.pid)
            report = driver.finish(printed)
        rows.extend(loopback_rows(scenario, report, capture, harness.quantum))
    return rows


SCENARIOS = {
    "S1": s1,
    "S1-stall": s1_stall,
    "S2": s2,
    "S2-latency60": s2_latency,
    "S2-rt": s2_rt,
    "S3": s3,
    "S4": s4,
    "S5": s5,
    "S6": s6,
}
CI_SCENARIOS = "S1,S1-stall,S3,S4,S5,S6"


def scenario_list(value):
    if value == "all":
        return list(SCENARIOS)
    chosen = value.split(",")
    unknown = [name for name in chosen if name not in SCENARIOS]
    if unknown:
        raise argparse.ArgumentTypeError(
            f"unknown scenarios {unknown}; choose from {', '.join(SCENARIOS)} or all"
        )
    return chosen


@contextmanager
def forced_quantum(quantum, server_name):
    if quantum is None:
        yield
        return
    if "PipeWire" not in server_name:
        raise HarnessError(f"--quantum needs PipeWire, not {server_name}")
    run(["pw-metadata", "-n", "settings", "0", "clock.force-quantum", str(quantum)])
    try:
        yield
    finally:
        run(["pw-metadata", "-n", "settings", "0", "clock.force-quantum", "0"])


def parser():
    parser = argparse.ArgumentParser(
        description="Play and record through Eclipse's AAudio layer on a private sound "
        "server and record the results. S6 also writes the S7 latency row."
    )
    parser.add_argument("--eclipse", required=True, help="the eclipse binary to drive")
    parser.add_argument(
        "--scenarios",
        type=scenario_list,
        default=scenario_list(CI_SCENARIOS),
        help=f"comma-separated scenarios or all (default {CI_SCENARIOS})",
    )
    parser.add_argument(
        "--quantum", type=int, help="force this PipeWire quantum for the whole run"
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=BASELINE,
        help=f"JSON lines file to append to (default {BASELINE})",
    )
    return parser


def main(argv):
    arguments = parser().parse_args(argv)
    try:
        require_session(os.environ)
        info = pactl_json("info")
        server = f"{info['server_name']} {info['server_version']}"
        eclipse = run([arguments.eclipse, "--version"]).strip()
        environment = flatpak_environment(MANIFEST.read_text(encoding="utf-8"))
        harness = Harness(
            arguments.eclipse, arguments.quantum, {**os.environ, **environment}
        )
        rows = []
        with forced_quantum(arguments.quantum, info["server_name"]):
            for scenario in arguments.scenarios:
                try:
                    rows.extend(SCENARIOS[scenario](harness))
                except HarnessError as error:
                    rows.append(
                        {"scenario": scenario, "passed": False, "error": str(error)}
                    )
    except HarnessError as error:
        print(f"check.py: {error}", file=sys.stderr)
        return 2
    stamp = {
        "time": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "server": server,
        "quantum": arguments.quantum,
        "eclipse": eclipse,
    }
    arguments.out.parent.mkdir(parents=True, exist_ok=True)
    with arguments.out.open("a", encoding="utf-8") as out:
        for row in rows:
            line = json.dumps({**stamp, **row})
            out.write(line + "\n")
            print(line)
    return 1 if any(row["passed"] is False for row in rows) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
