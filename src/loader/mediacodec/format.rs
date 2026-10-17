use std::ffi::{c_char, c_void, CStr, CString};
use std::fmt;

use super::{c_text, MediaStatus, AMEDIA_ERROR_INVALID_OBJECT, AMEDIA_OK};
use crate::loader::ndk_registry::{handle_to_ptr, ptr_to_handle, NdkRegistryError, Slab};

const MAX_FORMAT_ENTRIES: usize = 64;

pub(super) enum FormatValue {
    Int32(i32),
    Float(f32),
    String(CString),
    Buffer(Box<[u8]>),
}

impl fmt::Display for FormatValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int32(value) => write!(f, "int32({value})"),
            Self::Float(value) => write!(f, "float({value:.6})"),
            Self::String(value) => write!(f, "string({})", value.to_string_lossy()),
            Self::Buffer(bytes) => write!(f, "data({} bytes)", bytes.len()),
        }
    }
}

struct Description {
    revision: u64,
    text: CString,
}

#[derive(Default)]
pub(super) struct MediaFormat {
    entries: Vec<(CString, FormatValue)>,
    revision: u64,
    description: Option<Description>,
}

impl MediaFormat {
    fn position(&self, key: &CStr) -> Option<usize> {
        self.entries
            .iter()
            .position(|(name, _)| name.as_c_str() == key)
    }

    fn value(&self, key: &CStr) -> Option<&FormatValue> {
        let index = self.position(key)?;
        Some(&self.entries[index].1)
    }

    fn value_mut(&mut self, key: &CStr) -> Option<&mut FormatValue> {
        let index = self.position(key)?;
        Some(&mut self.entries[index].1)
    }

    pub(super) fn int32(&self, key: &CStr) -> Option<i32> {
        match self.value(key)? {
            FormatValue::Int32(value) => Some(*value),
            FormatValue::Float(_) | FormatValue::String(_) | FormatValue::Buffer(_) => None,
        }
    }

    pub(super) fn buffer(&self, key: &CStr) -> Option<&[u8]> {
        match self.value(key)? {
            FormatValue::Buffer(bytes) => Some(bytes),
            FormatValue::Int32(_) | FormatValue::Float(_) | FormatValue::String(_) => None,
        }
    }

    pub(super) fn summary(&self) -> String {
        self.entries
            .iter()
            .map(|(key, value)| format!("{}: {value}", key.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn set(&mut self, key: &CStr, value: FormatValue) -> Result<(), Rejected> {
        match self.position(key) {
            Some(index) => self.entries[index].1 = value,
            None if self.entries.len() >= MAX_FORMAT_ENTRIES => return Err(Rejected::Full),
            None => self.entries.push((key.to_owned(), value)),
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    fn description(&mut self) -> *const c_char {
        let revision = self.revision;
        if let Some(current) = self
            .description
            .as_ref()
            .filter(|cached| cached.revision == revision)
        {
            return current.text.as_ptr();
        }
        let text = CString::new(self.summary()).expect("format keys and values hold no NUL byte");
        let pointer = text.as_ptr();
        self.description = Some(Description { revision, text });
        pointer
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejected {
    NullKey,
    NullValue,
    Full,
    InvalidObject(NdkRegistryError),
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullKey => f.write_str("the key is NULL"),
            Self::NullValue => f.write_str("the value is NULL"),
            Self::Full => write!(f, "the format already holds {MAX_FORMAT_ENTRIES} keys"),
            Self::InvalidObject(error) => write!(f, "not a live AMediaFormat: {error}"),
        }
    }
}

fn formats() -> &'static Slab<MediaFormat> {
    static FORMATS: Slab<MediaFormat> = Slab::new();
    &FORMATS
}

fn with_format<R>(
    format: *mut c_void,
    f: impl FnOnce(&mut MediaFormat) -> R,
) -> Result<R, Rejected> {
    formats()
        .with(ptr_to_handle(format), f)
        .map_err(Rejected::InvalidObject)
}

pub(super) fn read<R>(
    format: *const c_void,
    f: impl FnOnce(&MediaFormat) -> R,
) -> Result<R, NdkRegistryError> {
    formats().with(ptr_to_handle(format), |media_format| f(media_format))
}

pub(super) fn create(
    entries: Vec<(&'static CStr, FormatValue)>,
) -> Result<*mut c_void, NdkRegistryError> {
    let media_format = MediaFormat {
        entries: entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        ..MediaFormat::default()
    };
    formats().insert(media_format).map(handle_to_ptr)
}

fn store(call: &'static str, format: *mut c_void, key: Option<&CStr>, value: Option<FormatValue>) {
    let stored = match (key, value) {
        (None, _) => Err(Rejected::NullKey),
        (Some(_), None) => Err(Rejected::NullValue),
        (Some(key), Some(value)) => {
            with_format(format, |media_format| media_format.set(key, value))
                .and_then(|stored| stored)
        }
    };
    if let Err(reason) = stored {
        tracing::warn!(target: "mediandk", ?format, ?key, %reason, "{call} dropped its value");
    }
}

fn find<R>(
    call: &'static str,
    format: *mut c_void,
    key: Option<&CStr>,
    pick: impl FnOnce(&mut FormatValue) -> Option<R>,
) -> Option<R> {
    let found = match key {
        None => Err(Rejected::NullKey),
        Some(key) => with_format(format, |media_format| {
            media_format.value_mut(key).and_then(pick)
        }),
    };
    found.unwrap_or_else(|reason| {
        tracing::warn!(target: "mediandk", ?format, ?key, %reason, "{call} found no value");
        None
    })
}

pub(super) extern "C" fn eclipse_amediaformat_new() -> *mut c_void {
    let format = match formats().insert(MediaFormat::default()) {
        Ok(handle) => handle_to_ptr(handle),
        Err(error) => {
            tracing::warn!(target: "mediandk", %error, "AMediaFormat_new failed");
            std::ptr::null_mut()
        }
    };
    tracing::debug!(target: "mediandk", ?format, "AMediaFormat_new");
    format
}

pub(super) unsafe extern "C" fn eclipse_amediaformat_delete(format: *mut c_void) -> MediaStatus {
    let status = match formats().remove(ptr_to_handle(format)) {
        Ok(()) => AMEDIA_OK,
        Err(error) => {
            tracing::warn!(target: "mediandk", ?format, %error, "AMediaFormat_delete of a dead format");
            AMEDIA_ERROR_INVALID_OBJECT
        }
    };
    tracing::debug!(target: "mediandk", ?format, status, "AMediaFormat_delete");
    status
}

pub(super) unsafe extern "C" fn eclipse_amediaformat_getint32(
    format: *mut c_void,
    name: *const c_char,
    out: *mut i32,
) -> bool {
    let key = unsafe { c_text(name) };
    let value = find("AMediaFormat_getInt32", format, key, |value| match value {
        FormatValue::Int32(value) => Some(*value),
        _ => None,
    });
    tracing::debug!(target: "mediandk", ?format, ?key, ?value, "AMediaFormat_getInt32");
    let Some(value) = value else {
        return false;
    };
    if out.is_null() {
        tracing::warn!(target: "mediandk", ?format, ?key, "AMediaFormat_getInt32 has a NULL output");
        return false;
    }
    unsafe { out.write(value) };
    true
}

pub(super) unsafe extern "C" fn eclipse_amediaformat_getbuffer(
    format: *mut c_void,
    name: *const c_char,
    data: *mut *mut c_void,
    size: *mut usize,
) -> bool {
    let key = unsafe { c_text(name) };
    let buffer = find("AMediaFormat_getBuffer", format, key, |value| match value {
        FormatValue::Buffer(bytes) => Some((bytes.as_mut_ptr(), bytes.len())),
        _ => None,
    });
    tracing::debug!(
        target: "mediandk",
        ?format,
        ?key,
        size = ?buffer.map(|(_, len)| len),
        "AMediaFormat_getBuffer"
    );
    let Some((bytes, len)) = buffer else {
        return false;
    };
    if data.is_null() || size.is_null() {
        tracing::warn!(target: "mediandk", ?format, ?key, "AMediaFormat_getBuffer has a NULL output");
        return false;
    }
    unsafe {
        data.write(bytes.cast());
        size.write(len);
    }
    true
}

pub(super) unsafe extern "C" fn eclipse_amediaformat_setint32(
    format: *mut c_void,
    name: *const c_char,
    value: i32,
) {
    let key = unsafe { c_text(name) };
    tracing::debug!(target: "mediandk", ?format, ?key, value, "AMediaFormat_setInt32");
    store(
        "AMediaFormat_setInt32",
        format,
        key,
        Some(FormatValue::Int32(value)),
    );
}

pub(super) unsafe extern "C" fn eclipse_amediaformat_setfloat(
    format: *mut c_void,
    name: *const c_char,
    value: f32,
) {
    let key = unsafe { c_text(name) };
    tracing::debug!(target: "mediandk", ?format, ?key, value, "AMediaFormat_setFloat");
    store(
        "AMediaFormat_setFloat",
        format,
        key,
        Some(FormatValue::Float(value)),
    );
}

pub(super) unsafe extern "C" fn eclipse_amediaformat_setstring(
    format: *mut c_void,
    name: *const c_char,
    value: *const c_char,
) {
    let key = unsafe { c_text(name) };
    let text = unsafe { c_text(value) };
    tracing::debug!(target: "mediandk", ?format, ?key, value = ?text, "AMediaFormat_setString");
    store(
        "AMediaFormat_setString",
        format,
        key,
        text.map(|text| FormatValue::String(text.to_owned())),
    );
}

pub(super) unsafe extern "C" fn eclipse_amediaformat_setbuffer(
    format: *mut c_void,
    name: *const c_char,
    data: *const c_void,
    size: usize,
) {
    let key = unsafe { c_text(name) };
    tracing::debug!(target: "mediandk", ?format, ?key, size, "AMediaFormat_setBuffer");
    let bytes = match (data.is_null(), size) {
        (_, 0) => Some(Box::default()),
        (true, _) => None,
        (false, size) => Some(Box::from(unsafe {
            std::slice::from_raw_parts(data.cast::<u8>(), size)
        })),
    };
    store(
        "AMediaFormat_setBuffer",
        format,
        key,
        bytes.map(FormatValue::Buffer),
    );
}

pub(super) unsafe extern "C" fn eclipse_amediaformat_tostring(
    format: *mut c_void,
) -> *const c_char {
    let description = with_format(format, MediaFormat::description);
    tracing::debug!(
        target: "mediandk",
        ?format,
        found = description.is_ok(),
        "AMediaFormat_toString"
    );
    description.unwrap_or_else(|reason| {
        tracing::warn!(target: "mediandk", ?format, %reason, "AMediaFormat_toString of a dead format");
        c"".as_ptr()
    })
}

#[cfg(test)]
mod tests;
