#!/usr/bin/env python3

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("release-notes.py")
METAINFO = Path("packaging/flatpak/io.github.kuenec.Eclipse.metainfo.xml")


def release(version, description):
    return (
        f'<release version="{version}"><description>{description}'
        "</description></release>"
    )


def cargo_toml(version):
    return f'[package]\nname = "eclipse"\nversion = "{version}"\n'


def metainfo(releases, screenshots=""):
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<component type="desktop-application">\n'
        f"{screenshots}"
        f"  <releases>\n{releases}\n  </releases>\n"
        "</component>\n"
    )


def screenshot(ref):
    return (
        "  <screenshots><screenshot><image>"
        f"https://raw.githubusercontent.com/Kuenec/Eclipse/{ref}/shot.png"
        "</image></screenshot></screenshots>\n"
    )


class ReleaseNotesTest(unittest.TestCase):
    def run_script(self, cargo_toml_text, metainfo_text, *arguments):
        scratch = Path(self.enterContext(tempfile.TemporaryDirectory()))
        script = scratch / "packaging/flatpak/release-notes.py"
        script.parent.mkdir(parents=True)
        shutil.copy(SCRIPT, script)
        (scratch / "Cargo.toml").write_text(cargo_toml_text, encoding="utf-8")
        (scratch / METAINFO).write_text(metainfo_text, encoding="utf-8")
        return subprocess.run(
            [sys.executable, script, *arguments],
            capture_output=True,
            encoding="utf-8",
            check=False,
        )

    def notes(self, version, releases, *arguments):
        return self.run_script(cargo_toml(version), metainfo(releases), *arguments)

    def assert_prints(self, run, markdown):
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout, markdown)

    def assert_fails(self, run, *names):
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertEqual(run.stdout, "")
        self.assertTrue(run.stderr.startswith("release-notes.py: "), run.stderr)
        for name in names:
            self.assertIn(name, run.stderr)

    def test_paragraphs_lists_and_code_become_markdown(self):
        description = """
            <p>
              Run <code>eclipse   config</code> to see
              the settings.
            </p>
            <ul>
              <li>First change.</li>
              <li>Second <code>change</code></li>
            </ul>
            <p>Last paragraph.</p>
        """
        releases = release("1.2.0", description) + release("1.1.0", "<p>Older.</p>")
        self.assert_prints(
            self.notes("1.2.0", releases),
            "Run `eclipse config` to see the settings.\n"
            "\n"
            "- First change.\n"
            "- Second `change`\n"
            "\n"
            "Last paragraph.\n",
        )

    def test_markdown_syntax_in_text_stays_literal(self):
        description = """
            <p>Use &lt;files&gt; with *care*, [links] and snake_case &amp; ~</p>
            <ul>
              <li>- not a nested list</li>
              <li># not a heading</li>
              <li>2. not a numbered list</li>
            </ul>
        """
        self.assert_prints(
            self.notes("1.0.0", release("1.0.0", description)),
            r"Use \<files\> with \*care\*, \[links\] and snake\_case \& \~"
            "\n\n"
            r"- \- not a nested list"
            "\n"
            r"- \# not a heading"
            "\n"
            r"- 2\. not a numbered list"
            "\n",
        )

    def test_a_matching_version_argument_prints_the_notes(self):
        releases = release("1.0.0", "<p>Notes.</p>")
        self.assert_prints(self.notes("1.0.0", releases, "1.0.0"), "Notes.\n")

    def test_a_version_argument_other_than_cargo_toml_fails_naming_both(self):
        releases = release("0.1.3", "<p>Notes.</p>")
        self.assert_fails(self.notes("0.1.3", releases, "0.1.2"), "0.1.2", "0.1.3")

    def test_the_tag_check_fails_on_a_screenshot_that_follows_a_branch(self):
        listing = metainfo(release("1.0.0", "<p>Notes.</p>"), screenshot("main"))
        self.assert_fails(
            self.run_script(cargo_toml("1.0.0"), listing, "1.0.0"),
            "Eclipse/main/shot.png",
            "v1.0.0",
        )

    def test_screenshots_pinned_to_a_tag_or_commit_pass_the_tag_check(self):
        for ref in ("v0.9.0", "0123456789abcdef0123456789abcdef01234567"):
            with self.subTest(ref=ref):
                listing = metainfo(release("1.0.0", "<p>Notes.</p>"), screenshot(ref))
                self.assert_prints(
                    self.run_script(cargo_toml("1.0.0"), listing, "1.0.0"), "Notes.\n"
                )

    def test_without_a_version_a_screenshot_on_a_branch_still_prints_the_notes(self):
        listing = metainfo(release("1.0.0", "<p>Notes.</p>"), screenshot("main"))
        self.assert_prints(self.run_script(cargo_toml("1.0.0"), listing), "Notes.\n")

    def test_a_version_without_a_release_entry_fails_naming_it(self):
        releases = release("0.1.2", "<p>Notes.</p>")
        self.assert_fails(self.notes("0.1.3", releases), '<release version="0.1.3">')

    def test_a_release_without_description_text_fails_naming_it(self):
        for releases in (
            '<release version="0.1.3" date="2026-10-02"/>',
            release("0.1.3", "\n  <p> </p>\n"),
        ):
            with self.subTest(releases=releases):
                self.assert_fails(
                    self.notes("0.1.3", releases),
                    '<release version="0.1.3">',
                    "no <description> text",
                )

    def test_an_element_without_a_markdown_form_fails_naming_it(self):
        for description, element in (
            ("<p>An <em>emphasised</em> word.</p>", "<em>"),
            ("<ol><li>Numbered.</li></ol>", "<ol>"),
            ("<ul><li>Item.</li><p>Paragraph in a list.</p></ul>", "<p>"),
        ):
            with self.subTest(description=description):
                self.assert_fails(
                    self.notes("1.0.0", release("1.0.0", description)),
                    '<release version="1.0.0">',
                    f"uses {element}",
                )

    def test_text_outside_paragraphs_and_items_fails_quoting_it(self):
        releases = release("1.0.0", "Loose text<p>Notes.</p>")
        self.assert_fails(self.notes("1.0.0", releases), "'Loose text'")

    def test_a_metainfo_that_is_not_xml_fails_naming_it_and_the_line(self):
        releases = (
            "<<<<<<< HEAD\n"
            + release("1.0.0", "<p>Notes.</p>")
            + "\n=======\n>>>>>>> branch"
        )
        self.assert_fails(
            self.notes("1.0.0", releases), str(METAINFO), "line 4, column 1"
        )

    def test_a_cargo_toml_without_a_readable_version_fails_naming_the_cause(self):
        listing = metainfo(release("1.0.0", "<p>Notes.</p>"))
        for manifest, cause in (
            ('[package]\nname = "eclipse"\n', "no package.version"),
            ("[package\n", "line 1"),
        ):
            with self.subTest(manifest=manifest):
                self.assert_fails(
                    self.run_script(manifest, listing), "Cargo.toml", cause
                )


if __name__ == "__main__":
    unittest.main()
