import argparse
import math
import unittest

from check import (
    MANIFEST,
    MIXER_IDENTITY,
    NOT_IN_SESSION,
    HarnessError,
    Tone,
    analyse,
    capture_state,
    flatpak_environment,
    hfp_row,
    loopback_rows,
    mixer_row,
    rate_change_row,
    realtime_priority_limit,
    report_line,
    require_session,
    scenario_list,
    stall_row,
    steady_row,
    stream_sink,
    switch_row,
    zero_crossing_hz,
)

RATE = 48000


def tone(seconds, hz=997.0, amplitude=0.5, silent=()):
    samples = [
        amplitude * math.sin(2 * math.pi * hz * index / RATE)
        for index in range(round(seconds * RATE))
    ]
    for start_s, end_s in silent:
        samples[round(start_s * RATE) : round(end_s * RATE)] = [0.0] * (
            round(end_s * RATE) - round(start_s * RATE)
        )
    return samples


def sine_report(**changes):
    return {
        "sample_rate": 48000,
        "channels": 2,
        "frames_per_burst": 512,
        "callbacks": 100,
        "xruns": 0,
        "stalls": 0,
        "stream_errors": 0,
        **changes,
    }


def loopback_report(**changes):
    return {
        "output_rate": 48000,
        "output_channels": 2,
        "input_rate": 48000,
        "input_channels": 2,
        "clicks": 10,
        "detected": 10,
        "round_trip_ms_p50": 15.0,
        "round_trip_ms_p95": 20.0,
        "tone_hz": 997.0,
        "capture_hz": 997.2,
        "stream_errors": 0,
        **changes,
    }


CLEAN_CAPTURE = {
    "probe": "none",
    "opened": "running",
    "started": "running",
    "stopped": "running",
    "closed": "none",
}

STEADY = Tone(seconds=60.0, steady_rms=0.35, gaps=0, gap_ms=0.0, hz=997.0)


class AnalyseTest(unittest.TestCase):
    def test_a_clean_tone_has_no_gaps_and_its_pitch(self):
        found = analyse(tone(2.0), RATE)
        self.assertEqual(found.gaps, 0)
        self.assertEqual(found.gap_ms, 0.0)
        self.assertAlmostEqual(found.hz, 997.0, delta=0.01)
        self.assertAlmostEqual(found.seconds, 2.0, delta=0.01)
        self.assertAlmostEqual(found.steady_rms, 0.5 / math.sqrt(2), delta=0.01)

    def test_a_dropout_counts_once_and_the_pitch_comes_from_the_longest_run(self):
        found = analyse(tone(2.0, silent=[(0.5, 0.52), (1.0, 1.01)]), RATE)
        self.assertEqual(found.gaps, 2)
        self.assertAlmostEqual(found.gap_ms, 30.0, delta=10.0)
        self.assertAlmostEqual(found.hz, 997.0, delta=0.01)

    def test_silence_before_and_after_the_tone_is_not_a_gap(self):
        found = analyse(tone(3.0, silent=[(0.0, 1.0), (2.5, 3.0)]), RATE)
        self.assertEqual(found.gaps, 0)
        self.assertAlmostEqual(found.seconds, 1.5, delta=0.01)

    def test_silence_has_no_tone(self):
        self.assertIsNone(analyse([0.0] * RATE, RATE))

    def test_a_shifted_pitch_is_measured(self):
        found = analyse(tone(1.0, hz=997.0 * 3), RATE)
        self.assertAlmostEqual(found.hz, 2991.0, delta=0.05)

    def test_zero_crossings_need_a_full_cycle(self):
        self.assertIsNone(zero_crossing_hz([0.0] * 100, RATE))
        self.assertIsNone(zero_crossing_hz([-1.0, 1.0, 1.0], RATE))


class SessionTest(unittest.TestCase):
    def test_only_a_server_inside_the_session_root_is_accepted(self):
        root = "/run/user/1000/eclipse-audio.abc"
        require_session(
            {
                "ECLIPSE_AUDIO_HARNESS": root,
                "PULSE_SERVER": f"unix:{root}/runtime/pulse/native",
            }
        )
        for environment in (
            {},
            {"PULSE_SERVER": f"unix:{root}/runtime/pulse/native"},
            {
                "ECLIPSE_AUDIO_HARNESS": root,
                "PULSE_SERVER": "unix:/run/user/1000/pulse/native",
            },
            {
                "ECLIPSE_AUDIO_HARNESS": root,
                "PULSE_SERVER": f"unix:{root}x/runtime/pulse/native",
            },
            {"ECLIPSE_AUDIO_HARNESS": root},
        ):
            with self.assertRaises(HarnessError) as raised:
                require_session(environment)
            self.assertEqual(str(raised.exception), NOT_IN_SESSION)


class VerdictTest(unittest.TestCase):
    def test_the_steady_tone_passes_only_without_gaps_xruns_or_pitch_drift(self):
        self.assertTrue(steady_row("S1", STEADY, sine_report(), True)["passed"])
        for found, report in (
            (Tone(60.0, 0.35, 1, 5.0, 997.0), sine_report()),
            (STEADY, sine_report(xruns=1)),
            (Tone(60.0, 0.35, 0, 0.0, 998.5), sine_report()),
            (None, sine_report()),
        ):
            self.assertFalse(steady_row("S1", found, report, True)["passed"])
        self.assertIsNone(
            steady_row("S2", STEADY, sine_report(xruns=3), False)["passed"]
        )
        self.assertEqual(steady_row("S1", None, sine_report(), True)["tone_s"], 0.0)

    def test_the_steady_stream_carries_eclipses_mixer_identity_and_no_role(self):
        passed = steady_row("S1", STEADY, sine_report(), True)
        named = {**MIXER_IDENTITY, "application.process.id": "41"}
        row = mixer_row(passed, named)
        self.assertTrue(row["passed"])
        self.assertEqual(row["mixer"], {**MIXER_IDENTITY, "media.role": None})
        for properties in (
            {**named, "media.role": "game"},
            {**named, "application.name": "ALSA plug-in [eclipse]"},
            {key: value for key, value in named.items() if key != "application.id"},
            {"application.process.id": "41"},
        ):
            self.assertFalse(mixer_row(passed, properties)["passed"], properties)
        failed = steady_row("S1", None, sine_report(), True)
        self.assertFalse(mixer_row(failed, named)["passed"])

    def test_each_stall_is_one_xrun_and_one_gap_give_or_take_one(self):
        def passed(gaps, xruns, stalls):
            found = Tone(60.0, 0.35, gaps, gaps * 20.0, 997.0)
            return stall_row(found, sine_report(xruns=xruns, stalls=stalls))["passed"]

        self.assertTrue(passed(11, 11, 11))
        self.assertTrue(passed(12, 11, 11))
        self.assertTrue(passed(10, 11, 11))
        self.assertFalse(passed(13, 11, 11))
        self.assertFalse(passed(11, 10, 11))
        self.assertFalse(passed(0, 0, 0))
        self.assertFalse(stall_row(None, sine_report(xruns=11, stalls=11))["passed"])

    def test_the_switch_needs_the_tone_on_the_new_sink_within_half_a_second(self):
        self.assertEqual(
            switch_row(0.3), {"scenario": "S3", "passed": True, "moved_after_ms": 300.0}
        )
        self.assertFalse(switch_row(0.6)["passed"])
        self.assertEqual(
            switch_row(None),
            {"scenario": "S3", "passed": False, "moved_after_ms": None},
        )

    def test_a_headset_default_still_opens_48_khz_stereo_at_the_right_pitch(self):
        self.assertTrue(hfp_row(sine_report(), STEADY)["passed"])
        self.assertFalse(hfp_row(sine_report(sample_rate=16000), STEADY)["passed"])
        self.assertFalse(hfp_row(sine_report(channels=1), STEADY)["passed"])
        shifted = Tone(10.0, 0.35, 0, 0.0, 332.3)
        self.assertFalse(hfp_row(sine_report(), shifted)["passed"])

    def test_a_rate_change_keeps_the_pitch_on_both_sides(self):
        self.assertTrue(rate_change_row(STEADY, STEADY)["passed"])
        shifted = Tone(5.0, 0.35, 0, 0.0, 2991.0)
        self.assertFalse(rate_change_row(STEADY, shifted)["passed"])
        self.assertFalse(rate_change_row(None, STEADY)["passed"])

    def test_the_loopback_hears_clicks_pitch_and_a_short_lived_capture_stream(self):
        rows = loopback_rows("S6", loopback_report(), CLEAN_CAPTURE, None)
        self.assertEqual([row["scenario"] for row in rows], ["S6", "S7"])
        self.assertTrue(rows[0]["passed"])
        for report, capture in (
            (loopback_report(detected=8), CLEAN_CAPTURE),
            (loopback_report(capture_hz=332.3), CLEAN_CAPTURE),
            (loopback_report(capture_hz=None), CLEAN_CAPTURE),
            (loopback_report(), {**CLEAN_CAPTURE, "probe": "running"}),
            (loopback_report(), {**CLEAN_CAPTURE, "started": "corked"}),
            (loopback_report(), {**CLEAN_CAPTURE, "closed": "corked"}),
            (loopback_report(), {}),
        ):
            self.assertFalse(
                loopback_rows("S6", report, capture, None)[0]["passed"], capture
            )
        hfp = loopback_rows("S6-hfp", loopback_report(), CLEAN_CAPTURE, None)
        self.assertEqual([row["scenario"] for row in hfp], ["S6-hfp"])

    def test_latency_is_gated_only_at_the_forced_quantum(self):
        def latency(p95, quantum):
            report = loopback_report(round_trip_ms_p95=p95)
            return loopback_rows("S6", report, CLEAN_CAPTURE, quantum)[1]["passed"]

        self.assertTrue(latency(40.0, 512))
        self.assertFalse(latency(40.1, 512))
        self.assertFalse(latency(None, 512))
        self.assertIsNone(latency(80.0, None))
        self.assertIsNone(latency(80.0, 1024))


class ParsingTest(unittest.TestCase):
    def test_the_game_stream_sink_is_found_by_process_id(self):
        sinks = [{"index": 51, "name": "harness_a"}, {"index": 58, "name": "harness_b"}]
        inputs = [
            {"sink": 51, "properties": {"application.process.id": "41"}},
            {"sink": 58, "properties": {"application.process.id": "42"}},
            {"sink": 99, "properties": {"application.process.id": "43"}},
        ]
        self.assertEqual(stream_sink(sinks, inputs, 42), "harness_b")
        self.assertEqual(stream_sink(sinks, inputs, 41), "harness_a")
        self.assertIsNone(stream_sink(sinks, inputs, 43))
        self.assertIsNone(stream_sink(sinks, inputs, 44))

    def test_the_flatpak_environment_comes_from_the_env_finish_args(self):
        manifest = (
            "finish-args:\n"
            "  - --socket=pulseaudio\n"
            "  - --env=FIRST=a=b c='d e'\n"
            "  - --env=SECOND=\n"
            "modules:\n"
            "  - name: eclipse\n"
            "    build-options:\n"
            "      env:\n"
            "        CARGO_NET_OFFLINE: 'true'\n"
        )
        self.assertEqual(
            flatpak_environment(manifest), {"FIRST": "a=b c='d e'", "SECOND": ""}
        )

    def test_the_manifest_gives_libpulse_eclipses_mixer_identity(self):
        environment = flatpak_environment(MANIFEST.read_text(encoding="utf-8"))
        override = environment["PULSE_PROP_OVERRIDE"]
        pairs = dict(token.split("=", 1) for token in override.split())
        self.assertEqual(pairs, MIXER_IDENTITY)
        self.assertNotIn("PULSE_PROP", environment)

    def test_capture_streams_are_found_by_process_id(self):
        outputs = [
            {"properties": {"application.process.id": "41"}, "corked": False},
            {"properties": {"application.process.id": "42"}, "corked": True},
            {"properties": {}, "corked": False},
        ]
        self.assertEqual(capture_state(outputs, 42), "corked")
        self.assertEqual(capture_state(outputs, 41), "running")
        self.assertEqual(capture_state(outputs, 43), "none")
        both = outputs + [
            {"properties": {"application.process.id": "42"}, "corked": False}
        ]
        self.assertEqual(capture_state(both, 42), "running")

    def test_the_report_is_the_last_line_that_is_not_a_phase(self):
        lines = ['{"phase": "probe"}', '{"detected": 10}', '{"phase": "closed"}']
        self.assertEqual(report_line(lines), {"detected": 10})
        self.assertIsNone(report_line(['{"phase": "probe"}']))

    def test_the_realtime_priority_limit_comes_from_proc_limits(self):
        limits = (
            "Limit                     Soft Limit           Hard Limit           Units\n"
            "Max realtime priority     98                   98\n"
        )
        self.assertEqual(realtime_priority_limit(limits), 98)
        self.assertEqual(
            realtime_priority_limit(
                "Max realtime priority     unlimited            unlimited\n"
            ),
            math.inf,
        )
        self.assertEqual(realtime_priority_limit("Max open files  1024  4096\n"), 0)

    def test_scenarios_are_named_or_all(self):
        self.assertEqual(scenario_list("S1,S6"), ["S1", "S6"])
        self.assertEqual(
            scenario_list("all"),
            [
                "S1",
                "S1-stall",
                "S2",
                "S2-latency60",
                "S2-rt",
                "S3",
                "S4",
                "S5",
                "S6",
            ],
        )
        with self.assertRaises(argparse.ArgumentTypeError):
            scenario_list("S1,S7")


if __name__ == "__main__":
    unittest.main()
