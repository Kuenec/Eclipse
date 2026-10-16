import struct
import zipfile
from dataclasses import dataclass

from dex import (
    ACC_PRIVATE,
    ACC_STATIC,
    DEX_MAGIC,
    ENDIAN_CONSTANT,
    FILL_ARRAY_DATA_PAYLOAD,
    FILLED_NEW_ARRAY,
    FILLED_NEW_ARRAY_RANGE,
    HEADER,
    INVOKE_VIRTUAL,
    NEW_INSTANCE,
    NO_INDEX,
    PACKED_SWITCH_PAYLOAD,
    SGET,
    SPARSE_SWITCH_PAYLOAD,
    VALUE_INT,
    VALUE_NULL,
)
from manifest import ANDROID_NS
from resources import (
    NO_ENTRY,
    NO_STRING,
    RES_STRING_POOL_TYPE,
    RES_TABLE_PACKAGE_TYPE,
    RES_TABLE_TYPE,
    RES_TABLE_TYPE_TYPE,
    RES_XML_END_ELEMENT_TYPE,
    RES_XML_START_ELEMENT_TYPE,
    RES_XML_TYPE,
    TYPE_REFERENCE,
    TYPE_STRING,
    UTF8_FLAG,
)

OBJECT = "Ljava/lang/Object;"
ACC_PUBLIC = 0x1
ACC_INTERFACE = 0x200
ACC_ABSTRACT = 0x400
RETURN_VOID = 0x0E
CONST_WIDE = 0x18
INVOKE_SUPER = INVOKE_VIRTUAL + 1
INVOKE_DIRECT = INVOKE_VIRTUAL + 2
INVOKE_STATIC = INVOKE_VIRTUAL + 3
INVOKE_INTERFACE = INVOKE_VIRTUAL + 4
INVOKE_STATIC_RANGE = INVOKE_STATIC + 6
SPUT = SGET + 7
CONFIG_SIZE = 64
TYPE_HEADER_SIZE = 20 + CONFIG_SIZE
PACKAGE_HEADER_SIZE = 288
PACKAGE_NAME_SIZE = 256


@dataclass(frozen=True)
class Try:
    start: int
    count: int
    catches: tuple
    catch_all: bool = False


@dataclass(frozen=True)
class FixtureMethod:
    name: str
    proto: str
    code: tuple | None = ((RETURN_VOID,),)
    flags: int = ACC_PUBLIC
    tries: tuple = ()


@dataclass(frozen=True)
class FixtureClass:
    name: str
    superclass: str | None = OBJECT
    interfaces: tuple = ()
    fields: tuple = ()
    methods: tuple = ()
    flags: int = ACC_PUBLIC


def call(opcode, owner, name, proto):
    return (opcode, ("method", owner, name, proto))


def field(opcode, owner, name, type_):
    return (opcode, ("field", owner, name, type_))


def type_ref(opcode, descriptor):
    return (opcode, ("type", descriptor))


def encoded_int(value):
    size = (value.bit_length() + 8) // 8
    return bytes(((size - 1) << 5 | VALUE_INT,)) + value.to_bytes(
        size, "little", signed=True
    )


def uleb(value):
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def sleb(value):
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        done = (value == 0 and not byte & 0x40) or (value == -1 and byte & 0x40)
        out.append(byte if done else byte | 0x80)
        if done:
            return bytes(out)


def split_proto(proto):
    arguments, result = proto[1:].split(")")
    types = []
    pos = 0
    while pos < len(arguments):
        end = pos
        while arguments[end] == "[":
            end += 1
        if arguments[end] == "L":
            end = arguments.index(";", end)
        types.append(arguments[pos : end + 1])
        pos = end + 1
    return result, types


def shorty(proto):
    result, arguments = split_proto(proto)
    return "".join("L" if len(type_) > 1 else type_ for type_ in (result, *arguments))


class DexWriter:
    def __init__(self, classes):
        self.classes = classes
        strings, types, protos, fields, methods = set(), set(), set(), set(), set()
        for definition in classes:
            types.update(filter(None, (definition.name, definition.superclass)))
            types.update(definition.interfaces)
            fields.update(field_key(definition, member) for member in definition.fields)
            for method in definition.methods:
                methods.add((definition.name, method.name, method.proto))
                for _, *operands in method.code or ():
                    match operands:
                        case [("method", *member)]:
                            methods.add(tuple(member))
                        case [("field", *member)]:
                            fields.add(tuple(member))
                        case [("type", descriptor)]:
                            types.add(descriptor)
                for attempt in method.tries:
                    types.update(attempt.catches)
        for owner, name, type_ in fields:
            types.update((owner, type_))
            strings.add(name)
        for owner, name, proto in methods:
            types.add(owner)
            strings.update((name, shorty(proto)))
            protos.add(proto)
        for proto in protos:
            result, arguments = split_proto(proto)
            types.update((result, *arguments))
        strings |= types
        self.strings = sorted(strings)
        self.types = sorted(types)
        self.protos = sorted(protos)
        self.fields = sorted(fields)
        self.methods = sorted(methods)
        self.string_index = {value: index for index, value in enumerate(self.strings)}
        self.type_index = {value: index for index, value in enumerate(self.types)}
        self.proto_index = {value: index for index, value in enumerate(self.protos)}
        self.field_index = {value: index for index, value in enumerate(self.fields)}
        self.method_index = {value: index for index, value in enumerate(self.methods)}
        self.out = bytearray()

    def build(self):
        tables = (
            (self.strings, 4),
            (self.types, 4),
            (self.protos, 12),
            (self.fields, 8),
            (self.methods, 8),
            (self.classes, 32),
        )
        offsets = []
        position = HEADER.size
        for items, size in tables:
            offsets.append(position)
            position += len(items) * size
        self.out = bytearray(position)
        string_ids, type_ids, proto_ids, field_ids, method_ids, class_defs = offsets
        for index, value in enumerate(self.strings):
            data = self.append(uleb(len(value)) + value.encode() + b"\0")
            struct.pack_into("<I", self.out, string_ids + 4 * index, data)
        for index, value in enumerate(self.types):
            struct.pack_into(
                "<I", self.out, type_ids + 4 * index, self.string_index[value]
            )
        for index, proto in enumerate(self.protos):
            result, arguments = split_proto(proto)
            struct.pack_into(
                "<III",
                self.out,
                proto_ids + 12 * index,
                self.string_index[shorty(proto)],
                self.type_index[result],
                self.type_list(arguments),
            )
        for index, (owner, name, type_) in enumerate(self.fields):
            struct.pack_into(
                "<HHI",
                self.out,
                field_ids + 8 * index,
                self.type_index[owner],
                self.type_index[type_],
                self.string_index[name],
            )
        for index, (owner, name, proto) in enumerate(self.methods):
            struct.pack_into(
                "<HHI",
                self.out,
                method_ids + 8 * index,
                self.type_index[owner],
                self.proto_index[proto],
                self.string_index[name],
            )
        for index, definition in enumerate(self.classes):
            struct.pack_into(
                "<8I",
                self.out,
                class_defs + 32 * index,
                self.type_index[definition.name],
                definition.flags,
                (
                    NO_INDEX
                    if definition.superclass is None
                    else self.type_index[definition.superclass]
                ),
                self.type_list(definition.interfaces),
                NO_INDEX,
                0,
                self.class_data(definition),
                self.static_values(definition),
            )
        layout = []
        for (items, _), offset in zip(tables, offsets, strict=True):
            layout += [len(items), offset]
        struct.pack_into(
            HEADER.format,
            self.out,
            0,
            DEX_MAGIC + b"035\0",
            len(self.out),
            HEADER.size,
            ENDIAN_CONSTANT,
            *layout,
        )
        return bytes(self.out)

    def append(self, blob, alignment=1):
        self.out.extend(bytes(-len(self.out) % alignment))
        offset = len(self.out)
        self.out.extend(blob)
        return offset

    def type_list(self, types):
        if not types:
            return 0
        entries = b"".join(struct.pack("<H", self.type_index[type_]) for type_ in types)
        return self.append(struct.pack("<I", len(types)) + entries, 4)

    def class_data(self, definition):
        fields = sorted(
            self.field_index[field_key(definition, member)]
            for member in definition.fields
        )
        methods = {
            self.method_index[(definition.name, method.name, method.proto)]: method
            for method in definition.methods
        }
        direct = sorted(
            index
            for index, method in methods.items()
            if method.flags & (ACC_PRIVATE | ACC_STATIC) or method.name.startswith("<")
        )
        virtual = sorted(set(methods) - set(direct))
        blob = uleb(len(fields)) + uleb(0) + uleb(len(direct)) + uleb(len(virtual))
        previous = 0
        for index in fields:
            blob += uleb(index - previous) + uleb(ACC_PUBLIC | ACC_STATIC)
            previous = index
        for group in (direct, virtual):
            previous = 0
            for index in group:
                method = methods[index]
                code = 0 if method.code is None else self.code_item(method)
                blob += uleb(index - previous) + uleb(method.flags) + uleb(code)
                previous = index
        return self.append(blob)

    def static_values(self, definition):
        members = sorted(
            definition.fields,
            key=lambda member: self.field_index[field_key(definition, member)],
        )
        values = [member[2:] for member in members]
        if not any(values):
            return 0
        encoded = b"".join(
            value[0] if value else bytes((VALUE_NULL,)) for value in values
        )
        return self.append(uleb(len(values)) + encoded)

    def code_item(self, method):
        units = []
        for opcode, *operands in method.code:
            units.extend(self.encode(opcode, operands))
        blob = struct.pack(
            f"<HHHHII{len(units)}H", 1, 0, 0, len(method.tries), 0, len(units), *units
        )
        if method.tries:
            blob += bytes(2 * (len(units) % 2))
            handlers = bytearray(uleb(len(method.tries)))
            items = b""
            for attempt in method.tries:
                items += struct.pack(
                    "<IHH", attempt.start, attempt.count, len(handlers)
                )
                size = len(attempt.catches)
                handlers += sleb(-size if attempt.catch_all else size)
                for catch in attempt.catches:
                    handlers += uleb(self.type_index[catch]) + uleb(0)
                if attempt.catch_all:
                    handlers += uleb(0)
            blob += items + handlers
        return self.append(blob, 4)

    def encode(self, opcode, operands):
        match operands:
            case []:
                return [opcode]
            case [("method", *member)]:
                return [opcode, self.method_index[tuple(member)], 0]
            case [("field", *member)]:
                return [opcode, self.field_index[tuple(member)]]
            case [("type", descriptor)]:
                units = [opcode, self.type_index[descriptor]]
                if opcode in (FILLED_NEW_ARRAY, FILLED_NEW_ARRAY_RANGE):
                    units.append(0)
                return units
            case [tuple(raw)]:
                return [opcode, *raw]


def field_key(definition, member):
    name, type_, *_ = member
    return (definition.name, name, type_)


def build_dex(classes):
    return DexWriter(tuple(classes)).build()


DECOYS = (
    (CONST_WIDE, (INVOKE_STATIC, INVOKE_STATIC, INVOKE_STATIC, INVOKE_STATIC)),
    (PACKED_SWITCH_PAYLOAD, (1, 0, 0, INVOKE_STATIC, 0)),
    (SPARSE_SWITCH_PAYLOAD, (1, INVOKE_STATIC, 0, INVOKE_STATIC, 0)),
    (FILL_ARRAY_DATA_PAYLOAD, (2, 3, 0, INVOKE_STATIC, NEW_INSTANCE, SGET)),
    (FILL_ARRAY_DATA_PAYLOAD, (1, 3, 0, NEW_INSTANCE << 8 | INVOKE_STATIC, SGET)),
)


def write_archive(path, entries):
    with zipfile.ZipFile(path, "w") as archive:
        for name, data in entries.items():
            archive.writestr(name, data)
    return path


class StringPoolWriter:
    def __init__(self):
        self.strings = []

    def index(self, value):
        if value is None:
            return NO_STRING
        if value not in self.strings:
            self.strings.append(value)
        return self.strings.index(value)

    def chunk(self):
        data = bytearray()
        offsets = []
        for value in self.strings:
            encoded = value.encode()
            offsets.append(len(data))
            data += bytes((len(value), len(encoded))) + encoded + b"\0"
        data += bytes(-len(data) % 4)
        header_size = 28
        strings_start = header_size + 4 * len(offsets)
        return (
            struct.pack(
                "<HHIIIIII",
                RES_STRING_POOL_TYPE,
                header_size,
                strings_start + len(data),
                len(offsets),
                0,
                UTF8_FLAG,
                strings_start,
                0,
            )
            + b"".join(struct.pack("<I", offset) for offset in offsets)
            + data
        )


def attribute_value(pool, value):
    if isinstance(value, int):
        return NO_STRING, TYPE_REFERENCE, value
    return pool.index(value), TYPE_STRING, pool.index(value)


def build_xml(element):
    pool = StringPoolWriter()
    body = bytearray()

    def write(tag, attributes, children):
        encoded = []
        for key, value in attributes.items():
            namespace, name = key.split(":") if ":" in key else (None, key)
            raw, value_type, data = attribute_value(pool, value)
            encoded.append(
                struct.pack(
                    "<IIIHBBI",
                    pool.index(ANDROID_NS if namespace else None),
                    pool.index(name),
                    raw,
                    8,
                    0,
                    value_type,
                    data,
                )
            )
        attribute_bytes = b"".join(encoded)
        body.extend(
            struct.pack(
                "<HHIIIIIHHHHHH",
                RES_XML_START_ELEMENT_TYPE,
                16,
                36 + len(attribute_bytes),
                1,
                NO_STRING,
                NO_STRING,
                pool.index(tag),
                20,
                20,
                len(encoded),
                0,
                0,
                0,
            )
            + attribute_bytes
        )
        for child in children:
            write(*child)
        body.extend(
            struct.pack(
                "<HHIIIII",
                RES_XML_END_ELEMENT_TYPE,
                16,
                24,
                1,
                NO_STRING,
                NO_STRING,
                pool.index(tag),
            )
        )

    write(*element)
    pool_chunk = pool.chunk()
    size = 8 + len(pool_chunk) + len(body)
    return struct.pack("<HHI", RES_XML_TYPE, 8, size) + pool_chunk + bytes(body)


def build_resource_table(files):
    pool = StringPoolWriter()
    keys = StringPoolWriter()
    type_names = StringPoolWriter()
    chunks = bytearray()
    for resource, path in files:
        type_id = resource >> 16 & 0xFF
        index = resource & 0xFFFF
        type_names.index(f"type{type_id}")
        offsets = struct.pack(f"<{index + 1}I", *([NO_ENTRY] * index), 0)
        entry = struct.pack(
            "<HHIHBBI", 8, 0, keys.index(path), 8, 0, TYPE_STRING, pool.index(path)
        )
        start = TYPE_HEADER_SIZE + len(offsets)
        chunks += struct.pack(
            "<HHIBBHII",
            RES_TABLE_TYPE_TYPE,
            TYPE_HEADER_SIZE,
            start + len(entry),
            type_id,
            0,
            0,
            index + 1,
            start,
        )
        chunks += struct.pack("<I", CONFIG_SIZE) + bytes(CONFIG_SIZE - 4)
        chunks += offsets + entry
    type_pool = type_names.chunk()
    key_pool = keys.chunk()
    package_body = type_pool + key_pool + chunks
    package = (
        struct.pack(
            "<HHII",
            RES_TABLE_PACKAGE_TYPE,
            PACKAGE_HEADER_SIZE,
            PACKAGE_HEADER_SIZE + len(package_body),
            files[0][0] >> 24,
        )
        + bytes(PACKAGE_NAME_SIZE)
        + struct.pack(
            "<5I",
            PACKAGE_HEADER_SIZE,
            0,
            PACKAGE_HEADER_SIZE + len(type_pool),
            0,
            0,
        )
        + package_body
    )
    strings = pool.chunk()
    size = 12 + len(strings) + len(package)
    return struct.pack("<HHII", RES_TABLE_TYPE, 12, size, 1) + strings + package


LAUNCHER_FILTER = (
    "intent-filter",
    {},
    (
        ("action", {"android:name": "android.intent.action.MAIN"}, ()),
        ("category", {"android:name": "android.intent.category.LAUNCHER"}, ()),
    ),
)
