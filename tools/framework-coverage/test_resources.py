import struct
import unittest

from fixtures import (
    TYPE_HEADER_SIZE,
    StringPoolWriter,
    build_resource_table,
    build_xml,
)
from manifest import ANDROID_NS
from resources import (
    FLAG_COMPACT,
    FLAG_COMPLEX,
    RES_TABLE_TYPE_TYPE,
    TYPE_HEADER,
    CompiledXml,
    ResourceError,
    StringPool,
    is_compiled_xml,
    read_compiled_xml,
    read_xml_files,
)

MAIN = 0x7F0C0001
BAR = 0x7F0C0003
FILES = (
    (MAIN, "res/layout/main.xml"),
    (MAIN, "res/layout-land/main.xml"),
    (BAR, "res/layout/bar.xml"),
    (0x7F0E0000, "res/raw/notes.txt"),
)
TYPE_CHUNK_START = struct.pack("<HH", RES_TABLE_TYPE_TYPE, TYPE_HEADER_SIZE)
TYPE_FLAGS = 9
ENTRIES_START = 16
ENTRY_FLAGS = 2


def type_chunk(table, number):
    pos = -1
    for _ in range(number + 1):
        pos = table.index(TYPE_CHUNK_START, pos + 1)
    return pos


def first_entry(table, number):
    chunk = type_chunk(table, number)
    *_, start = TYPE_HEADER.unpack_from(table, chunk)
    return chunk + start


class ResourceTableTest(unittest.TestCase):
    def test_maps_resources_to_their_compiled_xml_files(self):
        self.assertEqual(
            read_xml_files(build_resource_table(FILES)),
            {
                MAIN: ["res/layout/main.xml", "res/layout-land/main.xml"],
                BAR: ["res/layout/bar.xml"],
            },
        )

    def test_skips_complex_entries(self):
        table = bytearray(build_resource_table(FILES))
        struct.pack_into("<H", table, first_entry(table, 2) + ENTRY_FLAGS, FLAG_COMPLEX)
        self.assertEqual(
            read_xml_files(bytes(table)),
            {MAIN: ["res/layout/main.xml", "res/layout-land/main.xml"]},
        )

    def test_rejects_sparse_and_compact_encodings(self):
        for flags, message in ((0x1, "flags 0x1"), (0x2, "flags 0x2")):
            with self.subTest(flags=flags):
                table = bytearray(build_resource_table(FILES))
                table[type_chunk(table, 0) + TYPE_FLAGS] = flags
                with self.assertRaisesRegex(ResourceError, message):
                    read_xml_files(bytes(table))
        table = bytearray(build_resource_table(FILES))
        struct.pack_into("<H", table, first_entry(table, 0) + ENTRY_FLAGS, FLAG_COMPACT)
        with self.assertRaisesRegex(ResourceError, "compact"):
            read_xml_files(bytes(table))

    def test_rejects_a_damaged_table(self):
        table = bytearray(build_resource_table(FILES))
        with self.assertRaisesRegex(ResourceError, "declares"):
            read_xml_files(bytes(table[:-6]))
        struct.pack_into("<I", table, type_chunk(table, 0) + ENTRIES_START, 0xFFFFFF00)
        with self.assertRaisesRegex(ResourceError, "malformed resource table"):
            read_xml_files(bytes(table))


class CompiledXmlTest(unittest.TestCase):
    def test_reads_strings_and_references(self):
        data = build_xml(
            (
                "FrameLayout",
                {},
                (
                    ("com.example.app.GameView", {"android:id": 0x7F080001}, ()),
                    ("include", {"layout": BAR}, ()),
                ),
            )
        )
        self.assertTrue(is_compiled_xml(data))
        self.assertEqual(
            read_compiled_xml(data),
            CompiledXml(
                frozenset(
                    (
                        "FrameLayout",
                        "com.example.app.GameView",
                        ANDROID_NS,
                        "id",
                        "include",
                        "layout",
                    )
                ),
                frozenset((0x7F080001, BAR)),
            ),
        )

    def test_tells_plain_xml_apart(self):
        self.assertFalse(is_compiled_xml(b"<notes/>"))
        self.assertFalse(is_compiled_xml(b""))

    def test_rejects_truncated_xml(self):
        with self.assertRaises(ResourceError):
            read_compiled_xml(build_xml(("FrameLayout", {}, ()))[:-10])


class StringPoolTest(unittest.TestCase):
    def test_decodes_surrogate_pairs_in_utf8_pools(self):
        writer = StringPoolWriter()
        writer.index("ABCDEF")
        chunk = writer.chunk().replace(b"ABCDEF", b"\xed\xa0\xbd\xed\xb8\x80")
        self.assertEqual(StringPool(chunk, 0).get(0), "\U0001f600")


if __name__ == "__main__":
    unittest.main()
