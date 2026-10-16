import unittest
from pathlib import Path

from fixtures import LAUNCHER_FILTER, build_xml
from manifest import Components, ManifestError, read_components

REPOSITORY_FIXTURES = Path(__file__).resolve().parents[2] / "tests" / "fixtures"


def manifest(*components):
    return build_xml(
        (
            "manifest",
            {"package": "com.example.app"},
            (("application", {"android:name": ".App"}, components),),
        )
    )


class ReadComponentsTest(unittest.TestCase):
    def test_reads_utf8_and_utf16_string_pools(self):
        for name in ("AndroidManifest-min.bin", "AndroidManifest-utf16.bin"):
            with self.subTest(name=name):
                data = (REPOSITORY_FIXTURES / name).read_bytes()
                self.assertEqual(
                    read_components(data),
                    Components(None, (), "Lcom/example/app/SplashActivity;"),
                )

    def test_reads_application_providers_and_launcher(self):
        data = manifest(
            ("provider", {"android:name": "com.example.lib.Init"}, ()),
            (
                "provider",
                {"android:name": ".Sandbox", "android:process": ":sandbox"},
                (),
            ),
            (
                "provider",
                {"android:name": "Files", "android:process": "com.example.app"},
                (),
            ),
            ("activity", {"android:name": ".Settings"}, ()),
            ("activity", {"android:name": ".Splash"}, (LAUNCHER_FILTER,)),
            ("activity", {"android:name": ".Second"}, (LAUNCHER_FILTER,)),
        )
        self.assertEqual(
            read_components(data),
            Components(
                "Lcom/example/app/App;",
                ("Lcom/example/lib/Init;", "Lcom/example/app/Files;"),
                "Lcom/example/app/Splash;",
            ),
        )

    def test_an_alias_launches_its_target_activity(self):
        data = manifest(
            ("activity", {"android:name": ".Main"}, ()),
            (
                "activity-alias",
                {"android:name": ".Icon", "android:targetActivity": ".Main"},
                (LAUNCHER_FILTER,),
            ),
        )
        self.assertEqual(read_components(data).launcher, "Lcom/example/app/Main;")

    def test_needs_both_main_and_launcher(self):
        data = manifest(
            (
                "activity",
                {"android:name": ".Share"},
                (
                    (
                        "intent-filter",
                        {},
                        (
                            (
                                "action",
                                {"android:name": "android.intent.action.MAIN"},
                                (),
                            ),
                        ),
                    ),
                ),
            ),
        )
        self.assertIsNone(read_components(data).launcher)

    def test_rejects_malformed_xml(self):
        data = manifest(("activity", {"android:name": ".Splash"}, ()))
        with self.assertRaises(ManifestError):
            read_components(b"\x02\x00" + data[2:])
        with self.assertRaises(ManifestError):
            read_components(data[:-30])

    def test_requires_a_package(self):
        data = build_xml(("manifest", {}, ()))
        with self.assertRaisesRegex(ManifestError, "no package"):
            read_components(data)


if __name__ == "__main__":
    unittest.main()
