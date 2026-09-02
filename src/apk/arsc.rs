#![forbid(unsafe_code)]

use std::fmt;

const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_TABLE_TYPE: u16 = 0x0002;
const RES_TABLE_PACKAGE_TYPE: u16 = 0x0200;
const RES_TABLE_TYPE_TYPE: u16 = 0x0201;
const RES_TABLE_TYPE_SPEC_TYPE: u16 = 0x0202;

const CHUNK_HEADER_SIZE: usize = 8;

const TABLE_HEADER_SIZE: usize = 12;

const PACKAGE_HEADER_MIN: usize = 284;

const TYPE_HEADER_MIN: usize = 20;

const ENTRY_MIN_SIZE: usize = 8;

const RES_VALUE_SIZE: usize = 8;

const MAP_ENTRY_EXTRA: usize = 8;

const MAP_SIZE: usize = 12;

const MAX_MAP_ENTRIES: usize = 65536;

const PKG_ID_OFFSET: usize = 8;

const PKG_NAME_OFFSET: usize = 12;

const PKG_NAME_LEN: usize = 256;
const PKG_TYPE_STRINGS_OFFSET: usize = 268;
const PKG_KEY_STRINGS_OFFSET: usize = 276;

const ENTRY_FLAG_COMPLEX: u16 = 0x0001;

const NO_ENTRY: u32 = 0xFFFF_FFFF;

const MAX_PACKAGES: usize = 256;
const MAX_TYPES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArscError {
    Truncated,

    BadChunk,

    NotResTable,

    NoValuePool,

    BadPackage,

    BadStringPool,

    StringIndexOutOfRange,

    Overflow,

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedValue {
    pub type_: u8,

    pub data: u32,

    pub key_index: u32,

    pub is_complex: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyleEntry {
    pub attr_id: u32,

    pub type_: u8,

    pub data: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStyle {
    pub parent_id: u32,

    pub entries: Vec<StyleEntry>,
}

struct Package {
    id: u8,

    name: Option<String>,

    type_pool: Option<(usize, usize)>,

    key_pool: Option<(usize, usize)>,

    type_chunks: Vec<(usize, usize)>,
}

pub struct ResTable<'a> {
    buf: &'a [u8],

    value_pool: (usize, usize),
    packages: Vec<Package>,
}

impl<'a> ResTable<'a> {
    pub fn resource_value(&self, resource_id: u32) -> Option<ResolvedValue> {
        let package_id = (resource_id >> 24) as u8;
        let type_id = ((resource_id >> 16) & 0xff) as u8;
        let entry_id = (resource_id & 0xffff) as u16;
        self.resolve(package_id, type_id, entry_id)
    }

    pub fn resolve(&self, package_id: u8, type_id: u8, entry_id: u16) -> Option<ResolvedValue> {
        let package = self.packages.iter().find(|p| p.id == package_id)?;
        for &(start, end) in &package.type_chunks {
            let chunk = self.buf.get(start..end)?;

            if read_u8(chunk, CHUNK_HEADER_SIZE).ok()? != type_id {
                continue;
            }
            if let Some(value) = resolve_in_type_chunk(chunk, entry_id) {
                return Some(value);
            }
        }
        None
    }

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

    pub fn value_string(&self, index: u32) -> Result<Option<String>, ArscError> {
        let pool = self.value_pool()?;
        pool.get(index)
    }

    pub fn type_name(&self, package_id: u8, type_id: u8) -> Result<Option<String>, ArscError> {
        let Some(package) = self.packages.iter().find(|p| p.id == package_id) else {
            return Ok(None);
        };
        let Some((start, end)) = package.type_pool else {
            return Ok(None);
        };

        let Some(index) = type_id.checked_sub(1) else {
            return Ok(None);
        };
        let pool = self.pool_at(start, end)?;
        pool.get(u32::from(index))
    }

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

    pub fn package_ids(&self) -> Vec<u8> {
        self.packages.iter().map(|p| p.id).collect()
    }

    pub fn package_name(&self, package_id: u8) -> Option<&str> {
        self.packages
            .iter()
            .find(|p| p.id == package_id)
            .and_then(|p| p.name.as_deref())
    }

    pub fn find_resource_id(
        &self,
        package_name: Option<&str>,
        type_name: &str,
        entry_name: &str,
    ) -> Option<u32> {
        let want_pkg = package_name.filter(|p| !p.is_empty());
        for package in &self.packages {
            if let Some(want) = want_pkg {
                if package.name.as_deref() != Some(want) {
                    continue;
                }
            }
            for &(start, end) in &package.type_chunks {
                let Some(chunk) = self.buf.get(start..end) else {
                    continue;
                };
                let Ok(type_id) = read_u8(chunk, CHUNK_HEADER_SIZE) else {
                    continue;
                };

                if !matches!(self.type_name(package.id, type_id), Ok(Some(tn)) if tn == type_name) {
                    continue;
                }
                let Ok(entry_count) = read_u32(chunk, 12) else {
                    continue;
                };

                let bound = entry_count.min(0x1_0000);
                for entry_id in 0..bound {
                    let Ok(eid) = u16::try_from(entry_id) else {
                        break;
                    };
                    let Some(resolved) = resolve_in_type_chunk(chunk, eid) else {
                        continue;
                    };
                    if matches!(self.key_name(package.id, resolved.key_index), Ok(Some(kn)) if kn == entry_name)
                    {
                        return Some(
                            (u32::from(package.id) << 24)
                                | (u32::from(type_id) << 16)
                                | u32::from(eid),
                        );
                    }
                }
            }
        }
        None
    }

    fn value_pool(&self) -> Result<StringPool<'a>, ArscError> {
        self.pool_at(self.value_pool.0, self.value_pool.1)
    }

    fn pool_at(&self, start: usize, end: usize) -> Result<StringPool<'a>, ArscError> {
        let bytes = self.buf.get(start..end).ok_or(ArscError::Truncated)?;
        StringPool::parse(bytes)
    }
}

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
            _ => {}
        }
    }

    let value_pool = value_pool.ok_or(ArscError::NoValuePool)?;
    Ok(ResTable {
        buf: bytes,
        value_pool,
        packages,
    })
}

fn parse_package(buf: &[u8], pkg: &ChunkRef) -> Result<Package, ArscError> {
    let chunk = buf.get(pkg.start..pkg.end).ok_or(ArscError::Truncated)?;
    if pkg.header_size < PACKAGE_HEADER_MIN {
        return Err(ArscError::BadPackage);
    }

    let id = read_u8(chunk, PKG_ID_OFFSET).map_err(|_| ArscError::BadPackage)?;
    let name = read_package_name(chunk);
    let type_strings =
        read_u32(chunk, PKG_TYPE_STRINGS_OFFSET).map_err(|_| ArscError::BadPackage)?;
    let key_strings = read_u32(chunk, PKG_KEY_STRINGS_OFFSET).map_err(|_| ArscError::BadPackage)?;

    let type_pool = pool_range_in_chunk(pkg, type_strings as usize)?;
    let key_pool = pool_range_in_chunk(pkg, key_strings as usize)?;

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
            RES_TABLE_TYPE_SPEC_TYPE => {}
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

fn read_package_name(chunk: &[u8]) -> Option<String> {
    let bytes = chunk.get(PKG_NAME_OFFSET..PKG_NAME_OFFSET + PKG_NAME_LEN)?;
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .take_while(|&u| u != 0)
        .collect();
    if units.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(&units))
}

fn pool_range_in_chunk(pkg: &ChunkRef, rel: usize) -> Result<Option<(usize, usize)>, ArscError> {
    if rel == 0 {
        return Ok(None);
    }
    let abs = pkg.start.checked_add(rel).ok_or(ArscError::Overflow)?;
    if abs >= pkg.end {
        return Err(ArscError::BadPackage);
    }

    let pool = ChunkRef::parse(pkg.buf, abs)?;
    if pool.kind != RES_STRING_POOL_TYPE || pool.end > pkg.end {
        return Ok(None);
    }
    Ok(Some((pool.start, pool.end)))
}

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

    let off_pos = header_size.checked_add(index.checked_mul(4)?)?;
    let entry_off = read_u32(chunk, off_pos).ok()?;
    if entry_off == NO_ENTRY {
        return None;
    }
    let entry_at = entries_start.checked_add(entry_off as usize)?;

    let entry_size = read_u16(chunk, entry_at).ok()? as usize;
    if entry_size < ENTRY_MIN_SIZE {
        return None;
    }
    let flags = read_u16(chunk, entry_at.checked_add(2)?).ok()?;
    let key_index = read_u32(chunk, entry_at.checked_add(4)?).ok()?;

    if flags & ENTRY_FLAG_COMPLEX != 0 {
        return Some(ResolvedValue {
            type_: 0,
            data: 0,
            key_index,
            is_complex: true,
        });
    }

    let value_at = entry_at.checked_add(entry_size)?;

    let value_end = value_at.checked_add(RES_VALUE_SIZE)?;
    if value_end > chunk.len() {
        return None;
    }
    let type_ = read_u8(chunk, value_at.checked_add(3)?).ok()?;
    let data = read_u32(chunk, value_at.checked_add(4)?).ok()?;
    Some(ResolvedValue {
        type_,
        data,
        key_index,
        is_complex: false,
    })
}

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

    let entry_size = read_u16(chunk, entry_at).ok()? as usize;
    if entry_size < ENTRY_MIN_SIZE.checked_add(MAP_ENTRY_EXTRA)? {
        return None;
    }
    let flags = read_u16(chunk, entry_at.checked_add(2)?).ok()?;
    if flags & ENTRY_FLAG_COMPLEX == 0 {
        return None;
    }

    let parent_id = read_u32(chunk, entry_at.checked_add(ENTRY_MIN_SIZE)?).ok()?;
    let count = read_u32(chunk, entry_at.checked_add(ENTRY_MIN_SIZE + 4)?).ok()? as usize;
    let count = count.min(MAX_MAP_ENTRIES);

    let mut map_at = entry_at.checked_add(entry_size)?;
    let mut entries = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        let end = map_at.checked_add(MAP_SIZE)?;
        if end > chunk.len() {
            break;
        }
        let attr_id = read_u32(chunk, map_at).ok()?;

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

const UTF8_FLAG: u32 = 0x0100;

const STRING_POOL_HEADER_SIZE: usize = 28;

const NO_STRING: u32 = 0xFFFF_FFFF;

struct StringPool<'a> {
    chunk: &'a [u8],
    string_count: usize,

    offsets_start: usize,

    data_start: usize,
    is_utf8: bool,
}

impl<'a> StringPool<'a> {
    fn parse(chunk: &'a [u8]) -> Result<Self, ArscError> {
        if read_u16(chunk, 0)? != RES_STRING_POOL_TYPE {
            return Err(ArscError::BadStringPool);
        }

        let string_count = read_u32(chunk, 8)? as usize;
        let flags = read_u32(chunk, 16)?;
        let strings_start = read_u32(chunk, 20)? as usize;
        let is_utf8 = flags & UTF8_FLAG != 0;

        let offsets_start = STRING_POOL_HEADER_SIZE;
        let offsets_len = string_count.checked_mul(4).ok_or(ArscError::Overflow)?;
        let offsets_end = offsets_start
            .checked_add(offsets_len)
            .ok_or(ArscError::Overflow)?;
        if offsets_end > chunk.len() {
            return Err(ArscError::BadStringPool);
        }

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

fn decode_utf8(buf: &[u8], start: usize) -> Result<String, ArscError> {
    let (_, after_char) = read_var_len_u8(buf, start)?;
    let (byte_len, after_len) = read_var_len_u8(buf, after_char)?;
    let end = after_len.checked_add(byte_len).ok_or(ArscError::Overflow)?;
    let data = buf.get(after_len..end).ok_or(ArscError::BadStringPool)?;
    std::str::from_utf8(data)
        .map(str::to_owned)
        .map_err(|_| ArscError::BadStringPool)
}

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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

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

struct ChunkRef<'a> {
    buf: &'a [u8],
    kind: u16,
    header_size: usize,

    start: usize,
    end: usize,
}

impl<'a> ChunkRef<'a> {
    fn parse(buf: &'a [u8], off: usize) -> Result<Self, ArscError> {
        let kind = read_u16(buf, off)?;
        let header_size = read_u16(buf, off.checked_add(2).ok_or(ArscError::Overflow)?)? as usize;
        let size = read_u32(buf, off.checked_add(4).ok_or(ArscError::Overflow)?)? as usize;

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

    fn children(&self) -> ChildIter<'a> {
        ChildIter {
            buf: self.buf,
            end: self.end,

            off: self.start.saturating_add(self.header_size),
        }
    }
}

struct ChildIter<'a> {
    buf: &'a [u8],
    end: usize,
    off: usize,
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = Result<ChunkRef<'a>, ArscError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.off.checked_add(CHUNK_HEADER_SIZE)? > self.end {
            return None;
        }
        match ChunkRef::parse(self.buf, self.off) {
            Ok(chunk) => {
                if chunk.end > self.end {
                    self.off = self.end;
                    return None;
                }

                self.off = chunk.end;
                Some(Ok(chunk))
            }
            Err(e) => {
                self.off = self.end;
                Some(Err(e))
            }
        }
    }
}

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

    fn demo_apk_path() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("ECLIPSE_DEMO_APK") {
            return std::path::PathBuf::from(p);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        std::path::Path::new(&home).join("eclipse-m0/atl_test_apks/demo_app.apk")
    }

    fn demo_arsc() -> Option<Vec<u8>> {
        let path = demo_apk_path();
        let mut apk = Apk::open(&path).ok()?;

        apk.read_entry("resources.arsc").ok()
    }

    #[test]
    fn parses_real_demo_arsc_and_resolves_a_known_value() {
        let Some(bytes) = demo_arsc() else {
            eprintln!("demo_app.apk unavailable; covered by hand-built fixture test instead");
            return;
        };
        let table = parse_arsc(&bytes).expect("parse demo resources.arsc");

        assert!(
            table.package_ids().contains(&0x7f),
            "expected package 0x7f, got {:?}",
            table.package_ids()
        );

        let color = table
            .type_name(0x7f, 1)
            .expect("type pool readable")
            .expect("type id 1 present");
        assert_eq!(color, "color");

        let v = table
            .resource_value(0x7f01_0000)
            .expect("0x7f010000 resolves");
        assert!(!v.is_complex);
        assert_eq!(v.type_, 0x1c, "expected TYPE_INT_COLOR_ARGB8");
        assert_eq!(v.data, 0xff00_0000);

        let key = table
            .key_name(0x7f, v.key_index)
            .expect("key pool readable")
            .expect("key present");
        assert_eq!(key, "black");

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

    fn build_fixture() -> Vec<u8> {
        let mut pool = Vec::new();
        push_u16(&mut pool, RES_STRING_POOL_TYPE);
        push_u16(&mut pool, 28);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0);

        let mut type_chunk = Vec::new();
        let type_header_size = 20u16;
        let entries_start = 24u32;
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, type_header_size);

        push_u32(&mut type_chunk, 40);
        type_chunk.push(1);
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 1);
        push_u32(&mut type_chunk, entries_start);

        push_u32(&mut type_chunk, 0);

        push_u16(&mut type_chunk, 8);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 0);

        push_u16(&mut type_chunk, 8);
        type_chunk.push(0);
        type_chunk.push(0x10);
        push_u32(&mut type_chunk, 42);

        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut pkg, (PACKAGE_HEADER_MIN + type_chunk.len()) as u32);
        push_u32(&mut pkg, 0x7f);
        pkg.resize(pkg.len() + 256, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
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

    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    #[test]
    fn parses_hand_built_fixture_and_resolves_int_value() {
        let bytes = build_fixture();
        let table = parse_arsc(&bytes).expect("parse fixture");
        assert_eq!(table.package_ids(), vec![0x7f]);

        let v = table.resource_value(0x7f01_0000).expect("entry 0 resolves");
        assert!(!v.is_complex);
        assert_eq!(v.type_, 0x10, "TYPE_INT_DEC");
        assert_eq!(v.data, 42);
        assert_eq!(v.key_index, 0);

        assert!(
            table.resource_value(0x7e01_0000).is_none(),
            "unknown package"
        );
        assert!(table.resource_value(0x7f02_0000).is_none(), "unknown type");
        assert!(table.resource_value(0x7f01_0001).is_none(), "unknown entry");
    }

    fn build_style_fixture() -> Vec<u8> {
        let mut pool = Vec::new();
        push_u16(&mut pool, RES_STRING_POOL_TYPE);
        push_u16(&mut pool, 28);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0);

        let mut type_chunk = Vec::new();
        let type_header_size = 20u16;
        let entries_start = 24u32;
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, type_header_size);

        push_u32(&mut type_chunk, 64);
        type_chunk.push(8);
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 1);
        push_u32(&mut type_chunk, entries_start);

        push_u32(&mut type_chunk, 0);

        push_u16(&mut type_chunk, 16);
        push_u16(&mut type_chunk, ENTRY_FLAG_COMPLEX);
        push_u32(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 0x7f08_000a);
        push_u32(&mut type_chunk, 2);

        push_u32(&mut type_chunk, 0x7f01_0058);
        push_u16(&mut type_chunk, 8);
        type_chunk.push(0);
        type_chunk.push(0x12);
        push_u32(&mut type_chunk, 0xffff_ffff);

        push_u32(&mut type_chunk, 0x7f01_00a9);
        push_u16(&mut type_chunk, 8);
        type_chunk.push(0);
        type_chunk.push(0x01);
        push_u32(&mut type_chunk, 0x7f0a_0014);

        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut pkg, (PACKAGE_HEADER_MIN + type_chunk.len()) as u32);
        push_u32(&mut pkg, 0x7f);
        pkg.resize(pkg.len() + 256, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
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
        let bytes = build_style_fixture();
        let table = parse_arsc(&bytes).expect("parse style fixture");

        let v = table.resource_value(0x7f08_0000).expect("entry resolves");
        assert!(v.is_complex, "a style entry must surface as complex");

        let style = table.resolve_style(0x7f08_0000).expect("style resolves");
        assert_eq!(style.parent_id, 0x7f08_000a, "parent style id");
        assert_eq!(style.entries.len(), 2);
        assert_eq!(style.entries[0].attr_id, 0x7f01_0058);
        assert_eq!(style.entries[0].type_, 0x12, "TYPE_INT_BOOLEAN");
        assert_eq!(style.entries[0].data, 0xffff_ffff);
        assert_eq!(style.entries[1].attr_id, 0x7f01_00a9);
        assert_eq!(style.entries[1].type_, 0x01, "TYPE_REFERENCE");
        assert_eq!(style.entries[1].data, 0x7f0a_0014);

        let simple_bytes = build_fixture();
        let simple = parse_arsc(&simple_bytes).expect("parse simple fixture");
        assert!(
            simple.resolve_style(0x7f01_0000).is_none(),
            "a simple value entry is not a style bag"
        );

        assert!(simple.resolve_style(0x7f08_0000).is_none(), "unknown style");
    }

    #[test]
    fn find_resource_id_round_trips_with_resource_name_on_real_demo() {
        let Some(bytes) = demo_arsc() else {
            eprintln!("demo arsc unavailable; reverse-lookup parse/fallback covered elsewhere");
            return;
        };
        let table = parse_arsc(&bytes).expect("parse demo arsc");
        let pkg_id = *table.package_ids().first().expect("at least one package");
        let pkg_name = table.package_name(pkg_id).map(str::to_owned);

        let mut found: Option<(String, String, u32)> = None;
        'outer: for tid in 1u8..=32 {
            let Ok(Some(type_name)) = table.type_name(pkg_id, tid) else {
                continue;
            };
            for eid in 0u16..256 {
                let resid = (u32::from(pkg_id) << 24) | (u32::from(tid) << 16) | u32::from(eid);
                let Some(rv) = table.resource_value(resid) else {
                    continue;
                };
                if let Ok(Some(entry_name)) = table.key_name(pkg_id, rv.key_index) {
                    found = Some((type_name.clone(), entry_name, resid));
                    break 'outer;
                }
            }
        }
        let Some((type_name, entry_name, resid)) = found else {
            eprintln!("no concrete entry discovered in demo arsc; skipping round-trip");
            return;
        };

        assert_eq!(
            table.find_resource_id(pkg_name.as_deref(), &type_name, &entry_name),
            Some(resid),
            "find_resource_id must round-trip the forward-resolved id"
        );
        assert_eq!(
            table.find_resource_id(None, &type_name, &entry_name),
            Some(resid),
            "a None package matches any package"
        );

        assert_eq!(
            table.find_resource_id(pkg_name.as_deref(), &type_name, "__eclipse_no_such_entry__"),
            None
        );
    }

    #[test]
    fn resolves_real_demo_style_when_available() {
        let Some(bytes) = demo_arsc() else {
            eprintln!("demo arsc unavailable; covered by hand-built style fixture");
            return;
        };
        let table = parse_arsc(&bytes).expect("parse demo arsc");

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

        let mut found = false;
        for entry in 0u16..0x1000 {
            let id = (0x7fu32 << 24) | (u32::from(tid) << 16) | u32::from(entry);
            if let Some(style) = table.resolve_style(id) {
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
        let mut buf = Vec::new();
        push_u16(&mut buf, RES_STRING_POOL_TYPE);
        push_u16(&mut buf, 8);
        push_u32(&mut buf, 8);
        let err = parse_arsc(&buf).err().expect("non-table root must fail");
        assert_eq!(err, ArscError::NotResTable);
    }

    #[test]
    fn table_without_value_pool_is_typed_error() {
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

    fn parse_err(bytes: &[u8]) -> ArscError {
        parse_arsc(bytes).err().expect("expected a typed ArscError")
    }

    fn empty_value_pool() -> Vec<u8> {
        let mut pool = Vec::new();
        push_u16(&mut pool, RES_STRING_POOL_TYPE);
        push_u16(&mut pool, 28);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 0);
        push_u32(&mut pool, 28);
        push_u32(&mut pool, 0);
        pool
    }

    fn build_table(pkg: &[u8]) -> Vec<u8> {
        let pool = empty_value_pool();
        let mut table = Vec::new();
        push_u16(&mut table, RES_TABLE_TYPE);
        push_u16(&mut table, TABLE_HEADER_SIZE as u16);
        push_u32(
            &mut table,
            (TABLE_HEADER_SIZE + pool.len() + pkg.len()) as u32,
        );
        push_u32(&mut table, 1);
        table.extend_from_slice(&pool);
        table.extend_from_slice(pkg);
        table
    }

    #[test]
    fn bad_root_chunk_header_is_typed_error() {
        let mut b = Vec::new();
        push_u16(&mut b, RES_TABLE_TYPE);
        push_u16(&mut b, 4);
        push_u32(&mut b, 12);
        assert_eq!(parse_err(&b), ArscError::BadChunk);

        let mut b = Vec::new();
        push_u16(&mut b, RES_TABLE_TYPE);
        push_u16(&mut b, 12);
        push_u32(&mut b, 8);
        assert_eq!(parse_err(&b), ArscError::BadChunk);

        let mut b = Vec::new();
        push_u16(&mut b, RES_TABLE_TYPE);
        push_u16(&mut b, 12);
        push_u32(&mut b, 0xFFFF_FFF0);
        assert_eq!(parse_err(&b), ArscError::Truncated);

        let mut b = Vec::new();
        push_u16(&mut b, RES_TABLE_TYPE);
        push_u16(&mut b, 8);
        push_u32(&mut b, 8);
        assert_eq!(parse_err(&b), ArscError::BadChunk);
    }

    #[test]
    fn package_header_too_small_is_bad_package() {
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, 16);
        push_u32(&mut pkg, 16);
        pkg.resize(16, 0);
        let table = build_table(&pkg);
        assert_eq!(parse_err(&table), ArscError::BadPackage);
    }

    #[test]
    fn package_type_or_key_strings_offset_past_chunk_is_bad_package() {
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut pkg, PACKAGE_HEADER_MIN as u32);
        push_u32(&mut pkg, 0x7f);
        pkg.resize(pkg.len() + 256, 0);
        push_u32(&mut pkg, 0xFFFF_FFF0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        let table = build_table(&pkg);
        let err = parse_err(&table);
        assert!(
            matches!(err, ArscError::BadPackage | ArscError::Overflow),
            "got {err:?}"
        );
    }

    fn build_package_with_type(type_chunk: &[u8]) -> Vec<u8> {
        let mut pkg = Vec::new();
        push_u16(&mut pkg, RES_TABLE_PACKAGE_TYPE);
        push_u16(&mut pkg, PACKAGE_HEADER_MIN as u16);
        push_u32(&mut pkg, (PACKAGE_HEADER_MIN + type_chunk.len()) as u32);
        push_u32(&mut pkg, 0x7f);
        pkg.resize(pkg.len() + 256, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        push_u32(&mut pkg, 0);
        pkg.extend_from_slice(type_chunk);
        pkg
    }

    #[test]
    fn type_chunk_huge_entry_count_does_not_overrun_or_alloc() {
        let mut type_chunk = Vec::new();
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, TYPE_HEADER_MIN as u16);
        push_u32(&mut type_chunk, TYPE_HEADER_MIN as u32);
        type_chunk.push(1);
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 0xFFFF_FFFF);
        push_u32(&mut type_chunk, TYPE_HEADER_MIN as u32);
        let pkg = build_package_with_type(&type_chunk);
        let table = build_table(&pkg);

        let parsed = parse_arsc(&table);
        if let Ok(t) = parsed {
            assert!(t.resolve(0x7f, 1, 0).is_none());
            assert!(t.resolve(0x7f, 1, 0xFFFF).is_none());
            assert!(t.resolve_style(0x7f01_0000).is_none());
        }
    }

    #[test]
    fn type_entry_offset_past_type_data_is_none() {
        let mut type_chunk = Vec::new();
        let header_size = TYPE_HEADER_MIN as u16;
        let entries_start = (TYPE_HEADER_MIN + 4) as u32;
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, header_size);

        push_u32(&mut type_chunk, (TYPE_HEADER_MIN + 4) as u32);
        type_chunk.push(1);
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 1);
        push_u32(&mut type_chunk, entries_start);
        push_u32(&mut type_chunk, 0xFFFF_FF00);
        let pkg = build_package_with_type(&type_chunk);
        let table = build_table(&pkg);
        if let Ok(t) = parse_arsc(&table) {
            assert!(t.resolve(0x7f, 1, 0).is_none(), "offset past data ⇒ None");
        }
    }

    #[test]
    fn entry_size_below_minimum_is_none() {
        let mut type_chunk = Vec::new();
        let header_size = TYPE_HEADER_MIN as u16;
        let entries_start = (TYPE_HEADER_MIN + 4) as u32;
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, header_size);
        let size_pos = type_chunk.len();
        push_u32(&mut type_chunk, 0);
        type_chunk.push(1);
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 1);
        push_u32(&mut type_chunk, entries_start);
        push_u32(&mut type_chunk, 0);

        push_u16(&mut type_chunk, 4);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 0);
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
        let mut type_chunk = Vec::new();
        let header_size = TYPE_HEADER_MIN as u16;
        let entries_start = (TYPE_HEADER_MIN + 4) as u32;
        push_u16(&mut type_chunk, RES_TABLE_TYPE_TYPE);
        push_u16(&mut type_chunk, header_size);
        let size_pos = type_chunk.len();
        push_u32(&mut type_chunk, 0);
        type_chunk.push(8);
        type_chunk.push(0);
        push_u16(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 1);
        push_u32(&mut type_chunk, entries_start);
        push_u32(&mut type_chunk, 0);

        push_u16(&mut type_chunk, 16);
        push_u16(&mut type_chunk, ENTRY_FLAG_COMPLEX);
        push_u32(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 0);
        push_u32(&mut type_chunk, 0xFFFF_FFFF);

        push_u32(&mut type_chunk, 0x7f01_0058);
        push_u16(&mut type_chunk, 8);
        type_chunk.push(0);
        type_chunk.push(0x10);
        push_u32(&mut type_chunk, 1);
        let total = type_chunk.len() as u32;
        type_chunk[size_pos..size_pos + 4].copy_from_slice(&total.to_le_bytes());
        let pkg = build_package_with_type(&type_chunk);
        let table = build_table(&pkg);
        let t = parse_arsc(&table).expect("parse hostile-bag table");
        let style = t
            .resolve_style(0x7f08_0000)
            .expect("style entry resolves (bounded)");

        assert_eq!(
            style.entries.len(),
            1,
            "hostile count must be bounded by the chunk, not 4 billion"
        );
    }

    #[test]
    fn too_many_packages_is_typed_error() {
        let pool = empty_value_pool();

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
        let bytes = build_fixture();
        let t = parse_arsc(&bytes).expect("parse fixture");
        assert!(t.resolve(0x01, 1, 0).is_none(), "unknown package id");
        assert!(t.resolve(0x7f, 0xff, 0).is_none(), "unknown type id");
        assert!(t.resolve(0x7f, 1, 0xffff).is_none(), "unknown entry id");
        assert!(t.resolve_style(0x0108_0000).is_none(), "unknown style pkg");

        assert!(t.type_name(0x01, 1).unwrap().is_none());
        assert!(t.key_name(0x01, 0).unwrap().is_none());
        assert!(t.type_name(0x7f, 0).unwrap().is_none(), "type id 0 ⇒ None");
    }

    #[test]
    fn string_pool_with_bad_type_is_bad_string_pool() {
        let mut not_a_pool = Vec::new();
        push_u16(&mut not_a_pool, 0x9999);
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
        let base = demo_arsc().unwrap_or_else(build_fixture);

        for len in 0..=base.len() {
            if let Ok(table) = parse_arsc(&base[..len]) {
                let _ = table.resource_value(0x7f01_0000);
                let _ = table.resolve_style(0x7f08_0000);
                let _ = table.type_name(0x7f, 1);
            }
        }

        let stride = 7;
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
