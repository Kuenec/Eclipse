mod base64;
mod device;
mod proto;

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::{InstalledVersion, Store, StoreError, UpdateOutcome};
use super::{
    ApkSet, VersionCode, BASE_APK, MAX_APK_BYTES, NATIVE_SPLIT_APK, NATIVE_SPLIT_NAME,
    ROBLOX_PACKAGE,
};
use crate::https::{self, Download, DownloadError, Host, Resume};
use crate::status::StatusSink;
use proto::{
    bytes_field, fixed64_field, message_at, repeated_bytes, string_field, varint_field, Encoder,
    ProtoError,
};

const AUTH_URL: &str = "https://android.clients.google.com/auth";
const CHECKIN_URL: &str = "https://android.clients.google.com/checkin";
const FDFE_URL: &str = "https://android.clients.google.com/fdfe";
const GOOGLE_PLATFORM_SIGNATURE: &str = "38918a453d07199354f8b19af05ec6562ced5788";
const PLAY_SERVICES_PACKAGE: &str = "com.google.android.gms";
const PLAY_STORE_PACKAGE: &str = "com.android.vending";
const PLAY_SCOPE: &str = "oauth2:https://www.googleapis.com/auth/googleplay";
const PLAY_CLIENT_ID: &str = "am-android-google";
const PROTOBUF_CONTENT_TYPE: &str = "application/x-protobuf";
const OAUTH_TOKEN_PREFIX: &str = "oauth2_4/";
const FREE_OFFER: &str = "1";
const DELIVERY_OK: u64 = 1;
const CREDENTIALS_FILE: &str = "google-play.json";
const TEMP_SUFFIX: &str = ".tmp";
const MAX_API_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
const DOWNLOAD_HOSTS: &[Host] = &[
    Host::SubdomainOf("googleapis.com"),
    Host::SubdomainOf("gvt1.com"),
];
const ACQUIRE_NONCE_BYTES: usize = 256;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct GsfId(u64);

impl GsfId {
    fn hex(self) -> String {
        format!("{:x}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub email: String,
    aas_token: Secret,
    gsf_id: GsfId,
    device_config_token: Secret,
    checkin_consistency_token: Secret,
}

pub struct Account {
    dir: PathBuf,
}

impl Account {
    pub fn open() -> Result<Self, PlayError> {
        let dirs = directories::ProjectDirs::from("", "", "eclipse").ok_or(PlayError::NoDataDir)?;
        Ok(Self::at(dirs.data_dir().to_path_buf()))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.dir.join(CREDENTIALS_FILE)
    }

    pub fn credentials(&self) -> Result<Option<Credentials>, PlayError> {
        read_json(&self.credentials_path())
    }

    pub fn save(&self, credentials: &Credentials) -> Result<(), PlayError> {
        write_private_json(&self.dir, CREDENTIALS_FILE, credentials)
    }
}

pub fn sign_in(email: &str, oauth_token: &Secret) -> Result<Credentials, PlayError> {
    let email = email.trim();
    if email.is_empty() || !email.contains('@') {
        return Err(PlayError::InvalidEmail);
    }
    if !oauth_token.expose().starts_with(OAUTH_TOKEN_PREFIX) {
        return Err(PlayError::InvalidOauthToken);
    }

    let agent = https::request_agent();
    let sdk = device::SDK_INT.to_string();
    let services = device::PLAY_SERVICES_VERSION.to_string();
    let reply = auth_request(
        &agent,
        "the oauth_token exchange",
        None,
        &[
            ("Email", email),
            ("Token", oauth_token.expose()),
            ("service", "ac2dm"),
            ("ACCESS_TOKEN", "1"),
            ("add_account", "1"),
            ("get_accountid", "1"),
            ("callerPkg", PLAY_SERVICES_PACKAGE),
            ("callerSig", GOOGLE_PLATFORM_SIGNATURE),
            ("droidguard_results", "null"),
            ("lang", device::LANGUAGE),
            ("device_country", device::COUNTRY),
            ("sdk_version", &sdk),
            ("google_play_services_version", &services),
        ],
    )?;
    let aas_token =
        reply
            .get("token")
            .cloned()
            .map(Secret::new)
            .ok_or(PlayError::MissingField {
                step: "the oauth_token exchange",
                field: "Token",
            })?;

    let (gsf_id, checkin_consistency_token) = checkin(&agent)?;
    let device_config_token = upload_device_config(&agent, gsf_id, &checkin_consistency_token)?;
    Ok(Credentials {
        email: email.to_owned(),
        aas_token,
        gsf_id,
        device_config_token,
        checkin_consistency_token,
    })
}

pub fn update(
    credentials: &Credentials,
    store: &Store,
    current: Option<&ApkSet>,
    status: &StatusSink,
) -> Result<UpdateOutcome, PlayError> {
    let session = Session::start(credentials)?;
    let latest = session.latest_version()?;
    let previous = current.map(InstalledVersion::from);
    if let Some(installed) = previous
        .as_ref()
        .filter(|installed| installed.version_code >= latest)
    {
        return Ok(UpdateOutcome::UpToDate {
            installed: installed.clone(),
        });
    }

    if let Err(error) = session.acquire(latest) {
        tracing::debug!(%error, "Google Play acquire failed; continuing with purchase");
    }
    let delivery_token = session.purchase(latest)?;
    let delivery = session.delivery(latest, &delivery_token)?;

    let staging = store.begin(status)?;
    let downloads = https::download_agent();
    status.step(format!(
        "Downloading Roblox versionCode {latest} from Google Play…"
    ));
    download(
        &downloads,
        &delivery.base,
        &staging.dir().join(BASE_APK),
        status,
    )?;
    download(
        &downloads,
        &delivery.native_split,
        &staging.dir().join(NATIVE_SPLIT_APK),
        status,
    )?;
    status.step("Checking Roblox's signature and installing it…");
    let set = Box::new(staging.commit(Some(latest))?);
    Ok(UpdateOutcome::Updated { previous, set })
}

struct Session<'a> {
    agent: ureq::Agent,
    credentials: &'a Credentials,
    bearer: Secret,
}

impl<'a> Session<'a> {
    fn start(credentials: &'a Credentials) -> Result<Self, PlayError> {
        let agent = https::request_agent();
        let gsf = credentials.gsf_id.hex();
        let sdk = device::SDK_INT.to_string();
        let services = device::PLAY_SERVICES_VERSION.to_string();
        let reply = auth_request(
            &agent,
            "the Google Play sign-in",
            Some(credentials.gsf_id),
            &[
                ("androidId", &gsf),
                ("sdk_version", &sdk),
                ("Email", &credentials.email),
                ("google_play_services_version", &services),
                ("device_country", device::COUNTRY),
                ("lang", device::LANGUAGE),
                ("callerSig", GOOGLE_PLATFORM_SIGNATURE),
                ("app", PLAY_STORE_PACKAGE),
                ("client_sig", GOOGLE_PLATFORM_SIGNATURE),
                ("callerPkg", PLAY_SERVICES_PACKAGE),
                ("Token", credentials.aas_token.expose()),
                ("oauth2_foreground", "1"),
                ("token_request_options", "CAA4AVAB"),
                ("check_email", "1"),
                ("system_partition", "1"),
                ("service", PLAY_SCOPE),
            ],
        )?;
        let bearer =
            reply
                .get("auth")
                .cloned()
                .map(Secret::new)
                .ok_or(PlayError::MissingField {
                    step: "the Google Play sign-in",
                    field: "Auth",
                })?;
        Ok(Self {
            agent,
            credentials,
            bearer,
        })
    }

    fn store_headers<B>(&self, request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        store_headers(
            request,
            self.credentials.gsf_id,
            &self.credentials.checkin_consistency_token,
        )
        .header("Authorization", format!("Bearer {}", self.bearer.expose()))
        .header(
            "X-DFE-Device-Config-Token",
            self.credentials.device_config_token.expose(),
        )
    }

    fn latest_version(&self) -> Result<VersionCode, PlayError> {
        const STEP: &str = "the Roblox details request";
        let response = self
            .store_headers(self.agent.get(format!("{FDFE_URL}/details")))
            .query("doc", ROBLOX_PACKAGE)
            .call()
            .map_err(|error| PlayError::http(STEP, error))?;
        let body = api_body(STEP, response)?;
        let app = message_at(&body, &[1, 2, 4, 13, 1])
            .map_err(|source| PlayError::Response { step: STEP, source })?
            .ok_or(PlayError::NotOffered)?;
        let code = varint_field(app, 3)
            .map_err(|source| PlayError::Response { step: STEP, source })?
            .ok_or(PlayError::MissingField {
                step: STEP,
                field: "versionCode",
            })?;
        u32::try_from(code)
            .map(VersionCode)
            .map_err(|_| PlayError::VersionOutOfRange(code))
    }

    fn acquire(&self, version: VersionCode) -> Result<(), PlayError> {
        const STEP: &str = "the Roblox library request";
        let nonce =
            ring::rand::generate::<[u8; ACQUIRE_NONCE_BYTES]>(&ring::rand::SystemRandom::new())
                .map_err(|_| PlayError::Random)?
                .expose();
        let body = acquire_request(version, &nonce);
        let response = self
            .store_headers(self.agent.post(format!("{FDFE_URL}/acquire")))
            .header("Content-Type", PROTOBUF_CONTENT_TYPE)
            .send(&body[..])
            .map_err(|error| PlayError::http(STEP, error))?;
        api_body(STEP, response).map(drop)
    }

    fn purchase(&self, version: VersionCode) -> Result<String, PlayError> {
        const STEP: &str = "the Roblox purchase request";
        let response = self
            .store_headers(self.agent.post(format!("{FDFE_URL}/purchase")))
            .query("ot", FREE_OFFER)
            .query("doc", ROBLOX_PACKAGE)
            .query("vc", version.to_string())
            .send_empty()
            .map_err(|error| PlayError::http(STEP, error))?;
        let body = api_body(STEP, response)?;
        let buy = message_at(&body, &[1, 4])
            .map_err(|source| PlayError::Response { step: STEP, source })?
            .ok_or(PlayError::MissingField {
                step: STEP,
                field: "buyResponse",
            })?;
        string_field(buy, 55)
            .map_err(|source| PlayError::Response { step: STEP, source })?
            .map(str::to_owned)
            .ok_or(PlayError::MissingField {
                step: STEP,
                field: "encodedDeliveryToken",
            })
    }

    fn delivery(&self, version: VersionCode, token: &str) -> Result<Delivery, PlayError> {
        const STEP: &str = "the Roblox delivery request";
        let response = self
            .store_headers(self.agent.get(format!("{FDFE_URL}/delivery")))
            .query("ot", FREE_OFFER)
            .query("doc", ROBLOX_PACKAGE)
            .query("vc", version.to_string())
            .query("dtok", token)
            .call()
            .map_err(|error| PlayError::http(STEP, error))?;
        let body = api_body(STEP, response)?;
        parse_delivery(&body)
    }
}

fn store_headers<B>(
    request: ureq::RequestBuilder<B>,
    gsf_id: GsfId,
    checkin_consistency_token: &Secret,
) -> ureq::RequestBuilder<B> {
    request
        .header("User-Agent", device::store_user_agent())
        .header("X-DFE-Device-Id", gsf_id.hex())
        .header(
            "X-DFE-Device-Checkin-Consistency-Token",
            checkin_consistency_token.expose(),
        )
        .header("Accept-Language", device::LOCALE.replace('_', "-"))
        .header("X-DFE-UserLanguages", device::LOCALE)
        .header("X-DFE-Client-Id", PLAY_CLIENT_ID)
        .header("X-DFE-Network-Type", "4")
        .header("X-DFE-Request-Params", "timeoutMs=4000")
}

fn auth_request(
    agent: &ureq::Agent,
    step: &'static str,
    gsf_id: Option<GsfId>,
    form: &[(&str, &str)],
) -> Result<BTreeMap<String, String>, PlayError> {
    let mut request = agent
        .post(AUTH_URL)
        .header("User-Agent", device::auth_user_agent())
        .header("app", PLAY_SERVICES_PACKAGE);
    if let Some(gsf_id) = gsf_id {
        request = request.header("device", gsf_id.hex());
    }
    let response = request
        .send_form(form.iter().copied())
        .map_err(|error| PlayError::http(step, error))?;
    let status = response.status().as_u16();
    let body = read_limited(step, response)?;
    let reply = parse_auth_reply(&String::from_utf8_lossy(&body));
    if !(200..300).contains(&status) {
        return Err(PlayError::Rejected {
            step,
            status,
            reason: reply.get("error").cloned(),
        });
    }
    Ok(reply)
}

fn parse_auth_reply(body: &str) -> BTreeMap<String, String> {
    body.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

fn checkin(agent: &ureq::Agent) -> Result<(GsfId, Secret), PlayError> {
    const STEP: &str = "the device check-in";
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let body = device::checkin_request(now);
    let response = agent
        .post(CHECKIN_URL)
        .header("User-Agent", device::auth_user_agent())
        .header("app", PLAY_SERVICES_PACKAGE)
        .header("Content-Type", PROTOBUF_CONTENT_TYPE)
        .send(&body[..])
        .map_err(|error| PlayError::http(STEP, error))?;
    let reply = api_body(STEP, response)?;
    let android_id = fixed64_field(&reply, 7)
        .map_err(|source| PlayError::Response { step: STEP, source })?
        .filter(|&id| id != 0)
        .ok_or(PlayError::MissingField {
            step: STEP,
            field: "androidId",
        })?;
    let consistency = string_field(&reply, 12)
        .map_err(|source| PlayError::Response { step: STEP, source })?
        .map(|token| Secret::new(token.to_owned()))
        .ok_or(PlayError::MissingField {
            step: STEP,
            field: "deviceCheckinConsistencyToken",
        })?;
    Ok((GsfId(android_id), consistency))
}

fn upload_device_config(
    agent: &ureq::Agent,
    gsf_id: GsfId,
    checkin_consistency_token: &Secret,
) -> Result<Secret, PlayError> {
    const STEP: &str = "the device profile upload";
    let body = device::upload_config_request();
    let response = store_headers(
        agent.post(format!("{FDFE_URL}/uploadDeviceConfig")),
        gsf_id,
        checkin_consistency_token,
    )
    .header("Content-Type", PROTOBUF_CONTENT_TYPE)
    .send(&body[..])
    .map_err(|error| PlayError::http(STEP, error))?;
    let reply = api_body(STEP, response)?;
    let upload = message_at(&reply, &[1, 28])
        .map_err(|source| PlayError::Response { step: STEP, source })?
        .ok_or(PlayError::MissingField {
            step: STEP,
            field: "uploadDeviceConfigResponse",
        })?;
    string_field(upload, 1)
        .map_err(|source| PlayError::Response { step: STEP, source })?
        .map(|token| Secret::new(token.to_owned()))
        .ok_or(PlayError::MissingField {
            step: STEP,
            field: "uploadDeviceConfigToken",
        })
}

fn acquire_request(version: VersionCode, nonce: &[u8]) -> Vec<u8> {
    let mut package_payload = Encoder::default();
    package_payload
        .string(1, ROBLOX_PACKAGE)
        .int32(2, 1)
        .int32(3, 3);
    let mut package = Encoder::default();
    package.message(1, &package_payload).int32(2, 1);
    let mut version_message = Encoder::default();
    version_message.varint(1, u64::from(version.0)).int32(3, 0);
    let mut unknown_30 = Encoder::default();
    unknown_30.int32(1, 2).int32(2, 0);
    let mut request = Encoder::default();
    request
        .message(1, &package)
        .message(12, &version_message)
        .int32(13, 1)
        .int32(15, 0)
        .string(22, &format!("nonce={}", base64::encode_url_safe(nonce)))
        .int32(25, 2)
        .message(30, &unknown_30);
    request.into_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Artifact {
    file: &'static str,
    url: String,
    size: Option<u64>,
    sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Delivery {
    base: Artifact,
    native_split: Artifact,
}

fn parse_delivery(body: &[u8]) -> Result<Delivery, PlayError> {
    const STEP: &str = "the Roblox delivery request";
    let response_error = |source| PlayError::Response { step: STEP, source };
    let delivery =
        message_at(body, &[1, 21])
            .map_err(response_error)?
            .ok_or(PlayError::MissingField {
                step: STEP,
                field: "deliveryResponse",
            })?;
    let status = varint_field(delivery, 1)
        .map_err(response_error)?
        .unwrap_or(DELIVERY_OK);
    if status != DELIVERY_OK {
        return Err(PlayError::Delivery(status));
    }
    let app = bytes_field(delivery, 2)
        .map_err(response_error)?
        .ok_or(PlayError::MissingField {
            step: STEP,
            field: "appDeliveryData",
        })?;
    if bytes_field(app, 12).map_err(response_error)?.is_some() {
        return Err(PlayError::Encrypted);
    }
    let base = artifact(BASE_APK, app, 3, 1, 19)?;

    let mut offered = Vec::new();
    let mut native_split = None;
    for split in repeated_bytes(app, 15).map_err(response_error)? {
        let name = string_field(split, 1)
            .map_err(response_error)?
            .unwrap_or_default();
        if name == NATIVE_SPLIT_NAME {
            native_split = Some(artifact(NATIVE_SPLIT_APK, split, 5, 2, 9)?);
        }
        offered.push(name.to_owned());
    }
    let native_split = native_split.ok_or(PlayError::MissingNativeSplit { offered })?;
    Ok(Delivery { base, native_split })
}

fn artifact(
    file: &'static str,
    message: &[u8],
    url_field: u32,
    size_field: u32,
    sha256_field: u32,
) -> Result<Artifact, PlayError> {
    const STEP: &str = "the Roblox delivery request";
    let response_error = |source| PlayError::Response { step: STEP, source };
    let url = string_field(message, url_field)
        .map_err(response_error)?
        .ok_or(PlayError::MissingField {
            step: STEP,
            field: "downloadUrl",
        })?
        .to_owned();
    let size = varint_field(message, size_field).map_err(response_error)?;
    if size.is_some_and(|size| size > MAX_APK_BYTES) {
        return Err(PlayError::TooLarge { file });
    }
    let encoded = string_field(message, sha256_field)
        .map_err(response_error)?
        .ok_or(PlayError::MissingField {
            step: STEP,
            field: "sha256",
        })?;
    let sha256 = base64::decode_url_safe(encoded)
        .and_then(|digest| <[u8; 32]>::try_from(digest).ok())
        .ok_or(PlayError::BadHash { file })?;
    Ok(Artifact {
        file,
        url,
        size,
        sha256,
    })
}

fn download(
    agent: &ureq::Agent,
    artifact: &Artifact,
    dest: &Path,
    status: &StatusSink,
) -> Result<(), PlayError> {
    save_verified(
        |resume| https::open_download(agent, &artifact.url, DOWNLOAD_HOSTS, resume),
        artifact,
        dest,
        status,
    )
}

fn save_verified<R: Read>(
    open: impl FnMut(Option<&Resume>) -> Result<Download<R>, DownloadError>,
    artifact: &Artifact,
    dest: &Path,
    status: &StatusSink,
) -> Result<(), PlayError> {
    let file = artifact.file;
    let mut hasher = Sha256::new();
    let total = https::save_download(open, MAX_APK_BYTES, dest, status, |chunk| {
        hasher.update(chunk)
    })
    .map_err(|source| PlayError::Download { file, source })?;
    if let Some(expected) = artifact.size.filter(|&size| size != total) {
        return Err(PlayError::SizeMismatch {
            file,
            expected,
            actual: total,
        });
    }
    if hasher.finalize()[..] != artifact.sha256 {
        return Err(PlayError::HashMismatch { file });
    }
    Ok(())
}

fn api_body(
    step: &'static str,
    response: ureq::http::Response<ureq::Body>,
) -> Result<Vec<u8>, PlayError> {
    let status = response.status().as_u16();
    let body = read_limited(step, response)?;
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(PlayError::Status { step, status })
    }
}

fn read_limited(
    step: &'static str,
    response: ureq::http::Response<ureq::Body>,
) -> Result<Vec<u8>, PlayError> {
    response
        .into_body()
        .into_with_config()
        .limit(MAX_API_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|error| PlayError::http(step, error))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, PlayError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PlayError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| PlayError::StateFile {
            path: path.to_path_buf(),
            source,
        })
}

fn write_private_json(dir: &Path, name: &str, value: &impl Serialize) -> Result<(), PlayError> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| PlayError::Io { path, source }
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io_error(dir))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error(dir))?;

    let path = dir.join(name);
    let temp = dir.join(format!("{name}{TEMP_SUFFIX}"));
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(PlayError::Io { path: temp, source }),
    }
    let mut json = serde_json::to_vec_pretty(value).expect("Play state always serializes");
    json.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(io_error(&temp))?;
    file.write_all(&json).map_err(io_error(&temp))?;
    file.sync_all().map_err(io_error(&temp))?;
    drop(file);
    fs::rename(&temp, &path).map_err(io_error(&path))?;
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error(dir))
}

#[derive(Debug)]
pub enum PlayError {
    NoDataDir,

    Io {
        path: PathBuf,
        source: io::Error,
    },

    StateFile {
        path: PathBuf,
        source: serde_json::Error,
    },

    InvalidEmail,

    InvalidOauthToken,

    Random,

    Http {
        step: &'static str,
        detail: String,
    },

    Rejected {
        step: &'static str,
        status: u16,
        reason: Option<String>,
    },

    Status {
        step: &'static str,
        status: u16,
    },

    Response {
        step: &'static str,
        source: ProtoError,
    },

    MissingField {
        step: &'static str,
        field: &'static str,
    },

    NotOffered,

    VersionOutOfRange(u64),

    Delivery(u64),

    Encrypted,

    MissingNativeSplit {
        offered: Vec<String>,
    },

    BadHash {
        file: &'static str,
    },

    Download {
        file: &'static str,
        source: DownloadError,
    },

    TooLarge {
        file: &'static str,
    },

    SizeMismatch {
        file: &'static str,
        expected: u64,
        actual: u64,
    },

    HashMismatch {
        file: &'static str,
    },

    Store(StoreError),
}

impl PlayError {
    fn http(step: &'static str, error: ureq::Error) -> Self {
        Self::Http {
            step,
            detail: https::redacted(error),
        }
    }
}

impl fmt::Display for PlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataDir => {
                f.write_str("cannot determine Eclipse's data directory; set HOME or XDG_DATA_HOME")
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::StateFile { path, source } => write!(
                f,
                "{} is damaged ({source}); run `eclipse play-login` again",
                path.display()
            ),
            Self::InvalidEmail => f.write_str("enter the Google account's email address"),
            Self::InvalidOauthToken => write!(
                f,
                "the oauth_token cookie value starts with {OAUTH_TOKEN_PREFIX}; copy the whole \
                 value of the cookie"
            ),
            Self::Random => f.write_str("the system random number generator failed"),
            Self::Http { step, detail } => write!(f, "{step} failed: {detail}"),
            Self::Rejected {
                step,
                status,
                reason,
            } => {
                write!(f, "Google rejected {step} (HTTP {status}")?;
                if let Some(reason) = reason {
                    write!(f, ", {reason}")?;
                }
                f.write_str("); sign in again with `eclipse play-login`")
            }
            Self::Status { step, status } => {
                write!(f, "Google Play answered {step} with HTTP {status}")
            }
            Self::Response { step, source } => {
                write!(f, "Google Play sent a malformed reply to {step}: {source}")
            }
            Self::MissingField { step, field } => {
                write!(f, "Google Play's reply to {step} has no {field}")
            }
            Self::NotOffered => write!(
                f,
                "Google Play did not offer {ROBLOX_PACKAGE} to this account and device"
            ),
            Self::VersionOutOfRange(code) => {
                write!(f, "Google Play reported an invalid versionCode {code}")
            }
            Self::Delivery(2 | 9) => write!(
                f,
                "Google Play says {ROBLOX_PACKAGE} is not available for this device"
            ),
            Self::Delivery(3) => write!(
                f,
                "Google Play says {ROBLOX_PACKAGE} is not in this account's library; install it \
                 once from the Play Store website and try again"
            ),
            Self::Delivery(7) => write!(f, "Google Play says {ROBLOX_PACKAGE} was removed"),
            Self::Delivery(status) => {
                write!(
                    f,
                    "Google Play refused the download (delivery status {status})"
                )
            }
            Self::Encrypted => f.write_str("Google Play offered only an encrypted download"),
            Self::MissingNativeSplit { offered } => write!(
                f,
                "Google Play offered no {NATIVE_SPLIT_NAME} split (offered: {})",
                offered.join(", ")
            ),
            Self::BadHash { file } => {
                write!(f, "Google Play sent an invalid SHA-256 for {file}")
            }
            Self::Download { file, source } => {
                write!(f, "the {file} download from Google Play failed: {source}")
            }
            Self::TooLarge { file } => {
                write!(f, "{file} is larger than {MAX_APK_BYTES} bytes")
            }
            Self::SizeMismatch {
                file,
                expected,
                actual,
            } => write!(
                f,
                "{file} downloaded {actual} bytes but Google Play announced {expected}"
            ),
            Self::HashMismatch { file } => {
                write!(f, "{file} does not match the SHA-256 Google Play announced")
            }
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for PlayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::StateFile { source, .. } => Some(source),
            Self::Response { source, .. } => Some(source),
            Self::Store(error) => Some(error),
            Self::Download { source, .. } => Some(source),
            Self::NoDataDir
            | Self::InvalidEmail
            | Self::InvalidOauthToken
            | Self::Random
            | Self::Http { .. }
            | Self::Rejected { .. }
            | Self::Status { .. }
            | Self::MissingField { .. }
            | Self::NotOffered
            | Self::VersionOutOfRange(_)
            | Self::Delivery(_)
            | Self::Encrypted
            | Self::MissingNativeSplit { .. }
            | Self::BadHash { .. }
            | Self::TooLarge { .. }
            | Self::SizeMismatch { .. }
            | Self::HashMismatch { .. } => None,
        }
    }
}

impl From<StoreError> for PlayError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_TEXT: &str = "aas_et/very-secret-master-token";

    fn credentials() -> Credentials {
        Credentials {
            email: "player@example.com".to_owned(),
            aas_token: Secret::new(SECRET_TEXT.to_owned()),
            gsf_id: GsfId(0x3a5c_0000_1234_abcd),
            device_config_token: Secret::new("device-config-secret".to_owned()),
            checkin_consistency_token: Secret::new("consistency-secret".to_owned()),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-play-test-{tag}-{:?}",
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    fn digest_text(byte: u8) -> String {
        base64::encode_url_safe(&[byte; 32])
    }

    fn split_delivery(name: &str, url: &str, size: u64, sha256: &str) -> Encoder {
        let mut split = Encoder::default();
        split
            .string(1, name)
            .varint(2, size)
            .string(5, url)
            .string(9, sha256);
        split
    }

    fn delivery_reply(status: Option<u64>, app: Option<&Encoder>) -> Vec<u8> {
        let mut delivery = Encoder::default();
        if let Some(status) = status {
            delivery.varint(1, status);
        }
        if let Some(app) = app {
            delivery.message(2, app);
        }
        let mut payload = Encoder::default();
        payload.message(21, &delivery);
        let mut wrapper = Encoder::default();
        wrapper.message(1, &payload);
        wrapper.into_bytes()
    }

    fn roblox_app_delivery() -> Encoder {
        let mut app = Encoder::default();
        app.varint(1, 97_000_000)
            .string(
                3,
                "https://play.googleapis.com/download/by-token/download?token=base",
            )
            .string(19, &digest_text(0xaa))
            .message(
                15,
                &split_delivery(
                    NATIVE_SPLIT_NAME,
                    "https://play.googleapis.com/download/by-token/download?token=split",
                    54_000_000,
                    &digest_text(0xbb),
                ),
            );
        app
    }

    #[test]
    fn secrets_never_appear_in_debug_output() {
        let text = format!("{:?}", credentials());
        assert!(!text.contains(SECRET_TEXT), "{text}");
        assert!(!text.contains("device-config-secret"), "{text}");
        assert!(!text.contains("consistency-secret"), "{text}");
        assert!(text.contains("player@example.com"));
        assert!(text.contains("<redacted>"));
    }

    #[test]
    fn credentials_are_saved_owner_only_and_load_back() {
        let dir = temp_dir("credentials");
        let account = Account::at(dir.clone());
        assert_eq!(account.credentials().unwrap(), None);
        account.save(&credentials()).unwrap();
        assert_eq!(account.credentials().unwrap(), Some(credentials()));

        let file_mode = fs::metadata(account.credentials_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600);
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);

        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(account.credentials_path()).unwrap()).unwrap();
        assert_eq!(stored["aas_token"], SECRET_TEXT);
        assert_eq!(stored["gsf_id"], 0x3a5c_0000_1234_abcdu64);

        fs::write(account.credentials_path(), b"{").unwrap();
        let err = account.credentials().unwrap_err();
        assert!(matches!(err, PlayError::StateFile { .. }), "{err:?}");
        assert!(err.to_string().contains("eclipse play-login"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auth_replies_are_parsed_case_insensitively() {
        let reply = parse_auth_reply(
            "Token=aas_et/abc=def\nAuth=ya29.xyz\nError=BadAuthentication\n\njunk",
        );
        assert_eq!(
            reply.get("token").map(String::as_str),
            Some("aas_et/abc=def")
        );
        assert_eq!(reply.get("auth").map(String::as_str), Some("ya29.xyz"));
        assert_eq!(
            reply.get("error").map(String::as_str),
            Some("BadAuthentication")
        );
        assert_eq!(reply.len(), 3);
    }

    #[test]
    fn sign_in_rejects_input_that_is_not_an_email_and_oauth_token() {
        let token = Secret::new("oauth2_4/abc".to_owned());
        assert!(matches!(
            sign_in("not-an-email", &token),
            Err(PlayError::InvalidEmail)
        ));
        assert!(matches!(
            sign_in("player@example.com", &Secret::new("ya29.wrong".to_owned())),
            Err(PlayError::InvalidOauthToken)
        ));
    }

    #[test]
    fn delivery_yields_base_and_x86_64_split_with_hashes() {
        let delivery = parse_delivery(&delivery_reply(Some(1), Some(&roblox_app_delivery())))
            .expect("delivery parses");
        assert_eq!(delivery.base.file, BASE_APK);
        assert_eq!(delivery.base.size, Some(97_000_000));
        assert_eq!(delivery.base.sha256, [0xaa; 32]);
        assert!(delivery.base.url.ends_with("token=base"));
        assert_eq!(delivery.native_split.file, NATIVE_SPLIT_APK);
        assert_eq!(delivery.native_split.size, Some(54_000_000));
        assert_eq!(delivery.native_split.sha256, [0xbb; 32]);

        let implicit_ok = parse_delivery(&delivery_reply(None, Some(&roblox_app_delivery())));
        assert!(implicit_ok.is_ok(), "status defaults to 1: {implicit_ok:?}");
    }

    #[test]
    fn unusable_deliveries_are_typed_errors() {
        let err = parse_delivery(&delivery_reply(Some(3), None)).unwrap_err();
        assert!(matches!(err, PlayError::Delivery(3)), "{err:?}");
        assert!(err.to_string().contains("library"), "{err}");

        let mut arm_only = Encoder::default();
        arm_only
            .string(3, "https://play.googleapis.com/base")
            .string(19, &digest_text(1))
            .message(
                15,
                &split_delivery(
                    "config.arm64_v8a",
                    "https://play.googleapis.com/arm",
                    1,
                    &digest_text(2),
                ),
            );
        let err = parse_delivery(&delivery_reply(Some(1), Some(&arm_only))).unwrap_err();
        assert!(
            matches!(&err, PlayError::MissingNativeSplit { offered } if offered == &["config.arm64_v8a"]),
            "{err:?}"
        );

        let mut encrypted = roblox_app_delivery();
        let mut params = Encoder::default();
        params.int32(1, 1);
        encrypted.message(12, &params);
        let err = parse_delivery(&delivery_reply(Some(1), Some(&encrypted))).unwrap_err();
        assert!(matches!(err, PlayError::Encrypted), "{err:?}");

        let mut bad_hash = Encoder::default();
        bad_hash
            .string(3, "https://play.googleapis.com/base")
            .string(19, "not base64!");
        let err = parse_delivery(&delivery_reply(Some(1), Some(&bad_hash))).unwrap_err();
        assert!(
            matches!(err, PlayError::BadHash { file: BASE_APK }),
            "{err:?}"
        );

        let mut no_hash = Encoder::default();
        no_hash.string(3, "https://play.googleapis.com/base");
        let err = parse_delivery(&delivery_reply(Some(1), Some(&no_hash))).unwrap_err();
        assert!(
            matches!(
                err,
                PlayError::MissingField {
                    field: "sha256",
                    ..
                }
            ),
            "{err:?}"
        );

        let mut oversized = Encoder::default();
        oversized
            .string(3, "https://play.googleapis.com/base")
            .string(19, &digest_text(1))
            .message(
                15,
                &split_delivery(
                    NATIVE_SPLIT_NAME,
                    "https://play.googleapis.com/split",
                    MAX_APK_BYTES + 1,
                    &digest_text(2),
                ),
            );
        let err = parse_delivery(&delivery_reply(Some(1), Some(&oversized))).unwrap_err();
        assert!(
            matches!(
                err,
                PlayError::TooLarge {
                    file: NATIVE_SPLIT_APK
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn downloads_are_limited_to_google_hosts_over_https() {
        for url in [
            "https://play.googleapis.com/download/by-token/download?token=x",
            "https://r1---sn-abc.gvt1.com/edgedl/x.apk",
            "https://PLAY.GOOGLEAPIS.COM:443/x",
        ] {
            assert!(https::trusted_uri(url, DOWNLOAD_HOSTS).is_ok(), "{url}");
        }
        for url in [
            "http://play.googleapis.com/x",
            "https://googleapis.com/x",
            "https://googleapis.com.evil.example/x",
            "https://evilgoogleapis.com/x",
            "https://r2.cloudflarestorage.com/x",
        ] {
            let err = https::trusted_uri(url, DOWNLOAD_HOSTS).unwrap_err();
            assert!(matches!(err, DownloadError::Untrusted { .. }), "{url}");
        }
        let err = PlayError::Download {
            file: BASE_APK,
            source: https::trusted_uri("http://x", DOWNLOAD_HOSTS).unwrap_err(),
        };
        assert_eq!(
            err.to_string(),
            "the base.apk download from Google Play failed: the download link is not an HTTPS \
             URL on *.googleapis.com or *.gvt1.com"
        );
    }

    struct FailingBody;

    impl Read for FailingBody {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::TimedOut, "timeout: RecvBody"))
        }
    }

    fn announced(body: &[u8], size: Option<u64>) -> Artifact {
        Artifact {
            file: BASE_APK,
            url: "https://play.googleapis.com/download".to_owned(),
            size,
            sha256: Sha256::digest(body).into(),
        }
    }

    fn save(
        body: impl Read,
        length: Option<u64>,
        artifact: &Artifact,
        dest: &Path,
    ) -> Result<(), PlayError> {
        let mut body = Some(body);
        save_verified(
            |resume| {
                assert_eq!(resume, None, "an uninterrupted download is not continued");
                Ok(Download {
                    body: body.take().expect("the download is opened once"),
                    extent: https::Extent::Whole { length },
                    validator: None,
                })
            },
            artifact,
            dest,
            &StatusSink::terminal(),
        )
    }

    #[test]
    fn downloads_are_kept_only_when_size_and_sha256_match() {
        let dir = temp_dir("save");
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(BASE_APK);
        let body = b"official Roblox base.apk bytes";
        let size = Some(body.len() as u64);

        save(&body[..], size, &announced(body, size), &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        save(&body[..], None, &announced(body, None), &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);

        let err = save(&body[1..], None, &announced(body, size), &dest).unwrap_err();
        assert!(
            matches!(
                err,
                PlayError::SizeMismatch { expected, actual, .. }
                    if expected == body.len() as u64 && actual == body.len() as u64 - 1
            ),
            "{err:?}"
        );
        let longer = [&body[..], b"!"].concat();
        let err = save(&longer[..], None, &announced(body, size), &dest).unwrap_err();
        assert!(matches!(err, PlayError::SizeMismatch { .. }), "{err:?}");

        let mut tampered = body.to_vec();
        tampered[0] ^= 1;
        let err = save(&tampered[..], size, &announced(body, size), &dest).unwrap_err();
        assert!(
            matches!(err, PlayError::HashMismatch { file: BASE_APK }),
            "{err:?}"
        );

        let err = save(FailingBody, None, &announced(body, None), &dest).unwrap_err();
        assert!(
            matches!(
                err,
                PlayError::Download {
                    file: BASE_APK,
                    source: DownloadError::Interrupted(_)
                }
            ),
            "{err:?}"
        );
        let err = save(&body[1..], size, &announced(body, size), &dest).unwrap_err();
        assert!(
            matches!(
                err,
                PlayError::Download {
                    file: BASE_APK,
                    source: DownloadError::LengthMismatch { .. }
                }
            ),
            "{err:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acquire_request_names_roblox_its_version_and_a_fresh_nonce() {
        let request = acquire_request(VersionCode(3056), &[7; ACQUIRE_NONCE_BYTES]);
        let package = message_at(&request, &[1, 1]).unwrap().unwrap();
        assert_eq!(string_field(package, 1).unwrap(), Some(ROBLOX_PACKAGE));
        let version = message_at(&request, &[12]).unwrap().unwrap();
        assert_eq!(varint_field(version, 1).unwrap(), Some(3056));
        assert_eq!(varint_field(&request, 13).unwrap(), Some(1));
        let nonce = string_field(&request, 22).unwrap().unwrap();
        assert_eq!(
            nonce,
            format!(
                "nonce={}",
                base64::encode_url_safe(&[7; ACQUIRE_NONCE_BYTES])
            )
        );
    }

    #[test]
    fn transport_errors_never_echo_urls() {
        let err = PlayError::http(
            "the Roblox details request",
            ureq::Error::BadUri("https://android.clients.google.com/?token=secret".to_owned()),
        );
        assert!(!err.to_string().contains("token=secret"), "{err}");
    }
}
