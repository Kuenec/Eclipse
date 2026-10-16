import re
import struct
import sys
import zipfile
from array import array
from dataclasses import dataclass
from enum import Enum

DEX_MAGIC = b"dex\n"
ENDIAN_CONSTANT = 0x12345678
NO_INDEX = 0xFFFFFFFF
ACC_PRIVATE = 0x2
ACC_STATIC = 0x8
HEADER = struct.Struct("<8s24xIII12x12I8x")
TABLE_ENTRY_SIZES = (
    ("string", 4),
    ("type", 4),
    ("proto", 12),
    ("field", 8),
    ("method", 8),
    ("class", 32),
)
U16 = struct.Struct("<H")
U32 = struct.Struct("<I")
PROTO_ID = struct.Struct("<4xII")
MEMBER_ID = struct.Struct("<HHI")
CLASS_DEF = struct.Struct("<I4xII8xII")
CODE_HEADER = struct.Struct("<6xH4xI")
PACKED_SWITCH_PAYLOAD = 0x0100
SPARSE_SWITCH_PAYLOAD = 0x0200
FILL_ARRAY_DATA_PAYLOAD = 0x0300
CONST_CLASS = 0x1C
CHECK_CAST = 0x1F
INSTANCE_OF = 0x20
NEW_INSTANCE = 0x22
NEW_ARRAY = 0x23
FILLED_NEW_ARRAY = 0x24
FILLED_NEW_ARRAY_RANGE = 0x25
IGET = 0x52
SGET = 0x60
FIELD_OPCODES = 14
INVOKE_VIRTUAL = 0x6E
INVOKE_VIRTUAL_RANGE = 0x74
VALUE_BYTE = 0x00
VALUE_SHORT = 0x02
VALUE_CHAR = 0x03
VALUE_INT = 0x04
VALUE_LONG = 0x06
VALUE_FLOAT = 0x10
VALUE_DOUBLE = 0x11
VALUE_METHOD_TYPE = 0x15
VALUE_ENUM = 0x1B
VALUE_ARRAY = 0x1C
VALUE_ANNOTATION = 0x1D
VALUE_NULL = 0x1E
VALUE_BOOLEAN = 0x1F
SIZED_VALUES = frozenset(
    (
        VALUE_BYTE,
        VALUE_SHORT,
        VALUE_CHAR,
        VALUE_INT,
        VALUE_LONG,
        VALUE_FLOAT,
        VALUE_DOUBLE,
        *range(VALUE_METHOD_TYPE, VALUE_ENUM + 1),
    )
)
DEX_ENTRY = re.compile(r"classes(\d*)\.dex")


class DexError(Exception):
    pass


class Ref(Enum):
    TYPE = "type"
    CLASS_LITERAL = "class-literal"
    NEW_INSTANCE = "new-instance"
    INSTANCE_FIELD = "instance-field"
    STATIC_FIELD = "static-field"
    STATIC_CALL = "static-call"
    DIRECT_CALL = "direct-call"
    VIRTUAL_CALL = "virtual-call"


class Table(Enum):
    TYPE = "type"
    FIELD = "field"
    METHOD = "method"


REF_TABLE = {
    Ref.TYPE: Table.TYPE,
    Ref.CLASS_LITERAL: Table.TYPE,
    Ref.NEW_INSTANCE: Table.TYPE,
    Ref.INSTANCE_FIELD: Table.FIELD,
    Ref.STATIC_FIELD: Table.FIELD,
    Ref.STATIC_CALL: Table.METHOD,
    Ref.DIRECT_CALL: Table.METHOD,
    Ref.VIRTUAL_CALL: Table.METHOD,
}


def opcode_tables():
    widths = [1] * 256
    for first, last, width in (
        (0x02, 0x02, 2),
        (0x03, 0x03, 3),
        (0x05, 0x05, 2),
        (0x06, 0x06, 3),
        (0x08, 0x08, 2),
        (0x09, 0x09, 3),
        (0x13, 0x13, 2),
        (0x14, 0x14, 3),
        (0x15, 0x16, 2),
        (0x17, 0x17, 3),
        (0x18, 0x18, 5),
        (0x19, 0x1A, 2),
        (0x1B, 0x1B, 3),
        (0x1C, 0x1C, 2),
        (0x1F, 0x20, 2),
        (0x22, 0x23, 2),
        (0x24, 0x26, 3),
        (0x29, 0x29, 2),
        (0x2A, 0x2C, 3),
        (0x2D, 0x3D, 2),
        (0x44, 0x6D, 2),
        (0x6E, 0x72, 3),
        (0x74, 0x78, 3),
        (0x90, 0xAF, 2),
        (0xD0, 0xE2, 2),
        (0xFA, 0xFB, 4),
        (0xFC, 0xFD, 3),
        (0xFE, 0xFF, 2),
    ):
        widths[first : last + 1] = [width] * (last - first + 1)
    refs = [None] * 256
    for opcode in (
        CHECK_CAST,
        INSTANCE_OF,
        NEW_ARRAY,
        FILLED_NEW_ARRAY,
        FILLED_NEW_ARRAY_RANGE,
    ):
        refs[opcode] = Ref.TYPE
    refs[CONST_CLASS] = Ref.CLASS_LITERAL
    refs[NEW_INSTANCE] = Ref.NEW_INSTANCE
    refs[IGET : IGET + FIELD_OPCODES] = [Ref.INSTANCE_FIELD] * FIELD_OPCODES
    refs[SGET : SGET + FIELD_OPCODES] = [Ref.STATIC_FIELD] * FIELD_OPCODES
    invokes = (
        Ref.VIRTUAL_CALL,
        Ref.DIRECT_CALL,
        Ref.DIRECT_CALL,
        Ref.STATIC_CALL,
        Ref.VIRTUAL_CALL,
    )
    for first in (INVOKE_VIRTUAL, INVOKE_VIRTUAL_RANGE):
        refs[first : first + len(invokes)] = invokes
    return widths, refs


OPCODE_WIDTHS, OPCODE_REFS = opcode_tables()


@dataclass(slots=True)
class Method:
    flags: int
    code_off: int


@dataclass(slots=True)
class ClassDef:
    name: str
    superclass: str | None
    interfaces: tuple
    methods: dict
    fields: set
    int_constants: dict
    dex: "Dex"


def read_uleb(data, pos):
    result = 0
    shift = 0
    while True:
        byte = data[pos]
        pos += 1
        result |= (byte & 0x7F) << shift
        shift += 7
        if byte < 0x80:
            return result, pos


def read_sleb(data, pos):
    result, end = read_uleb(data, pos)
    bits = 7 * (end - pos)
    if result >> (bits - 1):
        result -= 1 << bits
    return result, end


def read_encoded_int(data, pos):
    header = data[pos]
    kind, argument = header & 0x1F, header >> 5
    pos += 1
    if kind == VALUE_ARRAY:
        count, pos = read_uleb(data, pos)
        for _ in range(count):
            _, pos = read_encoded_int(data, pos)
        return None, pos
    if kind == VALUE_ANNOTATION:
        _, pos = read_uleb(data, pos)
        count, pos = read_uleb(data, pos)
        for _ in range(count):
            _, pos = read_uleb(data, pos)
            _, pos = read_encoded_int(data, pos)
        return None, pos
    if kind in (VALUE_NULL, VALUE_BOOLEAN):
        return None, pos
    if kind not in SIZED_VALUES:
        raise ValueError(f"unknown encoded value type {kind:#04x}")
    end = pos + argument + 1
    if end > len(data):
        raise ValueError(f"encoded value at {pos - 1} runs past the file")
    if kind != VALUE_INT:
        return None, end
    return int.from_bytes(data[pos:end], "little", signed=True), end


def decode_mutf8(raw):
    text = raw.replace(b"\xc0\x80", b"\0").decode("utf-8", "surrogatepass")
    if text.isascii():
        return text
    return text.encode("utf-16-le", "surrogatepass").decode("utf-16-le")


def element_type(descriptor):
    return descriptor.lstrip("[")


class Dex:
    def __init__(self, data, origin):
        if len(data) < HEADER.size or not data.startswith(DEX_MAGIC):
            raise DexError(f"{origin} is not a dex file")
        _, file_size, _, endian, *tables = HEADER.unpack_from(data)
        if endian != ENDIAN_CONSTANT:
            raise DexError(f"{origin} has endian tag {endian:#x}")
        if file_size > len(data):
            raise DexError(f"{origin} declares {file_size} bytes but holds {len(data)}")
        layout = list(zip(tables[0::2], tables[1::2], strict=True))
        for (count, offset), (table, entry_size) in zip(
            layout, TABLE_ENTRY_SIZES, strict=True
        ):
            if offset + count * entry_size > len(data):
                raise DexError(f"{origin}: the {table} table ends past the file")
        self.data = data
        self.origin = origin
        self._units = array("H")
        self._units.frombytes(data[: len(data) & ~1])
        if sys.byteorder == "big":
            self._units.byteswap()
        (
            (string_count, self._string_ids),
            (type_count, self._type_ids),
            (proto_count, self._proto_ids),
            (field_count, self._field_ids),
            (method_count, self._method_ids),
            (self._class_count, self._class_defs),
        ) = layout
        self._strings = [None] * string_count
        self._types = [None] * type_count
        self._protos = [None] * proto_count
        self._fields = [None] * field_count
        self._methods = [None] * method_count

    def string(self, index):
        text = self._strings[index]
        if text is None:
            (offset,) = U32.unpack_from(self.data, self._string_ids + 4 * index)
            _, start = read_uleb(self.data, offset)
            text = decode_mutf8(self.data[start : self.data.index(0, start)])
            self._strings[index] = text
        return text

    def type_name(self, index):
        name = self._types[index]
        if name is None:
            (string,) = U32.unpack_from(self.data, self._type_ids + 4 * index)
            name = self.string(string)
            self._types[index] = name
        return name

    def proto(self, index):
        descriptor = self._protos[index]
        if descriptor is None:
            return_type, parameters = PROTO_ID.unpack_from(
                self.data, self._proto_ids + 12 * index
            )
            arguments = "".join(self._type_list(parameters))
            descriptor = f"({arguments}){self.type_name(return_type)}"
            self._protos[index] = descriptor
        return descriptor

    def field_ref(self, index):
        field = self._fields[index]
        if field is None:
            owner, type_, name = MEMBER_ID.unpack_from(
                self.data, self._field_ids + 8 * index
            )
            field = (self.type_name(owner), self.string(name), self.type_name(type_))
            self._fields[index] = field
        return field

    def method_ref(self, index):
        method = self._methods[index]
        if method is None:
            owner, proto, name = MEMBER_ID.unpack_from(
                self.data, self._method_ids + 8 * index
            )
            method = (self.type_name(owner), self.string(name), self.proto(proto))
            self._methods[index] = method
        return method

    def classes(self):
        try:
            return [self._class_def(index) for index in range(self._class_count)]
        except (struct.error, IndexError, ValueError) as error:
            raise DexError(f"{self.origin}: malformed class data: {error}") from error

    def code_refs(self, code_off):
        try:
            return self._code_refs(code_off)
        except (struct.error, IndexError, ValueError) as error:
            raise DexError(
                f"{self.origin}: malformed code item at {code_off:#x}: {error}"
            ) from error

    def _type_list(self, offset):
        if not offset:
            return ()
        (count,) = U32.unpack_from(self.data, offset)
        return tuple(
            self.type_name(U16.unpack_from(self.data, offset + 4 + 2 * item)[0])
            for item in range(count)
        )

    def _class_def(self, index):
        type_index, superclass, interfaces, class_data, static_values = (
            CLASS_DEF.unpack_from(self.data, self._class_defs + 32 * index)
        )
        definition = ClassDef(
            self.type_name(type_index),
            None if superclass == NO_INDEX else self.type_name(superclass),
            self._type_list(interfaces),
            {},
            set(),
            {},
            self,
        )
        if class_data:
            static_fields = self._read_class_data(definition, class_data)
            if static_values:
                self._read_static_values(definition, static_fields, static_values)
        return definition

    def _read_class_data(self, definition, pos):
        counts = []
        for _ in range(4):
            count, pos = read_uleb(self.data, pos)
            counts.append(count)
        static_fields, instance_fields, direct_methods, virtual_methods = counts
        static, pos = self._read_fields(pos, static_fields)
        instance, pos = self._read_fields(pos, instance_fields)
        definition.fields.update(static, instance)
        for count in (direct_methods, virtual_methods):
            index = 0
            for _ in range(count):
                step, pos = read_uleb(self.data, pos)
                flags, pos = read_uleb(self.data, pos)
                code_off, pos = read_uleb(self.data, pos)
                index += step
                _, name, proto = self.method_ref(index)
                definition.methods[(name, proto)] = Method(flags, code_off)
        return static

    def _read_fields(self, pos, count):
        keys = []
        index = 0
        for _ in range(count):
            step, pos = read_uleb(self.data, pos)
            _, pos = read_uleb(self.data, pos)
            index += step
            _, name, type_ = self.field_ref(index)
            keys.append((name, type_))
        return keys, pos

    def _read_static_values(self, definition, static_fields, pos):
        count, pos = read_uleb(self.data, pos)
        if count > len(static_fields):
            raise ValueError(f"{count} static values for {len(static_fields)} fields")
        for key in static_fields[:count]:
            value, pos = read_encoded_int(self.data, pos)
            if value is not None:
                definition.int_constants[key] = value

    def _code_refs(self, code_off):
        tries, insns_size = CODE_HEADER.unpack_from(self.data, code_off)
        units = self._units
        pc = (code_off + CODE_HEADER.size) // 2
        end = pc + insns_size
        if end > len(units):
            raise ValueError(f"{insns_size} code units run past the file")
        refs = []
        while pc < end:
            unit = units[pc]
            opcode = unit & 0xFF
            ref = OPCODE_REFS[opcode]
            if ref is not None:
                refs.append((ref, units[pc + 1]))
            elif unit and not opcode:
                pc += payload_width(units, pc)
                continue
            pc += OPCODE_WIDTHS[opcode]
        if tries:
            handlers = 2 * (end + (insns_size & 1)) + 8 * tries
            refs.extend((Ref.TYPE, index) for index in self._catch_types(handlers))
        return refs

    def _catch_types(self, pos):
        types = []
        count, pos = read_uleb(self.data, pos)
        for _ in range(count):
            size, pos = read_sleb(self.data, pos)
            for _ in range(abs(size)):
                type_index, pos = read_uleb(self.data, pos)
                _, pos = read_uleb(self.data, pos)
                types.append(type_index)
            if size <= 0:
                _, pos = read_uleb(self.data, pos)
        return types


def payload_width(units, pc):
    kind = units[pc]
    if kind == PACKED_SWITCH_PAYLOAD:
        return 4 + 2 * units[pc + 1]
    if kind == SPARSE_SWITCH_PAYLOAD:
        return 2 + 4 * units[pc + 1]
    if kind == FILL_ARRAY_DATA_PAYLOAD:
        count = units[pc + 2] | units[pc + 3] << 16
        return 4 + (units[pc + 1] * count + 1) // 2
    raise ValueError(f"unknown payload {kind:#06x}")


def read_dexes(path):
    with zipfile.ZipFile(path) as archive:
        entries = sorted(
            (int(match[1] or 1), match[0])
            for match in map(DEX_ENTRY.fullmatch, archive.namelist())
            if match
        )
        return [Dex(archive.read(name), f"{path}!{name}") for _, name in entries]
