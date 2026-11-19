import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from record import KEPT_VERSIONS, ListError, add, main, read_list, tested, write_list

OLD = {"version_code": 20836, "version_name": "2.746.1266", "eclipse": "0.1.20"}
NEW = {"version_code": 22294, "version_name": "2.748.1527", "eclipse": "0.1.24"}


class RecordTest(unittest.TestCase):
    def test_a_version_is_tested_from_the_eclipse_that_started_it(self):
        entries = [NEW, OLD]
        self.assertTrue(tested(entries, 22294, "0.1.24"))
        self.assertTrue(tested(entries, 22294, "0.1.100"))
        self.assertFalse(tested(entries, 22294, "0.1.23"))
        self.assertFalse(tested(entries, 22400, "0.1.24"))
        self.assertFalse(tested([], 22294, "0.1.24"))

    def test_adding_keeps_the_newest_first_and_the_earliest_eclipse(self):
        entries = add([OLD], 22294, "2.748.1527", "0.1.24")
        self.assertEqual(entries, [NEW, OLD])
        self.assertEqual(add(entries, 22294, "2.748.1527", "0.1.30"), [NEW, OLD])
        earlier = add(entries, 22294, "2.748.1527", "0.1.9")
        self.assertEqual(earlier[0]["eclipse"], "0.1.9")
        self.assertEqual(len(earlier), 2)

    def test_the_list_keeps_a_bounded_number_of_versions(self):
        entries = []
        for code in range(KEPT_VERSIONS + 5):
            entries = add(entries, code, f"2.{code}.1", "0.1.24")
        self.assertEqual(len(entries), KEPT_VERSIONS)
        self.assertEqual(entries[0]["version_code"], KEPT_VERSIONS + 4)
        self.assertEqual(entries[-1]["version_code"], 5)

    def test_a_written_list_reads_back(self):
        text = write_list([NEW, OLD])
        self.assertTrue(text.endswith("}\n"))
        self.assertEqual(json.loads(text)["schema"], 1)
        self.assertEqual(read_list(text), [NEW, OLD])

    def test_a_list_of_another_shape_is_an_error(self):
        for text in ("<html>", "[]", '{"schema": 2, "roblox": []}', '{"schema": 1}'):
            with self.assertRaises(ListError, msg=text):
                read_list(text)
        for entry in (
            {**NEW, "version_code": "22294"},
            {**NEW, "eclipse": "soon"},
            {"version_code": 22294, "eclipse": "0.1.24"},
            "2.748.1527",
        ):
            with self.assertRaises(ListError, msg=entry):
                read_list(json.dumps({"schema": 1, "roblox": [entry]}))
        with self.assertRaises(ListError):
            add([], 22294, "2.748.1527", "v0.1.24")

    def test_the_command_starts_a_missing_list_and_reports_a_version(self):
        with tempfile.TemporaryDirectory() as temp:
            file = Path(temp) / "roblox.json"
            version = ["--list", str(file), "--roblox-code", "22294", "--eclipse", "0.1.24"]
            self.assertEqual(main(["add", *version, "--roblox-name", "2.748.1527"]), 0)
            self.assertEqual(read_list(file.read_text("utf-8")), [NEW])
            printed = io.StringIO()
            with contextlib.redirect_stdout(printed):
                self.assertEqual(main(["status", *version]), 0)
                self.assertEqual(main(["status", *version[:-1], "0.1.23"]), 0)
            self.assertEqual(printed.getvalue(), "tested\nuntested\n")
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(main(["add", *version]), 1)
                file.write_text("{}", "utf-8")
                self.assertEqual(main(["status", *version]), 1)


if __name__ == "__main__":
    unittest.main()
