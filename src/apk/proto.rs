use std::fmt;

const WIRE_VARINT: u64 = 0;
const WIRE_FIXED64: u64 = 1;
const WIRE_LEN: u64 = 2;
const WIRE_START_GROUP: u64 = 3;
const WIRE_END_GROUP: u64 = 4;
const WIRE_FIXED32: u64 = 5;
const MAX_GROUP_DEPTH: usize = 64;
const MAX_VARINT_BYTES: usize = 10;

#[derive(Debug, Default, Clone)]
pub(super) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn key(&mut self, field: u32, wire: u64) {
        put_varint(&mut self.bytes, (u64::from(field) << 3) | wire);
    }

    pub(super) fn varint(&mut self, field: u32, value: u64) -> &mut Self {
        self.key(field, WIRE_VARINT);
        put_varint(&mut self.bytes, value);
        self
    }

    pub(super) fn int32(&mut self, field: u32, value: i32) -> &mut Self {
        self.varint(field, i64::from(value) as u64)
    }

    pub(super) fn int64(&mut self, field: u32, value: i64) -> &mut Self {
        self.varint(field, value as u64)
    }

    pub(super) fn bool(&mut self, field: u32, value: bool) -> &mut Self {
        self.varint(field, u64::from(value))
    }

    pub(super) fn bytes(&mut self, field: u32, value: &[u8]) -> &mut Self {
        self.key(field, WIRE_LEN);
        put_varint(&mut self.bytes, value.len() as u64);
        self.bytes.extend_from_slice(value);
        self
    }

    pub(super) fn string(&mut self, field: u32, value: &str) -> &mut Self {
        self.bytes(field, value.as_bytes())
    }

    pub(super) fn message(&mut self, field: u32, value: &Encoder) -> &mut Self {
        self.bytes(field, &value.bytes)
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
    Group,
}

pub(super) struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn read(&mut self) -> Result<(u32, Value<'a>), ProtoError> {
        let (field, wire) = take_key(&mut self.rest)?;
        let value = match wire {
            WIRE_VARINT => Value::Varint(take_varint(&mut self.rest)?),
            WIRE_FIXED64 => Value::Fixed64(u64::from_le_bytes(take_array(&mut self.rest)?)),
            WIRE_LEN => Value::Bytes(take_len_delimited(&mut self.rest)?),
            WIRE_START_GROUP => {
                skip_group(&mut self.rest, field, 1)?;
                Value::Group
            }
            WIRE_END_GROUP => return Err(ProtoError::UnbalancedGroup),
            WIRE_FIXED32 => Value::Fixed32(u32::from_le_bytes(take_array(&mut self.rest)?)),
            other => return Err(ProtoError::BadWireType(other)),
        };
        Ok((field, value))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), ProtoError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let item = self.read();
        if item.is_err() {
            self.rest = &[];
        }
        Some(item)
    }
}

fn take_key(rest: &mut &[u8]) -> Result<(u32, u64), ProtoError> {
    let key = take_varint(rest)?;
    let field = u32::try_from(key >> 3)
        .ok()
        .filter(|&field| field != 0)
        .ok_or(ProtoError::BadField)?;
    Ok((field, key & 7))
}

fn take_varint(rest: &mut &[u8]) -> Result<u64, ProtoError> {
    let mut value = 0u64;
    for (index, &byte) in rest.iter().enumerate().take(MAX_VARINT_BYTES) {
        let bits = u64::from(byte & 0x7f);
        if index == MAX_VARINT_BYTES - 1 && bits > 1 {
            return Err(ProtoError::VarintOverflow);
        }
        value |= bits << (7 * index);
        if byte & 0x80 == 0 {
            *rest = &rest[index + 1..];
            return Ok(value);
        }
    }
    if rest.len() >= MAX_VARINT_BYTES {
        Err(ProtoError::VarintOverflow)
    } else {
        Err(ProtoError::Truncated)
    }
}

fn take<'a>(rest: &mut &'a [u8], len: usize) -> Result<&'a [u8], ProtoError> {
    if rest.len() < len {
        return Err(ProtoError::Truncated);
    }
    let (value, tail) = rest.split_at(len);
    *rest = tail;
    Ok(value)
}

fn take_array<const N: usize>(rest: &mut &[u8]) -> Result<[u8; N], ProtoError> {
    let bytes = take(rest, N)?;
    Ok(bytes.try_into().expect("take returned exactly N bytes"))
}

fn take_len_delimited<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8], ProtoError> {
    let len = usize::try_from(take_varint(rest)?).map_err(|_| ProtoError::Truncated)?;
    take(rest, len)
}

fn skip_group(rest: &mut &[u8], group: u32, depth: usize) -> Result<(), ProtoError> {
    if depth > MAX_GROUP_DEPTH {
        return Err(ProtoError::TooDeep);
    }
    loop {
        if rest.is_empty() {
            return Err(ProtoError::Truncated);
        }
        let (field, wire) = take_key(rest)?;
        match wire {
            WIRE_VARINT => {
                take_varint(rest)?;
            }
            WIRE_FIXED64 => {
                take(rest, 8)?;
            }
            WIRE_LEN => {
                take_len_delimited(rest)?;
            }
            WIRE_START_GROUP => skip_group(rest, field, depth + 1)?,
            WIRE_END_GROUP if field == group => return Ok(()),
            WIRE_END_GROUP => return Err(ProtoError::UnbalancedGroup),
            WIRE_FIXED32 => {
                take(rest, 4)?;
            }
            other => return Err(ProtoError::BadWireType(other)),
        }
    }
}

fn last_value<'a>(bytes: &'a [u8], field: u32) -> Result<Option<Value<'a>>, ProtoError> {
    let mut found = None;
    for item in Fields::new(bytes) {
        let (number, value) = item?;
        if number == field {
            found = Some(value);
        }
    }
    Ok(found)
}

pub(super) fn bytes_field(bytes: &[u8], field: u32) -> Result<Option<&[u8]>, ProtoError> {
    match last_value(bytes, field)? {
        None => Ok(None),
        Some(Value::Bytes(value)) => Ok(Some(value)),
        Some(_) => Err(ProtoError::WrongType(field)),
    }
}

pub(super) fn string_field(bytes: &[u8], field: u32) -> Result<Option<&str>, ProtoError> {
    bytes_field(bytes, field)?
        .map(|value| std::str::from_utf8(value).map_err(|_| ProtoError::BadUtf8(field)))
        .transpose()
}

pub(super) fn varint_field(bytes: &[u8], field: u32) -> Result<Option<u64>, ProtoError> {
    match last_value(bytes, field)? {
        None => Ok(None),
        Some(Value::Varint(value)) => Ok(Some(value)),
        Some(_) => Err(ProtoError::WrongType(field)),
    }
}

pub(super) fn fixed64_field(bytes: &[u8], field: u32) -> Result<Option<u64>, ProtoError> {
    match last_value(bytes, field)? {
        None => Ok(None),
        Some(Value::Fixed64(value)) => Ok(Some(value)),
        Some(_) => Err(ProtoError::WrongType(field)),
    }
}

pub(super) fn repeated_bytes(bytes: &[u8], field: u32) -> Result<Vec<&[u8]>, ProtoError> {
    let mut values = Vec::new();
    for item in Fields::new(bytes) {
        match item? {
            (number, Value::Bytes(value)) if number == field => values.push(value),
            (number, _) if number == field => return Err(ProtoError::WrongType(field)),
            _ => {}
        }
    }
    Ok(values)
}

pub(super) fn message_at<'a>(
    bytes: &'a [u8],
    path: &[u32],
) -> Result<Option<&'a [u8]>, ProtoError> {
    let mut current = bytes;
    for &field in path {
        match bytes_field(current, field)? {
            Some(inner) => current = inner,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    Truncated,
    VarintOverflow,
    BadField,
    BadWireType(u64),
    UnbalancedGroup,
    TooDeep,
    WrongType(u32),
    BadUtf8(u32),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("protobuf message is truncated"),
            Self::VarintOverflow => f.write_str("protobuf varint is longer than 64 bits"),
            Self::BadField => f.write_str("protobuf field number is invalid"),
            Self::BadWireType(wire) => write!(f, "protobuf wire type {wire} is invalid"),
            Self::UnbalancedGroup => f.write_str("protobuf group is unbalanced"),
            Self::TooDeep => f.write_str("protobuf groups are nested too deeply"),
            Self::WrongType(field) => write!(f, "protobuf field {field} has an unexpected type"),
            Self::BadUtf8(field) => write!(f, "protobuf field {field} is not UTF-8"),
        }
    }
}

impl std::error::Error for ProtoError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_at_every_width() {
        for value in [
            0,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX - 1,
            u64::MAX,
        ] {
            let mut message = Encoder::default();
            message.varint(1, value);
            let bytes = message.into_bytes();
            assert_eq!(varint_field(&bytes, 1).unwrap(), Some(value), "{value}");
        }
    }

    #[test]
    fn negative_int32_uses_the_ten_byte_sign_extended_form() {
        let mut message = Encoder::default();
        message.int32(3, -1);
        let bytes = message.into_bytes();
        assert_eq!(bytes.len(), 1 + MAX_VARINT_BYTES);
        assert_eq!(varint_field(&bytes, 3).unwrap(), Some(u64::MAX));
    }

    #[test]
    fn nested_messages_strings_fixed64_and_repeated_fields_decode() {
        let mut inner = Encoder::default();
        inner.string(1, "config.x86_64").int64(2, 54_000_000);
        let mut other = Encoder::default();
        other.string(1, "config.xxhdpi");
        let mut outer = Encoder::default();
        outer
            .message(15, &inner)
            .message(15, &other)
            .bool(6, true)
            .string(3, "https://play.googleapis.com/x");
        outer.key(7, WIRE_FIXED64);
        outer
            .bytes
            .extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        let bytes = outer.into_bytes();

        let splits = repeated_bytes(&bytes, 15).unwrap();
        assert_eq!(splits.len(), 2);
        assert_eq!(string_field(splits[0], 1).unwrap(), Some("config.x86_64"));
        assert_eq!(varint_field(splits[0], 2).unwrap(), Some(54_000_000));
        assert_eq!(string_field(splits[1], 1).unwrap(), Some("config.xxhdpi"));
        assert_eq!(varint_field(&bytes, 6).unwrap(), Some(1));
        assert_eq!(
            fixed64_field(&bytes, 7).unwrap(),
            Some(0x1122_3344_5566_7788)
        );
        assert_eq!(varint_field(&bytes, 99).unwrap(), None);
        assert_eq!(
            string_field(&bytes, 6),
            Err(ProtoError::WrongType(6)),
            "a varint is not a string"
        );
    }

    #[test]
    fn message_at_walks_a_field_path() {
        let mut details = Encoder::default();
        details.int64(3, 3056).string(4, "2.737.1584");
        let mut document = Encoder::default();
        document.message(1, &details);
        let mut item = Encoder::default();
        item.message(13, &document);
        let mut response = Encoder::default();
        response.message(4, &item);
        let mut payload = Encoder::default();
        payload.message(2, &response);
        let mut wrapper = Encoder::default();
        wrapper.message(1, &payload);
        let bytes = wrapper.into_bytes();

        let app = message_at(&bytes, &[1, 2, 4, 13, 1]).unwrap().unwrap();
        assert_eq!(varint_field(app, 3).unwrap(), Some(3056));
        assert_eq!(string_field(app, 4).unwrap(), Some("2.737.1584"));
        assert_eq!(message_at(&bytes, &[1, 21, 2]).unwrap(), None);
    }

    #[test]
    fn groups_are_skipped_including_nested_groups() {
        let mut bytes = Vec::new();
        put_varint(&mut bytes, (2 << 3) | WIRE_START_GROUP);
        put_varint(&mut bytes, (3 << 3) | WIRE_VARINT);
        put_varint(&mut bytes, 42);
        put_varint(&mut bytes, (5 << 3) | WIRE_START_GROUP);
        put_varint(&mut bytes, (6 << 3) | WIRE_LEN);
        put_varint(&mut bytes, 2);
        bytes.extend_from_slice(b"ok");
        put_varint(&mut bytes, (5 << 3) | WIRE_END_GROUP);
        put_varint(&mut bytes, (2 << 3) | WIRE_END_GROUP);
        let mut tail = Encoder::default();
        tail.string(55, "delivery-token");
        bytes.extend_from_slice(&tail.into_bytes());

        let fields: Vec<_> = Fields::new(&bytes).collect::<Result<_, _>>().unwrap();
        assert_eq!(fields[0], (2, Value::Group));
        assert_eq!(string_field(&bytes, 55).unwrap(), Some("delivery-token"));
    }

    #[test]
    fn malformed_messages_are_typed_errors() {
        assert_eq!(varint_field(&[0x08], 1), Err(ProtoError::Truncated));
        assert_eq!(varint_field(&[0x08, 0x80], 1), Err(ProtoError::Truncated));
        assert_eq!(
            varint_field(
                &[0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
                1
            ),
            Err(ProtoError::VarintOverflow)
        );
        assert_eq!(varint_field(&[0x00, 0x01], 1), Err(ProtoError::BadField));
        assert_eq!(varint_field(&[0x0e], 1), Err(ProtoError::BadWireType(6)));
        assert_eq!(varint_field(&[0x0c], 1), Err(ProtoError::UnbalancedGroup));
        assert_eq!(
            bytes_field(&[0x0a, 0x05, b'a'], 1),
            Err(ProtoError::Truncated)
        );
        assert_eq!(
            varint_field(&[0x0b, 0x08, 0x01], 1),
            Err(ProtoError::Truncated)
        );
        assert_eq!(
            varint_field(&[0x0b, 0x14], 1),
            Err(ProtoError::UnbalancedGroup)
        );
        assert_eq!(
            string_field(&[0x0a, 0x01, 0xff], 1),
            Err(ProtoError::BadUtf8(1))
        );

        let mut deep = Vec::new();
        for _ in 0..=MAX_GROUP_DEPTH {
            put_varint(&mut deep, (1 << 3) | WIRE_START_GROUP);
        }
        assert_eq!(varint_field(&deep, 1), Err(ProtoError::TooDeep));
    }

    #[test]
    fn every_truncation_and_byte_flip_is_total() {
        let mut inner = Encoder::default();
        inner
            .string(1, "config.x86_64")
            .int64(2, 1 << 40)
            .bool(3, true);
        let mut outer = Encoder::default();
        outer.message(15, &inner).int32(1, -5).string(55, "token");
        let bytes = outer.into_bytes();
        for len in 0..=bytes.len() {
            let _ = repeated_bytes(&bytes[..len], 15);
            let _ = message_at(&bytes[..len], &[15, 1]);
        }
        for index in 0..bytes.len() {
            for value in [0x00, 0x7f, 0x80, 0xff] {
                let mut mutated = bytes.clone();
                mutated[index] = value;
                let _ = repeated_bytes(&mutated, 15);
                let _ = string_field(&mutated, 55);
            }
        }
    }
}
