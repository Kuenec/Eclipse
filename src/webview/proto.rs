#![forbid(unsafe_code)]

use std::io::Read;

pub const PROTO_VERSION: u16 = 6;

pub const MAGIC: [u8; 4] = *b"ECWV";

pub const GLOBAL_FRAME_CAP: u32 = 8 * 1024 * 1024;

pub const PERSISTENT_COOKIE_FILE: &str = "cookies.sqlite";

const DEFAULT_CAP: u32 = 64 * 1024;

const LOAD_URL_CAP: u32 = 32 * 1024;

const PAYLOAD_CAP: u32 = GLOBAL_FRAME_CAP;

mod ct {
    pub(super) const HELLO: u8 = 0x01;
    pub(super) const CREATE_VIEW: u8 = 0x02;
    pub(super) const CLOSE_VIEW: u8 = 0x03;
    pub(super) const SET_VISIBLE: u8 = 0x04;
    pub(super) const ACTIVATE: u8 = 0x05;
    pub(super) const SET_USER_AGENT: u8 = 0x06;
    pub(super) const LOAD_URL: u8 = 0x07;
    pub(super) const LOAD_DATA: u8 = 0x08;
    pub(super) const RELOAD: u8 = 0x09;
    pub(super) const STOP_LOADING: u8 = 0x0A;
    pub(super) const GO_BACK: u8 = 0x0B;
    pub(super) const EVALUATE_JS: u8 = 0x0C;
    pub(super) const BRIDGE_REGISTER: u8 = 0x0D;
    pub(super) const BRIDGE_UNREGISTER: u8 = 0x0E;
    pub(super) const BRIDGE_RESULT: u8 = 0x0F;
    pub(super) const POLICY_REPLY: u8 = 0x10;
    pub(super) const COOKIE_SET: u8 = 0x11;
    pub(super) const COOKIE_IMPORT: u8 = 0x12;
    pub(super) const COOKIE_GET: u8 = 0x13;
    pub(super) const COOKIES_CLEAR: u8 = 0x14;
    pub(super) const COOKIE_FLUSH: u8 = 0x15;
    pub(super) const SHUTDOWN: u8 = 0x16;
}

mod ht {
    pub(super) const HELLO_ACK: u8 = 0x81;
    pub(super) const FATAL: u8 = 0x82;
    pub(super) const LOAD_CHANGED: u8 = 0x83;
    pub(super) const NAVIGATION_STATE: u8 = 0x84;
    pub(super) const PROGRESS: u8 = 0x85;
    pub(super) const LOAD_FAILED: u8 = 0x86;
    pub(super) const RESOURCE_LOAD: u8 = 0x87;
    pub(super) const POLICY_REQUEST: u8 = 0x88;
    pub(super) const CLOSE_REQUESTED: u8 = 0x89;
    pub(super) const VIEW_CLOSED: u8 = 0x8A;
    pub(super) const WEB_PROCESS_GONE: u8 = 0x8B;
    pub(super) const BRIDGE_CALL: u8 = 0x8C;
    pub(super) const EVALUATE_JS_RESULT: u8 = 0x8D;
    pub(super) const COOKIE_SET_RESULT: u8 = 0x8E;
    pub(super) const COOKIE_IMPORT_RESULT: u8 = 0x8F;
    pub(super) const COOKIE_LIST: u8 = 0x90;
    pub(super) const COOKIES_CLEARED: u8 = 0x91;
    pub(super) const COOKIE_FLUSHED: u8 = 0x92;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Eof,

    Truncated,

    Io(std::io::ErrorKind),

    EmptyFrame,

    Oversized {
        type_byte: Option<u8>,
        declared_len: u32,
        cap: u32,
    },

    UnknownType {
        type_byte: u8,
    },

    TruncatedBody {
        type_byte: u8,
    },

    TrailingBytes {
        type_byte: u8,
        extra: usize,
    },

    BadBool {
        type_byte: u8,
        value: u8,
    },

    BadUtf8 {
        type_byte: u8,
    },

    BadMagic,

    BadValue {
        type_byte: u8,
        what: &'static str,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadEvent {
    Started,
    Redirected,
    Committed,
    Finished,
}

impl LoadEvent {
    pub fn android_state(self) -> i32 {
        match self {
            Self::Started => 0,
            Self::Redirected => 1,
            Self::Committed => 2,
            Self::Finished => 3,
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Started => 0,
            Self::Redirected => 1,
            Self::Committed => 2,
            Self::Finished => 3,
        }
    }

    fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Started),
            1 => Some(Self::Redirected),
            2 => Some(Self::Committed),
            3 => Some(Self::Finished),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    Unknown,
    HostLookup,
    Connect,
    Io,
    Timeout,
    UnsupportedScheme,
    FailedSslHandshake,
    BadUrl,
    FileNotFound,
}

impl LoadError {
    pub fn android_code(self) -> i32 {
        match self {
            Self::Unknown => -1,
            Self::HostLookup => -2,
            Self::Connect => -6,
            Self::Io => -7,
            Self::Timeout => -8,
            Self::UnsupportedScheme => -10,
            Self::FailedSslHandshake => -11,
            Self::BadUrl => -12,
            Self::FileNotFound => -14,
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::HostLookup => 1,
            Self::Connect => 2,
            Self::Io => 3,
            Self::Timeout => 4,
            Self::UnsupportedScheme => 5,
            Self::FailedSslHandshake => 6,
            Self::BadUrl => 7,
            Self::FileNotFound => 8,
        }
    }

    fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Unknown),
            1 => Some(Self::HostLookup),
            2 => Some(Self::Connect),
            3 => Some(Self::Io),
            4 => Some(Self::Timeout),
            5 => Some(Self::UnsupportedScheme),
            6 => Some(Self::FailedSslHandshake),
            7 => Some(Self::BadUrl),
            8 => Some(Self::FileNotFound),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearScope {
    All,
    Session,
}

impl ClearScope {
    fn byte(self) -> u8 {
        match self {
            Self::All => 0,
            Self::Session => 1,
        }
    }

    fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::All),
            1 => Some(Self::Session),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    None,
    Lax,
    Strict,
}

impl SameSite {
    fn byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Lax => 1,
            Self::Strict => 2,
        }
    }

    fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::Lax),
            2 => Some(Self::Strict),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieExpiry {
    Session,
    At { epoch_s: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
    pub expiry: CookieExpiry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookiePair {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumerMsg {
    Hello {
        version: u16,
    },

    CreateView {
        view: i64,
    },

    CloseView {
        view: i64,
    },

    SetVisible {
        view: i64,
        visible: bool,
    },

    Activate {
        view: i64,
        token: String,
    },

    SetUserAgent {
        view: i64,
        user_agent: String,
    },

    LoadUrl {
        view: i64,
        url: String,
    },

    LoadData {
        view: i64,
        base_url: String,
        data: String,
        mime: String,
        encoding: String,
    },

    Reload {
        view: i64,
    },

    StopLoading {
        view: i64,
    },

    GoBack {
        view: i64,
    },

    EvaluateJs {
        view: i64,
        request_id: u32,
        script: String,
    },

    BridgeRegister {
        view: i64,
        name: String,
        methods: Vec<String>,
    },

    BridgeUnregister {
        view: i64,
        name: String,
    },

    BridgeResult {
        call_id: u32,
        ok: bool,
        result_json: String,
    },

    PolicyReply {
        policy_id: u32,
        override_load: bool,
    },

    CookieSet {
        request_id: u32,
        url: String,
        header: String,
    },

    CookieImport {
        request_id: u32,
        cookies: Vec<StoredCookie>,
    },

    CookieGet {
        request_id: u32,
        url: String,
    },

    CookiesClear {
        request_id: u32,
        scope: ClearScope,
    },

    CookieFlush {
        request_id: u32,
    },

    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperMsg {
    HelloAck {
        version: u16,
        engine: String,
    },

    Fatal {
        reason: String,
    },

    LoadChanged {
        view: i64,
        event: LoadEvent,
        url: String,
    },

    NavigationState {
        view: i64,
        url: String,
        title: String,
        can_go_back: bool,
    },

    Progress {
        view: i64,
        percent: u8,
    },

    LoadFailed {
        view: i64,
        url: String,
        error: LoadError,
        description: String,
    },

    ResourceLoad {
        view: i64,
        url: String,
    },

    PolicyRequest {
        view: i64,
        policy_id: u32,
        url: String,
        redirect: bool,
        user_gesture: bool,
        method: String,
    },

    CloseRequested {
        view: i64,
    },

    ViewClosed {
        view: i64,
    },

    WebProcessGone {
        view: i64,
    },

    BridgeCall {
        view: i64,
        call_id: u32,
        payload_json: String,
    },

    EvaluateJsResult {
        request_id: u32,
        ok: bool,
        value_json: String,
    },

    CookieSetResult {
        request_id: u32,
        ok: bool,
    },

    CookieImportResult {
        request_id: u32,
        imported: u32,
        failed: u32,
    },

    CookieList {
        request_id: u32,
        cookies: Vec<CookiePair>,
    },

    CookiesCleared {
        request_id: u32,
        removed: bool,
    },

    CookieFlushed {
        request_id: u32,
        ok: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    FromConsumer,
    FromHelper,
}

fn type_cap(dir: Dir, type_byte: u8) -> Option<u32> {
    match dir {
        Dir::FromConsumer => match type_byte {
            ct::LOAD_URL => Some(LOAD_URL_CAP),
            ct::LOAD_DATA
            | ct::EVALUATE_JS
            | ct::BRIDGE_RESULT
            | ct::COOKIE_IMPORT
            | ct::COOKIE_SET
            | ct::COOKIE_GET => Some(PAYLOAD_CAP),
            ct::HELLO
            | ct::CREATE_VIEW
            | ct::CLOSE_VIEW
            | ct::SET_VISIBLE
            | ct::ACTIVATE
            | ct::SET_USER_AGENT
            | ct::RELOAD
            | ct::STOP_LOADING
            | ct::GO_BACK
            | ct::BRIDGE_REGISTER
            | ct::BRIDGE_UNREGISTER
            | ct::POLICY_REPLY
            | ct::COOKIES_CLEAR
            | ct::COOKIE_FLUSH
            | ct::SHUTDOWN => Some(DEFAULT_CAP),
            _ => None,
        },
        Dir::FromHelper => match type_byte {
            ht::LOAD_CHANGED
            | ht::NAVIGATION_STATE
            | ht::LOAD_FAILED
            | ht::RESOURCE_LOAD
            | ht::POLICY_REQUEST
            | ht::BRIDGE_CALL
            | ht::EVALUATE_JS_RESULT
            | ht::COOKIE_LIST => Some(PAYLOAD_CAP),
            ht::HELLO_ACK
            | ht::FATAL
            | ht::PROGRESS
            | ht::CLOSE_REQUESTED
            | ht::VIEW_CLOSED
            | ht::WEB_PROCESS_GONE
            | ht::COOKIE_SET_RESULT
            | ht::COOKIE_IMPORT_RESULT
            | ht::COOKIES_CLEARED
            | ht::COOKIE_FLUSHED => Some(DEFAULT_CAP),
            _ => None,
        },
    }
}

fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
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

fn put_count(buf: &mut Vec<u8>, type_byte: u8, len: usize) -> Result<(), ProtoError> {
    let count = u16::try_from(len).map_err(|_| ProtoError::BadValue {
        type_byte,
        what: "item count (at most 65535)",
    })?;
    put_u16(buf, count);
    Ok(())
}

fn put_cookies(
    buf: &mut Vec<u8>,
    type_byte: u8,
    cookies: &[StoredCookie],
) -> Result<(), ProtoError> {
    put_count(buf, type_byte, cookies.len())?;
    for cookie in cookies {
        put_stored_cookie(buf, cookie);
    }
    Ok(())
}

fn put_stored_cookie(buf: &mut Vec<u8>, cookie: &StoredCookie) {
    put_str(buf, &cookie.name);
    put_str(buf, &cookie.value);
    put_str(buf, &cookie.domain);
    put_str(buf, &cookie.path);
    put_bool(buf, cookie.secure);
    put_bool(buf, cookie.http_only);
    buf.push(cookie.same_site.byte());
    match cookie.expiry {
        CookieExpiry::Session => put_bool(buf, false),
        CookieExpiry::At { epoch_s } => {
            put_bool(buf, true);
            put_i64(buf, epoch_s);
        }
    }
}

fn compose_frame(dir: Dir, type_byte: u8, body: Vec<u8>) -> Result<Vec<u8>, ProtoError> {
    let declared_len = u32::try_from(body.len().saturating_add(1)).unwrap_or(u32::MAX);

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
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut b = Vec::new();
        let t = match self {
            Self::Hello { version } => {
                b.extend_from_slice(&MAGIC);
                put_u16(&mut b, *version);
                ct::HELLO
            }
            Self::CreateView { view } => {
                put_i64(&mut b, *view);
                ct::CREATE_VIEW
            }
            Self::CloseView { view } => {
                put_i64(&mut b, *view);
                ct::CLOSE_VIEW
            }
            Self::SetVisible { view, visible } => {
                put_i64(&mut b, *view);
                put_bool(&mut b, *visible);
                ct::SET_VISIBLE
            }
            Self::Activate { view, token } => {
                put_i64(&mut b, *view);
                put_str(&mut b, token);
                ct::ACTIVATE
            }
            Self::SetUserAgent { view, user_agent } => {
                put_i64(&mut b, *view);
                put_str(&mut b, user_agent);
                ct::SET_USER_AGENT
            }
            Self::LoadUrl { view, url } => {
                put_i64(&mut b, *view);
                put_str(&mut b, url);
                ct::LOAD_URL
            }
            Self::LoadData {
                view,
                base_url,
                data,
                mime,
                encoding,
            } => {
                put_i64(&mut b, *view);
                put_str(&mut b, base_url);
                put_str(&mut b, data);
                put_str(&mut b, mime);
                put_str(&mut b, encoding);
                ct::LOAD_DATA
            }
            Self::Reload { view } => {
                put_i64(&mut b, *view);
                ct::RELOAD
            }
            Self::StopLoading { view } => {
                put_i64(&mut b, *view);
                ct::STOP_LOADING
            }
            Self::GoBack { view } => {
                put_i64(&mut b, *view);
                ct::GO_BACK
            }
            Self::EvaluateJs {
                view,
                request_id,
                script,
            } => {
                put_i64(&mut b, *view);
                put_u32(&mut b, *request_id);
                put_str(&mut b, script);
                ct::EVALUATE_JS
            }
            Self::BridgeRegister {
                view,
                name,
                methods,
            } => {
                put_i64(&mut b, *view);
                put_str(&mut b, name);
                put_count(&mut b, ct::BRIDGE_REGISTER, methods.len())?;
                for method in methods {
                    put_str(&mut b, method);
                }
                ct::BRIDGE_REGISTER
            }
            Self::BridgeUnregister { view, name } => {
                put_i64(&mut b, *view);
                put_str(&mut b, name);
                ct::BRIDGE_UNREGISTER
            }
            Self::BridgeResult {
                call_id,
                ok,
                result_json,
            } => {
                put_u32(&mut b, *call_id);
                put_bool(&mut b, *ok);
                put_str(&mut b, result_json);
                ct::BRIDGE_RESULT
            }
            Self::PolicyReply {
                policy_id,
                override_load,
            } => {
                put_u32(&mut b, *policy_id);
                put_bool(&mut b, *override_load);
                ct::POLICY_REPLY
            }
            Self::CookieSet {
                request_id,
                url,
                header,
            } => {
                put_u32(&mut b, *request_id);
                put_str(&mut b, url);
                put_str(&mut b, header);
                ct::COOKIE_SET
            }
            Self::CookieImport {
                request_id,
                cookies,
            } => {
                put_u32(&mut b, *request_id);
                put_cookies(&mut b, ct::COOKIE_IMPORT, cookies)?;
                ct::COOKIE_IMPORT
            }
            Self::CookieGet { request_id, url } => {
                put_u32(&mut b, *request_id);
                put_str(&mut b, url);
                ct::COOKIE_GET
            }
            Self::CookiesClear { request_id, scope } => {
                put_u32(&mut b, *request_id);
                b.push(scope.byte());
                ct::COOKIES_CLEAR
            }
            Self::CookieFlush { request_id } => {
                put_u32(&mut b, *request_id);
                ct::COOKIE_FLUSH
            }
            Self::Shutdown => ct::SHUTDOWN,
        };
        compose_frame(Dir::FromConsumer, t, b)
    }
}

impl HelperMsg {
    pub fn name(&self) -> &'static str {
        match self {
            Self::HelloAck { .. } => "HelloAck",
            Self::Fatal { .. } => "Fatal",
            Self::LoadChanged { .. } => "LoadChanged",
            Self::NavigationState { .. } => "NavigationState",
            Self::Progress { .. } => "Progress",
            Self::LoadFailed { .. } => "LoadFailed",
            Self::ResourceLoad { .. } => "ResourceLoad",
            Self::PolicyRequest { .. } => "PolicyRequest",
            Self::CloseRequested { .. } => "CloseRequested",
            Self::ViewClosed { .. } => "ViewClosed",
            Self::WebProcessGone { .. } => "WebProcessGone",
            Self::BridgeCall { .. } => "BridgeCall",
            Self::EvaluateJsResult { .. } => "EvaluateJsResult",
            Self::CookieSetResult { .. } => "CookieSetResult",
            Self::CookieImportResult { .. } => "CookieImportResult",
            Self::CookieList { .. } => "CookieList",
            Self::CookiesCleared { .. } => "CookiesCleared",
            Self::CookieFlushed { .. } => "CookieFlushed",
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut b = Vec::new();
        let t = match self {
            Self::HelloAck { version, engine } => {
                put_u16(&mut b, *version);
                put_str(&mut b, engine);
                ht::HELLO_ACK
            }
            Self::Fatal { reason } => {
                put_str(&mut b, reason);
                ht::FATAL
            }
            Self::LoadChanged { view, event, url } => {
                put_i64(&mut b, *view);
                b.push(event.byte());
                put_str(&mut b, url);
                ht::LOAD_CHANGED
            }
            Self::NavigationState {
                view,
                url,
                title,
                can_go_back,
            } => {
                put_i64(&mut b, *view);
                put_str(&mut b, url);
                put_str(&mut b, title);
                put_bool(&mut b, *can_go_back);
                ht::NAVIGATION_STATE
            }
            Self::Progress { view, percent } => {
                put_i64(&mut b, *view);
                b.push(*percent);
                ht::PROGRESS
            }
            Self::LoadFailed {
                view,
                url,
                error,
                description,
            } => {
                put_i64(&mut b, *view);
                put_str(&mut b, url);
                b.push(error.byte());
                put_str(&mut b, description);
                ht::LOAD_FAILED
            }
            Self::ResourceLoad { view, url } => {
                put_i64(&mut b, *view);
                put_str(&mut b, url);
                ht::RESOURCE_LOAD
            }
            Self::PolicyRequest {
                view,
                policy_id,
                url,
                redirect,
                user_gesture,
                method,
            } => {
                put_i64(&mut b, *view);
                put_u32(&mut b, *policy_id);
                put_str(&mut b, url);
                put_bool(&mut b, *redirect);
                put_bool(&mut b, *user_gesture);
                put_str(&mut b, method);
                ht::POLICY_REQUEST
            }
            Self::CloseRequested { view } => {
                put_i64(&mut b, *view);
                ht::CLOSE_REQUESTED
            }
            Self::ViewClosed { view } => {
                put_i64(&mut b, *view);
                ht::VIEW_CLOSED
            }
            Self::WebProcessGone { view } => {
                put_i64(&mut b, *view);
                ht::WEB_PROCESS_GONE
            }
            Self::BridgeCall {
                view,
                call_id,
                payload_json,
            } => {
                put_i64(&mut b, *view);
                put_u32(&mut b, *call_id);
                put_str(&mut b, payload_json);
                ht::BRIDGE_CALL
            }
            Self::EvaluateJsResult {
                request_id,
                ok,
                value_json,
            } => {
                put_u32(&mut b, *request_id);
                put_bool(&mut b, *ok);
                put_str(&mut b, value_json);
                ht::EVALUATE_JS_RESULT
            }
            Self::CookieSetResult { request_id, ok } => {
                put_u32(&mut b, *request_id);
                put_bool(&mut b, *ok);
                ht::COOKIE_SET_RESULT
            }
            Self::CookieImportResult {
                request_id,
                imported,
                failed,
            } => {
                put_u32(&mut b, *request_id);
                put_u32(&mut b, *imported);
                put_u32(&mut b, *failed);
                ht::COOKIE_IMPORT_RESULT
            }
            Self::CookieList {
                request_id,
                cookies,
            } => {
                put_u32(&mut b, *request_id);
                put_count(&mut b, ht::COOKIE_LIST, cookies.len())?;
                for cookie in cookies {
                    put_str(&mut b, &cookie.name);
                    put_str(&mut b, &cookie.value);
                }
                ht::COOKIE_LIST
            }
            Self::CookiesCleared {
                request_id,
                removed,
            } => {
                put_u32(&mut b, *request_id);
                put_bool(&mut b, *removed);
                ht::COOKIES_CLEARED
            }
            Self::CookieFlushed { request_id, ok } => {
                put_u32(&mut b, *request_id);
                put_bool(&mut b, *ok);
                ht::COOKIE_FLUSHED
            }
        };
        compose_frame(Dir::FromHelper, t, b)
    }
}

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

    fn i64(&mut self) -> Result<i64, ProtoError> {
        let s = self.take(8)?;
        Ok(i64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    fn view(&mut self) -> Result<i64, ProtoError> {
        let view = self.i64()?;
        if view < 1 {
            return Err(self.bad("view handle (must be >= 1)"));
        }
        Ok(view)
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

    fn enumerated<T>(
        &mut self,
        parse: fn(u8) -> Option<T>,
        what: &'static str,
    ) -> Result<T, ProtoError> {
        let value = self.u8()?;
        parse(value).ok_or_else(|| self.bad(what))
    }

    fn stored_cookie(&mut self) -> Result<StoredCookie, ProtoError> {
        let name = self.string()?;
        let value = self.string()?;
        let domain = self.string()?;
        let path = self.string()?;
        let secure = self.bool()?;
        let http_only = self.bool()?;
        let same_site = self.enumerated(SameSite::from_byte, "cookie SameSite policy")?;
        let expiry = if self.bool()? {
            CookieExpiry::At {
                epoch_s: self.i64()?,
            }
        } else {
            CookieExpiry::Session
        };
        Ok(StoredCookie {
            name,
            value,
            domain,
            path,
            secure,
            http_only,
            same_site,
            expiry,
        })
    }

    fn cookies(&mut self) -> Result<Vec<StoredCookie>, ProtoError> {
        let count = self.u16()?;
        let mut cookies = Vec::with_capacity(usize::from(count).min(256));
        for _ in 0..count {
            cookies.push(self.stored_cookie()?);
        }
        Ok(cookies)
    }

    fn bad(&self, what: &'static str) -> ProtoError {
        ProtoError::BadValue {
            type_byte: self.type_byte,
            what,
        }
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

fn checked_len(dir: Dir, declared_len: u32, type_byte: Option<u8>) -> Result<(), ProtoError> {
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
    let Some(type_byte) = type_byte else {
        return Ok(());
    };
    let cap = type_cap(dir, type_byte).ok_or(ProtoError::UnknownType { type_byte })?;
    if declared_len > cap {
        return Err(ProtoError::Oversized {
            type_byte: Some(type_byte),
            declared_len,
            cap,
        });
    }
    Ok(())
}

pub fn consumer_frame_len(buf: &[u8]) -> Result<Option<usize>, ProtoError> {
    let Some(len_bytes) = buf.get(..4) else {
        return Ok(None);
    };
    let declared_len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]);
    checked_len(Dir::FromConsumer, declared_len, buf.get(4).copied())?;
    let total = 4 + declared_len as usize;
    Ok((buf.len() >= total).then_some(total))
}

fn read_frame<R: Read>(r: &mut R, dir: Dir) -> Result<(u8, Vec<u8>), ProtoError> {
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
    checked_len(dir, declared_len, None)?;
    let mut type_buf = [0u8; 1];
    read_exact_frame(r, &mut type_buf)?;
    let type_byte = type_buf[0];
    checked_len(dir, declared_len, Some(type_byte))?;

    let mut body = vec![0u8; (declared_len - 1) as usize];
    read_exact_frame(r, &mut body)?;
    Ok((type_byte, body))
}

fn read_exact_frame<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), ProtoError> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => ProtoError::Truncated,
        kind => ProtoError::Io(kind),
    })
}

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
        ct::CREATE_VIEW => ConsumerMsg::CreateView { view: b.view()? },
        ct::CLOSE_VIEW => ConsumerMsg::CloseView { view: b.view()? },
        ct::SET_VISIBLE => ConsumerMsg::SetVisible {
            view: b.view()?,
            visible: b.bool()?,
        },
        ct::ACTIVATE => ConsumerMsg::Activate {
            view: b.view()?,
            token: b.string()?,
        },
        ct::SET_USER_AGENT => ConsumerMsg::SetUserAgent {
            view: b.view()?,
            user_agent: b.string()?,
        },
        ct::LOAD_URL => ConsumerMsg::LoadUrl {
            view: b.view()?,
            url: b.string()?,
        },
        ct::LOAD_DATA => ConsumerMsg::LoadData {
            view: b.view()?,
            base_url: b.string()?,
            data: b.string()?,
            mime: b.string()?,
            encoding: b.string()?,
        },
        ct::RELOAD => ConsumerMsg::Reload { view: b.view()? },
        ct::STOP_LOADING => ConsumerMsg::StopLoading { view: b.view()? },
        ct::GO_BACK => ConsumerMsg::GoBack { view: b.view()? },
        ct::EVALUATE_JS => ConsumerMsg::EvaluateJs {
            view: b.view()?,
            request_id: b.u32()?,
            script: b.string()?,
        },
        ct::BRIDGE_REGISTER => {
            let view = b.view()?;
            let name = b.string()?;
            let count = b.u16()?;
            let mut methods = Vec::with_capacity(usize::from(count).min(256));
            for _ in 0..count {
                methods.push(b.string()?);
            }
            ConsumerMsg::BridgeRegister {
                view,
                name,
                methods,
            }
        }
        ct::BRIDGE_UNREGISTER => ConsumerMsg::BridgeUnregister {
            view: b.view()?,
            name: b.string()?,
        },
        ct::BRIDGE_RESULT => ConsumerMsg::BridgeResult {
            call_id: b.u32()?,
            ok: b.bool()?,
            result_json: b.string()?,
        },
        ct::POLICY_REPLY => ConsumerMsg::PolicyReply {
            policy_id: b.u32()?,
            override_load: b.bool()?,
        },
        ct::COOKIE_SET => ConsumerMsg::CookieSet {
            request_id: b.u32()?,
            url: b.string()?,
            header: b.string()?,
        },
        ct::COOKIE_IMPORT => ConsumerMsg::CookieImport {
            request_id: b.u32()?,
            cookies: b.cookies()?,
        },
        ct::COOKIE_GET => ConsumerMsg::CookieGet {
            request_id: b.u32()?,
            url: b.string()?,
        },
        ct::COOKIES_CLEAR => ConsumerMsg::CookiesClear {
            request_id: b.u32()?,
            scope: b.enumerated(ClearScope::from_byte, "cookie clear scope")?,
        },
        ct::COOKIE_FLUSH => ConsumerMsg::CookieFlush {
            request_id: b.u32()?,
        },
        ct::SHUTDOWN => ConsumerMsg::Shutdown,
        _ => return Err(ProtoError::UnknownType { type_byte: t }),
    };
    b.finish()?;
    Ok(msg)
}

pub fn read_helper_msg<R: Read>(r: &mut R) -> Result<HelperMsg, ProtoError> {
    let (t, body) = read_frame(r, Dir::FromHelper)?;
    let mut b = Body::new(&body, t);
    let msg = match t {
        ht::HELLO_ACK => HelperMsg::HelloAck {
            version: b.u16()?,
            engine: b.string()?,
        },
        ht::FATAL => HelperMsg::Fatal {
            reason: b.string()?,
        },
        ht::LOAD_CHANGED => HelperMsg::LoadChanged {
            view: b.view()?,
            event: b.enumerated(LoadEvent::from_byte, "load event")?,
            url: b.string()?,
        },
        ht::NAVIGATION_STATE => HelperMsg::NavigationState {
            view: b.view()?,
            url: b.string()?,
            title: b.string()?,
            can_go_back: b.bool()?,
        },
        ht::PROGRESS => {
            let view = b.view()?;
            let percent = b.u8()?;
            if percent > 100 {
                return Err(b.bad("load progress (must be 0..=100)"));
            }
            HelperMsg::Progress { view, percent }
        }
        ht::LOAD_FAILED => HelperMsg::LoadFailed {
            view: b.view()?,
            url: b.string()?,
            error: b.enumerated(LoadError::from_byte, "load error")?,
            description: b.string()?,
        },
        ht::RESOURCE_LOAD => HelperMsg::ResourceLoad {
            view: b.view()?,
            url: b.string()?,
        },
        ht::POLICY_REQUEST => HelperMsg::PolicyRequest {
            view: b.view()?,
            policy_id: b.u32()?,
            url: b.string()?,
            redirect: b.bool()?,
            user_gesture: b.bool()?,
            method: b.string()?,
        },
        ht::CLOSE_REQUESTED => HelperMsg::CloseRequested { view: b.view()? },
        ht::VIEW_CLOSED => HelperMsg::ViewClosed { view: b.view()? },
        ht::WEB_PROCESS_GONE => HelperMsg::WebProcessGone { view: b.view()? },
        ht::BRIDGE_CALL => HelperMsg::BridgeCall {
            view: b.view()?,
            call_id: b.u32()?,
            payload_json: b.string()?,
        },
        ht::EVALUATE_JS_RESULT => HelperMsg::EvaluateJsResult {
            request_id: b.u32()?,
            ok: b.bool()?,
            value_json: b.string()?,
        },
        ht::COOKIE_SET_RESULT => HelperMsg::CookieSetResult {
            request_id: b.u32()?,
            ok: b.bool()?,
        },
        ht::COOKIE_IMPORT_RESULT => HelperMsg::CookieImportResult {
            request_id: b.u32()?,
            imported: b.u32()?,
            failed: b.u32()?,
        },
        ht::COOKIE_LIST => {
            let request_id = b.u32()?;
            let count = b.u16()?;
            let mut cookies = Vec::with_capacity(usize::from(count).min(256));
            for _ in 0..count {
                cookies.push(CookiePair {
                    name: b.string()?,
                    value: b.string()?,
                });
            }
            HelperMsg::CookieList {
                request_id,
                cookies,
            }
        }
        ht::COOKIES_CLEARED => HelperMsg::CookiesCleared {
            request_id: b.u32()?,
            removed: b.bool()?,
        },
        ht::COOKIE_FLUSHED => HelperMsg::CookieFlushed {
            request_id: b.u32()?,
            ok: b.bool()?,
        },
        _ => return Err(ProtoError::UnknownType { type_byte: t }),
    };
    b.finish()?;
    Ok(msg)
}

pub fn encode_cookies(cookies: &[StoredCookie]) -> Result<Vec<u8>, ProtoError> {
    let mut buf = Vec::new();
    put_cookies(&mut buf, ct::COOKIE_IMPORT, cookies)?;
    Ok(buf)
}

pub fn decode_cookies(bytes: &[u8]) -> Result<Vec<StoredCookie>, ProtoError> {
    let mut body = Body::new(bytes, ct::COOKIE_IMPORT);
    let cookies = body.cookies()?;
    body.finish()?;
    Ok(cookies)
}

pub fn hello_ack_version_supported(version: u16) -> bool {
    version == PROTO_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_cookie(name: &str, expiry: CookieExpiry) -> StoredCookie {
        StoredCookie {
            name: name.to_string(),
            value: "v".to_string(),
            domain: ".roblox.com".to_string(),
            path: "/".to_string(),
            secure: true,
            http_only: true,
            same_site: SameSite::Lax,
            expiry,
        }
    }

    fn all_consumer_msgs() -> Vec<ConsumerMsg> {
        vec![
            ConsumerMsg::Hello {
                version: PROTO_VERSION,
            },
            ConsumerMsg::CreateView {
                view: 0x0000_0002_0000_0001,
            },
            ConsumerMsg::CloseView { view: 42 },
            ConsumerMsg::SetVisible {
                view: 42,
                visible: true,
            },
            ConsumerMsg::Activate {
                view: 42,
                token: "activation-token".to_string(),
            },
            ConsumerMsg::SetUserAgent {
                view: 42,
                user_agent: "Mozilla/5.0 ROBLOX Android App Hybrid()".to_string(),
            },
            ConsumerMsg::LoadUrl {
                view: 42,
                url: "https://apps.roblox.com/challenge?t=x".to_string(),
            },
            ConsumerMsg::LoadData {
                view: 42,
                base_url: "https://host".to_string(),
                data: "<html>päge</html>".to_string(),
                mime: "text/html".to_string(),
                encoding: "utf-8".to_string(),
            },
            ConsumerMsg::Reload { view: 42 },
            ConsumerMsg::StopLoading { view: 42 },
            ConsumerMsg::GoBack { view: 42 },
            ConsumerMsg::EvaluateJs {
                view: 42,
                request_id: 7,
                script: "navigator.userAgent".to_string(),
            },
            ConsumerMsg::BridgeRegister {
                view: 42,
                name: "EclipseTest".to_string(),
                methods: vec!["echo".to_string(), "pöst".to_string()],
            },
            ConsumerMsg::BridgeUnregister {
                view: 42,
                name: "EclipseTest".to_string(),
            },
            ConsumerMsg::BridgeResult {
                call_id: u32::MAX,
                ok: false,
                result_json: String::new(),
            },
            ConsumerMsg::PolicyReply {
                policy_id: 3,
                override_load: true,
            },
            ConsumerMsg::CookieSet {
                request_id: 9,
                url: "https://www.roblox.com/".to_string(),
                header: ".ROBLOSECURITY=v; Domain=.roblox.com; Path=/; Secure; HttpOnly"
                    .to_string(),
            },
            ConsumerMsg::CookieImport {
                request_id: 10,
                cookies: vec![
                    a_cookie("session", CookieExpiry::Session),
                    a_cookie(
                        "persistent",
                        CookieExpiry::At {
                            epoch_s: 1_900_000_000,
                        },
                    ),
                ],
            },
            ConsumerMsg::CookieGet {
                request_id: 11,
                url: "https://www.roblox.com/".to_string(),
            },
            ConsumerMsg::CookiesClear {
                request_id: 12,
                scope: ClearScope::Session,
            },
            ConsumerMsg::CookieFlush { request_id: 13 },
            ConsumerMsg::Shutdown,
        ]
    }

    fn all_helper_msgs() -> Vec<HelperMsg> {
        vec![
            HelperMsg::HelloAck {
                version: PROTO_VERSION,
                engine: "webkitgtk/2.54.0".to_string(),
            },
            HelperMsg::Fatal {
                reason: "no display".to_string(),
            },
            HelperMsg::LoadChanged {
                view: 42,
                event: LoadEvent::Committed,
                url: "https://www.roblox.com/login".to_string(),
            },
            HelperMsg::NavigationState {
                view: 42,
                url: "https://www.roblox.com/login".to_string(),
                title: "Roblox".to_string(),
                can_go_back: true,
            },
            HelperMsg::Progress {
                view: 42,
                percent: 100,
            },
            HelperMsg::LoadFailed {
                view: 42,
                url: "https://nx.invalid/".to_string(),
                error: LoadError::HostLookup,
                description: "Error resolving".to_string(),
            },
            HelperMsg::ResourceLoad {
                view: 42,
                url: "https://css.rbxcdn.com/a.css".to_string(),
            },
            HelperMsg::PolicyRequest {
                view: 42,
                policy_id: 3,
                url: "roblox://placeId=1".to_string(),
                redirect: false,
                user_gesture: true,
                method: "GET".to_string(),
            },
            HelperMsg::CloseRequested { view: 42 },
            HelperMsg::ViewClosed { view: 42 },
            HelperMsg::WebProcessGone { view: 42 },
            HelperMsg::BridgeCall {
                view: 42,
                call_id: 1,
                payload_json: "{\"iface\":\"EclipseTest\",\"method\":\"echo\",\"args\":[\"ping\"]}"
                    .to_string(),
            },
            HelperMsg::EvaluateJsResult {
                request_id: u32::MAX,
                ok: true,
                value_json: "\"echo:ping\"".to_string(),
            },
            HelperMsg::CookieSetResult {
                request_id: 9,
                ok: true,
            },
            HelperMsg::CookieImportResult {
                request_id: 10,
                imported: 2,
                failed: 0,
            },
            HelperMsg::CookieList {
                request_id: 11,
                cookies: vec![CookiePair {
                    name: "a".to_string(),
                    value: "b".to_string(),
                }],
            },
            HelperMsg::CookiesCleared {
                request_id: 12,
                removed: false,
            },
            HelperMsg::CookieFlushed {
                request_id: 13,
                ok: true,
            },
        ]
    }

    #[test]
    fn every_message_round_trips_through_its_encoding() {
        let consumer = all_consumer_msgs();
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

        let helper = all_helper_msgs();
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
    fn decoders_are_total_on_every_truncated_prefix() {
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
    fn oversized_declared_lengths_are_rejected_before_allocating() {
        let mut hostile = Vec::new();
        hostile.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            read_consumer_msg(&mut hostile.as_slice()),
            Err(ProtoError::Oversized {
                type_byte: None,
                declared_len: u32::MAX,
                cap: GLOBAL_FRAME_CAP,
            })
        );

        let declared = 33 * 1024;
        let mut hostile = Vec::new();
        hostile.extend_from_slice(&(declared as u32).to_le_bytes());
        hostile.push(ct::LOAD_URL);
        assert_eq!(
            read_consumer_msg(&mut hostile.as_slice()),
            Err(ProtoError::Oversized {
                type_byte: Some(ct::LOAD_URL),
                declared_len: declared as u32,
                cap: LOAD_URL_CAP,
            })
        );

        let declared = 65 * 1024;
        let mut hostile = Vec::new();
        hostile.extend_from_slice(&(declared as u32).to_le_bytes());
        hostile.push(ht::PROGRESS);
        assert_eq!(
            read_helper_msg(&mut hostile.as_slice()),
            Err(ProtoError::Oversized {
                type_byte: Some(ht::PROGRESS),
                declared_len: declared as u32,
                cap: DEFAULT_CAP,
            })
        );
    }

    #[test]
    fn unknown_types_trailing_bytes_bad_bools_and_bad_enums_are_rejected() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&1u32.to_le_bytes());
        frame.push(0x7F);
        assert_eq!(
            read_consumer_msg(&mut frame.as_slice()),
            Err(ProtoError::UnknownType { type_byte: 0x7F })
        );

        let mut frame = Vec::new();
        frame.extend_from_slice(&1u32.to_le_bytes());
        frame.push(ht::HELLO_ACK);
        assert_eq!(
            read_consumer_msg(&mut frame.as_slice()),
            Err(ProtoError::UnknownType {
                type_byte: ht::HELLO_ACK
            })
        );

        let mut frame = Vec::new();
        frame.extend_from_slice(&2u32.to_le_bytes());
        frame.push(ct::SHUTDOWN);
        frame.push(0xAA);
        assert_eq!(
            read_consumer_msg(&mut frame.as_slice()),
            Err(ProtoError::TrailingBytes {
                type_byte: ct::SHUTDOWN,
                extra: 1
            })
        );

        let mut bad = ConsumerMsg::SetVisible {
            view: 1,
            visible: true,
        }
        .encode()
        .expect("encode");
        let last = bad.len() - 1;
        bad[last] = 2;
        assert_eq!(
            read_consumer_msg(&mut bad.as_slice()),
            Err(ProtoError::BadBool {
                type_byte: ct::SET_VISIBLE,
                value: 2
            })
        );

        let mut bad = ConsumerMsg::CookiesClear {
            request_id: 1,
            scope: ClearScope::All,
        }
        .encode()
        .expect("encode");
        let last = bad.len() - 1;
        bad[last] = 9;
        assert!(matches!(
            read_consumer_msg(&mut bad.as_slice()),
            Err(ProtoError::BadValue {
                type_byte: ct::COOKIES_CLEAR,
                ..
            })
        ));

        let mut bad = HelperMsg::Progress {
            view: 1,
            percent: 100,
        }
        .encode()
        .expect("encode");
        let last = bad.len() - 1;
        bad[last] = 101;
        assert!(matches!(
            read_helper_msg(&mut bad.as_slice()),
            Err(ProtoError::BadValue {
                type_byte: ht::PROGRESS,
                ..
            })
        ));

        let bad = ConsumerMsg::CloseView { view: 0 }.encode().expect("encode");
        assert!(matches!(
            read_consumer_msg(&mut bad.as_slice()),
            Err(ProtoError::BadValue {
                type_byte: ct::CLOSE_VIEW,
                ..
            })
        ));
    }

    #[test]
    fn invalid_utf8_in_a_string_field_is_rejected() {
        let mut bad = ConsumerMsg::LoadUrl {
            view: 1,
            url: "https://host/ab".to_string(),
        }
        .encode()
        .expect("encode");
        let last = bad.len() - 1;
        bad[last] = 0xFF;
        bad[last - 1] = 0xFE;
        assert_eq!(
            read_consumer_msg(&mut bad.as_slice()),
            Err(ProtoError::BadUtf8 {
                type_byte: ct::LOAD_URL
            })
        );
    }

    #[test]
    fn hello_handshake_requires_exact_magic_and_version() {
        let mut bad = ConsumerMsg::Hello {
            version: PROTO_VERSION,
        }
        .encode()
        .expect("encode");
        bad[5] = b'X';
        assert_eq!(
            read_consumer_msg(&mut bad.as_slice()),
            Err(ProtoError::BadMagic)
        );
        assert!(hello_ack_version_supported(PROTO_VERSION));
        assert!(!hello_ack_version_supported(PROTO_VERSION - 1));
        assert!(!hello_ack_version_supported(PROTO_VERSION + 1));
    }

    #[test]
    fn consumer_frame_len_waits_for_a_whole_frame_and_rejects_hostile_lengths() {
        let frame = ConsumerMsg::GoBack { view: 3 }.encode().expect("encode");
        for cut in 0..frame.len() {
            assert_eq!(consumer_frame_len(&frame[..cut]), Ok(None), "cut {cut}");
        }
        let mut two = frame.clone();
        two.extend_from_slice(&frame);
        assert_eq!(consumer_frame_len(&two), Ok(Some(frame.len())));

        let mut hostile = Vec::new();
        hostile.extend_from_slice(&(GLOBAL_FRAME_CAP + 1).to_le_bytes());
        assert!(matches!(
            consumer_frame_len(&hostile),
            Err(ProtoError::Oversized {
                type_byte: None,
                ..
            })
        ));
        let mut hostile = Vec::new();
        hostile.extend_from_slice(&(DEFAULT_CAP + 1).to_le_bytes());
        hostile.push(ct::GO_BACK);
        assert!(matches!(
            consumer_frame_len(&hostile),
            Err(ProtoError::Oversized {
                type_byte: Some(ct::GO_BACK),
                ..
            })
        ));
        let mut unknown = Vec::new();
        unknown.extend_from_slice(&1u32.to_le_bytes());
        unknown.push(0x7F);
        assert_eq!(
            consumer_frame_len(&unknown),
            Err(ProtoError::UnknownType { type_byte: 0x7F })
        );
    }

    #[test]
    fn a_full_cookie_jar_for_one_url_fits_a_single_frame() {
        let msg = HelperMsg::CookieList {
            request_id: 9,
            cookies: (0..180)
                .map(|i| CookiePair {
                    name: format!("n{i:03}"),
                    value: "v".repeat(4096 - 4),
                })
                .collect(),
        };
        let bytes = msg.encode().expect("a 180-cookie jar fits the frame cap");
        assert_eq!(
            read_helper_msg(&mut bytes.as_slice()).expect("decode the full jar"),
            msg
        );
    }

    #[test]
    fn a_cookie_jar_round_trips_and_rejects_truncation_and_trailing_bytes() {
        let jar = vec![
            a_cookie("session", CookieExpiry::Session),
            a_cookie(
                "persistent",
                CookieExpiry::At {
                    epoch_s: 1_900_000_000,
                },
            ),
        ];
        let bytes = encode_cookies(&jar).expect("encode");
        assert_eq!(decode_cookies(&bytes), Ok(jar));
        assert_eq!(
            decode_cookies(&encode_cookies(&[]).expect("encode")),
            Ok(Vec::new())
        );
        assert!(matches!(
            decode_cookies(&bytes[..bytes.len() - 1]),
            Err(ProtoError::TruncatedBody { .. })
        ));
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(
            decode_cookies(&trailing),
            Err(ProtoError::TrailingBytes { extra: 1, .. })
        ));
    }

    #[test]
    fn lists_longer_than_the_count_field_fail_to_encode_instead_of_truncating() {
        let methods = vec![String::new(); usize::from(u16::MAX) + 1];
        assert_eq!(
            ConsumerMsg::BridgeRegister {
                view: 1,
                name: "EclipseTest".to_string(),
                methods,
            }
            .encode(),
            Err(ProtoError::BadValue {
                type_byte: ct::BRIDGE_REGISTER,
                what: "item count (at most 65535)",
            })
        );
        let cookies = vec![
            CookiePair {
                name: String::new(),
                value: String::new(),
            };
            usize::from(u16::MAX) + 1
        ];
        assert!(matches!(
            HelperMsg::CookieList {
                request_id: 1,
                cookies,
            }
            .encode(),
            Err(ProtoError::BadValue {
                type_byte: ht::COOKIE_LIST,
                ..
            })
        ));
        let jar = vec![a_cookie("n", CookieExpiry::Session); usize::from(u16::MAX) + 1];
        assert!(matches!(
            encode_cookies(&jar),
            Err(ProtoError::BadValue { .. })
        ));
    }

    #[test]
    fn load_events_and_errors_map_to_the_android_constants() {
        assert_eq!(LoadEvent::Started.android_state(), 0);
        assert_eq!(LoadEvent::Committed.android_state(), 2);
        assert_eq!(LoadEvent::Finished.android_state(), 3);
        assert_eq!(LoadError::Unknown.android_code(), -1);
        assert_eq!(LoadError::HostLookup.android_code(), -2);
        assert_eq!(LoadError::Connect.android_code(), -6);
        assert_eq!(LoadError::Timeout.android_code(), -8);
        assert_eq!(LoadError::UnsupportedScheme.android_code(), -10);
        assert_eq!(LoadError::FailedSslHandshake.android_code(), -11);
        assert_eq!(LoadError::BadUrl.android_code(), -12);
        assert_eq!(LoadError::FileNotFound.android_code(), -14);
    }
}
