import struct
from collections import defaultdict
from dataclasses import dataclass

from dex import decode_mutf8

RES_STRING_POOL_TYPE = 0x0001
RES_TABLE_TYPE = 0x0002
RES_XML_TYPE = 0x0003
RES_XML_START_ELEMENT_TYPE = 0x0102
RES_XML_END_ELEMENT_TYPE = 0x0103
RES_TABLE_PACKAGE_TYPE = 0x0200
RES_TABLE_TYPE_TYPE = 0x0201
UTF8_FLAG = 0x100
TYPE_REFERENCE = 0x01
TYPE_STRING = 0x03
NO_STRING = 0xFFFFFFFF
NO_ENTRY = 0xFFFFFFFF
FLAG_COMPLEX = 0x1
FLAG_COMPACT = 0x8
CHUNK = struct.Struct("<HHI")
STRING_POOL = struct.Struct("<8xI4xII")
ELEMENT = struct.Struct("<4xIHHH")
ATTRIBUTE = struct.Struct("<II7xBI")
PACKAGE_ID = struct.Struct("<8xI")
TYPE_HEADER = struct.Struct("<8xBBxxII")
ENTRY_HEADER = struct.Struct("<HH")
RES_VALUE = struct.Struct("<3xBI")
U16 = struct.Struct("<H")
U32 = struct.Struct("<I")
FILE_PREFIX = "res/"
XML_SUFFIX = ".xml"


class ResourceError(Exception):
    pass


@dataclass(frozen=True)
class CompiledXml:
    strings: frozenset
    references: frozenset


class StringPool:
    def __init__(self, data, offset):
        count, flags, strings_start = STRING_POOL.unpack_from(data, offset)
        _, header_size, _ = CHUNK.unpack_from(data, offset)
        self._data = data
        self._offsets = [
            U32.unpack_from(data, offset + header_size + 4 * index)[0]
            for index in range(count)
        ]
        self._base = offset + strings_start
        self._utf8 = bool(flags & UTF8_FLAG)

    def get(self, index):
        if index == NO_STRING:
            return None
        pos = self._base + self._offsets[index]
        if self._utf8:
            _, pos = self._length8(pos)
            size, pos = self._length8(pos)
            return decode_mutf8(self._data[pos : pos + size])
        size, pos = self._length16(pos)
        return self._data[pos : pos + 2 * size].decode("utf-16-le")

    def strings(self):
        return (self.get(index) for index in range(len(self._offsets)))

    def _length8(self, pos):
        first = self._data[pos]
        if first & 0x80:
            return (first & 0x7F) << 8 | self._data[pos + 1], pos + 2
        return first, pos + 1

    def _length16(self, pos):
        (first,) = U16.unpack_from(self._data, pos)
        if first & 0x8000:
            (second,) = U16.unpack_from(self._data, pos + 2)
            return (first & 0x7FFF) << 16 | second, pos + 4
        return first, pos + 2


def chunks(data, start, end):
    pos = start
    while pos < end:
        kind, _, size = CHUNK.unpack_from(data, pos)
        if size < CHUNK.size or pos + size > end:
            raise ResourceError(f"chunk {kind:#06x} at {pos} has size {size}")
        yield kind, pos
        pos += size


def document_chunks(data, kind):
    found, header_size, size = CHUNK.unpack_from(data, 0)
    if found != kind:
        raise ResourceError(f"the file starts with chunk {found:#06x}, not {kind:#06x}")
    if size > len(data):
        raise ResourceError(f"the file declares {size} bytes but holds {len(data)}")
    return chunks(data, header_size, size)


def start_element(data, pos, pool):
    _, header_size, _ = CHUNK.unpack_from(data, pos)
    extension = pos + header_size
    name, start, size, count = ELEMENT.unpack_from(data, extension)
    attributes = [
        ATTRIBUTE.unpack_from(data, extension + start + size * index)
        for index in range(count)
    ]
    return pool.get(name), attributes


def is_compiled_xml(data):
    return len(data) >= CHUNK.size and CHUNK.unpack_from(data)[0] == RES_XML_TYPE


def read_compiled_xml(data):
    try:
        return parse_compiled_xml(data)
    except (struct.error, IndexError, UnicodeDecodeError) as error:
        raise ResourceError(f"malformed binary XML: {error}") from error


def parse_compiled_xml(data):
    pool = None
    references = set()
    for kind, pos in document_chunks(data, RES_XML_TYPE):
        if kind == RES_STRING_POOL_TYPE and pool is None:
            pool = StringPool(data, pos)
        elif kind == RES_XML_START_ELEMENT_TYPE:
            if pool is None:
                raise ResourceError("an element precedes the string pool")
            _, attributes = start_element(data, pos, pool)
            references.update(
                value
                for _, _, value_type, value in attributes
                if value_type == TYPE_REFERENCE
            )
    strings = frozenset() if pool is None else frozenset(pool.strings())
    return CompiledXml(strings, frozenset(references))


def read_xml_files(data):
    try:
        return parse_xml_files(data)
    except (struct.error, IndexError, UnicodeDecodeError) as error:
        raise ResourceError(f"malformed resource table: {error}") from error


def parse_xml_files(data):
    pool = None
    files = defaultdict(list)
    for kind, pos in document_chunks(data, RES_TABLE_TYPE):
        if kind == RES_STRING_POOL_TYPE and pool is None:
            pool = StringPool(data, pos)
        elif kind == RES_TABLE_PACKAGE_TYPE:
            if pool is None:
                raise ResourceError("a package precedes the string pool")
            for resource, value in package_strings(data, pos):
                path = pool.get(value)
                if path.startswith(FILE_PREFIX) and path.endswith(XML_SUFFIX):
                    files[resource].append(path)
    return files


def package_strings(data, pos):
    (package,) = PACKAGE_ID.unpack_from(data, pos)
    _, header_size, size = CHUNK.unpack_from(data, pos)
    for kind, child in chunks(data, pos + header_size, pos + size):
        if kind == RES_TABLE_TYPE_TYPE:
            type_id, flags, count, start = TYPE_HEADER.unpack_from(data, child)
            if flags:
                raise ResourceError(
                    f"type chunk at {child} has flags {flags:#x}; "
                    "only dense entry offsets are supported"
                )
            _, entries, _ = CHUNK.unpack_from(data, child)
            for index in range(count):
                (offset,) = U32.unpack_from(data, child + entries + 4 * index)
                if offset == NO_ENTRY:
                    continue
                value = entry_string(data, child + start + offset)
                if value is not None:
                    yield package << 24 | type_id << 16 | index, value


def entry_string(data, pos):
    size, flags = ENTRY_HEADER.unpack_from(data, pos)
    if flags & FLAG_COMPACT:
        raise ResourceError(f"entry at {pos} is compact, which is not supported")
    if flags & FLAG_COMPLEX:
        return None
    value_type, value = RES_VALUE.unpack_from(data, pos + size)
    return value if value_type == TYPE_STRING else None
