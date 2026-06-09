//! Binary `AndroidManifest.xml` (AXML) reader — total, never-panicking (component-map B).
//!
//! 2026-06-04: Eclipse owns this parser instead of depending on `axmldecoder` 0.3, which
//! *panics* (via `unwrap`/`assert_eq!`/`unimplemented!`) on adversarial AXML rather than
//! returning an error. Under the release `panic = "abort"` profile (AGENTS.md §2.4) a panic
//! aborts the whole process, so a library parsing an untrusted manifest must not panic
//! (§2.8). This reader treats **every** byte read as fallible: it uses `.get()`, checked
//! integer math, explicit length checks, and a non-recursive (depth-capped) element walk, so
//! it returns a typed [`AxmlError`] for every malformed/short/out-of-bounds input and never
//! panics. `#![forbid(unsafe_code)]` (§2.3) makes the unaligned-load-free `from_le_bytes`
//! style mandatory anyway.
//!
//! Only the five fields Eclipse needs are extracted (package, launcher activity, min/target
//! SDK, `largeHeap`); other chunks and attributes are skipped, not decoded.
//!
//! ## Format
//! AXML is a sequence of little-endian chunks, each led by an 8-byte `ResChunk_header`
//! (`type:u16, headerSize:u16, size:u32`). The file is one outer `RES_XML_TYPE` chunk whose
//! body is: a `RES_STRING_POOL_TYPE` chunk (all element/attribute/value strings), an optional
//! `RES_XML_RESOURCE_MAP_TYPE` chunk, then the flat XML node stream (start/end namespace,
//! start/end element, cdata). Struct layouts follow AOSP
//! `frameworks/base/libs/androidfw/include/androidfw/ResourceTypes.h` (verified 2026-06-04).
//! The string pool is UTF-16 or UTF-8 depending on its `UTF8_FLAG`; both are supported
//! (detect-don't-assume, AGENTS.md §9 — aapt2/bundletool emit UTF-16, older aapt emits UTF-8).

#![forbid(unsafe_code)]

use std::fmt;

// --- ResChunk_header types (the `type` field) ---------------------------------------------
const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_TYPE: u16 = 0x0003;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;
const RES_XML_START_NAMESPACE_TYPE: u16 = 0x0100;
const RES_XML_END_NAMESPACE_TYPE: u16 = 0x0101;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_END_ELEMENT_TYPE: u16 = 0x0103;
const RES_XML_CDATA_TYPE: u16 = 0x0104;

// --- String pool flags ---
const UTF8_FLAG: u32 = 0x0100;

// --- Res_value dataType bytes ---
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;
const TYPE_INT_BOOLEAN: u8 = 0x12;

// --- Fixed struct sizes (bytes) ---
const CHUNK_HEADER_SIZE: usize = 8; // ResChunk_header
const STRING_POOL_HEADER_SIZE: usize = 28; // ResStringPool_header
const XML_NODE_HEADER_SIZE: usize = 16; // ResXMLTree_node (ResChunk_header + lineNumber + comment)
const ATTRIBUTE_MIN_SIZE: usize = 20; // ResXMLTree_attribute fields this reader reads

/// The "no string" sentinel for a `ResStringPool_ref` (`0xFFFFFFFF`).
const NO_STRING: u32 = 0xFFFF_FFFF;

/// Maximum element nesting depth. A real `AndroidManifest.xml` nests only a handful deep
/// (manifest > application > activity > intent-filter > action); this cap (2026-06-04) bounds
/// the explicit stack so a hostile deeply-nested file cannot exhaust memory.
const MAX_DEPTH: usize = 256;

/// The android resource namespace URI; attributes in this namespace are the `android:*` ones.
const ANDROID_NS_URI: &str = "http://schemas.android.com/apk/res/android";

/// Errors from reading a binary `AndroidManifest.xml`.
///
/// Every malformed/short/out-of-bounds input maps to one of these instead of a panic, which
/// is the whole point of owning this reader (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AxmlError {
    /// A fixed-size structure was read past the end of the buffer (or a chunk's bounds).
    Truncated,
    /// A chunk header was invalid (bad `type`, or `size`/`headerSize` out of range / would
    /// not advance the cursor).
    BadChunk,
    /// The file did not start with an outer `RES_XML_TYPE` chunk.
    NoXmlRoot,
    /// No `RES_STRING_POOL_TYPE` chunk was found (names cannot be resolved without it).
    NoStringPool,
    /// A string-pool entry was malformed (bad offset, length overrun, or invalid encoding).
    BadString,
    /// A `ResStringPool_ref` referenced an index outside the string pool.
    StringIndexOutOfRange,
    /// Integer overflow occurred in offset/length arithmetic on hostile input.
    Overflow,
    /// Element nesting exceeded [`MAX_DEPTH`].
    TooDeep,
    /// An end-element appeared with no matching start-element (stack underflow).
    UnbalancedElement,
    /// No root `<manifest>` element was present.
    NoManifestRoot,
    /// The `<manifest>` element declared no `package` attribute.
    NoPackage,
    /// No `<activity>`/`<activity-alias>` with a MAIN + LAUNCHER intent-filter was found.
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

/// The five fields extracted from a binary `AndroidManifest.xml`.
///
/// Mirrors the public `Manifest` fields in the parent module; `min_sdk`/`target_sdk` are
/// `Option` because `<uses-sdk>` may be absent (never fabricated), `large_heap` defaults to
/// `false` (the Android default when `android:largeHeap` is absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AxmlManifest {
    pub package: String,
    pub launcher_activity: String,
    pub min_sdk: Option<u32>,
    pub target_sdk: Option<u32>,
    pub large_heap: bool,
}

/// Read the five manifest fields from binary AXML `bytes`.
///
/// Returns a typed [`AxmlError`] for any malformed input — never panics, never reads out of
/// bounds (the totality guarantee that lets the release profile keep `panic = "abort"`).
pub(super) fn read_manifest(bytes: &[u8]) -> Result<AxmlManifest, AxmlError> {
    // The outer chunk must be RES_XML_TYPE and its body bounds the children we parse.
    let root = Chunk::parse(bytes, 0)?;
    if root.kind != RES_XML_TYPE {
        return Err(AxmlError::NoXmlRoot);
    }

    // First pass: find and parse the string pool (the node stream references it by index).
    let pool = find_string_pool(&root)?;

    // Second pass: walk the node stream, resolving strings on demand.
    walk(&root, &pool)
}

// === General XML event document (for the framework's XmlResourceParser) ===================
//
// 2026-06-05: AOSP's `AssetManager.openXmlBlockAsset` parses a binary-XML asset into a native
// `ResXMLTree`, wraps it as an `XmlBlock`, and exposes an `XmlBlock.Parser` (an
// `XmlResourceParser`/`XmlPullParser`) that the framework walks event-by-event (START_TAG,
// END_TAG, TEXT, …) reading attributes by index. The flat AXML node stream this reader already
// iterates for the manifest IS that event sequence; [`parse_document`] materializes it as an owned,
// fully-string-resolved [`XmlDocument`] so Eclipse's own (non-GTK) XmlBlock/parser natives can walk
// it without re-reading the raw bytes per call. The five-field [`read_manifest`] path is unchanged.

/// XmlPullParser event types, matching `org.xmlpull.v1.XmlPullParser` (stable public constants
/// AOSP's `XmlBlock.Parser.next()` returns). Only the events a binary-XML asset produces are
/// represented; the parser cursor reports `START_DOCUMENT` before the first event and
/// `END_DOCUMENT` after the last (synthesized by the walk natives, not stored as nodes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmlEventKind {
    /// `XmlPullParser.START_TAG` (2): an element opened. Index into [`XmlDocument::elements`].
    StartTag(usize),
    /// `XmlPullParser.END_TAG` (3): an element closed. Index into [`XmlDocument::elements`].
    EndTag(usize),
    /// `XmlPullParser.TEXT` (4): a CDATA/text node. Index into [`XmlDocument::texts`].
    Text(usize),
    /// `XmlPullParser.START_DOCUMENT`-adjacent namespace bookkeeping (`startNamespace`). Carries
    /// the (prefix, uri) string-pool refs resolved to indices in [`XmlDocument::namespaces`].
    StartNamespace(usize),
    /// `endNamespace` bookkeeping (closing a prefix scope).
    EndNamespace(usize),
}

/// One element (start-tag) with its resolved tag, optional namespace, and attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlElement {
    /// Resolved element namespace URI, if any.
    pub namespace: Option<String>,
    /// Resolved element local name (e.g. `manifest`, `activity`).
    pub name: Option<String>,
    /// The element's attributes, in declaration order.
    pub attributes: Vec<XmlAttribute>,
    /// Source line number from the AXML node header (0 if absent) — AOSP's `getLineNumber`.
    pub line: u32,
}

/// One attribute of a start-tag, fully decoded: resolved strings plus the raw `Res_value`
/// type/data the parser natives expose (`getAttributeValueType`/`getAttributeValueData`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlAttribute {
    /// Resolved attribute namespace URI, if any (`android:*` → [`ANDROID_NS_URI`]).
    pub namespace: Option<String>,
    /// Resolved attribute local name (e.g. `name`, `minSdkVersion`).
    pub name: Option<String>,
    /// The attribute's resource id (`ResXMLTree_attribute.name` resolves to a resource id via the
    /// resource-map chunk); `0` when not a framework resource attribute. AOSP's
    /// `getAttributeNameResource`.
    pub name_resource: u32,
    /// The `Res_value.dataType` byte (e.g. [`TYPE_STRING`], [`TYPE_INT_DEC`]).
    pub value_type: u8,
    /// The `Res_value.data` word (a string-pool ref for [`TYPE_STRING`], else the raw int/bool/etc.).
    pub value_data: u32,
    /// The resolved string value when `value_type == TYPE_STRING`, else `None`.
    pub value_string: Option<String>,
}

/// A CDATA/text node with its resolved text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlText {
    /// The resolved text content.
    pub text: Option<String>,
    /// Source line number (0 if absent).
    pub line: u32,
}

/// A resolved namespace declaration (prefix → uri) from a start/end-namespace node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlNamespace {
    /// The namespace prefix (e.g. `android`), if resolvable.
    pub prefix: Option<String>,
    /// The namespace URI (e.g. [`ANDROID_NS_URI`]), if resolvable.
    pub uri: Option<String>,
}

/// A fully-parsed binary-XML document as an event sequence with resolved strings.
///
/// The owned, allocation-once representation the framework's (non-GTK) XmlBlock/parser natives walk
/// (see the module note above). Built by [`parse_document`]; never panics on hostile input (every
/// field comes through the same bounds-checked readers as [`read_manifest`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlDocument {
    /// The flat event stream, in document order.
    pub events: Vec<XmlEventKind>,
    /// Element table referenced by [`XmlEventKind::StartTag`]/[`EndTag`](XmlEventKind::EndTag).
    pub elements: Vec<XmlElement>,
    /// Text table referenced by [`XmlEventKind::Text`].
    pub texts: Vec<XmlText>,
    /// Namespace table referenced by [`XmlEventKind::StartNamespace`]/[`EndNamespace`](XmlEventKind::EndNamespace).
    pub namespaces: Vec<XmlNamespace>,
    /// The fully materialized string pool, indexed by `ResStringPool_ref`. The framework's
    /// `XmlBlock.nativeGetPooledString(idx)` (reached for a `TYPE_STRING` styled attribute whose
    /// `TypedArray` cookie marks it as XmlBlock-owned) returns `strings[idx]`. An index whose pool
    /// entry was the `NO_STRING` sentinel or otherwise empty is an empty `String` (never panics).
    pub strings: Vec<String>,
}

/// Parse binary AXML `bytes` into an owned [`XmlDocument`] event sequence with all strings resolved.
///
/// Returns a typed [`AxmlError`] for any malformed input — never panics, never reads out of bounds
/// (the same totality guarantee as [`read_manifest`]; both go through [`Chunk`]/[`StringPool`] and
/// the checked little-endian readers). Used by Eclipse's own AssetManager XML backing to satisfy
/// `openXmlAssetNative` for `AndroidManifest.xml` (and other XML assets) without the GTK-coupled C
/// asset layer.
pub fn parse_document(bytes: &[u8]) -> Result<XmlDocument, AxmlError> {
    let root = Chunk::parse(bytes, 0)?;
    if root.kind != RES_XML_TYPE {
        return Err(AxmlError::NoXmlRoot);
    }
    let pool = find_string_pool(&root)?;
    // The optional resource-map chunk maps attribute name string-index → framework resource id, so
    // parse_full_element can populate XmlAttribute.name_resource (the id the framework's
    // retrieveAttributes matches against). Absent ⇒ empty ⇒ ids stay 0 (never fabricated).
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
                // Cap nesting like the manifest walk so a hostile deeply-nested asset cannot grow
                // the events/elements vectors without bound via the start/end pairing.
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
                // The end-tag mirrors the most recent still-open start-tag's element index. AOSP's
                // parser reports the same element (name/ns) for END_TAG; recover it by scanning the
                // event stack for the matching unclosed StartTag.
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
            _ => {} // string pool / resource map / unknown chunks are skipped by size.
        }
    }
    Ok(doc)
}

/// Find the element index of the innermost still-open start-tag, given the events so far (the last
/// event being the end-tag we are pairing). Counts start/end tags from the end to find the match.
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

/// Parse a start-element chunk into a fully-resolved [`XmlElement`] (tag + ns + all attributes with
/// raw type/data). Mirrors [`parse_start_element`]'s bounds checks but keeps every attribute and its
/// `Res_value` type/data so the parser natives can report them.
fn parse_full_element(
    chunk: &Chunk,
    pool: &StringPool,
    resource_map: &[u32],
) -> Result<XmlElement, AxmlError> {
    let buf = chunk.bytes;
    let line = read_u32(buf, 8)?; // ResXMLTree_node.lineNumber (offset 8 within the node).
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
        // For TYPE_STRING the data word is itself a string-pool ref; resolve it for value_string.
        let value_string = if value_type == TYPE_STRING {
            pool.get(value_data)?
        } else {
            None
        };
        // 2026-06-05: the attribute's framework resource id comes from the resource-map chunk,
        // indexed by the attribute NAME string index (`a_name_ref`): `resource_map[a_name_ref]`. An
        // index past the (possibly absent/short) map ⇒ 0 ("not a framework resource attribute"),
        // never a fabricated id. This is what the framework's retrieveAttributes matches against.
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

/// Parse a CDATA chunk into a resolved [`XmlText`].
fn parse_cdata(chunk: &Chunk, pool: &StringPool) -> Result<XmlText, AxmlError> {
    let buf = chunk.bytes;
    let line = read_u32(buf, 8)?;
    // ResXMLTree_cdataExt follows the 16-byte node header: `data` (ResStringPool_ref) then a
    // Res_value; the resolved text comes from the `data` string-pool ref.
    let data_ref = read_u32(buf, XML_NODE_HEADER_SIZE)?;
    let text = pool.get(data_ref)?;
    Ok(XmlText { text, line })
}

/// Parse a start/end-namespace chunk into a resolved [`XmlNamespace`].
fn parse_namespace(chunk: &Chunk, pool: &StringPool) -> Result<XmlNamespace, AxmlError> {
    let buf = chunk.bytes;
    // ResXMLTree_namespaceExt follows the 16-byte node header: `prefix` then `uri`
    // (both ResStringPool_ref).
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

/// A validated chunk view: its bounds are guaranteed inside `buf`, and `body()` returns only
/// the in-bounds body slice. Constructing one is the single place chunk-bound invariants are
/// established, so callers can slice freely within `body()` thereafter.
struct Chunk<'a> {
    kind: u16,
    header_size: usize,
    /// The chunk's full bytes (header + body), exactly `size` long, inside `buf`.
    bytes: &'a [u8],
}

impl<'a> Chunk<'a> {
    /// Parse the chunk starting at `off` in `buf`, validating all bounds.
    fn parse(buf: &'a [u8], off: usize) -> Result<Self, AxmlError> {
        let kind = read_u16(buf, off)?;
        let header_size = read_u16(buf, off.checked_add(2).ok_or(AxmlError::Overflow)?)? as usize;
        let size = read_u32(buf, off.checked_add(4).ok_or(AxmlError::Overflow)?)? as usize;
        // size must cover the header, headerSize must cover the 8-byte ResChunk_header, and the
        // whole chunk must fit in buf. size>=headerSize>=8 also guarantees forward progress
        // when advancing by size (item 11 in the spec's panic-risk list).
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

    /// Iterate this chunk's child chunks (parsing each within this chunk's body region).
    fn children(&self) -> ChunkIter<'a> {
        ChunkIter {
            buf: self.bytes,
            off: self.header_size,
        }
    }
}

/// Iterator over child chunks within a parent chunk's bytes, advancing by each child's `size`.
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
            Ok(chunk) => {
                // size >= 8 (validated in parse) guarantees off strictly advances → no infinite
                // loop even on a chunk that declares the minimum legal size.
                match self.off.checked_add(chunk.bytes.len()) {
                    Some(next) => {
                        self.off = next;
                        Some(Ok(chunk))
                    }
                    None => Some(Err(AxmlError::Overflow)),
                }
            }
            Err(e) => {
                // Stop iterating after an error (also prevents re-yielding from a stuck offset).
                self.off = self.buf.len();
                Some(Err(e))
            }
        }
    }
}

/// A validated, lazily-decoded string pool. Holds the chunk bytes plus the parsed header
/// fields; individual strings are decoded on demand by [`StringPool::get`].
struct StringPool<'a> {
    /// The whole string-pool chunk bytes.
    chunk: &'a [u8],
    string_count: usize,
    /// Offset array start (relative to `chunk`).
    offsets_start: usize,
    /// String data start (relative to `chunk`).
    data_start: usize,
    is_utf8: bool,
}

impl<'a> StringPool<'a> {
    /// Parse and validate a string-pool chunk.
    fn parse(chunk: &Chunk<'a>) -> Result<Self, AxmlError> {
        let buf = chunk.bytes;
        // ResStringPool_header fields (offsets within the chunk).
        let string_count = read_u32(buf, 8)? as usize;
        let flags = read_u32(buf, 16)?;
        let strings_start = read_u32(buf, 20)? as usize;
        let is_utf8 = flags & UTF8_FLAG != 0;

        // The offset array follows the 28-byte header; require it to fit in the chunk.
        let offsets_start = STRING_POOL_HEADER_SIZE;
        let offsets_len = string_count.checked_mul(4).ok_or(AxmlError::Overflow)?;
        let offsets_end = offsets_start
            .checked_add(offsets_len)
            .ok_or(AxmlError::Overflow)?;
        if offsets_end > buf.len() {
            return Err(AxmlError::BadString);
        }
        // strings_start is relative to the chunk; it must be inside the chunk.
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

    /// Resolve a `ResStringPool_ref`: `NO_STRING` and out-of-range indices yield `None`-style
    /// outcomes via the caller; an in-range index whose bytes are malformed is an error.
    ///
    /// Returns `Ok(None)` for the sentinel (no string) and `Err(StringIndexOutOfRange)` for an
    /// index past the pool, so callers can distinguish "absent" from "corrupt".
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

    /// Materialize the whole pool into a `Vec<String>` indexed by `ResStringPool_ref`, so the parsed
    /// document can answer `XmlBlock.nativeGetPooledString(idx)` in O(1) without re-walking the chunk.
    /// A `NO_STRING`/empty entry becomes an empty `String` (never panics); a truly corrupt in-range
    /// entry is a typed `Err` (same totality as [`get`](Self::get)).
    fn materialize(&self) -> Result<Vec<String>, AxmlError> {
        let mut out = Vec::with_capacity(self.string_count);
        for i in 0..self.string_count {
            out.push(self.get(i as u32)?.unwrap_or_default());
        }
        Ok(out)
    }
}

/// Decode a UTF-8 length-prefixed pool string at `start`.
///
/// Layout: a *character* length, then a *byte* length; each is one byte, or — if the high bit
/// `0x80` is set — two bytes `((first & 0x7F) << 8) | next`. The *byte length* is authoritative
/// for the slice; a trailing NUL follows the data but is not part of it and is not consumed here.
fn decode_utf8(buf: &[u8], start: usize) -> Result<String, AxmlError> {
    // Skip the character-count field, then read the byte-count field.
    let (_, after_char) = read_var_len_u8(buf, start)?;
    let (byte_len, after_len) = read_var_len_u8(buf, after_char)?;
    let end = after_len.checked_add(byte_len).ok_or(AxmlError::Overflow)?;
    let data = buf.get(after_len..end).ok_or(AxmlError::BadString)?;
    // Validate UTF-8 in place (no intermediate Vec copy) and allocate the String once (§2.6).
    std::str::from_utf8(data)
        .map(str::to_owned)
        .map_err(|_| AxmlError::BadString)
}

/// Decode a UTF-16LE length-prefixed pool string at `start`.
///
/// Layout: a `u16` *character* count; if its high bit `0x8000` is set it is a 31-bit length
/// `((first & 0x7FFF) << 16) | next_u16`. Then `count` UTF-16LE units (`2*count` bytes),
/// NUL-terminated.
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
    // try_into on a 2-byte chunk can't fail (chunks_exact yields exactly 2); decode lossily so
    // an unpaired surrogate cannot error out a field we don't even read.
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

/// Read a variable-length `u8`/`u16` length field (UTF-8 pool form), returning the value and
/// the offset just past it.
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

/// Find and parse the string-pool chunk among the XML root's children.
fn find_string_pool<'a>(root: &Chunk<'a>) -> Result<StringPool<'a>, AxmlError> {
    for child in root.children() {
        let child = child?;
        if child.kind == RES_STRING_POOL_TYPE {
            return StringPool::parse(&child);
        }
    }
    Err(AxmlError::NoStringPool)
}

/// Decode the optional `RES_XML_RESOURCE_MAP_TYPE` chunk into the resource-id array.
///
/// 2026-06-05: aapt does not store an attribute's framework resource id in the AXML node; instead it
/// emits one `RES_XML_RESOURCE_MAP_TYPE` chunk whose body is a flat `u32[]` of resource ids, parallel
/// to the string pool: the attribute whose **name** is string index `i` has resource id
/// `resource_map[i]` (AOSP `ResXMLParser::getAttributeNameResID` does exactly this lookup). The chunk
/// is optional — a manifest may omit it, in which case attribute resource ids are simply unknown
/// (`0`), never fabricated. Returns an empty `Vec` when the chunk is absent.
///
/// Each id is read with the same bounds-checked [`read_u32`] as everything else, so a truncated or
/// malformed chunk yields a typed [`AxmlError`], never a panic (totality preserved). The number of
/// ids is `(chunk size - headerSize) / 4`, clamped to what actually fits in the chunk body.
fn find_resource_map(root: &Chunk<'_>) -> Result<Vec<u32>, AxmlError> {
    for child in root.children() {
        let child = child?;
        if child.kind != RES_XML_RESOURCE_MAP_TYPE {
            continue;
        }
        let buf = child.bytes;
        // The id array is the chunk body (after headerSize). Each entry is a 4-byte resource id.
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
    // No resource-map chunk: attribute resource ids are unknown (0), never fabricated.
    Ok(Vec::new())
}

/// A typed attribute value, decoded from a `Res_value` for the fields we care about.
enum AttrValue {
    Str(String),
    Int(u32),
    Bool(bool),
    /// A value type we don't use (reference, null, …) — kept so matching can ignore it.
    Other,
}

/// One parsed attribute of a start-element.
struct Attribute {
    /// Resolved namespace URI, if any (`android:*` attributes resolve to [`ANDROID_NS_URI`]).
    ns: Option<String>,
    /// Resolved attribute name (e.g. `package`, `minSdkVersion`).
    name: Option<String>,
    value: AttrValue,
}

impl Attribute {
    /// `true` when this attribute is in the android namespace.
    fn is_android(&self) -> bool {
        self.ns.as_deref() == Some(ANDROID_NS_URI)
    }
}

/// Per-open-element state pushed on the walk stack.
struct OpenElement {
    /// The element tag (e.g. `manifest`, `activity`).
    tag: Option<String>,
    /// For an `<activity>`/`<activity-alias>`: its launch-target name, captured at open time.
    activity_name: Option<String>,
    /// For an `<intent-filter>`: whether `action MAIN` was seen among its children so far.
    saw_main: bool,
    /// For an `<intent-filter>`: whether `category LAUNCHER` was seen so far.
    saw_launcher: bool,
}

/// Walk the flat node stream and extract the five fields with an explicit (non-recursive)
/// depth-capped stack.
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

                // Capture an activity's launch target at open time (used if its intent-filter
                // turns out to be the MAIN/LAUNCHER one). targetActivity wins for aliases.
                let activity_name = if matches!(tag_str, Some("activity") | Some("activity-alias"))
                {
                    attr_string(&attrs, Ns::Android, "targetActivity")
                        .or_else(|| attr_string(&attrs, Ns::Android, "name"))
                } else {
                    None
                };

                // For intent-filter children (action/category), record MAIN/LAUNCHER on the
                // enclosing filter's stack entry.
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
                // On an intent-filter close, if it had MAIN+LAUNCHER, the enclosing activity
                // (now the stack top) is the launcher; remember its captured name once.
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
            _ => {} // namespace/cdata/resource-map/etc. are skipped by size.
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

/// Parse a start-element chunk into its tag and attribute list.
fn parse_start_element(
    chunk: &Chunk,
    pool: &StringPool,
) -> Result<(Option<String>, Vec<Attribute>), AxmlError> {
    let buf = chunk.bytes;
    // ResXMLTree_node is 16 bytes; ResXMLTree_attrExt follows it.
    // attrExt: ns(+0) name(+4) attributeStart(+8 u16) attributeSize(+10 u16) attributeCount(+12 u16)
    let ext = XML_NODE_HEADER_SIZE;
    let name_ref = read_u32(buf, ext.checked_add(4).ok_or(AxmlError::Overflow)?)?;
    let attribute_start = read_u16(buf, ext.checked_add(8).ok_or(AxmlError::Overflow)?)? as usize;
    let attribute_size = read_u16(buf, ext.checked_add(10).ok_or(AxmlError::Overflow)?)? as usize;
    let attribute_count = read_u16(buf, ext.checked_add(12).ok_or(AxmlError::Overflow)?)? as usize;

    let tag = pool.get(name_ref)?;

    // attributeStart is measured from the start of the attrExt struct (i.e. from `ext`).
    // Require each attribute to carry at least the fields we read, and the whole array to fit.
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
        // ResXMLTree_attribute: ns(+0) name(+4) rawValue(+8) Res_value{ size(+12 u16),
        // res0(+14 u8), dataType(+15 u8), data(+16 u32) }.
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

/// Namespace constraint for an attribute lookup (clearer than a bare `Option<()>` flag).
#[derive(Clone, Copy)]
enum Ns {
    /// The attribute must have **no** namespace (e.g. the root `package`).
    None,
    /// The attribute must be in the android namespace (e.g. `android:name`).
    Android,
}

/// Find an attribute by name + namespace constraint and return its string value.
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

/// Find an integer attribute (`TYPE_INT_DEC`/`HEX`) by name + namespace constraint.
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

/// Find a boolean attribute (`TYPE_INT_BOOLEAN`) by name + namespace constraint.
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

/// Namespace gate for [`Ns`].
fn ns_matches(a: &Attribute, ns: Ns) -> bool {
    match ns {
        Ns::Android => a.is_android(),
        Ns::None => a.ns.is_none(),
    }
}

// --- Bounds-checked little-endian readers (the only places raw bytes become integers) ------

fn read_u8(buf: &[u8], off: usize) -> Result<u8, AxmlError> {
    buf.get(off).copied().ok_or(AxmlError::Truncated)
}

fn read_u16(buf: &[u8], off: usize) -> Result<u16, AxmlError> {
    let end = off.checked_add(2).ok_or(AxmlError::Overflow)?;
    let b = buf.get(off..end).ok_or(AxmlError::Truncated)?;
    // try_into on a 2-byte slice is infallible by construction; map for totality regardless.
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

    // 2026-06-05: regression guard tied to the confirmed root cause that broke retrieveAttributes —
    // attribute resource ids (XmlAttribute.name_resource) were always 0 because the
    // RES_XML_RESOURCE_MAP_TYPE chunk was not decoded, so the framework's retrieveAttributes (which
    // matches requested ids against name_resource) found nothing and `<activity>`'s android:name was
    // unreadable. This builds a minimal in-memory AXML carrying a resource-map chunk and asserts
    // parse_document now populates name_resource from it (index = the attribute NAME string index).
    //
    // Layout helpers: build little-endian chunks by hand per the format in this module's docs.

    fn u16b(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32b(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Build a UTF-8 string-pool chunk for the given strings (each < 128 bytes so the length fields
    /// are single-byte), returning the chunk bytes. The string order defines each string's index.
    fn build_utf8_string_pool(strings: &[&str]) -> Vec<u8> {
        // Encode the string data (offset table is relative to data_start).
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        for s in strings {
            offsets.push(data.len() as u32);
            let bytes = s.as_bytes();
            data.push(bytes.len() as u8); // char count (ASCII: == byte count)
            data.push(bytes.len() as u8); // byte count
            data.extend_from_slice(bytes);
            data.push(0); // trailing NUL
        }
        let header_size = STRING_POOL_HEADER_SIZE; // 28
        let offsets_len = offsets.len() * 4;
        let strings_start = header_size + offsets_len; // relative to chunk start
        let total = strings_start + data.len();

        let mut chunk = Vec::new();
        u16b(&mut chunk, RES_STRING_POOL_TYPE);
        u16b(&mut chunk, header_size as u16);
        u32b(&mut chunk, total as u32);
        u32b(&mut chunk, strings.len() as u32); // stringCount
        u32b(&mut chunk, 0); // styleCount
        u32b(&mut chunk, UTF8_FLAG); // flags: UTF-8
        u32b(&mut chunk, strings_start as u32); // stringsStart
        u32b(&mut chunk, 0); // stylesStart
        for o in &offsets {
            u32b(&mut chunk, *o);
        }
        chunk.extend_from_slice(&data);
        chunk
    }

    /// Build a RES_XML_RESOURCE_MAP_TYPE chunk from the given resource-id array (parallel to the
    /// string pool by index).
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

    /// Build a RES_XML_START_ELEMENT_TYPE chunk with one attribute (the layout this reader parses).
    fn build_start_element(
        name_ref: u32,
        attr_name_ref: u32,
        value_type: u8,
        value_data: u32,
    ) -> Vec<u8> {
        // node header (16) + element ext + 1 attribute (20).
        let attr_start: u16 = 20; // attrs begin 20 bytes after the ext (node offset 16+20 = 36).
        let attr_size: u16 = ATTRIBUTE_MIN_SIZE as u16; // 20
        let mut chunk = Vec::new();
        u16b(&mut chunk, RES_XML_START_ELEMENT_TYPE);
        u16b(&mut chunk, XML_NODE_HEADER_SIZE as u16); // headerSize 16
        let size_pos = chunk.len();
        u32b(&mut chunk, 0); // size, patched below
        u32b(&mut chunk, 1); // lineNumber
        u32b(&mut chunk, NO_STRING); // comment ref
                                     // --- ResXMLTree_attrExt (the "ext" the reader reads from offset 16) ---
        u32b(&mut chunk, NO_STRING); // ns ref (no namespace)
        u32b(&mut chunk, name_ref); // element name ref
        u16b(&mut chunk, attr_start); // attributeStart
        u16b(&mut chunk, attr_size); // attributeSize
        u16b(&mut chunk, 1); // attributeCount
        u16b(&mut chunk, 0); // idIndex
        u16b(&mut chunk, 0); // classIndex
        u16b(&mut chunk, 0); // styleIndex
                             // attrExt is 20 bytes (ns+name+attrStart+attrSize+attrCount+idIndex+classIndex+styleIndex),
                             // so with attr_start=20 the attributes follow immediately (node offset 16+20 = 36).
                             // --- one ResXMLTree_attribute (20 bytes) ---
        u32b(&mut chunk, NO_STRING); // attr ns ref
        u32b(&mut chunk, attr_name_ref); // attr name ref (index into string pool)
        u32b(
            &mut chunk,
            if value_type == TYPE_STRING {
                value_data
            } else {
                NO_STRING
            },
        ); // rawValue ref
        u16b(&mut chunk, 8); // Res_value.size
        chunk.push(0); // Res_value.res0
        chunk.push(value_type); // Res_value.dataType (offset 15)
        u32b(&mut chunk, value_data); // Res_value.data (offset 16)
        let total = chunk.len() as u32;
        chunk[size_pos..size_pos + 4].copy_from_slice(&total.to_le_bytes());
        chunk
    }

    /// Wrap children in a RES_XML_TYPE root chunk.
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
        // Strings: [0]="activity", [1]="name", [2]="MyActivity". The resource map gives string index
        // 1 ("name") the framework id android.R.attr.name = 0x01010003 (what retrieveAttributes
        // requests). The attribute's name_ref is 1, so name_resource must resolve to 0x01010003.
        let pool = build_utf8_string_pool(&["activity", "name", "MyActivity"]);
        let resmap = build_resource_map(&[0x0000_0000, 0x0101_0003, 0x0000_0000]);
        let elem = build_start_element(0, 1, TYPE_STRING, 2); // name="activity", attr name idx 1, value idx 2
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
        // The root-cause fix: name_resource is now the resource-map id, not 0.
        assert_eq!(
            name_attr.name_resource, 0x0101_0003,
            "name_resource must come from the resource-map chunk (was always 0 before the fix)"
        );
        assert_eq!(name_attr.value_type, TYPE_STRING);
        assert_eq!(name_attr.value_string.as_deref(), Some("MyActivity"));
    }

    #[test]
    fn parse_document_name_resource_zero_when_no_resource_map() {
        // No resource-map chunk ⇒ name_resource stays 0 (never fabricated) — the documented absent
        // case. Same element, but no resource map between the pool and the element.
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

    // === Adversarial robustness pass (2026-06-05) ==========================================
    // Hand-crafted hostile AXML byte buffers: every one must yield a typed `AxmlError`
    // (Ok or specific Err), NEVER a panic / integer overflow / OOB slice / unbounded alloc.
    // These are direct negative tests (not byte-flip fuzz) that drive the chunk header, string
    // pool, element/attribute, and resource-map readers into their failure branches on purpose.
    // `#![forbid(unsafe_code)]` + debug `overflow-checks` mean any wrapping `+`/`*` would panic
    // here, so the tests completing is proof the checked-math/bounds discipline holds.

    /// Build a fully hand-controlled `RES_STRING_POOL_TYPE` chunk header with arbitrary field
    /// values (no trailing data), so a test can inject hostile `string_count`/`flags`/
    /// `strings_start`/declared-`size` values the valid builder never would.
    fn build_pool_header(
        size: u32,
        string_count: u32,
        flags: u32,
        strings_start: u32,
        trailing: &[u8],
    ) -> Vec<u8> {
        let mut c = Vec::new();
        u16b(&mut c, RES_STRING_POOL_TYPE);
        u16b(&mut c, STRING_POOL_HEADER_SIZE as u16); // headerSize 28
        u32b(&mut c, size);
        u32b(&mut c, string_count);
        u32b(&mut c, 0); // styleCount
        u32b(&mut c, flags);
        u32b(&mut c, strings_start);
        u32b(&mut c, 0); // stylesStart
        c.extend_from_slice(trailing);
        c
    }

    #[test]
    fn chunk_header_short_or_overrunning_is_typed_error() {
        // headerSize < 8 (cannot even cover ResChunk_header) → BadChunk.
        let mut b = Vec::new();
        u16b(&mut b, RES_XML_TYPE);
        u16b(&mut b, 4); // headerSize too small
        u32b(&mut b, 8);
        assert_eq!(read_manifest(&b), Err(AxmlError::BadChunk));

        // size < headerSize → BadChunk.
        let mut b = Vec::new();
        u16b(&mut b, RES_XML_TYPE);
        u16b(&mut b, 8);
        u32b(&mut b, 4); // size < headerSize
        assert_eq!(read_manifest(&b), Err(AxmlError::BadChunk));

        // size declares more bytes than the buffer holds → Truncated.
        let mut b = Vec::new();
        u16b(&mut b, RES_XML_TYPE);
        u16b(&mut b, 8);
        u32b(&mut b, 0xFFFF_FFF0); // size far past EOF
        assert_eq!(read_manifest(&b), Err(AxmlError::Truncated));

        // A non-RES_XML root chunk → NoXmlRoot (not a panic).
        let mut b = Vec::new();
        u16b(&mut b, RES_STRING_POOL_TYPE);
        u16b(&mut b, 8);
        u32b(&mut b, 8);
        assert_eq!(read_manifest(&b), Err(AxmlError::NoXmlRoot));
    }

    #[test]
    fn string_pool_count_times_four_overflow_is_typed_error() {
        // string_count whose *4 offset array overflows usize must be Overflow, never a wrap/OOB.
        let pool = build_pool_header(28, 0xFFFF_FFFF, UTF8_FLAG, 28, &[]);
        let axml = build_axml(&[&pool]);
        // find_string_pool → StringPool::parse: count*4 overflows on 32-bit, or offsets_end past
        // buf on 64-bit → BadString. Either way a typed error, never a panic.
        let err = read_manifest(&axml).expect_err("hostile string_count must fail");
        assert!(
            matches!(err, AxmlError::Overflow | AxmlError::BadString),
            "got {err:?}"
        );
    }

    #[test]
    fn string_pool_offsets_or_strings_start_past_chunk_is_bad_string() {
        // offset array claims more entries (100*4 bytes) than fit in the 28-byte chunk → BadString
        // raised inside StringPool::parse before any string is read.
        let pool = build_pool_header(28, 100, UTF8_FLAG, 28, &[]);
        let axml = build_axml(&[&pool]);
        assert_eq!(read_manifest(&axml), Err(AxmlError::BadString));

        // strings_start past the chunk end → BadString.
        let pool = build_pool_header(28, 0, UTF8_FLAG, 0xFFFF, &[]);
        let axml = build_axml(&[&pool]);
        assert_eq!(read_manifest(&axml), Err(AxmlError::BadString));
    }

    #[test]
    fn utf8_string_byte_len_runs_past_chunk_is_bad_string() {
        // A UTF-8 pool with one string whose byte-length field claims far more bytes than exist.
        // data: char-len=1, byte-len=200 (single-byte forms), then only 1 actual byte.
        let data: &[u8] = &[1, 200, b'A'];
        let strings_start = STRING_POOL_HEADER_SIZE + 4; // header + one u32 offset
        let size = (strings_start + data.len()) as u32;
        let mut pool = build_pool_header(size, 1, UTF8_FLAG, strings_start as u32, &[]);
        u32b(&mut pool, 0); // the single offset (relative to strings_start)
        pool.extend_from_slice(data);
        // Drive get() via parse_document's materialize so the decode path runs over the pool.
        let axml = build_axml(&[&pool]);
        let err = parse_document(&axml).expect_err("overrunning utf8 string must fail");
        assert!(
            matches!(err, AxmlError::BadString | AxmlError::Overflow),
            "got {err:?}"
        );
    }

    #[test]
    fn utf16_string_high_bit_length_overflow_is_typed_error() {
        // A UTF-16 pool whose first length word sets the 0x8000 high bit and the extension word
        // encodes a huge 31-bit char count; char_len*2 must be a checked Overflow or the slice
        // bound BadString — never a wrapping multiply.
        let mut data = Vec::new();
        u16b(&mut data, 0xFFFF); // high bit set → 31-bit length follows
        u16b(&mut data, 0xFFFF); // extension word → length ≈ 0x7FFF_FFFF
        let strings_start = STRING_POOL_HEADER_SIZE + 4;
        let size = (strings_start + data.len()) as u32;
        let mut pool = build_pool_header(size, 1, 0 /* UTF-16 */, strings_start as u32, &[]);
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
        // A start element whose name string index is past the pool → StringIndexOutOfRange.
        let pool = build_utf8_string_pool(&["only"]);
        let elem = build_start_element(99, 0, TYPE_INT_DEC, 0); // name_ref 99 (pool has 1 string)
        let axml = build_axml(&[&pool, &elem]);
        assert_eq!(parse_document(&axml), Err(AxmlError::StringIndexOutOfRange));
    }

    #[test]
    fn element_attribute_count_times_size_overflow_is_typed_error() {
        // A start element declaring attribute_count * attribute_size that overflows usize must be
        // a checked Overflow (or Truncated when the array is bounded but past EOF), never an OOB
        // read or a giant allocation. Build the node header by hand with hostile attr fields.
        let pool = build_utf8_string_pool(&["el"]);
        let mut elem = Vec::new();
        u16b(&mut elem, RES_XML_START_ELEMENT_TYPE);
        u16b(&mut elem, XML_NODE_HEADER_SIZE as u16); // headerSize 16
        let size_pos = elem.len();
        u32b(&mut elem, 0); // size patched below
        u32b(&mut elem, 1); // lineNumber
        u32b(&mut elem, NO_STRING); // comment
                                    // attrExt: ns, name, attributeStart, attributeSize, attributeCount, ...
        u32b(&mut elem, NO_STRING); // ns
        u32b(&mut elem, 0); // name idx 0 ("el")
        u16b(&mut elem, 20); // attributeStart
        u16b(&mut elem, 0xFFFF); // attributeSize (huge)
        u16b(&mut elem, 0xFFFF); // attributeCount (huge) → count*size = 0xFFFF*0xFFFF
        u16b(&mut elem, 0); // idIndex
        u16b(&mut elem, 0); // classIndex
        u16b(&mut elem, 0); // styleIndex
        let total = elem.len() as u32;
        elem[size_pos..size_pos + 4].copy_from_slice(&total.to_le_bytes());
        let axml = build_axml(&[&pool, &elem]);
        // 0xFFFF*0xFFFF fits in usize on 64-bit, so the array bound (attrs_end > buf.len) trips
        // Truncated; on a 32-bit target the multiply would be a checked Overflow. Accept either.
        let err = parse_document(&axml).expect_err("hostile attr count/size must fail");
        assert!(
            matches!(err, AxmlError::Truncated | AxmlError::Overflow),
            "got {err:?}"
        );
    }

    #[test]
    fn unbalanced_end_element_is_typed_error() {
        // An end-element with no matching start (stack underflow) must be UnbalancedElement.
        let pool = build_utf8_string_pool(&["el"]);
        let mut end = Vec::new();
        u16b(&mut end, RES_XML_END_ELEMENT_TYPE);
        u16b(&mut end, XML_NODE_HEADER_SIZE as u16);
        u32b(&mut end, XML_NODE_HEADER_SIZE as u32 + 8); // size: header + endExt(ns+name)
        u32b(&mut end, 1); // lineNumber
        u32b(&mut end, NO_STRING); // comment
        u32b(&mut end, NO_STRING); // ns
        u32b(&mut end, 0); // name
        let axml = build_axml(&[&pool, &end]);
        // read_manifest's walk pops an empty stack → UnbalancedElement.
        assert_eq!(read_manifest(&axml), Err(AxmlError::UnbalancedElement));
        // parse_document's checked_sub on depth 0 → UnbalancedElement (same root cause).
        assert_eq!(parse_document(&axml), Err(AxmlError::UnbalancedElement));
    }

    #[test]
    fn nesting_beyond_max_depth_is_typed_error() {
        // MAX_DEPTH+1 nested start-elements must yield TooDeep, never unbounded stack/vec growth.
        let pool = build_utf8_string_pool(&["el"]);
        let mut children: Vec<Vec<u8>> = Vec::new();
        children.push(pool);
        // build_start_element makes a self-contained start with one attr; reuse it as a generic
        // start tag (its attr is harmless). MAX_DEPTH+1 of them are all "open" (no end tags).
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
        // A resource-map shorter than the attribute name index → name_resource 0 (never fabricated,
        // never OOB). String idx 1 ("name") used as the attr name, but the map has only 1 id.
        let pool = build_utf8_string_pool(&["activity", "name", "MyActivity"]);
        let resmap = build_resource_map(&[0x0000_0000]); // only index 0; index 1 is past it
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
        // A child chunk declaring the minimum legal size (8) must advance the iterator (no infinite
        // loop). A root whose body is several back-to-back 8-byte string-pool-typed stubs: the walk
        // terminates (the test returning at all is the proof) with a typed error.
        let mut stub = Vec::new();
        u16b(&mut stub, RES_STRING_POOL_TYPE);
        u16b(&mut stub, 8);
        u32b(&mut stub, 8);
        let axml = build_axml(&[&stub, &stub, &stub]);
        // No usable string pool (these are malformed 8-byte stubs) → an error, but crucially it
        // returns rather than hanging.
        let _ = read_manifest(&axml);
    }
}
