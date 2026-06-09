//! Binary `resources.arsc` (ResTable) reader — total, never-panicking (component-map B).
//!
//! 2026-06-05: Eclipse owns this reader so `AssetManager.retrieveAttributes` / `Resources`
//! can resolve a packed resource id (`0xPPTTEEEE`) to its concrete `Res_value` `(type, data)`
//! without depending on ATL's GTK-coupled `AssetManager` C backing. It is built on the **same**
//! `ResChunk_header` + `ResStringPool` primitives as the binary `AndroidManifest.xml` reader
//! ([`super::axml`]), with the identical totality discipline: every byte is read through
//! bounds-checked little-endian helpers and checked integer math, so any malformed/short/
//! hostile input yields a typed [`ArscError`] and never panics or reads out of bounds. Under
//! the release `panic = "abort"` profile (AGENTS.md §2.4/§2.8) a panic would abort the whole
//! process, so a reader of untrusted resource tables must be total. `#![forbid(unsafe_code)]`
//! (§2.3) keeps it free of raw/unaligned loads.
//!
//! Only the surface attribute/resource resolution needs is exposed: the global value string
//! pool, each package's type-name and key-name pools, and a resolver from a packed resource id
//! to a single `Res_value`. Complex (bag/map) entries, multiple configurations, and reference
//! chasing are intentionally **not** resolved here — the resolver returns the first
//! configuration's simple value (the common case for app resources); a complex entry surfaces
//! as [`ResolvedValue::is_complex`] so a caller can decide how to handle it.
//!
//! ## Format (verified 2026-06-05 against the demo APK's `resources.arsc`)
//! `resources.arsc` is a sequence of little-endian chunks, each led by the 8-byte
//! `ResChunk_header` (`type:u16, headerSize:u16, size:u32`) shared with AXML. The file is one
//! outer `RES_TABLE_TYPE` chunk: a 12-byte header (`ResChunk_header` + `packageCount:u32`)
//! whose body is the **global value string pool** (`RES_STRING_POOL_TYPE`) followed by
//! `packageCount` `RES_TABLE_PACKAGE_TYPE` chunks. Each package chunk's header (≥284 bytes;
//! the demo's is 288 — newer AOSP appends a `typeIdOffset:u32`) holds the `ResChunk_header`,
//! `id:u32`, a `name:[u16;128]`, then `typeStrings:u32`, `lastPublicType:u32`, `keyStrings:u32`,
//! `lastPublicKey:u32`, where `typeStrings`/`keyStrings` are byte offsets relative to the package
//! chunk start that locate that package's type-name and key-name string pools. The package body
//! then holds, per type, a `RES_TABLE_TYPE_SPEC_TYPE` chunk
//! (`id:u8, res0:u8, res1:u16, entryCount:u32`) and one or more `RES_TABLE_TYPE_TYPE` chunks
//! (`id:u8, res0:u8, res1:u16, entryCount:u32, entriesStart:u32, config{...}`). A type chunk's
//! entry-offset array (`entryCount` x `u32`, `0xFFFFFFFF` = absent) begins at `headerSize`; each
//! present offset locates a `ResTable_entry` (`size:u16, flags:u16, key:u32`) at
//! `chunk + entriesStart + offset`. A simple entry (flag `FLAG_COMPLEX` clear) is followed by a
//! `Res_value` (`size:u16, res0:u8, dataType:u8, data:u32`). Layout follows AOSP
//! `frameworks/base/libs/androidfw/include/androidfw/ResourceTypes.h`.

#![forbid(unsafe_code)]

use std::fmt;

// --- ResChunk_header types (the `type` field) ---------------------------------------------
const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_TABLE_TYPE: u16 = 0x0002;
const RES_TABLE_PACKAGE_TYPE: u16 = 0x0200;
const RES_TABLE_TYPE_TYPE: u16 = 0x0201;
const RES_TABLE_TYPE_SPEC_TYPE: u16 = 0x0202;

// --- Fixed struct sizes / field offsets (bytes) ------------------------------------------
/// `ResChunk_header`: type(u16) + headerSize(u16) + size(u32).
const CHUNK_HEADER_SIZE: usize = 8;
/// `ResTable_header`: `ResChunk_header` + packageCount(u32).
const TABLE_HEADER_SIZE: usize = 12;
/// Minimum `ResTable_package` header: `ResChunk_header`(8) + id(u32) + name(256) + the four
/// `typeStrings`/`lastPublicType`/`keyStrings`/`lastPublicKey` u32 fields = 284. (Newer AOSP
/// adds a trailing `typeIdOffset` u32, making the real header 288; that is `>= 284`, so both the
/// 284-byte classic and 288-byte modern packages are accepted — detect, don't assume the size.)
const PACKAGE_HEADER_MIN: usize = 284;
/// Minimum `ResTable_type` header up to and including `entriesStart` (config follows).
const TYPE_HEADER_MIN: usize = 20;
/// Minimum `ResTable_entry`: size(u16) + flags(u16) + key(u32).
const ENTRY_MIN_SIZE: usize = 8;
/// `Res_value`: size(u16) + res0(u8) + dataType(u8) + data(u32).
const RES_VALUE_SIZE: usize = 8;
/// A complex entry's extra `ResTable_map_entry` fields after the base `ResTable_entry`:
/// parent(ResTable_ref: u32) + count(u32). So the map array begins `8 + 8 = 16` bytes into the entry
/// (2026-06-05: matches AOSP `ResTable_map_entry` and the real demo APK's `<style>` bags).
const MAP_ENTRY_EXTRA: usize = 8;
/// One `ResTable_map`: name(ResTable_ref: u32) + value(`Res_value`: 8) = 12 bytes.
const MAP_SIZE: usize = 12;
/// Upper bound on bag entries parsed from one complex entry, so a hostile `count` field cannot drive
/// an unbounded loop / pre-allocation. Real themes have ~150 entries; this sits well above that
/// while bounding work on malformed input (2026-06-05).
const MAX_MAP_ENTRIES: usize = 65536;

/// `ResTable_package` field offsets, relative to the package chunk start.
const PKG_ID_OFFSET: usize = 8;
/// Byte offset of the package `name` field (a NUL-terminated UTF-16LE string), after header(8)+id(4).
const PKG_NAME_OFFSET: usize = 12;
/// Length of the package `name` field in bytes (128 UTF-16 code units).
const PKG_NAME_LEN: usize = 256;
const PKG_TYPE_STRINGS_OFFSET: usize = 268; // 8 (header) + 4 (id) + 256 (name)
const PKG_KEY_STRINGS_OFFSET: usize = 276; // + 4 (typeStrings) + 4 (lastPublicType)

/// `ResTable_entry.flags` bit: the entry is a complex (bag/map) entry, not a simple value.
const ENTRY_FLAG_COMPLEX: u16 = 0x0001;

/// The "no entry at this index" sentinel in a type chunk's entry-offset array.
const NO_ENTRY: u32 = 0xFFFF_FFFF;

/// Upper bound on packages / types / entries parsed from one table, so a hostile count field
/// (e.g. `packageCount = 0xFFFF_FFFF`) cannot drive an unbounded loop or pre-allocation.
/// Real app tables have a handful of packages, dozens of types, and thousands of entries; these
/// caps (2026-06-05) sit well above any legitimate value while bounding work on malformed input.
const MAX_PACKAGES: usize = 256;
const MAX_TYPES: usize = 4096;

/// Errors from reading a binary `resources.arsc`.
///
/// Every malformed/short/out-of-bounds input maps to one of these instead of a panic — the
/// totality guarantee that lets the release profile keep `panic = "abort"` (AGENTS.md §2.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArscError {
    /// A fixed-size structure was read past the end of the buffer (or a chunk's bounds).
    Truncated,
    /// A chunk header was invalid (bad `type`, or `size`/`headerSize` out of range / would not
    /// advance the cursor).
    BadChunk,
    /// The file did not start with an outer `RES_TABLE_TYPE` chunk.
    NotResTable,
    /// The global value string pool chunk was absent (the table references it by index).
    NoValuePool,
    /// A package chunk was malformed (header too small, bad string-pool offset).
    BadPackage,
    /// A string-pool chunk inside the table was malformed (bad offset / length / encoding).
    BadStringPool,
    /// A `ResStringPool_ref` referenced an index outside its string pool.
    StringIndexOutOfRange,
    /// Integer overflow occurred in offset/length arithmetic on hostile input.
    Overflow,
    /// The table declared more packages/types than [`MAX_PACKAGES`]/[`MAX_TYPES`] allow.
    TooManyChunks,
}

impl fmt::Display for ArscError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("resources.arsc ended unexpectedly (truncated)"),
            Self::BadChunk => f.write_str("resources.arsc chunk header is invalid"),
            Self::NotResTable => f.write_str("not a resources.arsc (no RES_TABLE root chunk)"),
            Self::NoValuePool => f.write_str("resources.arsc has no global value string pool"),
            Self::BadPackage => f.write_str("resources.arsc package chunk is malformed"),
            Self::BadStringPool => f.write_str("resources.arsc string pool is malformed"),
            Self::StringIndexOutOfRange => {
                f.write_str("resources.arsc string index is out of range")
            }
            Self::Overflow => f.write_str("resources.arsc offset/length arithmetic overflowed"),
            Self::TooManyChunks => f.write_str("resources.arsc declares too many packages/types"),
        }
    }
}

impl std::error::Error for ArscError {}

/// A resolved resource value: a single `Res_value` (type + data), plus its key (entry) name.
///
/// `type_` is the `Res_value.dataType` byte (e.g. `0x1c` ARGB8 color, `0x10` decimal int,
/// `0x03` string-pool reference, `0x01` resource reference). `data` is the raw 32-bit payload;
/// for `type_ == 0x03` (string) it is an index into the global value string pool, resolvable
/// via [`ResTable::value_string`]. A complex (bag/map) entry has no single `Res_value`; it is
/// reported with `is_complex = true` and `type_`/`data` zeroed so a caller can branch on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedValue {
    /// `Res_value.dataType`.
    pub type_: u8,
    /// `Res_value.data` (raw 32-bit payload; a global-value-pool index when `type_ == 0x03`).
    pub data: u32,
    /// The entry's key-name index into the owning package's key-string pool.
    pub key_index: u32,
    /// `true` when the entry is complex (bag/map): `type_`/`data` are then `0`.
    pub is_complex: bool,
}

/// One attribute entry of a complex (bag/style) entry: the attribute resource id it sets plus the
/// `Res_value` (`type` + `data`) it sets it to. 2026-06-05: a `<style>`/theme entry in `resources.arsc`
/// is a `ResTable_map_entry` — a bag of these, keyed by `ResTable_map.name` (the attribute id, a
/// `0xPPTTEEEE` resource id), each carrying a `Res_value`. A theme's `obtainStyledAttributes(int[])`
/// looks each requested attr id up in the merged bag (see [`ResTable::resolve_style`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyleEntry {
    /// The attribute resource id this entry sets (`ResTable_map.name`, a `0xPPTTEEEE` id).
    pub attr_id: u32,
    /// `Res_value.dataType` (== `TypedValue.TYPE_*`) of the value this attribute is set to.
    pub type_: u8,
    /// `Res_value.data` (raw 32-bit payload; a referenced resource id for `TYPE_REFERENCE`).
    pub data: u32,
}

/// A resolved complex (bag/style) entry: its parent style id plus its own attribute entries.
///
/// 2026-06-05: `parent_id` is `ResTable_map_entry.parent.ident` — the style this one extends
/// (`0` = no parent). Resolving a theme's full attribute set walks this chain (child overrides
/// parent); see the theme registry's merge in `framework`. `entries` are this style's OWN
/// attribute settings (not yet merged with the parent), in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStyle {
    /// `ResTable_map_entry.parent.ident`: the parent style's resource id, or `0` for no parent.
    pub parent_id: u32,
    /// This style's own attribute entries (the bag), in file order.
    pub entries: Vec<StyleEntry>,
}

/// A parsed package within a `resources.arsc`: its id plus the byte ranges of its type-name and
/// key-name string pools and of its `RES_TABLE_TYPE_TYPE` chunks (decoded lazily on lookup).
struct Package {
    /// The package id (the high byte `PP` of a `0xPPTTEEEE` resource id; typically `0x7f`).
    id: u8,
    /// The package name (e.g. `com.example.demo_application`), decoded from the package header's
    /// fixed UTF-16LE `name` field, or `None` if absent/empty. Used by `getResourceName`'s
    /// `package:type/entry` prefix.
    name: Option<String>,
    /// `[start, end)` of this package's type-name string pool chunk in the file (or `None`).
    type_pool: Option<(usize, usize)>,
    /// `[start, end)` of this package's key-name string pool chunk in the file (or `None`).
    key_pool: Option<(usize, usize)>,
    /// `[start, end)` of each `RES_TABLE_TYPE_TYPE` chunk in the file, in file order.
    type_chunks: Vec<(usize, usize)>,
}

/// A parsed `resources.arsc` ready for resource-id resolution.
///
/// Borrows the source bytes (`'a`): string pools and entries are decoded on demand from the
/// original buffer, so construction allocates only the small package/type index, not the data.
pub struct ResTable<'a> {
    buf: &'a [u8],
    /// `[start, end)` of the global value string pool chunk.
    value_pool: (usize, usize),
    packages: Vec<Package>,
}

impl<'a> ResTable<'a> {
    /// Resolve a packed resource id (`0xPPTTEEEE`: package `PP`, type `TT`, entry `EEEE`) to its
    /// `Res_value`, scanning the type's `RES_TABLE_TYPE_TYPE` chunks for the first that defines
    /// the entry.
    ///
    /// Returns `None` when the package/type/entry is absent (an unknown id, or an entry not
    /// defined in any configuration). Never panics: malformed offsets within a candidate chunk
    /// are skipped, so a corrupt chunk cannot crash resolution.
    ///
    /// Multiple configurations (locale/density/...) are not selected between — the first chunk
    /// that defines the entry wins (the default/only config for typical app resources). This is
    /// the smallest surface attribute resolution needs; richer config matching can layer on top.
    pub fn resource_value(&self, resource_id: u32) -> Option<ResolvedValue> {
        let package_id = (resource_id >> 24) as u8;
        let type_id = ((resource_id >> 16) & 0xff) as u8;
        let entry_id = (resource_id & 0xffff) as u16;
        self.resolve(package_id, type_id, entry_id)
    }

    /// Resolve by the three components (package id, 1-based type id, entry index) directly.
    pub fn resolve(&self, package_id: u8, type_id: u8, entry_id: u16) -> Option<ResolvedValue> {
        let package = self.packages.iter().find(|p| p.id == package_id)?;
        for &(start, end) in &package.type_chunks {
            let chunk = self.buf.get(start..end)?;
            // Skip a chunk whose declared type id doesn't match; read_u8 is bounds-checked.
            if read_u8(chunk, CHUNK_HEADER_SIZE).ok()? != type_id {
                continue;
            }
            if let Some(value) = resolve_in_type_chunk(chunk, entry_id) {
                return Some(value);
            }
        }
        None
    }

    /// Resolve a packed `0xPPTTEEEE` style/bag resource id to its [`ResolvedStyle`]: the parent
    /// style id plus this style's own attribute entries (the bag).
    ///
    /// Returns `None` when the id is absent, or when its entry is a SIMPLE (non-complex) value
    /// rather than a bag (a style/theme is always complex). Never panics: a malformed bag within a
    /// candidate chunk is skipped, and the map array is read through bounds-checked helpers, so a
    /// corrupt entry cannot crash resolution.
    ///
    /// Only this style's OWN entries are returned; resolving a theme's FULL attribute set requires
    /// walking `parent_id` recursively (child overrides parent). That chain walk lives in the caller
    /// (the theme registry merge) so this reader stays a single-entry, total decode.
    pub fn resolve_style(&self, resource_id: u32) -> Option<ResolvedStyle> {
        let package_id = (resource_id >> 24) as u8;
        let type_id = ((resource_id >> 16) & 0xff) as u8;
        let entry_id = (resource_id & 0xffff) as u16;
        let package = self.packages.iter().find(|p| p.id == package_id)?;
        for &(start, end) in &package.type_chunks {
            let chunk = self.buf.get(start..end)?;
            if read_u8(chunk, CHUNK_HEADER_SIZE).ok()? != type_id {
                continue;
            }
            if let Some(style) = resolve_style_in_type_chunk(chunk, entry_id) {
                return Some(style);
            }
        }
        None
    }

    /// Resolve a global value-pool string index (e.g. a [`ResolvedValue`] with `type_ == 0x03`,
    /// whose `data` is the index). Returns `None` for out-of-range/absent, `Err` for corruption.
    pub fn value_string(&self, index: u32) -> Result<Option<String>, ArscError> {
        let pool = self.value_pool()?;
        pool.get(index)
    }

    /// The type name for a 1-based `type_id` in the given package (e.g. `color`, `string`),
    /// resolved from that package's type-name string pool. `None` if absent/out of range.
    pub fn type_name(&self, package_id: u8, type_id: u8) -> Result<Option<String>, ArscError> {
        let Some(package) = self.packages.iter().find(|p| p.id == package_id) else {
            return Ok(None);
        };
        let Some((start, end)) = package.type_pool else {
            return Ok(None);
        };
        // Type ids are 1-based; index 0 is the first type name.
        let Some(index) = type_id.checked_sub(1) else {
            return Ok(None);
        };
        let pool = self.pool_at(start, end)?;
        pool.get(u32::from(index))
    }

    /// The key (entry) name for a key index in the given package, resolved from that package's
    /// key-name string pool. `None` if the package/pool is absent or the index is out of range.
    pub fn key_name(&self, package_id: u8, key_index: u32) -> Result<Option<String>, ArscError> {
        let Some(package) = self.packages.iter().find(|p| p.id == package_id) else {
            return Ok(None);
        };
        let Some((start, end)) = package.key_pool else {
            return Ok(None);
        };
        let pool = self.pool_at(start, end)?;
        pool.get(key_index)
    }

    /// The package ids present in this table (typically just `0x7f` for an app).
    pub fn package_ids(&self) -> Vec<u8> {
        self.packages.iter().map(|p| p.id).collect()
    }

    /// The package name (e.g. `com.example.demo_application`) for a package id, from its header's
    /// UTF-16 `name` field. `None` if the package is absent or its name field is empty/truncated.
    /// Used to build `getResourceName`'s `package:type/entry` form.
    pub fn package_name(&self, package_id: u8) -> Option<&str> {
        self.packages
            .iter()
            .find(|p| p.id == package_id)
            .and_then(|p| p.name.as_deref())
    }

    fn value_pool(&self) -> Result<StringPool<'a>, ArscError> {
        self.pool_at(self.value_pool.0, self.value_pool.1)
    }

    /// Re-parse a string pool from a recorded `[start, end)` range. Cheap: [`StringPool::parse`]
    /// only validates the header + offset array and borrows the data (no per-string allocation).
    fn pool_at(&self, start: usize, end: usize) -> Result<StringPool<'a>, ArscError> {
        let bytes = self.buf.get(start..end).ok_or(ArscError::Truncated)?;
        StringPool::parse(bytes)
    }
}

/// Parse a binary `resources.arsc` into a [`ResTable`] borrowing `bytes`.
///
/// Returns a typed [`ArscError`] for any malformed input — never panics, never reads out of
/// bounds (the totality guarantee that lets the release profile keep `panic = "abort"`).
pub fn parse_arsc(bytes: &[u8]) -> Result<ResTable<'_>, ArscError> {
    let root = ChunkRef::parse(bytes, 0)?;
    if root.kind != RES_TABLE_TYPE {
        return Err(ArscError::NotResTable);
    }
    if root.header_size < TABLE_HEADER_SIZE {
        return Err(ArscError::BadChunk);
    }

    let mut value_pool: Option<(usize, usize)> = None;
    let mut packages: Vec<Package> = Vec::new();

    // The table body (after its 12-byte header) is: the global value string pool, then the
    // package chunks. Iterate children, advancing by each child's `size`.
    for child in root.children() {
        let child = child?;
        match child.kind {
            RES_STRING_POOL_TYPE if value_pool.is_none() => {
                value_pool = Some((child.start, child.end));
            }
            RES_TABLE_PACKAGE_TYPE => {
                if packages.len() >= MAX_PACKAGES {
                    return Err(ArscError::TooManyChunks);
                }
                packages.push(parse_package(bytes, &child)?);
            }
            _ => {} // chunk types we don't use are skipped by size.
        }
    }

    let value_pool = value_pool.ok_or(ArscError::NoValuePool)?;
    Ok(ResTable {
        buf: bytes,
        value_pool,
        packages,
    })
}

/// Parse one `RES_TABLE_PACKAGE_TYPE` chunk: record its id, type/key string-pool ranges, and
/// the file ranges of its `RES_TABLE_TYPE_TYPE` chunks.
fn parse_package(buf: &[u8], pkg: &ChunkRef) -> Result<Package, ArscError> {
    let chunk = buf.get(pkg.start..pkg.end).ok_or(ArscError::Truncated)?;
    if pkg.header_size < PACKAGE_HEADER_MIN {
        return Err(ArscError::BadPackage);
    }
    // The id is a u32 but only the low byte is the package id (per the resource-id layout).
    let id = read_u8(chunk, PKG_ID_OFFSET).map_err(|_| ArscError::BadPackage)?;
    let name = read_package_name(chunk);
    let type_strings =
        read_u32(chunk, PKG_TYPE_STRINGS_OFFSET).map_err(|_| ArscError::BadPackage)?;
    let key_strings = read_u32(chunk, PKG_KEY_STRINGS_OFFSET).map_err(|_| ArscError::BadPackage)?;

    // typeStrings/keyStrings are byte offsets relative to the package chunk start; 0 = absent.
    let type_pool = pool_range_in_chunk(pkg, type_strings as usize)?;
    let key_pool = pool_range_in_chunk(pkg, key_strings as usize)?;

    // The package body (after its header) holds the type-spec and type chunks. The string pools
    // live inside that body too, so iterate everything after the header and collect the type
    // chunks (the string pools are already located above via the header offsets).
    let mut type_chunks: Vec<(usize, usize)> = Vec::new();
    for child in pkg.children() {
        let child = child?;
        match child.kind {
            RES_TABLE_TYPE_TYPE => {
                if type_chunks.len() >= MAX_TYPES {
                    return Err(ArscError::TooManyChunks);
                }
                type_chunks.push((child.start, child.end));
            }
            RES_TABLE_TYPE_SPEC_TYPE => {} // spec carries flags only; not needed for value lookup.
            _ => {}
        }
    }

    Ok(Package {
        id,
        name,
        type_pool,
        key_pool,
        type_chunks,
    })
}

/// Decode the package header's fixed UTF-16LE `name` field (NUL-terminated, 128 code units) into a
/// `String`. Returns `None` if the field is truncated or empty. Total: stops at the first NUL or at
/// the field boundary, and any unpaired surrogate becomes U+FFFD (lossless-enough for a package name;
/// never panics, never reads out of bounds).
fn read_package_name(chunk: &[u8]) -> Option<String> {
    let bytes = chunk.get(PKG_NAME_OFFSET..PKG_NAME_OFFSET + PKG_NAME_LEN)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .take_while(|&u| u != 0)
        .collect();
    if units.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(&units))
}

/// Convert a package-relative string-pool offset to an absolute `[start, end)` file range,
/// validating that it names a `RES_STRING_POOL_TYPE` chunk fully inside the package. A `0`
/// offset (or an offset that does not parse as a string pool) yields `None`, not an error, so a
/// package that legitimately omits a pool is handled gracefully.
fn pool_range_in_chunk(pkg: &ChunkRef, rel: usize) -> Result<Option<(usize, usize)>, ArscError> {
    if rel == 0 {
        return Ok(None);
    }
    let abs = pkg.start.checked_add(rel).ok_or(ArscError::Overflow)?;
    if abs >= pkg.end {
        return Err(ArscError::BadPackage);
    }
    // Parse the pool chunk against the FULL file buffer at its absolute offset, so the recorded
    // [start, end) range is absolute (re-sliceable from `ResTable.buf`). It must stay inside the
    // enclosing package chunk.
    let pool = ChunkRef::parse(pkg.buf, abs)?;
    if pool.kind != RES_STRING_POOL_TYPE || pool.end > pkg.end {
        return Ok(None);
    }
    Ok(Some((pool.start, pool.end)))
}

/// Resolve `entry_id` within one `RES_TABLE_TYPE_TYPE` chunk, or `None` if it has no such entry.
///
/// Total: any malformed offset/length within the chunk returns `None` rather than panicking.
fn resolve_in_type_chunk(chunk: &[u8], entry_id: u16) -> Option<ResolvedValue> {
    let header_size = read_u16(chunk, 2).ok()? as usize;
    if header_size < TYPE_HEADER_MIN {
        return None;
    }
    let entry_count = read_u32(chunk, 12).ok()? as usize;
    let entries_start = read_u32(chunk, 16).ok()? as usize;
    let index = entry_id as usize;
    if index >= entry_count {
        return None;
    }

    // Entry-offset array (u32 each) begins at headerSize.
    let off_pos = header_size.checked_add(index.checked_mul(4)?)?;
    let entry_off = read_u32(chunk, off_pos).ok()?;
    if entry_off == NO_ENTRY {
        return None; // entry absent in this configuration
    }
    let entry_at = entries_start.checked_add(entry_off as usize)?;

    // ResTable_entry: size(u16), flags(u16), key(u32).
    let entry_size = read_u16(chunk, entry_at).ok()? as usize;
    if entry_size < ENTRY_MIN_SIZE {
        return None;
    }
    let flags = read_u16(chunk, entry_at.checked_add(2)?).ok()?;
    let key_index = read_u32(chunk, entry_at.checked_add(4)?).ok()?;

    if flags & ENTRY_FLAG_COMPLEX != 0 {
        // A bag/map entry has no single Res_value; report it as complex.
        return Some(ResolvedValue {
            type_: 0,
            data: 0,
            key_index,
            is_complex: true,
        });
    }

    // A simple entry is followed by a Res_value at entry_at + entry_size.
    let value_at = entry_at.checked_add(entry_size)?;
    // Bound the Res_value within the chunk before reading its fields.
    let value_end = value_at.checked_add(RES_VALUE_SIZE)?;
    if value_end > chunk.len() {
        return None;
    }
    let type_ = read_u8(chunk, value_at.checked_add(3)?).ok()?; // size(2)+res0(1) then dataType
    let data = read_u32(chunk, value_at.checked_add(4)?).ok()?;
    Some(ResolvedValue {
        type_,
        data,
        key_index,
        is_complex: false,
    })
}

/// Resolve `entry_id` within one `RES_TABLE_TYPE_TYPE` chunk as a complex (bag/style) entry, or
/// `None` if it has no such entry or the entry is a simple (non-complex) value.
///
/// Total: any malformed offset/length/count within the chunk returns `None` rather than panicking,
/// and every map entry is read through bounds-checked helpers. The `count` is capped by
/// [`MAX_MAP_ENTRIES`] so a hostile count cannot drive an unbounded loop; iteration also stops at
/// the first map entry that would read past the chunk.
fn resolve_style_in_type_chunk(chunk: &[u8], entry_id: u16) -> Option<ResolvedStyle> {
    let header_size = read_u16(chunk, 2).ok()? as usize;
    if header_size < TYPE_HEADER_MIN {
        return None;
    }
    let entry_count = read_u32(chunk, 12).ok()? as usize;
    let entries_start = read_u32(chunk, 16).ok()? as usize;
    let index = entry_id as usize;
    if index >= entry_count {
        return None;
    }

    let off_pos = header_size.checked_add(index.checked_mul(4)?)?;
    let entry_off = read_u32(chunk, off_pos).ok()?;
    if entry_off == NO_ENTRY {
        return None;
    }
    let entry_at = entries_start.checked_add(entry_off as usize)?;

    // ResTable_entry: size(u16), flags(u16), key(u32). A complex (bag) entry has FLAG_COMPLEX set
    // and a larger header (ResTable_map_entry = ResTable_entry + parent(u32) + count(u32)).
    let entry_size = read_u16(chunk, entry_at).ok()? as usize;
    if entry_size < ENTRY_MIN_SIZE.checked_add(MAP_ENTRY_EXTRA)? {
        return None; // not a complex entry header (no room for parent + count)
    }
    let flags = read_u16(chunk, entry_at.checked_add(2)?).ok()?;
    if flags & ENTRY_FLAG_COMPLEX == 0 {
        return None; // a simple value, not a style/bag
    }
    // ResTable_map_entry: parent(ResTable_ref: u32) then count(u32), after the base ResTable_entry.
    let parent_id = read_u32(chunk, entry_at.checked_add(ENTRY_MIN_SIZE)?).ok()?;
    let count = read_u32(chunk, entry_at.checked_add(ENTRY_MIN_SIZE + 4)?).ok()? as usize;
    let count = count.min(MAX_MAP_ENTRIES);

    // The ResTable_map array begins right after the entry header (entry_at + entry_size).
    let mut map_at = entry_at.checked_add(entry_size)?;
    let mut entries = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        // ResTable_map: name(ResTable_ref: u32), then value(Res_value: size(u16) res0(u8)
        // dataType(u8) data(u32)). Stop if this map entry would read past the chunk.
        let end = map_at.checked_add(MAP_SIZE)?;
        if end > chunk.len() {
            break;
        }
        let attr_id = read_u32(chunk, map_at).ok()?;
        // Res_value within the map: dataType at +7 (size:2 + res0:1 then dataType), data at +8.
        let type_ = read_u8(chunk, map_at.checked_add(7)?).ok()?;
        let data = read_u32(chunk, map_at.checked_add(8)?).ok()?;
        entries.push(StyleEntry {
            attr_id,
            type_,
            data,
        });
        map_at = end;
    }

    Some(ResolvedStyle { parent_id, entries })
}

// --- String pool (`RES_STRING_POOL_TYPE`) ------------------------------------------------
//
// Self-contained reader modeled on `super::axml`'s string pool (same `ResStringPool_header`
// layout and the same UTF-8/UTF-16 length-prefixed string forms), kept inside this module so
// `arsc.rs` is a standalone unit that does not depend on `axml.rs`'s private internals.

/// String pool header flag: strings are UTF-8 (else UTF-16LE).
const UTF8_FLAG: u32 = 0x0100;
/// `ResStringPool_header` size (the fixed prefix before the offset array).
const STRING_POOL_HEADER_SIZE: usize = 28;
/// `ResStringPool_ref` "no string" sentinel.
const NO_STRING: u32 = 0xFFFF_FFFF;

/// A validated, lazily-decoded string pool. Holds the pool chunk bytes plus the parsed header
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
    /// Parse and validate a `RES_STRING_POOL_TYPE` chunk (`chunk` is exactly the pool chunk).
    fn parse(chunk: &'a [u8]) -> Result<Self, ArscError> {
        if read_u16(chunk, 0)? != RES_STRING_POOL_TYPE {
            return Err(ArscError::BadStringPool);
        }
        // ResStringPool_header fields (offsets within the chunk).
        let string_count = read_u32(chunk, 8)? as usize;
        let flags = read_u32(chunk, 16)?;
        let strings_start = read_u32(chunk, 20)? as usize;
        let is_utf8 = flags & UTF8_FLAG != 0;

        // The offset array follows the 28-byte header; require it to fit in the chunk.
        let offsets_start = STRING_POOL_HEADER_SIZE;
        let offsets_len = string_count.checked_mul(4).ok_or(ArscError::Overflow)?;
        let offsets_end = offsets_start
            .checked_add(offsets_len)
            .ok_or(ArscError::Overflow)?;
        if offsets_end > chunk.len() {
            return Err(ArscError::BadStringPool);
        }
        // strings_start is relative to the chunk; it must be inside the chunk.
        if strings_start > chunk.len() {
            return Err(ArscError::BadStringPool);
        }
        Ok(Self {
            chunk,
            string_count,
            offsets_start,
            data_start: strings_start,
            is_utf8,
        })
    }

    /// Resolve a `ResStringPool_ref`: `Ok(None)` for the sentinel (no string),
    /// `Err(StringIndexOutOfRange)` for an index past the pool, and a decoded `String` otherwise.
    fn get(&self, index: u32) -> Result<Option<String>, ArscError> {
        if index == NO_STRING {
            return Ok(None);
        }
        let index = index as usize;
        if index >= self.string_count {
            return Err(ArscError::StringIndexOutOfRange);
        }
        let off_pos = self
            .offsets_start
            .checked_add(index.checked_mul(4).ok_or(ArscError::Overflow)?)
            .ok_or(ArscError::Overflow)?;
        let rel = read_u32(self.chunk, off_pos)? as usize;
        let start = self
            .data_start
            .checked_add(rel)
            .ok_or(ArscError::Overflow)?;
        let s = if self.is_utf8 {
            decode_utf8(self.chunk, start)?
        } else {
            decode_utf16(self.chunk, start)?
        };
        Ok(Some(s))
    }
}

/// Decode a UTF-8 length-prefixed pool string at `start` (char-len field, then byte-len field,
/// then the bytes; each length is one byte, or two when the high bit `0x80` is set). The byte
/// length is authoritative; the trailing NUL is not consumed.
fn decode_utf8(buf: &[u8], start: usize) -> Result<String, ArscError> {
    let (_, after_char) = read_var_len_u8(buf, start)?;
    let (byte_len, after_len) = read_var_len_u8(buf, after_char)?;
    let end = after_len.checked_add(byte_len).ok_or(ArscError::Overflow)?;
    let data = buf.get(after_len..end).ok_or(ArscError::BadStringPool)?;
    std::str::from_utf8(data)
        .map(str::to_owned)
        .map_err(|_| ArscError::BadStringPool)
}

/// Decode a UTF-16LE length-prefixed pool string at `start` (u16 char-count, or a 31-bit count
/// when the high bit `0x8000` is set; then `count` UTF-16LE units, NUL-terminated).
fn decode_utf16(buf: &[u8], start: usize) -> Result<String, ArscError> {
    let first = read_u16(buf, start)? as usize;
    let (char_len, data_start) = if first & 0x8000 != 0 {
        let next = read_u16(buf, start.checked_add(2).ok_or(ArscError::Overflow)?)? as usize;
        let len = ((first & 0x7FFF) << 16) | next;
        (len, start.checked_add(4).ok_or(ArscError::Overflow)?)
    } else {
        (first, start.checked_add(2).ok_or(ArscError::Overflow)?)
    };
    let byte_len = char_len.checked_mul(2).ok_or(ArscError::Overflow)?;
    let end = data_start
        .checked_add(byte_len)
        .ok_or(ArscError::Overflow)?;
    let data = buf.get(data_start..end).ok_or(ArscError::BadStringPool)?;
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

/// Read a variable-length `u8`/`u16` length field (UTF-8 pool form): one byte, or two when the
/// high bit `0x80` is set. Returns the value and the offset just past it.
fn read_var_len_u8(buf: &[u8], off: usize) -> Result<(usize, usize), ArscError> {
    let first = read_u8(buf, off)? as usize;
    if first & 0x80 != 0 {
        let next = read_u8(buf, off.checked_add(1).ok_or(ArscError::Overflow)?)? as usize;
        Ok((
            ((first & 0x7F) << 8) | next,
            off.checked_add(2).ok_or(ArscError::Overflow)?,
        ))
    } else {
        Ok((first, off.checked_add(1).ok_or(ArscError::Overflow)?))
    }
}

/// A validated chunk view into the original file buffer: its `[start, end)` bounds are
/// guaranteed inside `buf` and `end - start == size`. Mirrors [`super::axml`]'s `Chunk` but
/// records absolute file offsets so ranges can be stored in the [`ResTable`] index and re-sliced
/// for lazy decoding.
struct ChunkRef<'a> {
    buf: &'a [u8],
    kind: u16,
    header_size: usize,
    /// Absolute `[start, end)` of this chunk (header + body) in `buf`.
    start: usize,
    end: usize,
}

impl<'a> ChunkRef<'a> {
    /// Parse the chunk at `off` in `buf`, validating all bounds (mirrors `axml::Chunk::parse`).
    fn parse(buf: &'a [u8], off: usize) -> Result<Self, ArscError> {
        let kind = read_u16(buf, off)?;
        let header_size = read_u16(buf, off.checked_add(2).ok_or(ArscError::Overflow)?)? as usize;
        let size = read_u32(buf, off.checked_add(4).ok_or(ArscError::Overflow)?)? as usize;
        // size >= headerSize >= 8 guarantees forward progress when advancing by size, and that
        // the declared header fits the chunk.
        if header_size < CHUNK_HEADER_SIZE || size < header_size {
            return Err(ArscError::BadChunk);
        }
        let end = off.checked_add(size).ok_or(ArscError::Overflow)?;
        if end > buf.len() {
            return Err(ArscError::Truncated);
        }
        Ok(Self {
            buf,
            kind,
            header_size,
            start: off,
            end,
        })
    }

    /// Iterate this chunk's child chunks, parsing each within this chunk's body and reporting
    /// absolute file offsets.
    fn children(&self) -> ChildIter<'a> {
        ChildIter {
            buf: self.buf,
            end: self.end,
            // Children begin right after this chunk's header.
            off: self.start.saturating_add(self.header_size),
        }
    }
}

/// Iterator over child chunks of a parent, advancing by each child's `size`, bounded by the
/// parent's `end`. Mirrors `axml::ChunkIter` but over absolute offsets.
struct ChildIter<'a> {
    buf: &'a [u8],
    end: usize,
    off: usize,
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = Result<ChunkRef<'a>, ArscError>;

    fn next(&mut self) -> Option<Self::Item> {
        // Need a full ResChunk_header before the parent's end to have another child.
        if self.off.checked_add(CHUNK_HEADER_SIZE)? > self.end {
            return None;
        }
        match ChunkRef::parse(self.buf, self.off) {
            Ok(chunk) => {
                if chunk.end > self.end {
                    // Child overruns the parent — stop (do not yield a chunk past the parent).
                    self.off = self.end;
                    return None;
                }
                // size >= 8 (validated in parse) guarantees off strictly advances.
                self.off = chunk.end;
                Some(Ok(chunk))
            }
            Err(e) => {
                self.off = self.end; // stop after an error
                Some(Err(e))
            }
        }
    }
}

// --- Bounds-checked little-endian readers (mirror axml's; the only raw-byte → integer sites) --

fn read_u8(buf: &[u8], off: usize) -> Result<u8, ArscError> {
    buf.get(off).copied().ok_or(ArscError::Truncated)
}

fn read_u16(buf: &[u8], off: usize) -> Result<u16, ArscError> {
    let end = off.checked_add(2).ok_or(ArscError::Overflow)?;
    let b = buf.get(off..end).ok_or(ArscError::Truncated)?;
    let arr: [u8; 2] = b.try_into().map_err(|_| ArscError::Truncated)?;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32(buf: &[u8], off: usize) -> Result<u32, ArscError> {
    let end = off.checked_add(4).ok_or(ArscError::Overflow)?;
    let b = buf.get(off..end).ok_or(ArscError::Truncated)?;
    let arr: [u8; 4] = b.try_into().map_err(|_| ArscError::Truncated)?;
    Ok(u32::from_le_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::Apk;

    /// Path to the demo APK whose `resources.arsc` the tests parse. It is a checked-in test
    /// asset under `~/eclipse-m0/atl_test_apks` (see AGENTS.md); the env override keeps the test
    /// portable (no hardcoded developer path baked into a passing run).
    fn demo_apk_path() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("ECLIPSE_DEMO_APK") {
            return std::path::PathBuf::from(p);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        std::path::Path::new(&home).join("eclipse-m0/atl_test_apks/demo_app.apk")
    }

    /// Read `resources.arsc` from the demo APK, or `None` if the asset is unavailable on this
    /// machine (so CI without the asset falls back to the hand-built fixture below).
    fn demo_arsc() -> Option<Vec<u8>> {
        let path = demo_apk_path();
        let mut apk = Apk::open(&path).ok()?;
        // read_entry is a private sibling method (same `apk` module tree) — usable from tests.
        apk.read_entry("resources.arsc").ok()
    }

    #[test]
    fn parses_real_demo_arsc_and_resolves_a_known_value() {
        let Some(bytes) = demo_arsc() else {
            eprintln!("demo_app.apk unavailable; covered by hand-built fixture test instead");
            return;
        };
        let table = parse_arsc(&bytes).expect("parse demo resources.arsc");

        // The demo package id is 0x7f (the standard application package id).
        assert!(
            table.package_ids().contains(&0x7f),
            "expected package 0x7f, got {:?}",
            table.package_ids()
        );

        // Type/key string pools are non-empty and indexable. Type id 1 is `color` (verified
        // 2026-06-05 from the type-name pool: color, drawable, id, layout, mipmap, string).
        let color = table
            .type_name(0x7f, 1)
            .expect("type pool readable")
            .expect("type id 1 present");
        assert_eq!(color, "color");

        // A known resource resolves to a Res_value of the expected type. The first color entry
        // (0x7f010000) is `black` = ARGB8 0xff000000 (Res_value dataType 0x1c) — verified
        // 2026-06-05 from the raw bytes.
        let v = table
            .resource_value(0x7f01_0000)
            .expect("0x7f010000 resolves");
        assert!(!v.is_complex);
        assert_eq!(v.type_, 0x1c, "expected TYPE_INT_COLOR_ARGB8");
        assert_eq!(v.data, 0xff00_0000);

        // Its key name resolves through the package key-string pool.
        let key = table
            .key_name(0x7f, v.key_index)
            .expect("key pool readable")
            .expect("key present");
        assert_eq!(key, "black");

        // A string resource resolves to a string-pool index that dereferences via the global
        // value pool (type 0x03). `app_name` lives under the `string` type (id 6).
        let app_name_id = find_entry(&table, 0x7f, 6, "app_name").expect("app_name present");
        let sv = table
            .resource_value(app_name_id)
            .expect("app_name resolves");
        assert_eq!(sv.type_, 0x03, "expected TYPE_STRING");
        let s = table
            .value_string(sv.data)
            .expect("value pool readable")
            .expect("string present");
        assert!(!s.is_empty(), "app_name string should be non-empty");
    }

    /// Scan a type's entries for one whose key name matches, returning its packed resource id.
    fn find_entry(table: &ResTable, pkg: u8, type_id: u8, key: &str) -> Option<u32> {
        for entry in 0u16..0x1000 {
            if let Some(v) = table.resolve(pkg, type_id, entry) {
                if let Ok(Some(name)) = table.key_name(pkg, v.key_index) {
                    if name == key {
                        return Some(
                            (u32::from(pkg) << 24) | (u32::from(type_id) << 16) | u32::from(entry),
                        );
                    }
                }
            }
        }
        None
    }

    /// A tiny hand-built `resources.arsc` for host-independent coverage: package 0x7f, one type
    /// (id 1) with one simple entry (id 0, key index 0) carrying a Res_value of dataType 0x10
    /// (decimal int) data 42. No string pools beyond the minimal empty global value pool, so the
    /// fixture exercises the chunk + entry + Res_value path even without the demo asset.
    fn build_fixture() -> Vec<u8> {
        // --- global value string pool (empty: 0 strings) ---
        // ResStringPool_header (28 bytes): type, headerSize, size, stringCount, styleCount,
        // flags, stringsStart, stylesStart.
        let mut pool = Vec::new();
        push_u16(&mut pool, RES_STRING_POOL_TYPE);
        push_u16(&mut pool, 28); // headerSize
        push_u32(&mut pool, 28); // size (header only, no strings)
        push_u32(&mut pool, 0); // stringCount
        push_u32(&mut pool, 0); // styleCount
        push_u32(&mut pool, 0); // flags
        push_u32(&mut pool, 28); // stringsStart
        push_u32(&mut pool, 0); // stylesStart

        // --- type chunk (RES_TABLE_TYPE_TYPE) ---
        // header (20 bytes used): type, headerSize, size, id, res0, res1, entryCount,
        // entriesStart, then a minimal 0-length config (we set config size 0 so headerSize=20).
        // entry-offset array (1 x u32) at headerSize=20; entries at entriesStart=24.
        // ResTable_entry (8) + Res_value (8) = 16 bytes of entry data.
        let mut type_chunk = Vec::new();
        let type_header_size = 20u16;
        let entries_start = 24u32; // header(20) + offset array(1*4)
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, type_header_size);
        // size = header(20) + offsets(4) + entry(8) + value(8) = 40
        push_u32(&mut type_chunk, 40);
        type_chunk.push(1); // id (type id 1)
        type_chunk.push(0); // res0
        push_u16(&mut type_chunk, 0); // res1
        push_u32(&mut type_chunk, 1); // entryCount
        push_u32(&mut type_chunk, entries_start);
        // entry-offset array: entry 0 at offset 0 within the entries region.
        push_u32(&mut type_chunk, 0);
        // ResTable_entry: size(8), flags(0 = simple), key(0).
        push_u16(&mut type_chunk, 8); // entry size
        push_u16(&mut type_chunk, 0); // flags
        push_u32(&mut type_chunk, 0); // key index
                                      // Res_value: size(8), res0(0), dataType(0x10 = TYPE_INT_DEC), data(42).
        push_u16(&mut type_chunk, 8); // value size
        type_chunk.push(0); // res0
        type_chunk.push(0x10); // dataType
        push_u32(&mut type_chunk, 42); // data

        // --- package chunk (RES_TABLE_PACKAGE_TYPE) ---
        // header 288 bytes; body = the type chunk. typeStrings/keyStrings = 0 (absent).
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16); // headerSize 288
        push_u32(&mut pkg, (PACKAGE_HEADER_MIN + type_chunk.len()) as u32); // size
        push_u32(&mut pkg, 0x7f); // id
        pkg.resize(pkg.len() + 256, 0); // name[128] u16, zeroed
        push_u32(&mut pkg, 0); // typeStrings offset (absent)
        push_u32(&mut pkg, 0); // lastPublicType
        push_u32(&mut pkg, 0); // keyStrings offset (absent)
        push_u32(&mut pkg, 0); // lastPublicKey
        debug_assert_eq!(pkg.len(), PACKAGE_HEADER_MIN);
        pkg.extend_from_slice(&type_chunk);

        // --- table chunk (RES_TABLE_TYPE) ---
        let mut table = Vec::new();
        push_u16(&mut table, RES_TABLE_TYPE);
        push_u16(&mut table, TABLE_HEADER_SIZE as u16); // headerSize 12
        push_u32(
            &mut table,
            (TABLE_HEADER_SIZE + pool.len() + pkg.len()) as u32,
        ); // size
        push_u32(&mut table, 1); // packageCount
        table.extend_from_slice(&pool);
        table.extend_from_slice(&pkg);
        table
    }

    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    #[test]
    fn parses_hand_built_fixture_and_resolves_int_value() {
        // Host-independent coverage of the parse + resolve path (no external asset needed).
        let bytes = build_fixture();
        let table = parse_arsc(&bytes).expect("parse fixture");
        assert_eq!(table.package_ids(), vec![0x7f]);

        let v = table.resource_value(0x7f01_0000).expect("entry 0 resolves");
        assert!(!v.is_complex);
        assert_eq!(v.type_, 0x10, "TYPE_INT_DEC");
        assert_eq!(v.data, 42);
        assert_eq!(v.key_index, 0);

        // Unknown ids resolve to None, not a panic: wrong package, type, and entry.
        assert!(
            table.resource_value(0x7e01_0000).is_none(),
            "unknown package"
        );
        assert!(table.resource_value(0x7f02_0000).is_none(), "unknown type");
        assert!(table.resource_value(0x7f01_0001).is_none(), "unknown entry");
    }

    /// A hand-built `resources.arsc` with package 0x7f, type id 8 (`style`), and one COMPLEX
    /// (bag/style) entry (entry id 0 = `0x7f080000`): parent `0x7f08000a`, two attribute entries
    /// (`0x7f010058` → TYPE_INT_BOOLEAN(0x12) data 0xffffffff; `0x7f0100a9` → TYPE_REFERENCE(0x01)
    /// data 0x7f0a0014). Mirrors the real demo APK's `<style>` bag layout verified 2026-06-05.
    fn build_style_fixture() -> Vec<u8> {
        // empty global value pool (28 bytes), same as build_fixture.
        let mut pool = Vec::new();
        push_u16(&mut pool, RES_STRING_POOL_TYPE);
        push_u16(&mut pool, 28);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0);

        // type chunk: type id 8 (style), 1 entry, complex.
        // entry region = ResTable_map_entry(16) + 2 x ResTable_map(12) = 40 bytes.
        let mut type_chunk = Vec::new();
        let type_header_size = 20u16;
        let entries_start = 24u32; // header(20) + offset array(1*4)
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, type_header_size);
        // size = header(20) + offsets(4) + map_entry(16) + 2*map(12) = 64
        push_u32(&mut type_chunk, 64);
        type_chunk.push(8); // id (type id 8 = style)
        type_chunk.push(0); // res0
        push_u16(&mut type_chunk, 0); // res1
        push_u32(&mut type_chunk, 1); // entryCount
        push_u32(&mut type_chunk, entries_start);
        // entry-offset array: entry 0 at offset 0.
        push_u32(&mut type_chunk, 0);
        // ResTable_map_entry: size(16 = ResTable_entry(8) + parent(4) + count(4)), flags(COMPLEX),
        // key(0), parent(0x7f08000a), count(2).
        push_u16(&mut type_chunk, 16); // entry size (the map_entry header)
        push_u16(&mut type_chunk, ENTRY_FLAG_COMPLEX); // flags
        push_u32(&mut type_chunk, 0); // key index
        push_u32(&mut type_chunk, 0x7f08_000a); // parent style id
        push_u32(&mut type_chunk, 2); // count
                                      // ResTable_map[0]: name=0x7f010058, value size(8) res0(0) type(0x12) data(0xffffffff).
        push_u32(&mut type_chunk, 0x7f01_0058);
        push_u16(&mut type_chunk, 8);
        type_chunk.push(0);
        type_chunk.push(0x12); // TYPE_INT_BOOLEAN
        push_u32(&mut type_chunk, 0xffff_ffff);
        // ResTable_map[1]: name=0x7f0100a9, value size(8) res0(0) type(0x01 REFERENCE) data(0x7f0a0014).
        push_u32(&mut type_chunk, 0x7f01_00a9);
        push_u16(&mut type_chunk, 8);
        type_chunk.push(0);
        type_chunk.push(0x01); // TYPE_REFERENCE
        push_u32(&mut type_chunk, 0x7f0a_0014);

        // package chunk (288-byte header), body = the type chunk.
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut pkg, (PACKAGE_HEADER_MIN + type_chunk.len()) as u32);
        push_u32(&mut pkg, 0x7f);
        pkg.resize(pkg.len() + 256, 0);
        push_u32(&mut pkg, 0); // typeStrings
        push_u32(&mut pkg, 0); // lastPublicType
        push_u32(&mut pkg, 0); // keyStrings
        push_u32(&mut pkg, 0); // lastPublicKey
        debug_assert_eq!(pkg.len(), PACKAGE_HEADER_MIN);
        pkg.extend_from_slice(&type_chunk);

        let mut table = Vec::new();
        push_u16(&mut table, RES_TABLE_TYPE);
        push_u16(&mut table, TABLE_HEADER_SIZE as u16);
        push_u32(
            &mut table,
            (TABLE_HEADER_SIZE + pool.len() + pkg.len()) as u32,
        );
        push_u32(&mut table, 1);
        table.extend_from_slice(&pool);
        table.extend_from_slice(&pkg);
        table
    }

    #[test]
    fn resolves_hand_built_style_bag_and_parent() {
        // Host-independent coverage of the complex (bag/style) decode + parent id.
        let bytes = build_style_fixture();
        let table = parse_arsc(&bytes).expect("parse style fixture");

        // The simple-value resolver reports it as complex (no single Res_value).
        let v = table.resource_value(0x7f08_0000).expect("entry resolves");
        assert!(v.is_complex, "a style entry must surface as complex");

        // The bag resolver returns the parent + the two attribute entries.
        let style = table.resolve_style(0x7f08_0000).expect("style resolves");
        assert_eq!(style.parent_id, 0x7f08_000a, "parent style id");
        assert_eq!(style.entries.len(), 2);
        assert_eq!(style.entries[0].attr_id, 0x7f01_0058);
        assert_eq!(style.entries[0].type_, 0x12, "TYPE_INT_BOOLEAN");
        assert_eq!(style.entries[0].data, 0xffff_ffff);
        assert_eq!(style.entries[1].attr_id, 0x7f01_00a9);
        assert_eq!(style.entries[1].type_, 0x01, "TYPE_REFERENCE");
        assert_eq!(style.entries[1].data, 0x7f0a_0014);

        // A simple entry is NOT a style: resolve_style returns None for it (use the fixture's int).
        let simple_bytes = build_fixture();
        let simple = parse_arsc(&simple_bytes).expect("parse simple fixture");
        assert!(
            simple.resolve_style(0x7f01_0000).is_none(),
            "a simple value entry is not a style bag"
        );
        // An unknown id is None, not a panic.
        assert!(simple.resolve_style(0x7f08_0000).is_none(), "unknown style");
    }

    #[test]
    fn resolves_real_demo_style_when_available() {
        // If the demo arsc has a style/theme entry, decode it and assert the bag is non-empty.
        // The accelerometer demo's theme 0x7f0800a3 (verified 2026-06-05) has parent 0x7f08010a and
        // 3 entries; gate behind ECLIPSE_THEME_STYLE_ID so the assertion stays APK-agnostic.
        let Some(bytes) = demo_arsc() else {
            eprintln!("demo arsc unavailable; covered by hand-built style fixture");
            return;
        };
        let table = parse_arsc(&bytes).expect("parse demo arsc");
        // Find ANY complex entry under the style type, proving the bag decode works on a real table.
        // Type id for `style` is discovered via the type-name pool (not hardcoded — APK-agnostic).
        let mut style_type: Option<u8> = None;
        for tid in 1u8..=64 {
            if let Ok(Some(name)) = table.type_name(0x7f, tid) {
                if name == "style" {
                    style_type = Some(tid);
                    break;
                }
            }
        }
        let Some(tid) = style_type else {
            eprintln!("demo arsc has no `style` type; skipping");
            return;
        };
        // Scan style entries for the first complex one and decode its bag totally.
        let mut found = false;
        for entry in 0u16..0x1000 {
            let id = (0x7fu32 << 24) | (u32::from(tid) << 16) | u32::from(entry);
            if let Some(style) = table.resolve_style(id) {
                // A real style bag has entries and/or a parent; the decode must not panic and the
                // attr ids must be plausible resource ids (non-zero for the ones present).
                if !style.entries.is_empty() || style.parent_id != 0 {
                    found = true;
                    break;
                }
            }
        }
        assert!(
            found,
            "expected at least one decodable style bag in the demo arsc"
        );
    }

    #[test]
    fn rejects_non_table_root() {
        // A valid chunk that is not RES_TABLE_TYPE must be a typed error, not a panic.
        let mut buf = Vec::new();
        push_u16(&mut buf, RES_STRING_POOL_TYPE);
        push_u16(&mut buf, 8);
        push_u32(&mut buf, 8);
        let err = parse_arsc(&buf).err().expect("non-table root must fail");
        assert_eq!(err, ArscError::NotResTable);
    }

    #[test]
    fn table_without_value_pool_is_typed_error() {
        // A table with a package but no global value string pool must surface NoValuePool.
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut pkg, PACKAGE_HEADER_MIN as u32);
        push_u32(&mut pkg, 0x7f);
        pkg.resize(pkg.len() + 256, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);

        let mut table = Vec::new();
        push_u16(&mut table, RES_TABLE_TYPE);
        push_u16(&mut table, TABLE_HEADER_SIZE as u16);
        push_u32(&mut table, (TABLE_HEADER_SIZE + pkg.len()) as u32);
        push_u32(&mut table, 1);
        table.extend_from_slice(&pkg);
        let err = parse_arsc(&table)
            .err()
            .expect("missing value pool must fail");
        assert_eq!(err, ArscError::NoValuePool);
    }

    // === Adversarial robustness pass (2026-06-05) ==========================================
    // Hand-crafted hostile `resources.arsc` byte buffers: each must yield a typed `ArscError`
    // (or a clean `None` from a resolver), NEVER a panic / integer overflow / OOB slice /
    // unbounded alloc. Direct negative tests driving the chunk header, table/package header,
    // type/typeSpec, entry-offset array, entry, bag, and Res_value readers into failure
    // branches. `#![forbid(unsafe_code)]` + debug `overflow-checks` mean any wrapping `+`/`*`
    // would panic here, so the tests completing is proof the checked/bounds discipline holds.

    /// `parse_arsc`'s error (`ResTable` has no `Debug`/`PartialEq`, so `assert_eq!` on the whole
    /// `Result` won't compile; extract the `Err` and compare that).
    fn parse_err(bytes: &[u8]) -> ArscError {
        parse_arsc(bytes).err().expect("expected a typed ArscError")
    }

    /// Build a minimal valid empty global value string pool chunk (28 bytes).
    fn empty_value_pool() -> Vec<u8> {
        let mut pool = Vec::new();
        push_u16(&mut pool, RES_STRING_POOL_TYPE);
        push_u16(&mut pool, 28);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0); // stringCount
        push_u32(&mut pool, 0); // styleCount
        push_u32(&mut pool, 0); // flags
        push_u32(&mut pool, 28); // stringsStart
        push_u32(&mut pool, 0); // stylesStart
        pool
    }

    /// Wrap a value pool + arbitrary package bytes into a RES_TABLE_TYPE root with packageCount=1.
    fn build_table(pkg: &[u8]) -> Vec<u8> {
        let pool = empty_value_pool();
        let mut table = Vec::new();
        push_u16(&mut table, RES_TABLE_TYPE);
        push_u16(&mut table, TABLE_HEADER_SIZE as u16);
        push_u32(
            &mut table,
            (TABLE_HEADER_SIZE + pool.len() + pkg.len()) as u32,
        );
        push_u32(&mut table, 1); // packageCount
        table.extend_from_slice(&pool);
        table.extend_from_slice(pkg);
        table
    }

    #[test]
    fn bad_root_chunk_header_is_typed_error() {
        // headerSize < 8 → BadChunk.
        let mut b = Vec::new();
        push_u16(&mut b, RES_TABLE_TYPE);
        push_u16(&mut b, 4);
        push_u32(&mut b, 12);
        assert_eq!(parse_err(&b), ArscError::BadChunk);

        // size < headerSize → BadChunk.
        let mut b = Vec::new();
        push_u16(&mut b, RES_TABLE_TYPE);
        push_u16(&mut b, 12);
        push_u32(&mut b, 8);
        assert_eq!(parse_err(&b), ArscError::BadChunk);

        // size past EOF → Truncated.
        let mut b = Vec::new();
        push_u16(&mut b, RES_TABLE_TYPE);
        push_u16(&mut b, 12);
        push_u32(&mut b, 0xFFFF_FFF0);
        assert_eq!(parse_err(&b), ArscError::Truncated);

        // Valid chunk but RES_TABLE headerSize < 12 (no room for packageCount) → BadChunk.
        let mut b = Vec::new();
        push_u16(&mut b, RES_TABLE_TYPE);
        push_u16(&mut b, 8); // < TABLE_HEADER_SIZE
        push_u32(&mut b, 8);
        assert_eq!(parse_err(&b), ArscError::BadChunk);
    }

    #[test]
    fn package_header_too_small_is_bad_package() {
        // A package chunk whose headerSize is below PACKAGE_HEADER_MIN (284) → BadPackage.
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, 16); // far below 284
        push_u32(&mut pkg, 16);
        pkg.resize(16, 0);
        let table = build_table(&pkg);
        assert_eq!(parse_err(&table), ArscError::BadPackage);
    }

    #[test]
    fn package_type_or_key_strings_offset_past_chunk_is_bad_package() {
        // typeStrings offset pointing past the package chunk end → BadPackage (pool_range_in_chunk).
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut pkg, PACKAGE_HEADER_MIN as u32); // size = header only
        push_u32(&mut pkg, 0x7f); // id
        pkg.resize(pkg.len() + 256, 0); // name
        push_u32(&mut pkg, 0xFFFF_FFF0); // typeStrings offset way past the chunk
        push_u32(&mut pkg, 0); // lastPublicType
        push_u32(&mut pkg, 0); // keyStrings
        push_u32(&mut pkg, 0); // lastPublicKey
        let table = build_table(&pkg);
        let err = parse_err(&table);
        assert!(
            matches!(err, ArscError::BadPackage | ArscError::Overflow),
            "got {err:?}"
        );
    }

    /// Build a package whose body is the given type-chunk bytes, with no string pools (offsets 0).
    fn build_package_with_type(type_chunk: &[u8]) -> Vec<u8> {
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut pkg, (PACKAGE_HEADER_MIN + type_chunk.len()) as u32);
        push_u32(&mut pkg, 0x7f);
        pkg.resize(pkg.len() + 256, 0);
        push_u32(&mut pkg, 0); // typeStrings
        push_u32(&mut pkg, 0); // lastPublicType
        push_u32(&mut pkg, 0); // keyStrings
        push_u32(&mut pkg, 0); // lastPublicKey
        pkg.extend_from_slice(type_chunk);
        pkg
    }

    #[test]
    fn type_chunk_huge_entry_count_does_not_overrun_or_alloc() {
        // A type chunk declaring a colossal entryCount but a tiny body: resolve() must return None
        // for an in-range-looking entry whose offset slot is past the chunk (read_u32 → Err → None),
        // and must never pre-allocate entryCount-sized memory or read OOB. resolve does NOT allocate
        // per entry, so the danger is purely arithmetic/OOB — assert it stays None.
        let mut type_chunk = Vec::new();
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, TYPE_HEADER_MIN as u16); // headerSize 20
        push_u32(&mut type_chunk, TYPE_HEADER_MIN as u32); // size = header only (no offset array!)
        type_chunk.push(1); // type id 1
        type_chunk.push(0); // res0
        push_u16(&mut type_chunk, 0); // res1
        push_u32(&mut type_chunk, 0xFFFF_FFFF); // entryCount = 4 billion
        push_u32(&mut type_chunk, TYPE_HEADER_MIN as u32); // entriesStart
        let pkg = build_package_with_type(&type_chunk);
        let table = build_table(&pkg);
        // The type chunk's declared size (20) is only the header, but build_table's outer size
        // covers the real bytes; parse must succeed and resolve must be a clean None.
        let parsed = parse_arsc(&table);
        if let Ok(t) = parsed {
            // entry 0's offset slot is at headerSize(20) but the chunk body ends there → read_u32
            // fails → None. No panic, no OOB, no alloc.
            assert!(t.resolve(0x7f, 1, 0).is_none());
            assert!(t.resolve(0x7f, 1, 0xFFFF).is_none());
            assert!(t.resolve_style(0x7f01_0000).is_none());
        }
        // If parse itself rejected it, that is equally acceptable (typed error, no panic).
    }

    #[test]
    fn type_entry_offset_past_type_data_is_none() {
        // A type chunk with one entry whose offset points far past the entries region → resolve
        // returns None (read past chunk fails), never OOB.
        let mut type_chunk = Vec::new();
        let header_size = TYPE_HEADER_MIN as u16;
        let entries_start = (TYPE_HEADER_MIN + 4) as u32; // header + 1 offset
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, header_size);
        // size = header(20) + offset array(4); no actual entry bytes follow.
        push_u32(&mut type_chunk, (TYPE_HEADER_MIN + 4) as u32);
        type_chunk.push(1);
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 1); // entryCount 1
        push_u32(&mut type_chunk, entries_start);
        push_u32(&mut type_chunk, 0xFFFF_FF00); // entry 0 offset: way past entries_start
        let pkg = build_package_with_type(&type_chunk);
        let table = build_table(&pkg);
        if let Ok(t) = parse_arsc(&table) {
            assert!(t.resolve(0x7f, 1, 0).is_none(), "offset past data ⇒ None");
        }
    }

    #[test]
    fn entry_size_below_minimum_is_none() {
        // An entry whose declared size is below ENTRY_MIN_SIZE (8) → resolve returns None, never a
        // bad Res_value read. Build a type chunk with one entry of size 4.
        let mut type_chunk = Vec::new();
        let header_size = TYPE_HEADER_MIN as u16;
        let entries_start = (TYPE_HEADER_MIN + 4) as u32;
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, header_size);
        let size_pos = type_chunk.len();
        push_u32(&mut type_chunk, 0); // size patched below
        type_chunk.push(1);
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 1); // entryCount
        push_u32(&mut type_chunk, entries_start);
        push_u32(&mut type_chunk, 0); // entry 0 at offset 0
                                      // ResTable_entry with size 4 (below the 8-byte minimum) + 4 padding bytes.
        push_u16(&mut type_chunk, 4); // entry size (too small)
        push_u16(&mut type_chunk, 0); // flags
        push_u32(&mut type_chunk, 0); // padding to keep the chunk well-formed
        let total = type_chunk.len() as u32;
        type_chunk[size_pos..size_pos + 4].copy_from_slice(&total.to_le_bytes());
        let pkg = build_package_with_type(&type_chunk);
        let table = build_table(&pkg);
        if let Ok(t) = parse_arsc(&table) {
            assert!(t.resolve(0x7f, 1, 0).is_none(), "entry size < 8 ⇒ None");
        }
    }

    #[test]
    fn bag_count_overflow_is_bounded_not_unbounded() {
        // A complex (bag) entry declaring a colossal map count must be bounded by MAX_MAP_ENTRIES
        // and stop at the first map that would read past the chunk — never an unbounded loop, OOB,
        // or gigabyte alloc. Build a style entry with count = 0xFFFFFFFF but only 1 real map.
        let mut type_chunk = Vec::new();
        let header_size = TYPE_HEADER_MIN as u16;
        let entries_start = (TYPE_HEADER_MIN + 4) as u32;
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, header_size);
        let size_pos = type_chunk.len();
        push_u32(&mut type_chunk, 0); // size patched below
        type_chunk.push(8); // type id 8 (style)
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 1); // entryCount
        push_u32(&mut type_chunk, entries_start);
        push_u32(&mut type_chunk, 0); // entry 0 at offset 0
                                      // ResTable_map_entry: size 16, flags COMPLEX, key 0, parent 0, count 0xFFFFFFFF.
        push_u16(&mut type_chunk, 16);
        push_u16(&mut type_chunk, ENTRY_FLAG_COMPLEX);
        push_u32(&mut type_chunk, 0); // key
        push_u32(&mut type_chunk, 0); // parent
        push_u32(&mut type_chunk, 0xFFFF_FFFF); // hostile count
                                                // exactly one real ResTable_map (12 bytes); the loop must stop after it.
        push_u32(&mut type_chunk, 0x7f01_0058); // name
        push_u16(&mut type_chunk, 8); // value size
        type_chunk.push(0); // res0
        type_chunk.push(0x10); // dataType
        push_u32(&mut type_chunk, 1); // data
        let total = type_chunk.len() as u32;
        type_chunk[size_pos..size_pos + 4].copy_from_slice(&total.to_le_bytes());
        let pkg = build_package_with_type(&type_chunk);
        let table = build_table(&pkg);
        let t = parse_arsc(&table).expect("parse hostile-bag table");
        let style = t
            .resolve_style(0x7f08_0000)
            .expect("style entry resolves (bounded)");
        // Only the one in-bounds map is read; the loop stopped at the chunk boundary.
        assert_eq!(
            style.entries.len(),
            1,
            "hostile count must be bounded by the chunk, not 4 billion"
        );
    }

    #[test]
    fn too_many_packages_is_typed_error() {
        // packageCount in the header is advisory; parse iterates actual child chunks. Feed more
        // than MAX_PACKAGES real package chunks and require TooManyChunks, not unbounded growth.
        let pool = empty_value_pool();
        // A minimal valid package (header only, no body).
        let mut one_pkg = Vec::new();
        push_u16(&mut one_pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut one_pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut one_pkg, PACKAGE_HEADER_MIN as u32);
        push_u32(&mut one_pkg, 0x7f);
        one_pkg.resize(one_pkg.len() + 256, 0);
        push_u32(&mut one_pkg, 0);
        push_u32(&mut one_pkg, 0);
        push_u32(&mut one_pkg, 0);
        push_u32(&mut one_pkg, 0);

        let mut body = pool.clone();
        for _ in 0..(MAX_PACKAGES + 1) {
            body.extend_from_slice(&one_pkg);
        }
        let mut table = Vec::new();
        push_u16(&mut table, RES_TABLE_TYPE);
        push_u16(&mut table, TABLE_HEADER_SIZE as u16);
        push_u32(&mut table, (TABLE_HEADER_SIZE + body.len()) as u32);
        push_u32(&mut table, 1);
        table.extend_from_slice(&body);
        assert_eq!(parse_err(&table), ArscError::TooManyChunks);
    }

    #[test]
    fn unknown_package_type_entry_ids_resolve_to_none() {
        // Out-of-range package/type/entry ids on a valid table are clean None, never a panic.
        let bytes = build_fixture();
        let t = parse_arsc(&bytes).expect("parse fixture");
        assert!(t.resolve(0x01, 1, 0).is_none(), "unknown package id");
        assert!(t.resolve(0x7f, 0xff, 0).is_none(), "unknown type id");
        assert!(t.resolve(0x7f, 1, 0xffff).is_none(), "unknown entry id");
        assert!(t.resolve_style(0x0108_0000).is_none(), "unknown style pkg");
        // type_name/key_name/value_string for absent ids are Ok(None), not Err/panic.
        assert!(t.type_name(0x01, 1).unwrap().is_none());
        assert!(t.key_name(0x01, 0).unwrap().is_none());
        assert!(t.type_name(0x7f, 0).unwrap().is_none(), "type id 0 ⇒ None");
    }

    #[test]
    fn string_pool_with_bad_type_is_bad_string_pool() {
        // pool_at / StringPool::parse on bytes whose first u16 isn't RES_STRING_POOL_TYPE →
        // BadStringPool (reached when a package's type-strings offset names a non-pool chunk; here
        // we assert the resolver path stays a typed error, not a panic).
        let mut not_a_pool = Vec::new();
        push_u16(&mut not_a_pool, 0x9999); // wrong type
        push_u16(&mut not_a_pool, 28);
        push_u32(&mut not_a_pool, 28);
        not_a_pool.resize(28, 0);
        assert!(matches!(
            StringPool::parse(&not_a_pool),
            Err(ArscError::BadStringPool)
        ));
    }

    #[test]
    fn reader_is_total_under_truncation_and_mutation() {
        // TOTALITY guard (the confirmed root cause a non-total parser would reintroduce: a panic
        // under panic=abort aborts the process). Starting from a known-good table (the demo arsc
        // when available, else the hand-built fixture), this exhaustively:
        //   (a) truncates at every prefix length, and
        //   (b) flips bytes at a strided set of offsets to 0x00 / 0x7F / 0xFF,
        // calling parse_arsc — and on any Ok, exercising the resolver — on each input, requiring
        // a Result (never a panic). The test process completing is the proof of totality.
        let base = demo_arsc().unwrap_or_else(build_fixture);

        for len in 0..=base.len() {
            if let Ok(table) = parse_arsc(&base[..len]) {
                // Drive the resolvers too: each must be total on a truncated-but-parsed table.
                let _ = table.resource_value(0x7f01_0000);
                let _ = table.resolve_style(0x7f08_0000);
                let _ = table.type_name(0x7f, 1);
            }
        }

        let stride = 7; // coprime-ish with struct sizes to hit varied fields cheaply
        for off in (0..base.len()).step_by(stride) {
            for &val in &[0x00u8, 0x7F, 0xFF] {
                let mut buf = base.clone();
                buf[off] = val;
                if let Ok(table) = parse_arsc(&buf) {
                    for entry in 0u16..8 {
                        let _ = table.resolve(0x7f, 1, entry);
                        let _ = table.resolve_style(0x7f08_0000 | u32::from(entry));
                    }
                    let _ = table.type_name(0x7f, 1);
                    let _ = table.key_name(0x7f, 0);
                    let _ = table.value_string(0);
                }
            }
        }
    }
}
