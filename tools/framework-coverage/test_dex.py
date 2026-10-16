import tempfile
import unittest
from pathlib import Path

from dex import (
    ACC_STATIC,
    CHECK_CAST,
    CONST_CLASS,
    FILLED_NEW_ARRAY,
    IGET,
    NEW_INSTANCE,
    SGET,
    VALUE_ANNOTATION,
    VALUE_ARRAY,
    VALUE_BOOLEAN,
    VALUE_LONG,
    VALUE_NULL,
    Dex,
    DexError,
    Method,
    Ref,
    decode_mutf8,
    read_dexes,
)
from fixtures import (
    ACC_ABSTRACT,
    ACC_PUBLIC,
    DECOYS,
    INVOKE_DIRECT,
    INVOKE_INTERFACE,
    INVOKE_STATIC,
    INVOKE_STATIC_RANGE,
    INVOKE_SUPER,
    INVOKE_VIRTUAL,
    OBJECT,
    RETURN_VOID,
    SPUT,
    FixtureClass,
    FixtureMethod,
    Try,
    build_dex,
    call,
    encoded_int,
    field,
    type_ref,
    write_archive,
)

TARGET = "Lcom/example/Target;"


def method_refs(code, tries=()):
    method = FixtureMethod("run", "()V", code=code, tries=tries)
    dex = Dex(build_dex([FixtureClass("Lcom/example/Caller;", methods=(method,))]), "t")
    [definition] = dex.classes()
    described = []
    for ref, index in dex.code_refs(definition.methods[("run", "()V")].code_off):
        match ref:
            case Ref.TYPE | Ref.CLASS_LITERAL | Ref.NEW_INSTANCE:
                described.append((ref, dex.type_name(index)))
            case Ref.INSTANCE_FIELD | Ref.STATIC_FIELD:
                owner, name, type_ = dex.field_ref(index)
                described.append((ref, f"{owner}->{name}:{type_}"))
            case Ref.STATIC_CALL | Ref.DIRECT_CALL | Ref.VIRTUAL_CALL:
                owner, name, proto = dex.method_ref(index)
                described.append((ref, f"{owner}->{name}{proto}"))
    return described


class CodeRefsTest(unittest.TestCase):
    def test_reads_every_reference_in_order(self):
        refs = method_refs(
            (
                call(INVOKE_VIRTUAL, TARGET, "virtual", "()V"),
                call(INVOKE_SUPER, TARGET, "parent", "()V"),
                call(INVOKE_DIRECT, TARGET, "<init>", "()V"),
                call(INVOKE_STATIC, TARGET, "create", "(I[Ljava/lang/String;)J"),
                call(INVOKE_INTERFACE, TARGET, "callback", "()Z"),
                call(INVOKE_STATIC_RANGE, TARGET, "ranged", "(JJJ)V"),
                field(IGET, TARGET, "count", "I"),
                field(SGET, TARGET, "NAME", "Ljava/lang/String;"),
                field(SPUT, TARGET, "FLAG", "Z"),
                type_ref(CONST_CLASS, TARGET),
                type_ref(NEW_INSTANCE, TARGET),
                type_ref(CHECK_CAST, "Lcom/example/Cast;"),
                type_ref(FILLED_NEW_ARRAY, "[Lcom/example/Item;"),
                (RETURN_VOID,),
            )
        )
        self.assertEqual(
            refs,
            [
                (Ref.VIRTUAL_CALL, f"{TARGET}->virtual()V"),
                (Ref.DIRECT_CALL, f"{TARGET}->parent()V"),
                (Ref.DIRECT_CALL, f"{TARGET}-><init>()V"),
                (Ref.STATIC_CALL, f"{TARGET}->create(I[Ljava/lang/String;)J"),
                (Ref.VIRTUAL_CALL, f"{TARGET}->callback()Z"),
                (Ref.STATIC_CALL, f"{TARGET}->ranged(JJJ)V"),
                (Ref.INSTANCE_FIELD, f"{TARGET}->count:I"),
                (Ref.STATIC_FIELD, f"{TARGET}->NAME:Ljava/lang/String;"),
                (Ref.STATIC_FIELD, f"{TARGET}->FLAG:Z"),
                (Ref.CLASS_LITERAL, TARGET),
                (Ref.NEW_INSTANCE, TARGET),
                (Ref.TYPE, "Lcom/example/Cast;"),
                (Ref.TYPE, "[Lcom/example/Item;"),
            ],
        )

    def test_skips_payloads_and_wide_literals_whole(self):
        refs = method_refs(
            ((RETURN_VOID,), *DECOYS, call(INVOKE_STATIC, TARGET, "after", "()V"))
        )
        self.assertEqual(refs, [(Ref.STATIC_CALL, f"{TARGET}->after()V")])

    def test_reads_catch_types_of_every_handler(self):
        refs = method_refs(
            ((RETURN_VOID,),),
            (
                Try(0, 1, ("Landroid/os/RemoteException;",)),
                Try(0, 1, ("Ljava/io/IOException;", "Ljava/lang/Error;"), True),
            ),
        )
        self.assertEqual(
            refs,
            [
                (Ref.TYPE, "Landroid/os/RemoteException;"),
                (Ref.TYPE, "Ljava/io/IOException;"),
                (Ref.TYPE, "Ljava/lang/Error;"),
            ],
        )

    def test_names_the_dex_of_a_malformed_code_item(self):
        dex = Dex(build_dex([FixtureClass(OBJECT, superclass=None)]), "base.apk!x.dex")
        with self.assertRaisesRegex(DexError, r"base\.apk!x\.dex: malformed code"):
            dex.code_refs(len(dex.data) - 8)


class ClassesTest(unittest.TestCase):
    def test_reads_hierarchy_fields_and_methods(self):
        dex = Dex(
            build_dex(
                [
                    FixtureClass(
                        "Lcom/example/Widget;",
                        superclass="Landroid/view/View;",
                        interfaces=("Ljava/lang/Runnable;", "Lcom/example/Listener;"),
                        fields=(("count", "I"),),
                        methods=(
                            FixtureMethod("<init>", "()V"),
                            FixtureMethod("draw", "(I[Ljava/lang/String;)V"),
                            FixtureMethod(
                                "measure",
                                "()J",
                                code=None,
                                flags=ACC_PUBLIC | ACC_ABSTRACT,
                            ),
                            FixtureMethod("make", "()V", flags=ACC_STATIC),
                        ),
                    ),
                    FixtureClass(OBJECT, superclass=None),
                ]
            ),
            "t",
        )
        widget, root = dex.classes()
        self.assertEqual(widget.name, "Lcom/example/Widget;")
        self.assertEqual(widget.superclass, "Landroid/view/View;")
        self.assertEqual(
            widget.interfaces, ("Ljava/lang/Runnable;", "Lcom/example/Listener;")
        )
        self.assertEqual(widget.fields, {("count", "I")})
        self.assertEqual(
            set(widget.methods),
            {
                ("<init>", "()V"),
                ("draw", "(I[Ljava/lang/String;)V"),
                ("measure", "()J"),
                ("make", "()V"),
            },
        )
        self.assertEqual(
            widget.methods[("measure", "()J")], Method(ACC_PUBLIC | ACC_ABSTRACT, 0)
        )
        self.assertNotEqual(
            widget.methods[("draw", "(I[Ljava/lang/String;)V")].code_off, 0
        )
        self.assertEqual(widget.methods[("make", "()V")].flags, ACC_STATIC)
        self.assertIsNone(root.superclass)
        self.assertIs(widget.dex, dex)

    def test_reads_static_int_values(self):
        array = bytes((VALUE_ARRAY, 2)) + encoded_int(5)
        annotation = bytes((VALUE_ANNOTATION, 0, 1, 0, 1 << 5 | VALUE_BOOLEAN))
        long_value = bytes((7 << 5 | VALUE_LONG,)) + (1 << 40).to_bytes(8, "little")
        [layouts] = Dex(
            build_dex(
                [
                    FixtureClass(
                        "Lcom/example/R$layout;",
                        fields=(
                            ("big", "J", long_value),
                            ("main", "I", encoded_int(0x7F0C0001)),
                            ("negative", "I", encoded_int(-2)),
                            ("nested", "[Ljava/lang/Object;", array + annotation),
                            ("none", "Ljava/lang/Object;", bytes((VALUE_NULL,))),
                            ("tail", "I", encoded_int(7)),
                        ),
                    )
                ]
            ),
            "t",
        ).classes()
        self.assertEqual(
            layouts.int_constants,
            {("main", "I"): 0x7F0C0001, ("negative", "I"): -2, ("tail", "I"): 7},
        )

    def test_rejects_an_unknown_static_value_type(self):
        dex = Dex(
            build_dex([FixtureClass("Lcom/example/R;", fields=(("a", "I", b"\x01"),))]),
            "base.apk!classes.dex",
        )
        with self.assertRaisesRegex(DexError, "unknown encoded value type 0x01"):
            dex.classes()

    def test_reads_dex_entries_in_load_order(self):
        dex = build_dex([FixtureClass(OBJECT, superclass=None)])
        with tempfile.TemporaryDirectory() as directory:
            archive = write_archive(
                Path(directory) / "app.apk",
                {
                    "classes10.dex": dex,
                    "classes2.dex": dex,
                    "lib/classes3.dex": dex,
                    "classes.dex": dex,
                    "classesX.dex": dex,
                },
            )
            origins = [found.origin for found in read_dexes(archive)]
        self.assertEqual(
            origins,
            [
                f"{archive}!classes.dex",
                f"{archive}!classes2.dex",
                f"{archive}!classes10.dex",
            ],
        )


class HeaderTest(unittest.TestCase):
    def test_rejects_a_file_that_is_not_dex(self):
        with self.assertRaisesRegex(DexError, "app.apk!classes.dex is not a dex file"):
            Dex(b"PK\x03\x04" + bytes(200), "app.apk!classes.dex")

    def test_rejects_tables_past_the_end(self):
        data = build_dex([FixtureClass(OBJECT, superclass=None)])
        with self.assertRaisesRegex(DexError, "declares"):
            Dex(data[:-4], "t")
        with self.assertRaisesRegex(DexError, "the string table ends past the file"):
            Dex(data[:0x3C] + (0xFFFF).to_bytes(4, "little") + data[0x40:], "t")


class Mutf8Test(unittest.TestCase):
    def test_decodes_embedded_nul_and_surrogate_pairs(self):
        self.assertEqual(decode_mutf8(b"a\xc0\x80b"), "a\0b")
        self.assertEqual(decode_mutf8(b"\xed\xa0\xbd\xed\xb8\x80"), "\U0001f600")
        self.assertEqual(decode_mutf8("é".encode()), "é")


if __name__ == "__main__":
    unittest.main()
