//! Wire protocol v1 for the `eclipse-webview` helper — the NORMATIVE message-set/framing spec
//! (docs/web-engine-plan.md M2 keeps only the summary; this file is the source of truth).
//!
//! 2026-07-03: an owned, std-only protocol over ONE `SOCK_STREAM` `UnixStream` (the control
//! socket from the spawn contract in [`super`]) — no tokio, no async runtime; blocking
//! `read_exact`/`write_all` on dedicated std threads.
//!
//! # Framing (v1, FROZEN by the round-trip pins below)
//!
//! Every frame is `[len: u32 LE][type: u8][body: len-1 bytes]` — `len` counts type+body.
//! Byte order is LE throughout; strings are `[u32 LE byte-len][UTF-8, validated]`; bools are
//! one byte `0`/`1` (anything else is malformed). The global cap `len <= 8 MiB` is enforced
//! BEFORE any allocation, plus per-type caps (default 64 KiB; `LoadUrl` 32 KiB;
//! `LoadDataWithBaseUrl`/`EvaluateJs` 8 MiB).
//!
//! **Exact-length reads only:** the decoder reads exactly the bytes each frame declares and
//! NEVER buffers ahead — the byte after a decoded [`HelperMsg::FrameBufferNew`] frame is the
//! [`super::fdpass`] sentinel carrying the memfd via `SCM_RIGHTS`, and a buffered reader would
//! swallow it and silently drop the fd (pinned by the fdpass unit test). Do not wrap the
//! control socket in `BufReader`.
//!
//! # Handshake
//!
//! The first consumer→helper frame MUST be [`ConsumerMsg::Hello`] (magic `b"ECWV"`, version);
//! the first helper→consumer frame MUST be [`HelperMsg::HelloAck`]. v1 requires an exact
//! version match: on a bad magic the helper treats the stream as malformed (close); on an
//! unsupported version the helper sends `HelloAck` carrying ITS version then closes, so the
//! consumer can raise an actionable mismatch error. The helper exits if no `Hello` arrives
//! within 10 s of spawn. Wire evolution is gated solely by this version — v1 never skips
//! unknown frames.
//!
//! # Malformed input (both sides, symmetric — never UB, never a panic)
//!
//! Unknown type byte, len over cap, truncated body/EOF mid-frame, invalid UTF-8, non-0/1
//! bool, or a handshake-order violation → the codec returns a typed [`ProtoError`] (a total
//! decoder — the `src/apk/axml.rs` house shape; the root ships `panic = "abort"`). The owning
//! side logs ONE loud payload-free line (type byte, declared len, error kind — never body
//! bytes), stops reading, and closes the socket; the helper then quits the CEF loop, closes
//! all browsers, shuts CEF down, and exits with code 2; the consumer kills+reaps the helper.
//! Unexpected EOF is treated identically minus the log severity.

#![forbid(unsafe_code)]

use std::io::Read;

/// The `Hello` magic. A stream not starting with a `Hello` carrying this magic is malformed.
pub const MAGIC: [u8; 4] = *b"ECWV";

/// Global frame cap (`len`, i.e. type+body), enforced before any allocation.
pub const GLOBAL_FRAME_CAP: u32 = 8 * 1024 * 1024;
/// Default per-type frame cap.
const DEFAULT_CAP: u32 = 64 * 1024;
/// `LoadUrl` frame cap (a URL, never a document).
const LOAD_URL_CAP: u32 = 32 * 1024;
/// `LoadDataWithBaseUrl` / `EvaluateJs` frame cap (inline documents/scripts).
const PAYLOAD_CAP: u32 = GLOBAL_FRAME_CAP;

/// v1 consumer→helper type bytes (0x01–0x10).
mod ct {
    pub(super) const HELLO: u8 = 0x01;
    pub(super) const CREATE_VIEW: u8 = 0x02;
    pub(super) const CLOSE_VIEW: u8 = 0x03;
    pub(super) const RESIZE_VIEW: u8 = 0x04;
    pub(super) const LOAD_URL: u8 = 0x05;
    pub(super) const LOAD_DATA_WITH_BASE_URL: u8 = 0x06;
    pub(super) const MOUSE_MOVE: u8 = 0x07;
    pub(super) const MOUSE_CLICK: u8 = 0x08;
    pub(super) const MOUSE_WHEEL: u8 = 0x09;
    pub(super) const KEY: u8 = 0x0A;
    pub(super) const EVALUATE_JS: u8 = 0x0B;
    pub(super) const COOKIE_SET: u8 = 0x0C;
    pub(super) const COOKIE_GET: u8 = 0x0D;
    pub(super) const COOKIES_CLEAR: u8 = 0x0E;
    pub(super) const FRAME_ACK: u8 = 0x0F;
    pub(super) const SHUTDOWN: u8 = 0x10;
}

/// v1 helper→consumer type bytes (0x81–0x88).
mod ht {
    pub(super) const HELLO_ACK: u8 = 0x81;
    pub(super) const LOAD_STATE: u8 = 0x82;
    pub(super) const FRAME_BUFFER_NEW: u8 = 0x83;
    pub(super) const FRAME_READY: u8 = 0x84;
    pub(super) const CONSOLE: u8 = 0x85;
    pub(super) const CRASH: u8 = 0x86;
    pub(super) const COOKIE_LIST: u8 = 0x87;
    pub(super) const VIEW_CLOSED: u8 = 0x88;
}

/// Typed protocol error. Deliberately PAYLOAD-FREE: variants carry the type byte, declared
/// length, and error kind — never body bytes (the malformed-input log line must not leak).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// Clean end-of-stream at a frame boundary (the peer closed; not a wire violation).
    Eof,
    /// EOF in the middle of a frame (header or declared body).
    Truncated,
    /// A non-EOF I/O failure on the stream.
    Io(std::io::ErrorKind),
    /// Declared `len == 0` (a frame must at least carry its type byte).
    EmptyFrame,
    /// Declared length exceeds the global or per-type cap (checked before allocation).
    Oversized {
        type_byte: Option<u8>,
        declared_len: u32,
        cap: u32,
    },
    /// Type byte unknown to v1 (in this direction). v1 never skips unknown frames.
    UnknownType { type_byte: u8 },
    /// The body ended before the message's fields were fully consumed.
    TruncatedBody { type_byte: u8 },
    /// The body was longer than the message's fields consume.
    TrailingBytes { type_byte: u8, extra: usize },
    /// A bool byte that was neither 0 nor 1.
    BadBool { type_byte: u8, value: u8 },
    /// A string field that was not valid UTF-8.
    BadUtf8 { type_byte: u8 },
    /// A `Hello` whose magic is not [`MAGIC`] (the bytes are deliberately not carried).
    BadMagic,
    /// A field value outside its v1-legal set (named, payload-free).
    BadValue { type_byte: u8, what: &'static str },
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => write!(f, "clean EOF at frame boundary"),
            Self::Truncated => write!(f, "unexpected EOF mid-frame"),
            Self::Io(kind) => write!(f, "stream I/O error: {kind}"),
            Self::EmptyFrame => write!(f, "declared frame length 0"),
            Self::Oversized {
                type_byte,
                declared_len,
                cap,
            } => match type_byte {
                Some(t) => write!(
                    f,
                    "frame type 0x{t:02X} declared len {declared_len} exceeds cap {cap}"
                ),
                None => write!(f, "declared len {declared_len} exceeds global cap {cap}"),
            },
            Self::UnknownType { type_byte } => write!(f, "unknown frame type 0x{type_byte:02X}"),
            Self::TruncatedBody { type_byte } => {
                write!(
                    f,
                    "frame type 0x{type_byte:02X}: body shorter than its fields"
                )
            }
            Self::TrailingBytes { type_byte, extra } => write!(
                f,
                "frame type 0x{type_byte:02X}: {extra} trailing byte(s) after its fields"
            ),
            Self::BadBool { type_byte, value } => write!(
                f,
                "frame type 0x{type_byte:02X}: bool byte {value} (must be 0 or 1)"
            ),
            Self::BadUtf8 { type_byte } => {
                write!(
                    f,
                    "frame type 0x{type_byte:02X}: invalid UTF-8 in string field"
                )
            }
            Self::BadMagic => write!(f, "Hello magic mismatch (expected \"ECWV\")"),
            Self::BadValue { type_byte, what } => {
                write!(f, "frame type 0x{type_byte:02X}: illegal value for {what}")
            }
        }
    }
}

impl std::error::Error for ProtoError {}

/// One cookie in a [`HelperMsg::CookieList`] reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
}

/// A console event whose text is STRUCTURALLY absent: the only public constructor,
/// [`Console::from_raw`], redacts the source URL through the shared contract
/// ([`super::redact::url_scheme_and_host_for_log`]) and stores only the text's byte length —
/// no consumer at any milestone can log console text it never received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Console {
    severity: u8,
    /// Pre-redacted scheme+host (or `"<non-url>"`). Private: unforgeable outside this module.
    source: String,
    line: u32,
    message_len: u32,
}

impl Console {
    /// Build a console event from the RAW engine callback values. The raw source URL is
    /// redacted here (scheme+host only) and `message_text` is dropped — only its length
    /// survives. This is the ONLY way to construct a [`Console`].
    pub fn from_raw(severity: u8, raw_source_url: &str, line: u32, message_text: &str) -> Self {
        Self {
            severity,
            source: super::redact::url_scheme_and_host_for_log(raw_source_url),
            line,
            message_len: u32::try_from(message_text.len()).unwrap_or(u32::MAX),
        }
    }

    pub fn severity(&self) -> u8 {
        self.severity
    }

    /// The pre-redacted source (scheme+host or `"<non-url>"`) — safe to log verbatim.
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn line(&self) -> u32 {
        self.line
    }

    pub fn message_len(&self) -> u32 {
        self.message_len
    }
}

/// Consumer→helper messages (type bytes 0x01–0x10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumerMsg {
    /// 0x01 — MUST be the first frame on the stream. The magic is implicit (written at
    /// encode, validated at decode) so a well-typed `Hello` cannot carry a wrong magic.
    Hello { version: u16 },
    /// 0x02 — `view` is the existing `view_registry` jlong handle (>= 1; no new registry).
    CreateView { view: i64, width: u16, height: u16 },
    /// 0x03
    CloseView { view: i64 },
    /// 0x04
    ResizeView { view: i64, width: u16, height: u16 },
    /// 0x05 — the full URL crosses the wire (the helper must load it); it is never logged
    /// raw on either side (redaction contract).
    LoadUrl { view: i64, url: String },
    /// 0x06 — the Android 5-arg `loadDataWithBaseURL` shape.
    LoadDataWithBaseUrl {
        view: i64,
        base_url: String,
        data: String,
        mime: String,
        encoding: String,
        history_url: String,
    },
    /// 0x07
    MouseMove {
        view: i64,
        x: i32,
        y: i32,
        modifiers: u32,
        leave: bool,
    },
    /// 0x08 — `button`: 0=left, 1=middle, 2=right.
    MouseClick {
        view: i64,
        x: i32,
        y: i32,
        button: u8,
        down: bool,
        click_count: u8,
        modifiers: u32,
    },
    /// 0x09
    MouseWheel {
        view: i64,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    },
    /// 0x0A — `kind`: 0=down, 1=up, 2=char (the `cef_key_event_t` field set).
    Key {
        view: i64,
        kind: u8,
        windows_key_code: i32,
        native_key_code: i32,
        character: u16,
        modifiers: u32,
    },
    /// 0x0B — fire-and-forget in v1 (the result path is M4 work and a version bump).
    EvaluateJs { view: i64, script: String },
    /// 0x0C — the overlay 3-arg `setCookie`/`.ROBLOSECURITY` handoff shape.
    /// `expires_epoch_s == 0` means a session cookie.
    CookieSet {
        url: String,
        name: String,
        value: String,
        domain: String,
        path: String,
        secure: bool,
        http_only: bool,
        expires_epoch_s: i64,
    },
    /// 0x0D — solicits a [`HelperMsg::CookieList`] correlated by `request_id`.
    CookieGet { request_id: u32, url: String },
    /// 0x0E — solicits an empty [`HelperMsg::CookieList`] (completion) by `request_id`.
    CookiesClear { request_id: u32 },
    /// 0x0F — releases the published slot named by the matching [`HelperMsg::FrameReady`].
    FrameAck {
        view: i64,
        generation: u32,
        seq: u32,
    },
    /// 0x10
    Shutdown,
}

/// Helper→consumer messages (type bytes 0x81–0x88).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperMsg {
    /// 0x81 — MUST be the first frame on the stream. `engine` names the backing engine,
    /// e.g. `"cef/149.0.6+g0d0eeb6+chromium-149.0.7827.201"`.
    HelloAck { version: u16, engine: String },
    /// 0x82 — exactly the `internalLoadChanged` codes: 0 = started (`OnLoadStart`
    /// main-frame), 3 = finished (`OnLoadEnd` main-frame). `http_status` finished-only
    /// (else 0).
    LoadState {
        view: i64,
        state: u8,
        http_status: i32,
    },
    /// 0x83 — a new buffer generation. Immediately followed ON THE STREAM by one sentinel
    /// byte (`0xF5`) sent via `sendmsg` with `SCM_RIGHTS` carrying the memfd (see
    /// [`super::fdpass`]). v1 fixes `slot_count = 2` and `stride = 4 * width` (validated).
    FrameBufferNew {
        view: i64,
        generation: u32,
        width: u16,
        height: u16,
        stride: u32,
        slot_bytes: u32,
        slot_count: u8,
    },
    /// 0x84 — slot `slot` of `generation` holds a complete frame; the consumer may read it
    /// until it sends the matching [`ConsumerMsg::FrameAck`]. At most one in flight.
    FrameReady {
        view: i64,
        generation: u32,
        slot: u8,
        seq: u32,
    },
    /// 0x85 — structurally text-free console event (see [`Console`]).
    Console { view: i64, console: Console },
    /// 0x86 — `view == 0` is process-global. `kind`: 0=renderer-gone, 1=engine-init-failed
    /// (e.g. no display / ozone), 2=internal.
    Crash { view: i64, kind: u8, code: i32 },
    /// 0x87 — the solicited reply to `CookieGet`/`CookiesClear`, correlated by `request_id`.
    CookieList {
        request_id: u32,
        cookies: Vec<CookieEntry>,
    },
    /// 0x88 — `CloseView` completion (the M3 re-load/teardown semantics hook).
    ViewClosed { view: i64 },
}

/// Which side's message set a decoder expects (the two type-byte namespaces are disjoint,
/// and each direction rejects the other's bytes as unknown).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    FromConsumer,
    FromHelper,
}

/// Per-type frame cap (`len`, type+body). `None` = unknown type in this direction.
fn type_cap(dir: Dir, type_byte: u8) -> Option<u32> {
    match dir {
        Dir::FromConsumer => match type_byte {
            ct::LOAD_URL => Some(LOAD_URL_CAP),
            ct::LOAD_DATA_WITH_BASE_URL | ct::EVALUATE_JS => Some(PAYLOAD_CAP),
            ct::HELLO
            | ct::CREATE_VIEW
            | ct::CLOSE_VIEW
            | ct::RESIZE_VIEW
            | ct::MOUSE_MOVE
            | ct::MOUSE_CLICK
            | ct::MOUSE_WHEEL
            | ct::KEY
            | ct::COOKIE_SET
            | ct::COOKIE_GET
            | ct::COOKIES_CLEAR
            | ct::FRAME_ACK
            | ct::SHUTDOWN => Some(DEFAULT_CAP),
            _ => None,
        },
        Dir::FromHelper => match type_byte {
            ht::HELLO_ACK
            | ht::LOAD_STATE
            | ht::FRAME_BUFFER_NEW
            | ht::FRAME_READY
            | ht::CONSOLE
            | ht::CRASH
            | ht::COOKIE_LIST
            | ht::VIEW_CLOSED => Some(DEFAULT_CAP),
            _ => None,
        },
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_i32(buf: &mut Vec<u8>, v: i32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_bool(buf: &mut Vec<u8>, v: bool) {
    buf.push(u8::from(v));
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, u32::try_from(s.len()).unwrap_or(u32::MAX));
    buf.extend_from_slice(s.as_bytes());
}

/// Compose `[len][type][body]`, enforcing the same caps the decoder enforces (symmetric: an
/// oversized message fails HERE with a typed error instead of shipping a doomed frame).
fn compose_frame(dir: Dir, type_byte: u8, body: Vec<u8>) -> Result<Vec<u8>, ProtoError> {
    let declared_len = u32::try_from(body.len().saturating_add(1)).unwrap_or(u32::MAX);
    // Unwrap-free: every type byte passed here is a v1 const of the right direction.
    let cap = type_cap(dir, type_byte).unwrap_or(0).min(GLOBAL_FRAME_CAP);
    if declared_len > cap {
        return Err(ProtoError::Oversized {
            type_byte: Some(type_byte),
            declared_len,
            cap,
        });
    }
    let mut out = Vec::with_capacity(4 + 1 + body.len());
    out.extend_from_slice(&declared_len.to_le_bytes());
    out.push(type_byte);
    out.extend_from_slice(&body);
    Ok(out)
}

impl ConsumerMsg {
    /// Encode into a complete frame (`[len][type][body]`), enforcing the v1 caps.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut b = Vec::new();
        let t = match self {
            Self::Hello { version } => {
                b.extend_from_slice(&MAGIC);
                put_u16(&mut b, *version);
                ct::HELLO
            }
            Self::CreateView {
                view,
                width,
                height,
            } => {
                put_i64(&mut b, *view);
                put_u16(&mut b, *width);
                put_u16(&mut b, *height);
                ct::CREATE_VIEW
            }
            Self::CloseView { view } => {
                put_i64(&mut b, *view);
                ct::CLOSE_VIEW
            }
            Self::ResizeView {
                view,
                width,
                height,
            } => {
                put_i64(&mut b, *view);
                put_u16(&mut b, *width);
                put_u16(&mut b, *height);
                ct::RESIZE_VIEW
            }
            Self::LoadUrl { view, url } => {
                put_i64(&mut b, *view);
                put_str(&mut b, url);
                ct::LOAD_URL
            }
            Self::LoadDataWithBaseUrl {
                view,
                base_url,
                data,
                mime,
                encoding,
                history_url,
            } => {
                put_i64(&mut b, *view);
                put_str(&mut b, base_url);
                put_str(&mut b, data);
                put_str(&mut b, mime);
                put_str(&mut b, encoding);
                put_str(&mut b, history_url);
                ct::LOAD_DATA_WITH_BASE_URL
            }
            Self::MouseMove {
                view,
                x,
                y,
                modifiers,
                leave,
            } => {
                put_i64(&mut b, *view);
                put_i32(&mut b, *x);
                put_i32(&mut b, *y);
                put_u32(&mut b, *modifiers);
                put_bool(&mut b, *leave);
                ct::MOUSE_MOVE
            }
            Self::MouseClick {
                view,
                x,
                y,
                button,
                down,
                click_count,
                modifiers,
            } => {
                put_i64(&mut b, *view);
                put_i32(&mut b, *x);
                put_i32(&mut b, *y);
                b.push(*button);
                put_bool(&mut b, *down);
                b.push(*click_count);
                put_u32(&mut b, *modifiers);
                ct::MOUSE_CLICK
            }
            Self::MouseWheel {
                view,
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => {
                put_i64(&mut b, *view);
                put_i32(&mut b, *x);
                put_i32(&mut b, *y);
                put_i32(&mut b, *delta_x);
                put_i32(&mut b, *delta_y);
                put_u32(&mut b, *modifiers);
                ct::MOUSE_WHEEL
            }
            Self::Key {
                view,
                kind,
                windows_key_code,
                native_key_code,
                character,
                modifiers,
            } => {
                put_i64(&mut b, *view);
                b.push(*kind);
                put_i32(&mut b, *windows_key_code);
                put_i32(&mut b, *native_key_code);
                put_u16(&mut b, *character);
                put_u32(&mut b, *modifiers);
                ct::KEY
            }
            Self::EvaluateJs { view, script } => {
                put_i64(&mut b, *view);
                put_str(&mut b, script);
                ct::EVALUATE_JS
            }
            Self::CookieSet {
                url,
                name,
                value,
                domain,
                path,
                secure,
                http_only,
                expires_epoch_s,
            } => {
                put_str(&mut b, url);
                put_str(&mut b, name);
                put_str(&mut b, value);
                put_str(&mut b, domain);
                put_str(&mut b, path);
                put_bool(&mut b, *secure);
                put_bool(&mut b, *http_only);
                put_i64(&mut b, *expires_epoch_s);
                ct::COOKIE_SET
            }
            Self::CookieGet { request_id, url } => {
                put_u32(&mut b, *request_id);
                put_str(&mut b, url);
                ct::COOKIE_GET
            }
            Self::CookiesClear { request_id } => {
                put_u32(&mut b, *request_id);
                ct::COOKIES_CLEAR
            }
            Self::FrameAck {
                view,
                generation,
                seq,
            } => {
                put_i64(&mut b, *view);
                put_u32(&mut b, *generation);
                put_u32(&mut b, *seq);
                ct::FRAME_ACK
            }
            Self::Shutdown => ct::SHUTDOWN,
        };
        compose_frame(Dir::FromConsumer, t, b)
    }
}

impl HelperMsg {
    /// Encode into a complete frame (`[len][type][body]`), enforcing the v1 caps.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut b = Vec::new();
        let t = match self {
            Self::HelloAck { version, engine } => {
                put_u16(&mut b, *version);
                put_str(&mut b, engine);
                ht::HELLO_ACK
            }
            Self::LoadState {
                view,
                state,
                http_status,
            } => {
                put_i64(&mut b, *view);
                b.push(*state);
                put_i32(&mut b, *http_status);
                ht::LOAD_STATE
            }
            Self::FrameBufferNew {
                view,
                generation,
                width,
                height,
                stride,
                slot_bytes,
                slot_count,
            } => {
                put_i64(&mut b, *view);
                put_u32(&mut b, *generation);
                put_u16(&mut b, *width);
                put_u16(&mut b, *height);
                put_u32(&mut b, *stride);
                put_u32(&mut b, *slot_bytes);
                b.push(*slot_count);
                ht::FRAME_BUFFER_NEW
            }
            Self::FrameReady {
                view,
                generation,
                slot,
                seq,
            } => {
                put_i64(&mut b, *view);
                put_u32(&mut b, *generation);
                b.push(*slot);
                put_u32(&mut b, *seq);
                ht::FRAME_READY
            }
            Self::Console { view, console } => {
                put_i64(&mut b, *view);
                b.push(console.severity);
                put_str(&mut b, &console.source);
                put_u32(&mut b, console.line);
                put_u32(&mut b, console.message_len);
                ht::CONSOLE
            }
            Self::Crash { view, kind, code } => {
                put_i64(&mut b, *view);
                b.push(*kind);
                put_i32(&mut b, *code);
                ht::CRASH
            }
            Self::CookieList {
                request_id,
                cookies,
            } => {
                put_u32(&mut b, *request_id);
                put_u16(&mut b, u16::try_from(cookies.len()).unwrap_or(u16::MAX));
                for c in cookies.iter().take(usize::from(u16::MAX)) {
                    put_str(&mut b, &c.name);
                    put_str(&mut b, &c.value);
                    put_str(&mut b, &c.domain);
                    put_str(&mut b, &c.path);
                    put_bool(&mut b, c.secure);
                    put_bool(&mut b, c.http_only);
                }
                ht::COOKIE_LIST
            }
            Self::ViewClosed { view } => {
                put_i64(&mut b, *view);
                ht::VIEW_CLOSED
            }
        };
        compose_frame(Dir::FromHelper, t, b)
    }
}

// ---------------------------------------------------------------------------
// Decoding (total; exact-length reads; caps before allocation)
// ---------------------------------------------------------------------------

/// Exact-length body cursor. Every getter returns a typed error on exhaustion; [`Body::finish`]
/// rejects trailing bytes.
struct Body<'a> {
    buf: &'a [u8],
    pos: usize,
    type_byte: u8,
}

impl<'a> Body<'a> {
    fn new(buf: &'a [u8], type_byte: u8) -> Self {
        Self {
            buf,
            pos: 0,
            type_byte,
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        let end = self.pos.checked_add(n).ok_or(ProtoError::TruncatedBody {
            type_byte: self.type_byte,
        })?;
        if end > self.buf.len() {
            return Err(ProtoError::TruncatedBody {
                type_byte: self.type_byte,
            });
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtoError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32, ProtoError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn i32(&mut self) -> Result<i32, ProtoError> {
        let s = self.take(4)?;
        Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn i64(&mut self) -> Result<i64, ProtoError> {
        let s = self.take(8)?;
        Ok(i64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    fn bool(&mut self) -> Result<bool, ProtoError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(ProtoError::BadBool {
                type_byte: self.type_byte,
                value,
            }),
        }
    }

    fn string(&mut self) -> Result<String, ProtoError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ProtoError::BadUtf8 {
                type_byte: self.type_byte,
            })
    }

    fn finish(self) -> Result<(), ProtoError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(ProtoError::TrailingBytes {
                type_byte: self.type_byte,
                extra: self.buf.len() - self.pos,
            })
        }
    }
}

/// Read one frame header + body: `(type_byte, body)`. Caps are enforced BEFORE the body
/// allocation; a clean EOF before the first header byte is [`ProtoError::Eof`].
fn read_frame<R: Read>(r: &mut R, dir: Dir) -> Result<(u8, Vec<u8>), ProtoError> {
    // Distinguish a clean peer close (0 bytes of a new frame) from a truncated header.
    let mut len_buf = [0u8; 4];
    let mut got = 0usize;
    while got < 4 {
        match r.read(&mut len_buf[got..]) {
            Ok(0) => {
                return Err(if got == 0 {
                    ProtoError::Eof
                } else {
                    ProtoError::Truncated
                });
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ProtoError::Io(e.kind())),
        }
    }
    let declared_len = u32::from_le_bytes(len_buf);
    if declared_len == 0 {
        return Err(ProtoError::EmptyFrame);
    }
    if declared_len > GLOBAL_FRAME_CAP {
        return Err(ProtoError::Oversized {
            type_byte: None,
            declared_len,
            cap: GLOBAL_FRAME_CAP,
        });
    }
    let mut type_buf = [0u8; 1];
    read_exact_frame(r, &mut type_buf)?;
    let type_byte = type_buf[0];
    let cap = type_cap(dir, type_byte).ok_or(ProtoError::UnknownType { type_byte })?;
    if declared_len > cap {
        return Err(ProtoError::Oversized {
            type_byte: Some(type_byte),
            declared_len,
            cap,
        });
    }
    // Cap validated above — the allocation is bounded.
    let mut body = vec![0u8; (declared_len - 1) as usize];
    read_exact_frame(r, &mut body)?;
    Ok((type_byte, body))
}

/// `read_exact` with the protocol's error mapping (EOF mid-frame → [`ProtoError::Truncated`]).
fn read_exact_frame<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), ProtoError> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => ProtoError::Truncated,
        kind => ProtoError::Io(kind),
    })
}

/// Decode the next consumer→helper message (helper side).
pub fn read_consumer_msg<R: Read>(r: &mut R) -> Result<ConsumerMsg, ProtoError> {
    let (t, body) = read_frame(r, Dir::FromConsumer)?;
    let mut b = Body::new(&body, t);
    let msg = match t {
        ct::HELLO => {
            let magic = b.take(4)?;
            if magic != MAGIC {
                return Err(ProtoError::BadMagic);
            }
            ConsumerMsg::Hello { version: b.u16()? }
        }
        ct::CREATE_VIEW => {
            let view = b.i64()?;
            let width = b.u16()?;
            let height = b.u16()?;
            if view < 1 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "view handle (must be >= 1)",
                });
            }
            if width == 0 || height == 0 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "view dimensions (must be nonzero)",
                });
            }
            ConsumerMsg::CreateView {
                view,
                width,
                height,
            }
        }
        ct::CLOSE_VIEW => ConsumerMsg::CloseView { view: b.i64()? },
        ct::RESIZE_VIEW => {
            let view = b.i64()?;
            let width = b.u16()?;
            let height = b.u16()?;
            if width == 0 || height == 0 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "view dimensions (must be nonzero)",
                });
            }
            ConsumerMsg::ResizeView {
                view,
                width,
                height,
            }
        }
        ct::LOAD_URL => ConsumerMsg::LoadUrl {
            view: b.i64()?,
            url: b.string()?,
        },
        ct::LOAD_DATA_WITH_BASE_URL => ConsumerMsg::LoadDataWithBaseUrl {
            view: b.i64()?,
            base_url: b.string()?,
            data: b.string()?,
            mime: b.string()?,
            encoding: b.string()?,
            history_url: b.string()?,
        },
        ct::MOUSE_MOVE => ConsumerMsg::MouseMove {
            view: b.i64()?,
            x: b.i32()?,
            y: b.i32()?,
            modifiers: b.u32()?,
            leave: b.bool()?,
        },
        ct::MOUSE_CLICK => {
            let view = b.i64()?;
            let x = b.i32()?;
            let y = b.i32()?;
            let button = b.u8()?;
            let down = b.bool()?;
            let click_count = b.u8()?;
            let modifiers = b.u32()?;
            if button > 2 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "mouse button (0=L/1=M/2=R)",
                });
            }
            ConsumerMsg::MouseClick {
                view,
                x,
                y,
                button,
                down,
                click_count,
                modifiers,
            }
        }
        ct::MOUSE_WHEEL => ConsumerMsg::MouseWheel {
            view: b.i64()?,
            x: b.i32()?,
            y: b.i32()?,
            delta_x: b.i32()?,
            delta_y: b.i32()?,
            modifiers: b.u32()?,
        },
        ct::KEY => {
            let view = b.i64()?;
            let kind = b.u8()?;
            let windows_key_code = b.i32()?;
            let native_key_code = b.i32()?;
            let character = b.u16()?;
            let modifiers = b.u32()?;
            if kind > 2 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "key kind (0=down/1=up/2=char)",
                });
            }
            ConsumerMsg::Key {
                view,
                kind,
                windows_key_code,
                native_key_code,
                character,
                modifiers,
            }
        }
        ct::EVALUATE_JS => ConsumerMsg::EvaluateJs {
            view: b.i64()?,
            script: b.string()?,
        },
        ct::COOKIE_SET => ConsumerMsg::CookieSet {
            url: b.string()?,
            name: b.string()?,
            value: b.string()?,
            domain: b.string()?,
            path: b.string()?,
            secure: b.bool()?,
            http_only: b.bool()?,
            expires_epoch_s: b.i64()?,
        },
        ct::COOKIE_GET => ConsumerMsg::CookieGet {
            request_id: b.u32()?,
            url: b.string()?,
        },
        ct::COOKIES_CLEAR => ConsumerMsg::CookiesClear {
            request_id: b.u32()?,
        },
        ct::FRAME_ACK => ConsumerMsg::FrameAck {
            view: b.i64()?,
            generation: b.u32()?,
            seq: b.u32()?,
        },
        ct::SHUTDOWN => ConsumerMsg::Shutdown,
        // read_frame validated the type byte against this direction's cap table already.
        _ => return Err(ProtoError::UnknownType { type_byte: t }),
    };
    b.finish()?;
    Ok(msg)
}

/// Decode the next helper→consumer message (consumer side).
pub fn read_helper_msg<R: Read>(r: &mut R) -> Result<HelperMsg, ProtoError> {
    let (t, body) = read_frame(r, Dir::FromHelper)?;
    let mut b = Body::new(&body, t);
    let msg = match t {
        ht::HELLO_ACK => HelperMsg::HelloAck {
            version: b.u16()?,
            engine: b.string()?,
        },
        ht::LOAD_STATE => {
            let view = b.i64()?;
            let state = b.u8()?;
            let http_status = b.i32()?;
            if state != 0 && state != 3 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "load state (0=started/3=finished)",
                });
            }
            HelperMsg::LoadState {
                view,
                state,
                http_status,
            }
        }
        ht::FRAME_BUFFER_NEW => {
            let view = b.i64()?;
            let generation = b.u32()?;
            let width = b.u16()?;
            let height = b.u16()?;
            let stride = b.u32()?;
            let slot_bytes = b.u32()?;
            let slot_count = b.u8()?;
            if width == 0 || height == 0 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "frame dimensions (must be nonzero)",
                });
            }
            if stride != 4 * u32::from(width) {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "stride (must be 4*width in v1)",
                });
            }
            if slot_bytes != stride.saturating_mul(u32::from(height)) {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "slot_bytes (must be stride*height)",
                });
            }
            if slot_count != 2 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "slot_count (v1 fixes 2)",
                });
            }
            HelperMsg::FrameBufferNew {
                view,
                generation,
                width,
                height,
                stride,
                slot_bytes,
                slot_count,
            }
        }
        ht::FRAME_READY => {
            let view = b.i64()?;
            let generation = b.u32()?;
            let slot = b.u8()?;
            let seq = b.u32()?;
            if slot > 1 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "slot index (v1 has slots 0/1)",
                });
            }
            HelperMsg::FrameReady {
                view,
                generation,
                slot,
                seq,
            }
        }
        ht::CONSOLE => {
            let view = b.i64()?;
            let severity = b.u8()?;
            let source = b.string()?;
            let line = b.u32()?;
            let message_len = b.u32()?;
            HelperMsg::Console {
                view,
                // 2026-07-03: defense-in-depth — the wire value is re-redacted through the
                // shared contract (idempotent for scheme+host / "<non-url>"), so even a buggy
                // or hostile peer cannot make the consumer hold a full URL here.
                console: Console {
                    severity,
                    source: super::redact::url_scheme_and_host_for_log(&source),
                    line,
                    message_len,
                },
            }
        }
        ht::CRASH => {
            let view = b.i64()?;
            let kind = b.u8()?;
            let code = b.i32()?;
            if kind > 2 {
                return Err(ProtoError::BadValue {
                    type_byte: t,
                    what: "crash kind (0=renderer/1=init/2=internal)",
                });
            }
            HelperMsg::Crash { view, kind, code }
        }
        ht::COOKIE_LIST => {
            let request_id = b.u32()?;
            let count = b.u16()?;
            let mut cookies = Vec::with_capacity(usize::from(count).min(256));
            for _ in 0..count {
                cookies.push(CookieEntry {
                    name: b.string()?,
                    value: b.string()?,
                    domain: b.string()?,
                    path: b.string()?,
                    secure: b.bool()?,
                    http_only: b.bool()?,
                });
            }
            HelperMsg::CookieList {
                request_id,
                cookies,
            }
        }
        ht::VIEW_CLOSED => HelperMsg::ViewClosed { view: b.i64()? },
        _ => return Err(ProtoError::UnknownType { type_byte: t }),
    };
    b.finish()?;
    Ok(msg)
}

/// Consumer-side version gate: v1 requires an exact `HelloAck` version match.
pub fn hello_ack_version_supported(version: u16) -> bool {
    version == super::PROTO_V1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of every v1 message (16 consumer + 8 helper = 24), with adversarial-ish field
    /// values (negative ints, empty + non-ASCII strings, max modifiers).
    fn all_consumer_msgs() -> Vec<ConsumerMsg> {
        vec![
            ConsumerMsg::Hello {
                version: super::super::PROTO_V1,
            },
            ConsumerMsg::CreateView {
                view: 0x0000_0002_0000_0001,
                width: 1024,
                height: 768,
            },
            ConsumerMsg::CloseView { view: 42 },
            ConsumerMsg::ResizeView {
                view: 42,
                width: 800,
                height: 600,
            },
            ConsumerMsg::LoadUrl {
                view: 42,
                url: "https://apps.roblox.com/challenge?t=x".to_string(),
            },
            ConsumerMsg::LoadDataWithBaseUrl {
                view: 42,
                base_url: "https://host".to_string(),
                data: "<html>päge</html>".to_string(),
                mime: "text/html".to_string(),
                encoding: "utf-8".to_string(),
                history_url: String::new(),
            },
            ConsumerMsg::MouseMove {
                view: 42,
                x: -1,
                y: 7,
                modifiers: u32::MAX,
                leave: true,
            },
            ConsumerMsg::MouseClick {
                view: 42,
                x: 10,
                y: 20,
                button: 2,
                down: false,
                click_count: 1,
                modifiers: 0,
            },
            ConsumerMsg::MouseWheel {
                view: 42,
                x: 0,
                y: 0,
                delta_x: -120,
                delta_y: 120,
                modifiers: 0,
            },
            ConsumerMsg::Key {
                view: 42,
                kind: 2,
                windows_key_code: 0x41,
                native_key_code: 30,
                character: 0x2764,
                modifiers: 4,
            },
            ConsumerMsg::EvaluateJs {
                view: 42,
                script: "console.log(1)".to_string(),
            },
            ConsumerMsg::CookieSet {
                url: "https://www.roblox.com/".to_string(),
                name: ".ROBLOSECURITY".to_string(),
                value: "v".to_string(),
                domain: ".roblox.com".to_string(),
                path: "/".to_string(),
                secure: true,
                http_only: true,
                expires_epoch_s: 0,
            },
            ConsumerMsg::CookieGet {
                request_id: 7,
                url: "https://www.roblox.com/".to_string(),
            },
            ConsumerMsg::CookiesClear { request_id: 8 },
            ConsumerMsg::FrameAck {
                view: 42,
                generation: 3,
                seq: 99,
            },
            ConsumerMsg::Shutdown,
        ]
    }

    fn all_helper_msgs() -> Vec<HelperMsg> {
        vec![
            HelperMsg::HelloAck {
                version: super::super::PROTO_V1,
                engine: "cef/149.0.6+g0d0eeb6+chromium-149.0.7827.201".to_string(),
            },
            HelperMsg::LoadState {
                view: 42,
                state: 3,
                http_status: 200,
            },
            HelperMsg::FrameBufferNew {
                view: 42,
                generation: 3,
                width: 1024,
                height: 768,
                stride: 4096,
                slot_bytes: 4096 * 768,
                slot_count: 2,
            },
            HelperMsg::FrameReady {
                view: 42,
                generation: 3,
                slot: 1,
                seq: 99,
            },
            HelperMsg::Console {
                view: 42,
                console: Console::from_raw(2, "https://host/x?y", 12, "text"),
            },
            HelperMsg::Crash {
                view: 0,
                kind: 1,
                code: -7,
            },
            HelperMsg::CookieList {
                request_id: 7,
                cookies: vec![CookieEntry {
                    name: "a".to_string(),
                    value: "b".to_string(),
                    domain: ".roblox.com".to_string(),
                    path: "/".to_string(),
                    secure: true,
                    http_only: false,
                }],
            },
            HelperMsg::ViewClosed { view: 42 },
        ]
    }

    #[test]
    fn proto_roundtrip_encodes_and_decodes_every_v1_message() {
        // 2026-07-03: v1 is FROZEN — a field/order/width/endianness change fails here.
        // Byte-exactness: decode(encode(m)) == m AND encode(decode(bytes)) == bytes.
        let consumer = all_consumer_msgs();
        let helper = all_helper_msgs();
        assert_eq!(consumer.len() + helper.len(), 24, "v1 has exactly 24 types");

        let mut stream = Vec::new();
        for m in &consumer {
            stream.extend_from_slice(&m.encode().expect("encode"));
        }
        let mut r = stream.as_slice();
        for m in &consumer {
            let decoded = read_consumer_msg(&mut r).expect("decode");
            assert_eq!(&decoded, m);
            assert_eq!(decoded.encode().expect("re-encode"), m.encode().unwrap());
        }
        assert_eq!(read_consumer_msg(&mut r), Err(ProtoError::Eof));

        let mut stream = Vec::new();
        for m in &helper {
            stream.extend_from_slice(&m.encode().expect("encode"));
        }
        let mut r = stream.as_slice();
        for m in &helper {
            let decoded = read_helper_msg(&mut r).expect("decode");
            assert_eq!(&decoded, m);
            assert_eq!(decoded.encode().expect("re-encode"), m.encode().unwrap());
        }
        assert_eq!(read_helper_msg(&mut r), Err(ProtoError::Eof));
    }

    #[test]
    fn proto_decoder_is_total_on_truncated_frames() {
        // Every strict prefix of a valid multi-message stream must decode to a typed
        // Truncated/Eof result — never a panic (root ships panic=abort).
        let mut stream = Vec::new();
        for m in all_consumer_msgs() {
            stream.extend_from_slice(&m.encode().expect("encode"));
        }
        for cut in 0..stream.len() {
            let mut r = &stream[..cut];
            loop {
                match read_consumer_msg(&mut r) {
                    Ok(_) => continue,
                    Err(ProtoError::Eof) | Err(ProtoError::Truncated) => break,
                    Err(other) => panic!("prefix len {cut}: unexpected error {other:?}"),
                }
            }
        }
        // Same totality for the helper direction.
        let mut stream = Vec::new();
        for m in all_helper_msgs() {
            stream.extend_from_slice(&m.encode().expect("encode"));
        }
        for cut in 0..stream.len() {
            let mut r = &stream[..cut];
            loop {
                match read_helper_msg(&mut r) {
                    Ok(_) => continue,
                    Err(ProtoError::Eof) | Err(ProtoError::Truncated) => break,
                    Err(other) => panic!("prefix len {cut}: unexpected error {other:?}"),
                }
            }
        }
    }

    #[test]
    fn proto_rejects_oversized_declared_length_before_allocating() {
        // A hostile header declaring a huge len errors BEFORE any body allocation: the
        // stream holds ONLY the header bytes, so a decoder that tried to read/allocate the
        // declared body would report Truncated instead of Oversized.
        let mut hostile = Vec::new();
        hostile.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut r = hostile.as_slice();
        assert_eq!(
            read_consumer_msg(&mut r),
            Err(ProtoError::Oversized {
                type_byte: None,
                declared_len: u32::MAX,
                cap: GLOBAL_FRAME_CAP,
            })
        );

        // Per-type cap: a LoadUrl declaring 33 KiB is over its 32 KiB cap even though it is
        // far below the global cap.
        let declared = 33 * 1024;
        let mut hostile = Vec::new();
        hostile.extend_from_slice(&(declared as u32).to_le_bytes());
        hostile.push(ct::LOAD_URL);
        let mut r = hostile.as_slice();
        assert_eq!(
            read_consumer_msg(&mut r),
            Err(ProtoError::Oversized {
                type_byte: Some(ct::LOAD_URL),
                declared_len: declared as u32,
                cap: LOAD_URL_CAP,
            })
        );
    }

    #[test]
    fn proto_rejects_unknown_type_trailing_bytes_and_bad_bools() {
        // Unknown type byte (0x7F is in neither direction's namespace).
        let mut frame = Vec::new();
        frame.extend_from_slice(&1u32.to_le_bytes());
        frame.push(0x7F);
        let mut r = frame.as_slice();
        assert_eq!(
            read_consumer_msg(&mut r),
            Err(ProtoError::UnknownType { type_byte: 0x7F })
        );
        // A helper-direction byte is unknown in the consumer direction (and vice versa).
        let mut frame = Vec::new();
        frame.extend_from_slice(&1u32.to_le_bytes());
        frame.push(ht::HELLO_ACK);
        let mut r = frame.as_slice();
        assert_eq!(
            read_consumer_msg(&mut r),
            Err(ProtoError::UnknownType {
                type_byte: ht::HELLO_ACK
            })
        );

        // Trailing byte after a complete Shutdown body.
        let mut frame = Vec::new();
        frame.extend_from_slice(&2u32.to_le_bytes());
        frame.push(ct::SHUTDOWN);
        frame.push(0xAA);
        let mut r = frame.as_slice();
        assert_eq!(
            read_consumer_msg(&mut r),
            Err(ProtoError::TrailingBytes {
                type_byte: ct::SHUTDOWN,
                extra: 1
            })
        );

        // Body shorter than the fields consume (CloseView with a 4-byte body).
        let mut frame = Vec::new();
        frame.extend_from_slice(&5u32.to_le_bytes());
        frame.push(ct::CLOSE_VIEW);
        frame.extend_from_slice(&[0, 0, 0, 0]);
        let mut r = frame.as_slice();
        assert_eq!(
            read_consumer_msg(&mut r),
            Err(ProtoError::TruncatedBody {
                type_byte: ct::CLOSE_VIEW
            })
        );

        // A bool byte > 1 (MouseMove's `leave`).
        let good = ConsumerMsg::MouseMove {
            view: 1,
            x: 0,
            y: 0,
            modifiers: 0,
            leave: false,
        }
        .encode()
        .expect("encode");
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] = 2;
        let mut r = bad.as_slice();
        assert_eq!(
            read_consumer_msg(&mut r),
            Err(ProtoError::BadBool {
                type_byte: ct::MOUSE_MOVE,
                value: 2
            })
        );
    }

    #[test]
    fn proto_rejects_invalid_utf8_in_string_fields() {
        // Corrupt the URL bytes of a valid LoadUrl frame into invalid UTF-8.
        let good = ConsumerMsg::LoadUrl {
            view: 1,
            url: "https://host/ab".to_string(),
        }
        .encode()
        .expect("encode");
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] = 0xFF;
        bad[last - 1] = 0xFE;
        let mut r = bad.as_slice();
        assert_eq!(
            read_consumer_msg(&mut r),
            Err(ProtoError::BadUtf8 {
                type_byte: ct::LOAD_URL
            })
        );
    }

    #[test]
    fn hello_handshake_requires_exact_magic_and_version() {
        // Wrong magic → BadMagic (malformed stream; the helper closes). The bytes are not
        // carried in the error (payload-free rule).
        let good = ConsumerMsg::Hello {
            version: super::super::PROTO_V1,
        }
        .encode()
        .expect("encode");
        let mut bad = good.clone();
        bad[5] = b'X'; // first magic byte (after 4-byte len + 1-byte type)
        let mut r = bad.as_slice();
        assert_eq!(read_consumer_msg(&mut r), Err(ProtoError::BadMagic));

        // A future/unsupported HelloAck version decodes fine (so the helper can announce its
        // own version) but the consumer-side gate rejects it — the rule every future wire
        // evolution (e.g. M4's eval-result messages) hangs off.
        let ack = HelperMsg::HelloAck {
            version: super::super::PROTO_V1 + 1,
            engine: "cef/test".to_string(),
        };
        let bytes = ack.encode().expect("encode");
        let mut r = bytes.as_slice();
        match read_helper_msg(&mut r).expect("decode") {
            HelperMsg::HelloAck { version, .. } => {
                assert!(!hello_ack_version_supported(version));
                assert!(hello_ack_version_supported(super::super::PROTO_V1));
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    #[test]
    fn console_event_never_carries_message_text_or_raw_url() {
        // The M1 stderr-leak class, closed at the wire: a console event built from a
        // token-bearing source URL + secret text yields frame bytes containing neither.
        let secret_text = "SECRET-CONSOLE-TEXT-DO-NOT-SHIP";
        let console = Console::from_raw(
            2,
            "https://apps.roblox.com/challenge/verify?token=TOKENSECRET",
            7,
            secret_text,
        );
        assert_eq!(console.source(), "https://apps.roblox.com");
        assert_eq!(console.message_len(), secret_text.len() as u32);

        let msg = HelperMsg::Console { view: 42, console };
        let bytes = msg.encode().expect("encode");
        let hay = bytes.as_slice();
        for needle in [
            secret_text.as_bytes(),
            b"TOKENSECRET".as_slice(),
            b"/challenge".as_slice(),
            b"?token".as_slice(),
        ] {
            assert!(
                !hay.windows(needle.len()).any(|w| w == needle),
                "encoded console frame leaked {:?}",
                String::from_utf8_lossy(needle)
            );
        }

        // The decoded message exposes only severity/redacted-source/line/len.
        let mut r = bytes.as_slice();
        match read_helper_msg(&mut r).expect("decode") {
            HelperMsg::Console { view, console } => {
                assert_eq!(view, 42);
                assert_eq!(console.severity(), 2);
                assert_eq!(console.source(), "https://apps.roblox.com");
                assert_eq!(console.line(), 7);
                assert_eq!(console.message_len(), secret_text.len() as u32);
            }
            other => panic!("expected Console, got {other:?}"),
        }
    }
}
