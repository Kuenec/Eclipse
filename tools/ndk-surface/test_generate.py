import unittest

from generate import answer, exported_functions, prototypes, surface

MAP = """
LIBMEDIANDK {
  global:
    AMEDIAFORMAT_KEY_MIME; # var introduced=21
    AMediaCodec_createCodecByName;
    AMediaCodec_queueInputBuffer;
    AMediaCodec_getName; # introduced=28
  local:
    *;
};

LIBMEDIANDK_PLATFORM {
  global:
    AMediaCodec_createCodecByNameForClient;
} LIBMEDIANDK;
"""

HEADER = """
/** Opens a codec; see AMediaCodec_delete(). */
AMediaCodec* AMediaCodec_createCodecByName(const char *name) __INTRODUCED_IN(21);

static_assert(sizeof(long) == 8, "see https://example.invalid/issue; really");

media_status_t AMediaCodec_queueInputBuffer(AMediaCodec*, size_t idx,
                                            off_t offset, size_t size,
                                            uint64_t time, uint32_t flags) __INTRODUCED_IN(21);

#define AMEDIA_SOMETHING(x) \\
    call(x)

typedef void (*AMediaCodecOnError)(AMediaCodec *codec, void *userdata);
"""


class GenerateTest(unittest.TestCase):
    def test_a_map_lists_public_functions_without_variables_or_platform_symbols(self):
        self.assertEqual(
            exported_functions(MAP),
            ["AMediaCodec_createCodecByName", "AMediaCodec_queueInputBuffer", "AMediaCodec_getName"],
        )

    def test_prototypes_survive_comments_strings_macros_and_line_breaks(self):
        self.assertEqual(
            prototypes(HEADER),
            {
                "AMediaCodec_createCodecByName": "AMediaCodec*",
                "AMediaCodec_queueInputBuffer": "media_status_t",
            },
        )

    def test_each_return_type_has_its_unsupported_answer(self):
        for returns, expected in (
            ("AMediaCodec*", "Zero"),
            ("char *", "Zero"),
            ("void", "Zero"),
            ("bool", "Zero"),
            ("size_t", "Zero"),
            ("uint64_t", "Zero"),
            ("float", "FloatZero"),
            ("media_status_t", "MediaUnsupported"),
            ("int32_t", "MinusOne"),
            ("ssize_t", "MinusOne"),
            ("AThermalStatus", "MinusOne"),
        ):
            self.assertEqual(answer(returns), expected, returns)

    def test_exported_functions_without_a_declaration_are_reported_not_guessed(self):
        answers, undeclared = surface([MAP], [HEADER])
        self.assertEqual(
            answers,
            {
                "AMediaCodec_createCodecByName": "Zero",
                "AMediaCodec_queueInputBuffer": "MediaUnsupported",
            },
        )
        self.assertEqual(undeclared, ["AMediaCodec_getName"])


if __name__ == "__main__":
    unittest.main()
