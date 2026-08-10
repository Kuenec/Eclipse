#![forbid(unsafe_code)]

use std::fmt;

const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_TYPE: u16 = 0x0003;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;
const RES_XML_START_NAMESPACE_TYPE: u16 = 0x0100;
const RES_XML_END_NAMESPACE_TYPE: u16 = 0x0101;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_END_ELEMENT_TYPE: u16 = 0x0103;
const RES_XML_CDATA_TYPE: u16 = 0x0104;

const UTF8_FLAG: u32 = 0x0100;

const TYPE_STRING: u8 = 0x03;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;
const TYPE_INT_BOOLEAN: u8 = 0x12;

const CHUNK_HEADER_SIZE: usize = 8;
const STRING_POOL_HEADER_SIZE: usize = 28;
const XML_NODE_HEADER_SIZE: usize = 16;
const ATTRIBUTE_MIN_SIZE: usize = 20;

const NO_STRING: u32 = 0xFFFF_FFFF;

const MAX_DEPTH: usize = 256;

const ANDROID_NS_URI: &str = "http://schemas.android.com/apk/res/android";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AxmlError {
    Truncated,

    BadChunk,

    NoXmlRoot,

    NoStringPool,

    BadString,

    StringIndexOutOfRange,

    Overflow,

    TooDeep,

    UnbalancedElement,

    NoManifestRoot,

    NoPackage,

    NoLauncher,
}

impl fmt::Display for AxmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Truncated => "binary XML ended unexpectedly (truncated structure)",
            Self::BadChunk => "binary XML chunk header is invalid",
            Self::NoXmlRoot => "not a binary AndroidManifest.xml (no RES_XML root chunk)",
            Self::NoStringPool => "binary XML has no string pool chunk",
            Self::BadString => "binary XML string pool entry is malformed",
            Self::StringIndexOutOfRange => "binary XML string index is out of range",
            Self::Overflow => "binary XML offset/length arithmetic overflowed",
            Self::TooDeep => "binary XML element nesting is too deep",
            Self::UnbalancedElement => "binary XML has an unbalanced end-element",
            Self::NoManifestRoot => "binary XML has no root <manifest> element",
            Self::NoPackage => "binary XML <manifest> declares no package",
            Self::NoLauncher => "binary XML has no MAIN/LAUNCHER activity",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for AxmlError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AxmlManifest {
    pub package: String,
    pub launcher_activity: String,
    pub min_sdk: Option<u32>,
    pub target_sdk: Option<u32>,
    pub large_heap: bool,
}

pub(super) fn read_manifest(bytes: &[u8]) -> Result<AxmlManifest, AxmlError> {
    let root = Chunk::parse(bytes, 0)?;
    if root.kind != RES_XML_TYPE {
        return Err(AxmlError::NoXmlRoot);
    }

    let pool = find_string_pool(&root)?;

    walk(&root, &pool)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmlEventKind {
    StartTag(usize),

    EndTag(usize),

    Text(usize),

    StartNamespace(usize),

    EndNamespace(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlElement {
    pub namespace: Option<String>,

    pub name: Option<String>,

    pub attributes: Vec<XmlAttribute>,

    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlAttribute {
    pub namespace: Option<String>,

    pub name: Option<String>,

    pub name_resource: u32,

    pub value_type: u8,

    pub value_data: u32,

    pub value_string: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlText {
    pub text: Option<String>,

    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlNamespace {
    pub prefix: Option<String>,

    pub uri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlDocument {
    pub events: Vec<XmlEventKind>,

    pub elements: Vec<XmlElement>,

    pub texts: Vec<XmlText>,

    pub namespaces: Vec<XmlNamespace>,

    pub strings: Vec<String>,
}

pub fn parse_document(bytes: &[u8]) -> Result<XmlDocument, AxmlError> {
    let root = Chunk::parse(bytes, 0)?;
    if root.kind != RES_XML_TYPE {
        return Err(AxmlError::NoXmlRoot);
    }
    let pool = find_string_pool(&root)?;

    let resource_map = find_resource_map(&root)?;

    let mut doc = XmlDocument {
        events: Vec::new(),
        elements: Vec::new(),
        texts: Vec::new(),
        namespaces: Vec::new(),
        strings: pool.materialize()?,
    };
    let mut depth: usize = 0;

    for child in root.children() {
        let child = child?;
        match child.kind {
            RES_XML_START_ELEMENT_TYPE => {
                depth = depth.checked_add(1).ok_or(AxmlError::Overflow)?;
                if depth > MAX_DEPTH {
                    return Err(AxmlError::TooDeep);
                }
                let element = parse_full_element(&child, &pool, &resource_map)?;
                let idx = doc.elements.len();
                doc.elements.push(element);
                doc.events.push(XmlEventKind::StartTag(idx));
            }
            RES_XML_END_ELEMENT_TYPE => {
                depth = depth.checked_sub(1).ok_or(AxmlError::UnbalancedElement)?;

                let idx = matching_start_index(&doc.events).ok_or(AxmlError::UnbalancedElement)?;
                doc.events.push(XmlEventKind::EndTag(idx));
            }
            RES_XML_CDATA_TYPE => {
                let text = parse_cdata(&child, &pool)?;
                let idx = doc.texts.len();
                doc.texts.push(text);
                doc.events.push(XmlEventKind::Text(idx));
            }
            RES_XML_START_NAMESPACE_TYPE => {
                let ns = parse_namespace(&child, &pool)?;
                let idx = doc.namespaces.len();
                doc.namespaces.push(ns);
                doc.events.push(XmlEventKind::StartNamespace(idx));
            }
            RES_XML_END_NAMESPACE_TYPE => {
                let ns = parse_namespace(&child, &pool)?;
                let idx = doc.namespaces.len();
                doc.namespaces.push(ns);
                doc.events.push(XmlEventKind::EndNamespace(idx));
            }
            _ => {}
        }
    }
    Ok(doc)
}

fn matching_start_index(events: &[XmlEventKind]) -> Option<usize> {
    let mut closed = 0usize;
    for ev in events.iter().rev() {
        match ev {
            XmlEventKind::EndTag(_) => closed += 1,
            XmlEventKind::StartTag(idx) => {
                if closed == 0 {
                    return Some(*idx);
                }
                closed -= 1;
            }
            _ => {}
        }
    }
    None
}

fn parse_full_element(
    chunk: &Chunk,
    pool: &StringPool,
    resource_map: &[u32],
) -> Result<XmlElement, AxmlError> {
    let buf = chunk.bytes;
    let line = read_u32(buf, 8)?;
    let ext = XML_NODE_HEADER_SIZE;
    let ns_ref = read_u32(buf, ext)?;
    let name_ref = read_u32(buf, ext.checked_add(4).ok_or(AxmlError::Overflow)?)?;
    let attribute_start = read_u16(buf, ext.checked_add(8).ok_or(AxmlError::Overflow)?)? as usize;
    let attribute_size = read_u16(buf, ext.checked_add(10).ok_or(AxmlError::Overflow)?)? as usize;
    let attribute_count = read_u16(buf, ext.checked_add(12).ok_or(AxmlError::Overflow)?)? as usize;

    let namespace = pool.get(ns_ref)?;
    let name = pool.get(name_ref)?;

    if attribute_size < ATTRIBUTE_MIN_SIZE && attribute_count > 0 {
        return Err(AxmlError::BadChunk);
    }
    let attrs_base = ext
        .checked_add(attribute_start)
        .ok_or(AxmlError::Overflow)?;
    let array_len = attribute_count
        .checked_mul(attribute_size)
        .ok_or(AxmlError::Overflow)?;
    let attrs_end = attrs_base
        .checked_add(array_len)
        .ok_or(AxmlError::Overflow)?;
    if attrs_end > buf.len() {
        return Err(AxmlError::Truncated);
    }

    let mut attributes = Vec::with_capacity(attribute_count);
    for i in 0..attribute_count {
        let base = attrs_base
            .checked_add(i.checked_mul(attribute_size).ok_or(AxmlError::Overflow)?)
            .ok_or(AxmlError::Overflow)?;
        let a_ns_ref = read_u32(buf, base)?;
        let a_name_ref = read_u32(buf, base.checked_add(4).ok_or(AxmlError::Overflow)?)?;
        let value_type = read_u8(buf, base.checked_add(15).ok_or(AxmlError::Overflow)?)?;
        let value_data = read_u32(buf, base.checked_add(16).ok_or(AxmlError::Overflow)?)?;

        let a_ns = pool.get(a_ns_ref)?;
        let a_name = pool.get(a_name_ref)?;

        let value_string = if value_type == TYPE_STRING {
            pool.get(value_data)?
        } else {
            None
        };

        let name_resource = usize::try_from(a_name_ref)
            .ok()
            .and_then(|i| resource_map.get(i).copied())
            .unwrap_or(0);
        attributes.push(XmlAttribute {
            namespace: a_ns,
            name: a_name,
            name_resource,
            value_type,
            value_data,
            value_string,
        });
    }
    Ok(XmlElement {
        namespace,
        name,
        attributes,
        line,
    })
}

fn parse_cdata(chunk: &Chunk, pool: &StringPool) -> Result<XmlText, AxmlError> {
    let buf = chunk.bytes;
    let line = read_u32(buf, 8)?;

    let data_ref = read_u32(buf, XML_NODE_HEADER_SIZE)?;
    let text = pool.get(data_ref)?;
    Ok(XmlText { text, line })
}

fn parse_namespace(chunk: &Chunk, pool: &StringPool) -> Result<XmlNamespace, AxmlError> {
    let buf = chunk.bytes;

    let prefix_ref = read_u32(buf, XML_NODE_HEADER_SIZE)?;
    let uri_ref = read_u32(
        buf,
        XML_NODE_HEADER_SIZE
            .checked_add(4)
            .ok_or(AxmlError::Overflow)?,
    )?;
    let prefix = pool.get(prefix_ref)?;
    let uri = pool.get(uri_ref)?;
    Ok(XmlNamespace { prefix, uri })
}

struct Chunk<'a> {
    kind: u16,
    header_size: usize,

    bytes: &'a [u8],
}

impl<'a> Chunk<'a> {
    fn parse(buf: &'a [u8], off: usize) -> Result<Self, AxmlError> {
        let kind = read_u16(buf, off)?;
        let header_size = read_u16(buf, off.checked_add(2).ok_or(AxmlError::Overflow)?)? as usize;
        let size = read_u32(buf, off.checked_add(4).ok_or(AxmlError::Overflow)?)? as usize;

        if header_size < CHUNK_HEADER_SIZE || size < header_size {
            return Err(AxmlError::BadChunk);
        }
        let end = off.checked_add(size).ok_or(AxmlError::Overflow)?;
        let bytes = buf.get(off..end).ok_or(AxmlError::Truncated)?;
        Ok(Self {
            kind,
            header_size,
            bytes,
        })
    }

    fn children(&self) -> ChunkIter<'a> {
        ChunkIter {
            buf: self.bytes,
            off: self.header_size,
        }
    }
}

struct ChunkIter<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> Iterator for ChunkIter<'a> {
    type Item = Result<Chunk<'a>, AxmlError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.off >= self.buf.len() {
            return None;
        }
        match Chunk::parse(self.buf, self.off) {
            Ok(chunk) => match self.off.checked_add(chunk.bytes.len()) {
                Some(next) => {
                    self.off = next;
                    Some(Ok(chunk))
                }
                None => Some(Err(AxmlError::Overflow)),
            },
            Err(e) => {
                self.off = self.buf.len();
                Some(Err(e))
            }
        }
    }
}

struct StringPool<'a> {
    chunk: &'a [u8],
    string_count: usize,

    offsets_start: usize,

    data_start: usize,
    is_utf8: bool,
}

impl<'a> StringPool<'a> {
    fn parse(chunk: &Chunk<'a>) -> Result<Self, AxmlError> {
        let buf = chunk.bytes;

        let string_count = read_u32(buf, 8)? as usize;
        let flags = read_u32(buf, 16)?;
        let strings_start = read_u32(buf, 20)? as usize;
        let is_utf8 = flags & UTF8_FLAG != 0;

        let offsets_start = STRING_POOL_HEADER_SIZE;
        let offsets_len = string_count.checked_mul(4).ok_or(AxmlError::Overflow)?;
        let offsets_end = offsets_start
            .checked_add(offsets_len)
            .ok_or(AxmlError::Overflow)?;
        if offsets_end > buf.len() {
            return Err(AxmlError::BadString);
        }

        if strings_start > buf.len() {
            return Err(AxmlError::BadString);
        }
        Ok(Self {
            chunk: buf,
            string_count,
            offsets_start,
            data_start: strings_start,
            is_utf8,
        })
    }

    fn get(&self, index: u32) -> Result<Option<String>, AxmlError> {
        if index == NO_STRING {
            return Ok(None);
        }
        let index = index as usize;
        if index >= self.string_count {
            return Err(AxmlError::StringIndexOutOfRange);
        }
        let off_pos = self
            .offsets_start
            .checked_add(index.checked_mul(4).ok_or(AxmlError::Overflow)?)
            .ok_or(AxmlError::Overflow)?;
        let rel = read_u32(self.chunk, off_pos)? as usize;
        let start = self
            .data_start
            .checked_add(rel)
            .ok_or(AxmlError::Overflow)?;
        let s = if self.is_utf8 {
            decode_utf8(self.chunk, start)?
        } else {
            decode_utf16(self.chunk, start)?
        };
        Ok(Some(s))
    }

    fn materialize(&self) -> Result<Vec<String>, AxmlError> {
        let mut out = Vec::with_capacity(self.string_count);
        for i in 0..self.string_count {
            out.push(self.get(i as u32)?.unwrap_or_default());
        }
        Ok(out)
    }
}

fn decode_utf8(buf: &[u8], start: usize) -> Result<String, AxmlError> {
    let (_, after_char) = read_var_len_u8(buf, start)?;
    let (byte_len, after_len) = read_var_len_u8(buf, after_char)?;
    let end = after_len.checked_add(byte_len).ok_or(AxmlError::Overflow)?;
    let data = buf.get(after_len..end).ok_or(AxmlError::BadString)?;

    std::str::from_utf8(data)
        .map(str::to_owned)
        .map_err(|_| AxmlError::BadString)
}

fn decode_utf16(buf: &[u8], start: usize) -> Result<String, AxmlError> {
    let first = read_u16(buf, start)? as usize;
    let (char_len, data_start) = if first & 0x8000 != 0 {
        let next = read_u16(buf, start.checked_add(2).ok_or(AxmlError::Overflow)?)? as usize;
        let len = ((first & 0x7FFF) << 16) | next;
        (len, start.checked_add(4).ok_or(AxmlError::Overflow)?)
    } else {
        (first, start.checked_add(2).ok_or(AxmlError::Overflow)?)
    };
    let byte_len = char_len.checked_mul(2).ok_or(AxmlError::Overflow)?;
    let end = data_start
        .checked_add(byte_len)
        .ok_or(AxmlError::Overflow)?;
    let data = buf.get(data_start..end).ok_or(AxmlError::BadString)?;

    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

fn read_var_len_u8(buf: &[u8], off: usize) -> Result<(usize, usize), AxmlError> {
    let first = read_u8(buf, off)? as usize;
    if first & 0x80 != 0 {
        let next = read_u8(buf, off.checked_add(1).ok_or(AxmlError::Overflow)?)? as usize;
        Ok((
            ((first & 0x7F) << 8) | next,
            off.checked_add(2).ok_or(AxmlError::Overflow)?,
        ))
    } else {
        Ok((first, off.checked_add(1).ok_or(AxmlError::Overflow)?))
    }
}

fn find_string_pool<'a>(root: &Chunk<'a>) -> Result<StringPool<'a>, AxmlError> {
    for child in root.children() {
        let child = child?;
        if child.kind == RES_STRING_POOL_TYPE {
            return StringPool::parse(&child);
        }
    }
    Err(AxmlError::NoStringPool)
}

fn find_resource_map(root: &Chunk<'_>) -> Result<Vec<u32>, AxmlError> {
    for child in root.children() {
        let child = child?;
        if child.kind != RES_XML_RESOURCE_MAP_TYPE {
            continue;
        }
        let buf = child.bytes;

        let body_start = child.header_size;
        if body_start > buf.len() {
            return Err(AxmlError::Truncated);
        }
        let count = (buf.len() - body_start) / 4;
        let mut ids = Vec::with_capacity(count);
        for i in 0..count {
            let off = body_start
                .checked_add(i.checked_mul(4).ok_or(AxmlError::Overflow)?)
                .ok_or(AxmlError::Overflow)?;
            ids.push(read_u32(buf, off)?);
        }
        return Ok(ids);
    }

    Ok(Vec::new())
}

enum AttrValue {
    Str(String),
    Int(u32),
    Bool(bool),

    Other,
}

struct Attribute {
    ns: Option<String>,

    name: Option<String>,
    value: AttrValue,
}

impl Attribute {
    fn is_android(&self) -> bool {
        self.ns.as_deref() == Some(ANDROID_NS_URI)
    }
}

struct OpenElement {
    tag: Option<String>,

    activity_name: Option<String>,

    saw_main: bool,

    saw_launcher: bool,
}

fn walk(root: &Chunk, pool: &StringPool) -> Result<AxmlManifest, AxmlError> {
    let mut stack: Vec<OpenElement> = Vec::new();

    let mut package: Option<String> = None;
    let mut min_sdk: Option<u32> = None;
    let mut target_sdk: Option<u32> = None;
    let mut large_heap = false;
    let mut launcher: Option<String> = None;
    let mut saw_manifest = false;

    for child in root.children() {
        let child = child?;
        match child.kind {
            RES_XML_START_ELEMENT_TYPE => {
                if stack.len() >= MAX_DEPTH {
                    return Err(AxmlError::TooDeep);
                }
                let (tag, attrs) = parse_start_element(&child, pool)?;
                let tag_str = tag.as_deref();

                match tag_str {
                    Some("manifest") => {
                        saw_manifest = true;
                        if let Some(p) = attr_string(&attrs, Ns::None, "package") {
                            package = Some(p);
                        }
                    }
                    Some("uses-sdk") => {
                        if let Some(v) = attr_int(&attrs, Ns::Android, "minSdkVersion") {
                            min_sdk = Some(v);
                        }
                        if let Some(v) = attr_int(&attrs, Ns::Android, "targetSdkVersion") {
                            target_sdk = Some(v);
                        }
                    }
                    Some("application") => {
                        if let Some(b) = attr_bool(&attrs, Ns::Android, "largeHeap") {
                            large_heap = b;
                        }
                    }
                    _ => {}
                }

                let activity_name = if matches!(tag_str, Some("activity") | Some("activity-alias"))
                {
                    attr_string(&attrs, Ns::Android, "targetActivity")
                        .or_else(|| attr_string(&attrs, Ns::Android, "name"))
                } else {
                    None
                };

                if matches!(tag_str, Some("action") | Some("category")) {
                    if let Some(filter) = stack.last_mut() {
                        if let Some(name) = attr_string(&attrs, Ns::Android, "name") {
                            match (tag_str, name.as_str()) {
                                (Some("action"), "android.intent.action.MAIN") => {
                                    filter.saw_main = true;
                                }
                                (Some("category"), "android.intent.category.LAUNCHER") => {
                                    filter.saw_launcher = true;
                                }
                                _ => {}
                            }
                        }
                    }
                }

                stack.push(OpenElement {
                    tag,
                    activity_name,
                    saw_main: false,
                    saw_launcher: false,
                });
            }
            RES_XML_END_ELEMENT_TYPE => {
                let closing = stack.pop().ok_or(AxmlError::UnbalancedElement)?;

                if closing.tag.as_deref() == Some("intent-filter")
                    && closing.saw_main
                    && closing.saw_launcher
                {
                    if let Some(activity) = stack.last() {
                        if launcher.is_none() {
                            launcher = activity.activity_name.clone();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if !saw_manifest {
        return Err(AxmlError::NoManifestRoot);
    }
    let package = package.ok_or(AxmlError::NoPackage)?;
    let launcher_activity = launcher.ok_or(AxmlError::NoLauncher)?;

    Ok(AxmlManifest {
        package,
        launcher_activity,
        min_sdk,
        target_sdk,
        large_heap,
    })
}

fn parse_start_element(
    chunk: &Chunk,
    pool: &StringPool,
) -> Result<(Option<String>, Vec<Attribute>), AxmlError> {
    let buf = chunk.bytes;

    let ext = XML_NODE_HEADER_SIZE;
    let name_ref = read_u32(buf, ext.checked_add(4).ok_or(AxmlError::Overflow)?)?;
    let attribute_start = read_u16(buf, ext.checked_add(8).ok_or(AxmlError::Overflow)?)? as usize;
    let attribute_size = read_u16(buf, ext.checked_add(10).ok_or(AxmlError::Overflow)?)? as usize;
    let attribute_count = read_u16(buf, ext.checked_add(12).ok_or(AxmlError::Overflow)?)? as usize;

    let tag = pool.get(name_ref)?;

    if attribute_size < ATTRIBUTE_MIN_SIZE && attribute_count > 0 {
        return Err(AxmlError::BadChunk);
    }
    let attrs_base = ext
        .checked_add(attribute_start)
        .ok_or(AxmlError::Overflow)?;
    let array_len = attribute_count
        .checked_mul(attribute_size)
        .ok_or(AxmlError::Overflow)?;
    let attrs_end = attrs_base
        .checked_add(array_len)
        .ok_or(AxmlError::Overflow)?;
    if attrs_end > buf.len() {
        return Err(AxmlError::Truncated);
    }

    let mut attrs = Vec::with_capacity(attribute_count);
    for i in 0..attribute_count {
        let base = attrs_base
            .checked_add(i.checked_mul(attribute_size).ok_or(AxmlError::Overflow)?)
            .ok_or(AxmlError::Overflow)?;

        let ns_ref = read_u32(buf, base)?;
        let attr_name_ref = read_u32(buf, base.checked_add(4).ok_or(AxmlError::Overflow)?)?;
        let data_type = read_u8(buf, base.checked_add(15).ok_or(AxmlError::Overflow)?)?;
        let data = read_u32(buf, base.checked_add(16).ok_or(AxmlError::Overflow)?)?;

        let ns = pool.get(ns_ref)?;
        let name = pool.get(attr_name_ref)?;
        let value = match data_type {
            TYPE_STRING => match pool.get(data)? {
                Some(s) => AttrValue::Str(s),
                None => AttrValue::Other,
            },
            TYPE_INT_DEC | TYPE_INT_HEX => AttrValue::Int(data),
            TYPE_INT_BOOLEAN => AttrValue::Bool(data != 0),
            _ => AttrValue::Other,
        };
        attrs.push(Attribute { ns, name, value });
    }
    Ok((tag, attrs))
}

#[derive(Clone, Copy)]
enum Ns {
    None,

    Android,
}

fn attr_string(attrs: &[Attribute], ns: Ns, name: &str) -> Option<String> {
    for a in attrs {
        if a.name.as_deref() == Some(name) && ns_matches(a, ns) {
            if let AttrValue::Str(s) = &a.value {
                return Some(s.clone());
            }
        }
    }
    None
}

fn attr_int(attrs: &[Attribute], ns: Ns, name: &str) -> Option<u32> {
    for a in attrs {
        if a.name.as_deref() == Some(name) && ns_matches(a, ns) {
            if let AttrValue::Int(v) = &a.value {
                return Some(*v);
            }
        }
    }
    None
}

fn attr_bool(attrs: &[Attribute], ns: Ns, name: &str) -> Option<bool> {
    for a in attrs {
        if a.name.as_deref() == Some(name) && ns_matches(a, ns) {
            if let AttrValue::Bool(b) = &a.value {
                return Some(*b);
            }
        }
    }
    None
}

fn ns_matches(a: &Attribute, ns: Ns) -> bool {
    match ns {
        Ns::Android => a.is_android(),
        Ns::None => a.ns.is_none(),
    }
}

fn read_u8(buf: &[u8], off: usize) -> Result<u8, AxmlError> {
    buf.get(off).copied().ok_or(AxmlError::Truncated)
}

fn read_u16(buf: &[u8], off: usize) -> Result<u16, AxmlError> {
    let end = off.checked_add(2).ok_or(AxmlError::Overflow)?;
    let b = buf.get(off..end).ok_or(AxmlError::Truncated)?;

    let arr: [u8; 2] = b.try_into().map_err(|_| AxmlError::Truncated)?;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32(buf: &[u8], off: usize) -> Result<u32, AxmlError> {
    let end = off.checked_add(4).ok_or(AxmlError::Overflow)?;
    let b = buf.get(off..end).ok_or(AxmlError::Truncated)?;
    let arr: [u8; 4] = b.try_into().map_err(|_| AxmlError::Truncated)?;
    Ok(u32::from_le_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16b(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32b(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn build_utf8_string_pool(strings: &[&str]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        for s in strings {
            offsets.push(data.len() as u32);
            let bytes = s.as_bytes();
            data.push(bytes.len() as u8);
            data.push(bytes.len() as u8);
            data.extend_from_slice(bytes);
            data.push(0);
        }
        let header_size = STRING_POOL_HEADER_SIZE;
        let offsets_len = offsets.len() * 4;
        let strings_start = header_size + offsets_len;
        let total = strings_start + data.len();

        let mut chunk = Vec::new();
        u16b(&mut chunk, RES_STRING_POOL_TYPE);
        u16b(&mut chunk, header_size as u16);
        u32b(&mut chunk, total as u32);
        u32b(&mut chunk, strings.len() as u32);
        u32b(&mut chunk, 0);
        u32b(&mut chunk, UTF8_FLAG);
        u32b(&mut chunk, strings_start as u32);
        u32b(&mut chunk, 0);
        for o in &offsets {
            u32b(&mut chunk, *o);
        }
        chunk.extend_from_slice(&data);
        chunk
    }

    fn build_resource_map(ids: &[u32]) -> Vec<u8> {
        let total = CHUNK_HEADER_SIZE + ids.len() * 4;
        let mut chunk = Vec::new();
        u16b(&mut chunk, RES_XML_RESOURCE_MAP_TYPE);
        u16b(&mut chunk, CHUNK_HEADER_SIZE as u16);
        u32b(&mut chunk, total as u32);
        for id in ids {
            u32b(&mut chunk, *id);
        }
        chunk
    }

    fn build_start_element(
        name_ref: u32,
        attr_name_ref: u32,
        value_type: u8,
        value_data: u32,
    ) -> Vec<u8> {
        let attr_start: u16 = 20;
        let attr_size: u16 = ATTRIBUTE_MIN_SIZE as u16;
        let mut chunk = Vec::new();
        u16b(&mut chunk, RES_XML_START_ELEMENT_TYPE);
        u16b(&mut chunk, XML_NODE_HEADER_SIZE as u16);
        let size_pos = chunk.len();
        u32b(&mut chunk, 0);
        u32b(&mut chunk, 1);
        u32b(&mut chunk, NO_STRING);

        u32b(&mut chunk, NO_STRING);
        u32b(&mut chunk, name_ref);
        u16b(&mut chunk, attr_start);
        u16b(&mut chunk, attr_size);
        u16b(&mut chunk, 1);
        u16b(&mut chunk, 0);
        u16b(&mut chunk, 0);
        u16b(&mut chunk, 0);

        u32b(&mut chunk, NO_STRING);
        u32b(&mut chunk, attr_name_ref);
        u32b(
            &mut chunk,
            if value_type == TYPE_STRING {
                value_data
            } else {
                NO_STRING
            },
        );
        u16b(&mut chunk, 8);
        chunk.push(0);
        chunk.push(value_type);
        u32b(&mut chunk, value_data);
        let total = chunk.len() as u32;
        chunk[size_pos..size_pos + 4].copy_from_slice(&total.to_le_bytes());
        chunk
    }

    fn build_axml(children: &[&[u8]]) -> Vec<u8> {
        let body: usize = children.iter().map(|c| c.len()).sum();
        let total = CHUNK_HEADER_SIZE + body;
        let mut buf = Vec::new();
        u16b(&mut buf, RES_XML_TYPE);
        u16b(&mut buf, CHUNK_HEADER_SIZE as u16);
        u32b(&mut buf, total as u32);
        for c in children {
            buf.extend_from_slice(c);
        }
        buf
    }

    #[test]
    fn parse_document_populates_name_resource_from_resource_map() {
        let pool = build_utf8_string_pool(&["activity", "name", "MyActivity"]);
        let resmap = build_resource_map(&[0x0000_0000, 0x0101_0003, 0x0000_0000]);
        let elem = build_start_element(0, 1, TYPE_STRING, 2);
        let axml = build_axml(&[&pool, &resmap, &elem]);

        let doc = parse_document(&axml).expect("parse minimal axml with resource map");
        let activity = doc
            .elements
            .iter()
            .find(|e| e.name.as_deref() == Some("activity"))
            .expect("activity element");
        let name_attr = activity
            .attributes
            .iter()
            .find(|a| a.name.as_deref() == Some("name"))
            .expect("name attribute");

        assert_eq!(
            name_attr.name_resource, 0x0101_0003,
            "name_resource must come from the resource-map chunk (was always 0 before the fix)"
        );
        assert_eq!(name_attr.value_type, TYPE_STRING);
        assert_eq!(name_attr.value_string.as_deref(), Some("MyActivity"));
    }

    #[test]
    fn parse_document_name_resource_zero_when_no_resource_map() {
        let pool = build_utf8_string_pool(&["activity", "name", "MyActivity"]);
        let elem = build_start_element(0, 1, TYPE_STRING, 2);
        let axml = build_axml(&[&pool, &elem]);

        let doc = parse_document(&axml).expect("parse minimal axml without resource map");
        let activity = doc
            .elements
            .iter()
            .find(|e| e.name.as_deref() == Some("activity"))
            .expect("activity element");
        assert_eq!(activity.attributes[0].name_resource, 0, "absent map ⇒ id 0");
    }

    fn build_pool_header(
        size: u32,
        string_count: u32,
        flags: u32,
        strings_start: u32,
        trailing: &[u8],
    ) -> Vec<u8> {
        let mut c = Vec::new();
        u16b(&mut c, RES_STRING_POOL_TYPE);
        u16b(&mut c, STRING_POOL_HEADER_SIZE as u16);
        u32b(&mut c, size);
        u32b(&mut c, string_count);
        u32b(&mut c, 0);
        u32b(&mut c, flags);
        u32b(&mut c, strings_start);
        u32b(&mut c, 0);
        c.extend_from_slice(trailing);
        c
    }

    #[test]
    fn chunk_header_short_or_overrunning_is_typed_error() {
        let mut b = Vec::new();
        u16b(&mut b, RES_XML_TYPE);
        u16b(&mut b, 4);
        u32b(&mut b, 8);
        assert_eq!(read_manifest(&b), Err(AxmlError::BadChunk));

        let mut b = Vec::new();
        u16b(&mut b, RES_XML_TYPE);
        u16b(&mut b, 8);
        u32b(&mut b, 4);
        assert_eq!(read_manifest(&b), Err(AxmlError::BadChunk));

        let mut b = Vec::new();
        u16b(&mut b, RES_XML_TYPE);
        u16b(&mut b, 8);
        u32b(&mut b, 0xFFFF_FFF0);
        assert_eq!(read_manifest(&b), Err(AxmlError::Truncated));

        let mut b = Vec::new();
        u16b(&mut b, RES_STRING_POOL_TYPE);
        u16b(&mut b, 8);
        u32b(&mut b, 8);
        assert_eq!(read_manifest(&b), Err(AxmlError::NoXmlRoot));
    }

    #[test]
    fn string_pool_count_times_four_overflow_is_typed_error() {
        let pool = build_pool_header(28, 0xFFFF_FFFF, UTF8_FLAG, 28, &[]);
        let axml = build_axml(&[&pool]);

        let err = read_manifest(&axml).expect_err("hostile string_count must fail");
        assert!(
            matches!(err, AxmlError::Overflow | AxmlError::BadString),
            "got {err:?}"
        );
    }

    #[test]
    fn string_pool_offsets_or_strings_start_past_chunk_is_bad_string() {
        let pool = build_pool_header(28, 100, UTF8_FLAG, 28, &[]);
        let axml = build_axml(&[&pool]);
        assert_eq!(read_manifest(&axml), Err(AxmlError::BadString));

        let pool = build_pool_header(28, 0, UTF8_FLAG, 0xFFFF, &[]);
        let axml = build_axml(&[&pool]);
        assert_eq!(read_manifest(&axml), Err(AxmlError::BadString));
    }

    #[test]
    fn utf8_string_byte_len_runs_past_chunk_is_bad_string() {
        let data: &[u8] = &[1, 200, b'A'];
        let strings_start = STRING_POOL_HEADER_SIZE + 4;
        let size = (strings_start + data.len()) as u32;
        let mut pool = build_pool_header(size, 1, UTF8_FLAG, strings_start as u32, &[]);
        u32b(&mut pool, 0);
        pool.extend_from_slice(data);

        let axml = build_axml(&[&pool]);
        let err = parse_document(&axml).expect_err("overrunning utf8 string must fail");
        assert!(
            matches!(err, AxmlError::BadString | AxmlError::Overflow),
            "got {err:?}"
        );
    }

    #[test]
    fn utf16_string_high_bit_length_overflow_is_typed_error() {
        let mut data = Vec::new();
        u16b(&mut data, 0xFFFF);
        u16b(&mut data, 0xFFFF);
        let strings_start = STRING_POOL_HEADER_SIZE + 4;
        let size = (strings_start + data.len()) as u32;
        let mut pool = build_pool_header(size, 1, 0, strings_start as u32, &[]);
        u32b(&mut pool, 0);
        pool.extend_from_slice(&data);
        let axml = build_axml(&[&pool]);
        let err = parse_document(&axml).expect_err("overrunning utf16 string must fail");
        assert!(
            matches!(err, AxmlError::Overflow | AxmlError::BadString),
            "got {err:?}"
        );
    }

    #[test]
    fn element_name_index_out_of_range_is_typed_error() {
        let pool = build_utf8_string_pool(&["only"]);
        let elem = build_start_element(99, 0, TYPE_INT_DEC, 0);
        let axml = build_axml(&[&pool, &elem]);
        assert_eq!(parse_document(&axml), Err(AxmlError::StringIndexOutOfRange));
    }

    #[test]
    fn element_attribute_count_times_size_overflow_is_typed_error() {
        let pool = build_utf8_string_pool(&["el"]);
        let mut elem = Vec::new();
        u16b(&mut elem, RES_XML_START_ELEMENT_TYPE);
        u16b(&mut elem, XML_NODE_HEADER_SIZE as u16);
        let size_pos = elem.len();
        u32b(&mut elem, 0);
        u32b(&mut elem, 1);
        u32b(&mut elem, NO_STRING);

        u32b(&mut elem, NO_STRING);
        u32b(&mut elem, 0);
        u16b(&mut elem, 20);
        u16b(&mut elem, 0xFFFF);
        u16b(&mut elem, 0xFFFF);
        u16b(&mut elem, 0);
        u16b(&mut elem, 0);
        u16b(&mut elem, 0);
        let total = elem.len() as u32;
        elem[size_pos..size_pos + 4].copy_from_slice(&total.to_le_bytes());
        let axml = build_axml(&[&pool, &elem]);

        let err = parse_document(&axml).expect_err("hostile attr count/size must fail");
        assert!(
            matches!(err, AxmlError::Truncated | AxmlError::Overflow),
            "got {err:?}"
        );
    }

    #[test]
    fn unbalanced_end_element_is_typed_error() {
        let pool = build_utf8_string_pool(&["el"]);
        let mut end = Vec::new();
        u16b(&mut end, RES_XML_END_ELEMENT_TYPE);
        u16b(&mut end, XML_NODE_HEADER_SIZE as u16);
        u32b(&mut end, XML_NODE_HEADER_SIZE as u32 + 8);
        u32b(&mut end, 1);
        u32b(&mut end, NO_STRING);
        u32b(&mut end, NO_STRING);
        u32b(&mut end, 0);
        let axml = build_axml(&[&pool, &end]);

        assert_eq!(read_manifest(&axml), Err(AxmlError::UnbalancedElement));

        assert_eq!(parse_document(&axml), Err(AxmlError::UnbalancedElement));
    }

    #[test]
    fn nesting_beyond_max_depth_is_typed_error() {
        let pool = build_utf8_string_pool(&["el"]);
        let mut children: Vec<Vec<u8>> = Vec::new();
        children.push(pool);

        for _ in 0..=MAX_DEPTH {
            children.push(build_start_element(0, 0, TYPE_INT_DEC, 0));
        }
        let refs: Vec<&[u8]> = children.iter().map(|c| c.as_slice()).collect();
        let axml = build_axml(&refs);
        assert_eq!(read_manifest(&axml), Err(AxmlError::TooDeep));
        assert_eq!(parse_document(&axml), Err(AxmlError::TooDeep));
    }

    #[test]
    fn resource_map_shorter_than_attr_index_yields_zero_not_panic() {
        let pool = build_utf8_string_pool(&["activity", "name", "MyActivity"]);
        let resmap = build_resource_map(&[0x0000_0000]);
        let elem = build_start_element(0, 1, TYPE_STRING, 2);
        let axml = build_axml(&[&pool, &resmap, &elem]);
        let doc = parse_document(&axml).expect("short resource map must not panic");
        let activity = doc
            .elements
            .iter()
            .find(|e| e.name.as_deref() == Some("activity"))
            .expect("activity element");
        assert_eq!(
            activity.attributes[0].name_resource, 0,
            "attr index past a short resource map ⇒ 0, never OOB"
        );
    }

    #[test]
    fn child_chunk_with_min_size_does_not_loop_forever() {
        let mut stub = Vec::new();
        u16b(&mut stub, RES_STRING_POOL_TYPE);
        u16b(&mut stub, 8);
        u32b(&mut stub, 8);
        let axml = build_axml(&[&stub, &stub, &stub]);

        let _ = read_manifest(&axml);
    }
}
