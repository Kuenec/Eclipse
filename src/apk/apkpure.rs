use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY, SHA1_OUTPUT_LEN};
use ureq::http::header::USER_AGENT;
use ureq::http::{Request, Response, Uri};

use super::proto::{
    bytes_field, message_at, repeated_bytes, string_field, varint_field, ProtoError,
};
use super::signature::ROBLOX_CERTIFICATE_SHA1;
use super::store::{bundle_directory_names_only_other_abis, DeclaredFile, Staging, StoreError};
use super::{ApkSet, ApkSetError, VersionCode, BASE_APK, MAX_APK_BYTES, ROBLOX_PACKAGE};
use crate::abi::Abi;
use crate::https::{self, Download, DownloadError, Extent, Host, Resume};
use crate::status::{mebibytes, StatusSink};

const API: &str = "https://api.pureapk.com";
const VERSIONS_PATH: &str = "/m/v3/cms/app_version";
const API_VERSION_HEADER: &str = "x-sv";
const API_VERSION: &str = "29";
const ABIS_HEADER: &str = "x-abis";
const VERSION_LIST: &str = "version_list";
const DOWNLOAD_HOSTS: &[Host] = &[
    Host::Exact("download.pureapk.com"),
    Host::Exact("d.apkpure.com"),
    Host::Exact("data.winudf.com"),
];
const MAX_LISTING_BYTES: u64 = 4 * 1024 * 1024;
const BUNDLE_FILE: &str = "apkpure.xapk";
const DIRECTORY_TAIL_BYTES: u64 = 64 * 1024;
const EXPIRED_LINK_STATUSES: [u16; 3] = [403, 404, 410];
const REFUSING_STATUSES: [u16; 3] = [403, 429, 503];

const LIST_FIELDS: [u32; 2] = [1, 7];
const LIST_ITEM: u32 = 2;
const LIST_KIND: u32 = 1;
const LIST_BODY: u32 = 3;
const LIST_APP: u32 = 2;
const APP_PACKAGE: u32 = 4;
const APP_VERSION_CODE: u32 = 5;
const APP_VERSION_NAME: u32 = 6;
const APP_SIGNER_SHA1: u32 = 7;
const APP_FILE: u32 = 24;
const FILE_SHA1: u32 = 3;
const FILE_SIZE: u32 = 4;
const FILE_KIND: u32 = 8;
const FILE_URL: u32 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Packaging {
    Apk,
    Xapk,
}

impl Packaging {
    fn parse(kind: &str) -> Option<Self> {
        match kind {
            "APK" => Some(Self::Apk),
            "XAPK" => Some(Self::Xapk),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    version_name: String,
    version_code: VersionCode,
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Roblox {} (versionCode {})",
            self.version_name, self.version_code
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    release: Release,
    packaging: Packaging,
    size: u64,
    sha1: [u8; SHA1_OUTPUT_LEN],
    url: Uri,
}

impl Offer {
    pub fn version(&self) -> VersionCode {
        self.release.version_code
    }

    pub fn declared_file(&self) -> DeclaredFile {
        DeclaredFile::Sha1(self.sha1)
    }
}

impl fmt::Display for Offer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let packaging = match self.packaging {
            Packaging::Apk => "APK",
            Packaging::Xapk => "XAPK",
        };
        write!(
            f,
            "{} ({packaging}, {:.1} MiB)",
            self.release,
            mebibytes(self.size)
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Listing {
    pub offers: Vec<Offer>,

    pub newer_without_build: Option<Release>,
}

pub fn list(
    installed: Option<VersionCode>,
    deadline: Option<Instant>,
) -> Result<Listing, ApkPureError> {
    let agent = https::request_agent();
    list_from(Abi::HOST, installed, |abi| {
        fetch_versions(&agent, &versions_url(API), abi, deadline)
    })
}

fn list_from(
    abi: Abi,
    installed: Option<VersionCode>,
    mut fetch: impl FnMut(Abi) -> Result<Vec<u8>, ApkPureError>,
) -> Result<Listing, ApkPureError> {
    let offers = offers(&fetch(abi)?)?;
    let newest_offer = offers.first().map(Offer::version);
    let newer = |version: VersionCode| installed.is_none_or(|installed| version > installed);
    if newest_offer.is_some_and(newer) {
        return Ok(Listing {
            offers,
            newer_without_build: None,
        });
    }
    let other = abi.other();
    let newest = match fetch(other).and_then(|body| newest_release(&body)) {
        Ok(newest) => newest,
        Err(error) => {
            tracing::warn!(
                %error,
                "APKPure's list of {} builds could not be read, so the check does not name the \
                 newest Roblox release",
                other.android_name()
            );
            None
        }
    };
    let newer_without_build = newest.filter(|release| {
        newer(release.version_code)
            && newest_offer.is_none_or(|offered| release.version_code > offered)
    });
    Ok(Listing {
        offers,
        newer_without_build,
    })
}

fn versions_url(api: &str) -> String {
    format!("{api}{VERSIONS_PATH}?hl=en-US&package_name={ROBLOX_PACKAGE}")
}

fn versions_request(url: &str, abi: Abi) -> Request<()> {
    Request::get(url)
        .header(USER_AGENT, format!("Eclipse/{}", crate::VERSION))
        .header(API_VERSION_HEADER, API_VERSION)
        .header(ABIS_HEADER, abi.android_name())
        .body(())
        .expect("a GET of APKPure's version list with ASCII headers is a valid request")
}

fn fetch_versions(
    agent: &ureq::Agent,
    url: &str,
    abi: Abi,
    deadline: Option<Instant>,
) -> Result<Vec<u8>, ApkPureError> {
    let request = match deadline {
        None => versions_request(url, abi),
        Some(deadline) => {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ApkPureError::OutOfTime);
            }
            agent
                .configure_request(versions_request(url, abi))
                .timeout_global(Some(left))
                .timeout_connect(None)
                .build()
        }
    };
    let response = agent
        .run(request)
        .map_err(|error| ApkPureError::listing(deadline, error))?;
    listing_body(deadline, response)
}

fn listing_body(
    deadline: Option<Instant>,
    response: Response<ureq::Body>,
) -> Result<Vec<u8>, ApkPureError> {
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(ApkPureError::Status(status));
    }
    response
        .into_body()
        .into_with_config()
        .limit(MAX_LISTING_BYTES)
        .read_to_vec()
        .map_err(|error| ApkPureError::listing(deadline, error))
}

fn apps(body: &[u8]) -> Result<Vec<&[u8]>, ApkPureError> {
    let list = message_at(body, &LIST_FIELDS)
        .map_err(ApkPureError::Malformed)?
        .ok_or(ApkPureError::Layout("it has no version list"))?;
    let mut apps = Vec::new();
    for item in repeated_bytes(list, LIST_ITEM).map_err(ApkPureError::Malformed)? {
        if string_field(item, LIST_KIND).map_err(ApkPureError::Malformed)? != Some(VERSION_LIST) {
            continue;
        }
        let Some(listed) = bytes_field(item, LIST_BODY).map_err(ApkPureError::Malformed)? else {
            continue;
        };
        apps.extend(repeated_bytes(listed, LIST_APP).map_err(ApkPureError::Malformed)?);
    }
    Ok(apps)
}

fn roblox_release(app: &[u8]) -> Result<Option<Release>, ApkPureError> {
    let text = |field| string_field(app, field).map_err(ApkPureError::Malformed);
    let package = text(APP_PACKAGE)?;
    let signer = text(APP_SIGNER_SHA1)?;
    let roblox_signed =
        signer.is_some_and(|signer| signer.eq_ignore_ascii_case(ROBLOX_CERTIFICATE_SHA1));
    if package != Some(ROBLOX_PACKAGE) || !roblox_signed {
        tracing::warn!(
            package,
            signer,
            "APKPure lists an app that is not Roblox signed by Roblox Corporation; Eclipse \
             ignores it"
        );
        return Ok(None);
    }
    let version_code = text(APP_VERSION_CODE)?
        .filter(|code| is_decimal(code))
        .and_then(|code| code.parse().ok())
        .map(VersionCode)
        .ok_or(ApkPureError::Layout("a release has no versionCode"))?;
    let version_name = text(APP_VERSION_NAME)?
        .filter(|name| {
            name.split('.')
                .all(|part| !part.is_empty() && is_decimal(part))
        })
        .ok_or(ApkPureError::Layout("a release has no version name"))?;
    Ok(Some(Release {
        version_name: version_name.to_owned(),
        version_code,
    }))
}

fn is_decimal(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn offer(app: &[u8], release: Release) -> Result<Option<Offer>, ApkPureError> {
    let file = bytes_field(app, APP_FILE)
        .map_err(ApkPureError::Malformed)?
        .ok_or(ApkPureError::Layout("a release has no file"))?;
    let text = |field| string_field(file, field).map_err(ApkPureError::Malformed);
    let sha1 = text(FILE_SHA1)?
        .and_then(parse_sha1)
        .ok_or(ApkPureError::Layout("a file has no SHA-1"))?;
    let size = varint_field(file, FILE_SIZE)
        .map_err(ApkPureError::Malformed)?
        .filter(|&size| size > 0)
        .ok_or(ApkPureError::Layout("a file has no size"))?;
    let kind = text(FILE_KIND)?.ok_or(ApkPureError::Layout("a file has no type"))?;
    let url = text(FILE_URL)?.ok_or(ApkPureError::Layout("a file has no download link"))?;
    let Some(packaging) = Packaging::parse(kind) else {
        tracing::warn!(%release, kind, "APKPure lists a Roblox file Eclipse cannot install");
        return Ok(None);
    };
    if size > MAX_APK_BYTES {
        tracing::warn!(
            %release,
            size,
            "APKPure lists a Roblox file larger than Eclipse installs"
        );
        return Ok(None);
    }
    let url = https::trusted_uri(url, DOWNLOAD_HOSTS).map_err(|_| ApkPureError::UntrustedLink {
        release: release.clone(),
    })?;
    Ok(Some(Offer {
        release,
        packaging,
        size,
        sha1,
        url,
    }))
}

pub(super) fn offers(body: &[u8]) -> Result<Vec<Offer>, ApkPureError> {
    let mut offers = Vec::new();
    for app in apps(body)? {
        if let Some(release) = roblox_release(app)? {
            offers.extend(offer(app, release)?);
        }
    }
    offers.sort_by_key(|offer| {
        (
            std::cmp::Reverse(offer.version()),
            offer.packaging == Packaging::Xapk,
        )
    });
    Ok(offers)
}

fn newest_release(body: &[u8]) -> Result<Option<Release>, ApkPureError> {
    let mut newest: Option<Release> = None;
    for app in apps(body)? {
        if let Some(release) = roblox_release(app)? {
            if newest
                .as_ref()
                .is_none_or(|newest| release.version_code > newest.version_code)
            {
                newest = Some(release);
            }
        }
    }
    Ok(newest)
}

fn hex_byte(high: u8, low: u8) -> Option<u8> {
    let high = char::from(high).to_digit(16)?;
    let low = char::from(low).to_digit(16)?;
    u8::try_from(high * 16 + low).ok()
}

fn parse_sha1(hex: &str) -> Option<[u8; SHA1_OUTPUT_LEN]> {
    let (pairs, rest) = hex.as_bytes().as_chunks::<2>();
    if !rest.is_empty() || pairs.len() != SHA1_OUTPUT_LEN {
        return None;
    }
    let mut digest = [0u8; SHA1_OUTPUT_LEN];
    for (byte, pair) in digest.iter_mut().zip(pairs) {
        *byte = hex_byte(pair[0], pair[1])?;
    }
    Some(digest)
}

pub fn install(
    offer: &Offer,
    staging: Staging<'_>,
    status: &StatusSink,
) -> Result<ApkSet, ApkPureError> {
    let downloads = https::download_agent();
    let api = https::request_agent();
    let mut link = offer.url.clone();
    install_with(offer, staging, status, |resume| {
        open_renewing(
            offer,
            &mut link,
            resume,
            |url, resume| {
                https::open_download(&downloads, &url.to_string(), DOWNLOAD_HOSTS, resume)
            },
            || offers(&fetch_versions(&api, &versions_url(API), Abi::HOST, None)?),
        )
    })
}

fn open_renewing<R>(
    offer: &Offer,
    link: &mut Uri,
    resume: Option<&Resume>,
    mut open: impl FnMut(&Uri, Option<&Resume>) -> Result<Download<R>, DownloadError>,
    renew: impl FnOnce() -> Result<Vec<Offer>, ApkPureError>,
) -> Result<Download<R>, ApkPureError> {
    match open(link, resume) {
        Err(DownloadError::Status(status))
            if resume.is_some() && EXPIRED_LINK_STATUSES.contains(&status) => {}
        opened => return opened.map_err(ApkPureError::Download),
    }
    let renewed = renew()?
        .into_iter()
        .find(|renewed| renewed.sha1 == offer.sha1 && renewed.size == offer.size)
        .ok_or_else(|| ApkPureError::Superseded {
            offered: offer.to_string(),
        })?;
    *link = renewed.url;
    open(link, resume).map_err(ApkPureError::Download)
}

fn install_with<R: Read>(
    offer: &Offer,
    staging: Staging<'_>,
    status: &StatusSink,
    mut open: impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkPureError>,
) -> Result<ApkSet, ApkPureError> {
    match offer.packaging {
        Packaging::Apk => save_verified(open, offer, &staging.dir().join(BASE_APK), status)?,
        Packaging::Xapk => {
            if is_for_other_abis(&mut open, offer)? {
                return Err(ApkPureError::NoHostSplit);
            }
            let bundle = staging.dir().join(BUNDLE_FILE);
            save_verified(open, offer, &bundle, status)?;
            staging.add_source(&bundle).map_err(store_failure)?;
            fs::remove_file(&bundle).map_err(|source| ApkPureError::Io {
                path: bundle,
                source,
            })?;
        }
    }
    status.step("Checking Roblox's signature and installing it…");
    staging.commit(Some(offer.version())).map_err(store_failure)
}

fn is_for_other_abis<R: Read>(
    open: &mut impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkPureError>,
    offer: &Offer,
) -> Result<bool, ApkPureError> {
    let start = offer.size.saturating_sub(DIRECTORY_TAIL_BYTES);
    let tail = open(Some(&Resume {
        offset: start,
        validator: None,
    }))?;
    let Extent::Partial(range) = tail.extent else {
        return Ok(false);
    };
    if range.start != start || range.total != Some(offer.size) {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    tail.body
        .take(DIRECTORY_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .map_err(DownloadError::Interrupted)?;
    Ok(bundle_directory_names_only_other_abis(start, bytes))
}

fn save_verified<R: Read>(
    mut open: impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkPureError>,
    offer: &Offer,
    dest: &Path,
    status: &StatusSink,
) -> Result<(), ApkPureError> {
    let mut sha1 = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    let actual = https::save_download(
        |resume| {
            let download = open(resume)?;
            match download.extent {
                Extent::Whole {
                    length: Some(actual),
                } if actual != offer.size => Err(ApkPureError::SizeMismatch {
                    listed: offer.size,
                    actual,
                }),
                _ => Ok(download),
            }
        },
        offer.size,
        dest,
        status,
        |chunk| sha1.update(chunk),
    )?;
    if actual != offer.size {
        return Err(ApkPureError::SizeMismatch {
            listed: offer.size,
            actual,
        });
    }
    if sha1.finish().as_ref() != offer.sha1 {
        return Err(ApkPureError::HashMismatch);
    }
    Ok(())
}

fn store_failure(error: StoreError) -> ApkPureError {
    match error {
        StoreError::NoDataDir
        | StoreError::Io { .. }
        | StoreError::Replace { .. }
        | StoreError::Corrupt { .. }
        | StoreError::MissingInstall { .. }
        | StoreError::Set(ApkSetError::Locate { .. }) => ApkPureError::Store(error),
        StoreError::Set(_)
        | StoreError::Apk { .. }
        | StoreError::UnneededSplit { .. }
        | StoreError::Duplicate { .. }
        | StoreError::NoBaseGiven
        | StoreError::NoNativeSplitGiven
        | StoreError::TooLarge { .. }
        | StoreError::UnreadableBundle { .. }
        | StoreError::EncryptedBundle(_)
        | StoreError::BundleMissingBase(_)
        | StoreError::UnexpectedVersion { .. } => ApkPureError::Rejected(error),
    }
}

#[derive(Debug)]
pub enum ApkPureError {
    Listing { detail: String },

    OutOfTime,

    Status(u16),

    Malformed(ProtoError),

    Layout(&'static str),

    UntrustedLink { release: Release },

    Download(DownloadError),

    Superseded { offered: String },

    SizeMismatch { listed: u64, actual: u64 },

    HashMismatch,

    NoHostSplit,

    Io { path: PathBuf, source: io::Error },

    Rejected(StoreError),

    Store(StoreError),
}

impl ApkPureError {
    fn listing(deadline: Option<Instant>, error: ureq::Error) -> Self {
        match (deadline, error) {
            (Some(_), ureq::Error::Timeout(_)) => Self::OutOfTime,
            (_, error) => Self::Listing {
                detail: https::redacted(error),
            },
        }
    }

    pub fn rejects_offer(&self) -> bool {
        matches!(
            self,
            Self::Download(DownloadError::TooLarge { .. })
                | Self::SizeMismatch { .. }
                | Self::HashMismatch
                | Self::Rejected(_)
        )
    }
}

impl fmt::Display for ApkPureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listing { detail } => {
                write!(f, "cannot load APKPure's list of Roblox releases: {detail}")
            }
            Self::OutOfTime => f.write_str(
                "APKPure did not answer in the time a launch gives the update check; run \
                 `eclipse update` to check without that limit",
            ),
            Self::Status(status) if REFUSING_STATUSES.contains(status) => write!(
                f,
                "APKPure answered HTTP {status}; it may be refusing automated requests right now"
            ),
            Self::Status(status) => write!(f, "APKPure answered HTTP {status}"),
            Self::Malformed(error) => write!(
                f,
                "APKPure's list of Roblox releases is malformed ({error})"
            ),
            Self::Layout(problem) => write!(
                f,
                "APKPure's list of Roblox releases has an unexpected layout ({problem})"
            ),
            Self::UntrustedLink { release } => write!(
                f,
                "APKPure's download link for {release} does not point at APKPure's download \
                 servers over HTTPS"
            ),
            Self::Download(error) => write!(f, "the Roblox download from APKPure failed: {error}"),
            Self::Superseded { offered } => write!(
                f,
                "APKPure's download link for {offered} expired during the download and APKPure \
                 no longer lists that file; the next update check starts over"
            ),
            Self::SizeMismatch { listed, actual } => write!(
                f,
                "the Roblox download from APKPure has {actual} bytes but APKPure listed {listed}; \
                 it was discarded"
            ),
            Self::HashMismatch => f.write_str(
                "the Roblox download from APKPure does not match the SHA-1 APKPure listed; it was \
                 discarded",
            ),
            Self::NoHostSplit => write!(
                f,
                "this APKPure bundle has no {}, so it holds no {} build of Roblox",
                Abi::HOST.xapk_split_entry(),
                Abi::HOST.android_name()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Rejected(StoreError::Set(error @ ApkSetError::Signature { .. })) => write!(
                f,
                "APKPure served a file that Roblox did not sign, so Eclipse discarded it and does \
                 not download it again automatically: {error}"
            ),
            Self::Rejected(error) => write!(
                f,
                "the Roblox download from APKPure failed verification and was discarded: {error}"
            ),
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ApkPureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Malformed(error) => Some(error),
            Self::Download(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Rejected(error) | Self::Store(error) => Some(error),
            Self::Listing { .. }
            | Self::OutOfTime
            | Self::Status(_)
            | Self::Layout(_)
            | Self::UntrustedLink { .. }
            | Self::Superseded { .. }
            | Self::SizeMismatch { .. }
            | Self::HashMismatch
            | Self::NoHostSplit => None,
        }
    }
}

impl From<DownloadError> for ApkPureError {
    fn from(error: DownloadError) -> Self {
        Self::Download(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::proto::Encoder;
    use crate::apk::store::Store;
    use crate::apk::ApkSetPaths;
    use std::cell::RefCell;
    use std::fs::File;
    use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
    use std::net::TcpListener;
    use std::time::Duration;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    const X86_64_LIST: &[u8] = include_bytes!("../../tests/fixtures/apkpure-x86_64.bin");
    const ARM64_LIST: &[u8] = include_bytes!("../../tests/fixtures/apkpure-arm64-v8a.bin");
    const NEWEST_SHA1: &str = "11e803666e1fb0a93784653138ea53194568788f";
    const OTHER_SIGNER: &str = "8fd2f2d244b3d962b6f9bce303c44b1fd8ed13b7";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-apkpure-test-{tag}-{:?}",
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create the test directory");
        dir
    }

    fn hex(digest: &[u8]) -> String {
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    struct Entry<'a> {
        package: &'a str,
        code: &'a str,
        name: &'a str,
        signer: &'a str,
        sha1: &'a str,
        size: u64,
        kind: &'a str,
        url: &'a str,
    }

    const ROBLOX_ENTRY: Entry<'static> = Entry {
        package: ROBLOX_PACKAGE,
        code: "3222",
        name: "2.742.1059",
        signer: ROBLOX_CERTIFICATE_SHA1,
        sha1: NEWEST_SHA1,
        size: 100,
        kind: "APK",
        url: "https://download.pureapk.com/b/APK/x",
    };

    fn version_list(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut list = Encoder::default();
        for entry in entries {
            let mut file = Encoder::default();
            file.string(FILE_SHA1, entry.sha1)
                .varint(FILE_SIZE, entry.size)
                .string(FILE_KIND, entry.kind)
                .string(FILE_URL, entry.url);
            let mut app = Encoder::default();
            app.string(APP_PACKAGE, entry.package)
                .string(APP_VERSION_CODE, entry.code)
                .string(APP_VERSION_NAME, entry.name)
                .string(APP_SIGNER_SHA1, entry.signer)
                .message(APP_FILE, &file);
            let mut body = Encoder::default();
            body.message(LIST_APP, &app);
            let mut item = Encoder::default();
            item.string(LIST_KIND, VERSION_LIST)
                .message(LIST_BODY, &body);
            list.message(LIST_ITEM, &item);
        }
        let mut seven = Encoder::default();
        seven.message(LIST_FIELDS[1], &list);
        let mut top = Encoder::default();
        top.message(LIST_FIELDS[0], &seven);
        top.into_bytes()
    }

    fn release(name: &str, code: u32) -> Release {
        Release {
            version_name: name.to_owned(),
            version_code: VersionCode(code),
        }
    }

    #[test]
    fn the_version_list_request_is_an_anonymous_get_with_an_honest_user_agent() {
        let request = versions_request(&versions_url(API), Abi::X86_64);
        assert_eq!(request.method(), "GET");
        assert_eq!(
            request.uri().to_string(),
            "https://api.pureapk.com/m/v3/cms/app_version?hl=en-US&package_name=com.roblox.client"
        );
        assert_eq!(
            request.headers()[USER_AGENT],
            format!("Eclipse/{}", crate::VERSION)
        );
        assert_eq!(request.headers()[API_VERSION_HEADER], "29");
        assert_eq!(request.headers()[ABIS_HEADER], "x86_64");
        assert_eq!(
            request.headers().len(),
            3,
            "no cookies, credentials or other headers: {:?}",
            request.headers()
        );
    }

    #[test]
    fn store_failures_after_verification_are_not_called_discarded() {
        let replace = store_failure(StoreError::Replace {
            temp: PathBuf::from("/store/current.json.1.tmp"),
            path: PathBuf::from("/store/current.json"),
            source: io::Error::other("busy"),
        });
        assert!(matches!(replace, ApkPureError::Store(_)), "{replace:?}");
        assert!(!replace.to_string().contains("discarded"), "{replace}");
        assert!(!replace.rejects_offer());

        let unreadable = store_failure(StoreError::Set(ApkSetError::Locate {
            path: PathBuf::from("/store/3222.partial/base.apk"),
            source: io::Error::other("read failed"),
        }));
        assert!(
            matches!(unreadable, ApkPureError::Store(_)),
            "{unreadable:?}"
        );
        assert!(
            !unreadable.to_string().contains("discarded"),
            "{unreadable}"
        );
        assert!(!unreadable.rejects_offer());

        let rejected = store_failure(StoreError::UnexpectedVersion {
            expected: VersionCode(3222),
            found: VersionCode(3212),
        });
        assert!(
            matches!(rejected, ApkPureError::Rejected(_)),
            "{rejected:?}"
        );
        assert!(rejected.to_string().contains("discarded"), "{rejected}");
        assert!(rejected.rejects_offer());
    }

    #[test]
    fn apkpures_real_x86_64_list_offers_roblox_signed_apks_newest_first() {
        let offers = offers(X86_64_LIST).unwrap();
        let listed: Vec<_> = offers
            .iter()
            .map(|offer| (offer.release.to_string(), hex(&offer.sha1), offer.size))
            .collect();
        assert_eq!(
            listed,
            [
                (
                    "Roblox 2.742.1059 (versionCode 3222)".to_owned(),
                    NEWEST_SHA1.to_owned(),
                    234_844_202
                ),
                (
                    "Roblox 2.738.1397 (versionCode 3092)".to_owned(),
                    "4ab4e3ce235bd53755d77f4c81939c6c3353f87f".to_owned(),
                    229_466_269
                ),
                (
                    "Roblox 2.738.1393 (versionCode 3084)".to_owned(),
                    "f2140376703a6e2658dc16bc8799460882267c88".to_owned(),
                    229_429_405
                ),
            ]
        );
        let newest = &offers[0];
        assert_eq!(newest.packaging, Packaging::Apk);
        assert_eq!(newest.url.host(), Some("download.pureapk.com"));
        assert_eq!(
            newest.to_string(),
            "Roblox 2.742.1059 (versionCode 3222) (APK, 224.0 MiB)"
        );
        assert_eq!(
            newest.declared_file(),
            DeclaredFile::Sha1(parse_sha1(NEWEST_SHA1).unwrap())
        );
    }

    #[test]
    fn apkpures_real_arm64_list_names_the_newest_release() {
        assert_eq!(
            newest_release(ARM64_LIST).unwrap(),
            Some(release("2.743.1136", 18934))
        );
        let offers = offers(ARM64_LIST).unwrap();
        assert_eq!(offers[0].version(), VersionCode(18934));
        assert_eq!(offers[0].packaging, Packaging::Xapk);
        assert_eq!(
            offers
                .iter()
                .filter(|offer| offer.version() == VersionCode(3222))
                .map(|offer| offer.packaging)
                .collect::<Vec<_>>(),
            [Packaging::Apk, Packaging::Xapk, Packaging::Xapk],
            "a universal APK comes before the XAPKs of the same release"
        );
    }

    #[test]
    fn only_roblox_signed_by_roblox_is_offered() {
        let upper = ROBLOX_CERTIFICATE_SHA1.to_ascii_uppercase();
        let body = version_list(&[
            Entry {
                signer: OTHER_SIGNER,
                code: "9999",
                ..ROBLOX_ENTRY
            },
            Entry {
                package: "com.roblox.client.fake",
                code: "9998",
                ..ROBLOX_ENTRY
            },
            Entry {
                signer: &upper,
                ..ROBLOX_ENTRY
            },
        ]);
        let offers = offers(&body).unwrap();
        assert_eq!(
            offers.iter().map(Offer::version).collect::<Vec<_>>(),
            [VersionCode(3222)]
        );
        assert_eq!(
            newest_release(&body).unwrap(),
            Some(release("2.742.1059", 3222))
        );
    }

    #[test]
    fn files_eclipse_cannot_install_are_not_offered() {
        let body = version_list(&[
            Entry {
                kind: "OBB",
                ..ROBLOX_ENTRY
            },
            Entry {
                size: MAX_APK_BYTES + 1,
                ..ROBLOX_ENTRY
            },
        ]);
        assert_eq!(offers(&body).unwrap(), []);
    }

    #[test]
    fn lists_with_an_unexpected_layout_are_refused() {
        for (body, problem) in [
            (Vec::new(), "it has no version list"),
            (
                version_list(&[Entry {
                    code: "32a2",
                    ..ROBLOX_ENTRY
                }]),
                "a release has no versionCode",
            ),
            (
                version_list(&[Entry {
                    name: "2..1059",
                    ..ROBLOX_ENTRY
                }]),
                "a release has no version name",
            ),
            (
                version_list(&[Entry {
                    sha1: "11e8",
                    ..ROBLOX_ENTRY
                }]),
                "a file has no SHA-1",
            ),
            (
                version_list(&[Entry {
                    size: 0,
                    ..ROBLOX_ENTRY
                }]),
                "a file has no size",
            ),
        ] {
            let err = offers(&body).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("APKPure's list of Roblox releases has an unexpected layout ({problem})")
            );
        }
        let err = offers(&[0x0a, 0x05, 0x01]).unwrap_err();
        assert!(matches!(err, ApkPureError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn download_links_must_point_at_apkpures_servers_over_https() {
        for url in [
            "https://evil.example/b/APK/x",
            "http://download.pureapk.com/b/APK/x",
            "https://download.pureapk.com.evil.example/b/APK/x",
            "https://user@download.pureapk.com/b/APK/x",
        ] {
            let err = offers(&version_list(&[Entry {
                url,
                ..ROBLOX_ENTRY
            }]))
            .unwrap_err();
            assert!(
                matches!(&err, ApkPureError::UntrustedLink { release: listed } if *listed == release("2.742.1059", 3222)),
                "{url}: {err:?}"
            );
        }
        for url in [
            "https://download.pureapk.com/b/APK/x?_fn=a",
            "https://d.apkpure.com/b/APK/com.roblox.client?versionCode=3222",
            "https://data.winudf.com/APK/x",
        ] {
            assert!(https::trusted_uri(url, DOWNLOAD_HOSTS).is_ok(), "{url}");
        }
    }

    fn listed_by<'a>(
        answers: &'a [(Abi, Result<&'a [u8], u16>)],
        asked: &'a mut Vec<Abi>,
    ) -> impl FnMut(Abi) -> Result<Vec<u8>, ApkPureError> + 'a {
        move |abi| {
            asked.push(abi);
            let (_, answer) = answers
                .iter()
                .find(|(listed, _)| *listed == abi)
                .expect("only listed ABIs are asked for");
            answer.map(<[u8]>::to_vec).map_err(ApkPureError::Status)
        }
    }

    #[test]
    fn a_newer_x86_64_build_is_offered_without_asking_for_other_abis() {
        let answers = [(Abi::X86_64, Ok(X86_64_LIST))];
        for installed in [None, Some(VersionCode(3212))] {
            let mut asked = Vec::new();
            let listing =
                list_from(Abi::X86_64, installed, listed_by(&answers, &mut asked)).unwrap();
            assert_eq!(listing.offers[0].version(), VersionCode(3222));
            assert_eq!(listing.newer_without_build, None);
            assert_eq!(asked, [Abi::X86_64]);
        }
    }

    #[test]
    fn apkpure_offers_the_newest_release_directly_to_arm64() {
        let answers = [(Abi::Arm64V8a, Ok(ARM64_LIST))];
        for installed in [None, Some(VersionCode(3222))] {
            let mut asked = Vec::new();
            let listing =
                list_from(Abi::Arm64V8a, installed, listed_by(&answers, &mut asked)).unwrap();
            assert_eq!(listing.offers[0].release, release("2.743.1136", 18934));
            assert_eq!(listing.newer_without_build, None);
            assert_eq!(asked, [Abi::Arm64V8a]);
        }
    }

    #[test]
    fn a_newer_release_without_an_x86_64_build_is_named() {
        let answers = [
            (Abi::X86_64, Ok(X86_64_LIST)),
            (Abi::Arm64V8a, Ok(ARM64_LIST)),
        ];
        let mut asked = Vec::new();
        let listing = list_from(
            Abi::X86_64,
            Some(VersionCode(3222)),
            listed_by(&answers, &mut asked),
        )
        .unwrap();
        assert_eq!(
            listing.newer_without_build,
            Some(release("2.743.1136", 18934))
        );
        assert_eq!(asked, [Abi::X86_64, Abi::Arm64V8a]);

        let mut asked = Vec::new();
        let listing = list_from(
            Abi::X86_64,
            Some(VersionCode(18934)),
            listed_by(&answers, &mut asked),
        )
        .unwrap();
        assert_eq!(listing.newer_without_build, None, "nothing is newer");

        let unanswered = [(Abi::X86_64, Ok(X86_64_LIST)), (Abi::Arm64V8a, Err(503))];
        let mut asked = Vec::new();
        let listing = list_from(
            Abi::X86_64,
            Some(VersionCode(3222)),
            listed_by(&unanswered, &mut asked),
        )
        .unwrap();
        assert_eq!(
            listing.newer_without_build, None,
            "the x86_64 answer stands when the other list is unavailable"
        );
        assert_eq!(listing.offers.len(), 3);

        let down = [(Abi::X86_64, Err(503))];
        let mut asked = Vec::new();
        let err = list_from(Abi::X86_64, None, listed_by(&down, &mut asked)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "APKPure answered HTTP 503; it may be refusing automated requests right now"
        );
    }

    struct MockServer {
        url: String,
        requests: std::sync::mpsc::Receiver<String>,
        thread: std::thread::JoinHandle<()>,
    }

    fn serve(responses: Vec<Vec<u8>>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (sender, requests) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                sender.send(head).unwrap();
                stream.write_all(&response).ok();
            }
        });
        MockServer {
            url,
            requests,
            thread,
        }
    }

    fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn plain_http_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into()
    }

    #[test]
    fn the_version_list_is_fetched_with_its_headers_from_an_http_server() {
        let oversized = vec![0u8; MAX_LISTING_BYTES as usize + 1];
        let server = serve(vec![
            http_response("200 OK", X86_64_LIST),
            http_response("404 Not Found", b""),
            http_response("429 Too Many Requests", b""),
            http_response("200 OK", &oversized),
        ]);
        let agent = plain_http_agent();
        let url = versions_url(&server.url);

        let body = fetch_versions(&agent, &url, Abi::X86_64, None).unwrap();
        assert_eq!(offers(&body).unwrap(), offers(X86_64_LIST).unwrap());
        let head = server.requests.recv().unwrap().to_ascii_lowercase();
        assert!(
            head.starts_with(
                "get /m/v3/cms/app_version?hl=en-us&package_name=com.roblox.client http/1.1\r\n"
            ),
            "{head}"
        );
        for header in [
            "x-sv: 29\r\n".to_owned(),
            "x-abis: x86_64\r\n".to_owned(),
            format!("user-agent: eclipse/{}\r\n", crate::VERSION),
        ] {
            assert!(head.contains(&header), "{header} missing from {head}");
        }
        assert!(!head.contains("cookie"), "{head}");

        let err = fetch_versions(&agent, &url, Abi::Arm64V8a, None).unwrap_err();
        assert!(matches!(err, ApkPureError::Status(404)), "{err:?}");
        assert!(server
            .requests
            .recv()
            .unwrap()
            .to_ascii_lowercase()
            .contains("x-abis: arm64-v8a\r\n"));
        let err = fetch_versions(&agent, &url, Abi::X86_64, None).unwrap_err();
        assert!(
            err.to_string().contains("refusing automated requests"),
            "{err}"
        );
        let err = fetch_versions(&agent, &url, Abi::X86_64, None).unwrap_err();
        assert!(matches!(err, ApkPureError::Listing { .. }), "{err:?}");
        server.thread.join().unwrap();
    }

    #[test]
    fn a_list_request_ends_at_the_check_deadline() {
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = proxy.local_addr().unwrap();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(60)))
            .proxy(Some(
                ureq::Proxy::new(&format!("http://{address}")).unwrap(),
            ))
            .build()
            .into();
        let url = versions_url(API);

        let started = Instant::now();
        let err = fetch_versions(
            &agent,
            &url,
            Abi::X86_64,
            Some(started + Duration::from_millis(300)),
        )
        .expect_err("the proxy never answers");
        let waited = started.elapsed();
        assert!(matches!(err, ApkPureError::OutOfTime), "{err:?}");
        assert!(
            waited >= Duration::from_millis(300) && waited < Duration::from_secs(10),
            "{waited:?}"
        );
        assert!(err.to_string().contains("`eclipse update`"), "{err}");

        let passed = fetch_versions(&agent, &url, Abi::Arm64V8a, Some(Instant::now()));
        assert!(matches!(passed, Err(ApkPureError::OutOfTime)), "{passed:?}");
        proxy.set_nonblocking(true).unwrap();
        assert!(
            proxy.accept().is_ok(),
            "the first request reached the proxy"
        );
        assert_eq!(
            proxy.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "a list asked for after the deadline is not requested at all"
        );
    }

    struct FailingBody;

    impl Read for FailingBody {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timeout: receive body",
            ))
        }
    }

    fn whole<R: Read>(body: R, length: Option<u64>) -> Download<R> {
        Download {
            body,
            extent: Extent::Whole { length },
            validator: None,
        }
    }

    fn served<R: Read>(
        body: R,
        length: Option<u64>,
    ) -> impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkPureError> {
        let mut body = Some(body);
        move |resume| {
            assert_eq!(resume, None, "an uninterrupted download is not continued");
            Ok(whole(
                body.take().expect("the download is opened once"),
                length,
            ))
        }
    }

    fn offer_of(bytes: &[u8]) -> Offer {
        let digest = ring::digest::digest(&SHA1_FOR_LEGACY_USE_ONLY, bytes);
        Offer {
            size: bytes.len() as u64,
            sha1: digest.as_ref().try_into().unwrap(),
            ..offers(X86_64_LIST).unwrap().remove(0)
        }
    }

    fn save<R: Read>(
        open: impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkPureError>,
        offer: &Offer,
        dest: &Path,
    ) -> Result<(), ApkPureError> {
        save_verified(open, offer, dest, &StatusSink::terminal())
    }

    #[test]
    fn downloads_are_kept_only_at_the_listed_size_and_sha1() {
        let dir = temp_dir("save");
        let dest = dir.join("download");
        let body: Vec<u8> = (0..100u8).collect();
        let offer = offer_of(&body);

        save(served(&body[..], Some(100)), &offer, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        save(served(&body[..], None), &offer, &dest).unwrap();

        let err = save(served(&body[..99], Some(99)), &offer, &dest).unwrap_err();
        assert!(
            matches!(
                err,
                ApkPureError::SizeMismatch {
                    listed: 100,
                    actual: 99
                }
            ),
            "{err:?}"
        );
        assert!(err.rejects_offer());
        let err = save(served(&body[..99], None), &offer, &dest).unwrap_err();
        assert!(matches!(err, ApkPureError::SizeMismatch { .. }), "{err:?}");
        let longer = [&body[..], &[0]].concat();
        let err = save(served(&longer[..], None), &offer, &dest).unwrap_err();
        assert!(
            matches!(
                err,
                ApkPureError::Download(DownloadError::TooLarge { limit: 100 })
            ),
            "{err:?}"
        );
        assert!(err.rejects_offer());
        let mut altered = body.clone();
        altered[50] ^= 1;
        let err = save(served(&altered[..], Some(100)), &offer, &dest).unwrap_err();
        assert!(matches!(err, ApkPureError::HashMismatch), "{err:?}");
        assert!(err.rejects_offer());
        assert!(err.to_string().contains("discarded"), "{err}");

        let err = save(served(FailingBody, Some(100)), &offer, &dest).unwrap_err();
        assert!(
            matches!(err, ApkPureError::Download(DownloadError::Interrupted(_))),
            "{err:?}"
        );
        assert!(
            !err.rejects_offer(),
            "a dropped connection is retried later"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dropped_download_continues_and_the_sha1_covers_every_byte() {
        let dir = temp_dir("resume");
        let dest = dir.join("download");
        let body: Vec<u8> = (0..100u8).collect();
        let offer = offer_of(&body);
        let mut offsets = Vec::new();
        save(
            |resume: Option<&Resume>| {
                offsets.push(resume.map(|resume| resume.offset));
                Ok(match resume {
                    None => whole(
                        Box::new((&body[..60]).chain(FailingBody)) as Box<dyn Read>,
                        Some(100),
                    ),
                    Some(resume) => Download {
                        body: Box::new(&body[resume.offset as usize..]) as Box<dyn Read>,
                        extent: Extent::Partial(https::ContentRange {
                            start: resume.offset,
                            total: Some(100),
                        }),
                        validator: None,
                    },
                })
            },
            &offer,
            &dest,
        )
        .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(offsets, [None, Some(60)]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_expired_link_is_renewed_only_for_the_same_file() {
        let offer = offers(X86_64_LIST).unwrap().remove(0);
        let renewed_url =
            Uri::try_from("https://download.pureapk.com/b/APK/renewed?_fn=x").unwrap();
        let resume = Resume {
            offset: 60,
            validator: Some("\"v1\"".to_owned()),
        };
        let answer = |url: &Uri| -> Result<Download<&'static [u8]>, DownloadError> {
            if url == &offer.url {
                return Err(DownloadError::Status(403));
            }
            Ok(Download {
                body: &[],
                extent: Extent::Partial(https::ContentRange {
                    start: 60,
                    total: Some(offer.size),
                }),
                validator: None,
            })
        };

        let mut link = offer.url.clone();
        let mut opened = Vec::new();
        open_renewing(
            &offer,
            &mut link,
            Some(&resume),
            |url, _| {
                opened.push(url.clone());
                answer(url)
            },
            || {
                Ok(vec![Offer {
                    url: renewed_url.clone(),
                    ..offer.clone()
                }])
            },
        )
        .expect("the download continues from the renewed link");
        assert_eq!(link, renewed_url);
        assert_eq!(opened, [offer.url.clone(), renewed_url.clone()]);

        let mut other = offer.clone();
        other.sha1[0] ^= 1;
        other.url = renewed_url.clone();
        let mut link = offer.url.clone();
        let err = open_renewing(
            &offer,
            &mut link,
            Some(&resume),
            |url, _| answer(url),
            || Ok(vec![other]),
        )
        .err()
        .expect("another file is not appended");
        assert!(matches!(err, ApkPureError::Superseded { .. }), "{err:?}");
        assert_eq!(link, offer.url);

        let mut link = offer.url.clone();
        let err = open_renewing(
            &offer,
            &mut link,
            None,
            |url, _| answer(url),
            || panic!("a refused first request is not renewed"),
        )
        .err()
        .expect("the first request fails");
        assert!(
            matches!(err, ApkPureError::Download(DownloadError::Status(403))),
            "{err:?}"
        );
    }

    fn write_bundle(path: &Path, members: &[(&str, &Path)]) {
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let mut writer = ZipWriter::new(File::create(path).unwrap());
        writer.start_file("manifest.json", stored).unwrap();
        writer
            .write_all(b"{\"package_name\":\"com.roblox.client\"}")
            .unwrap();
        for (name, source) in members {
            writer.start_file(*name, stored).unwrap();
            io::copy(&mut File::open(source).unwrap(), &mut writer).unwrap();
        }
        writer.finish().unwrap();
    }

    fn local_offer(set: &ApkSet, packaging: Packaging, download: &Path) -> Offer {
        let bytes = fs::read(download).unwrap();
        let digest = ring::digest::digest(&SHA1_FOR_LEGACY_USE_ONLY, &bytes);
        Offer {
            release: Release {
                version_name: set.version_name().unwrap().to_owned(),
                version_code: set.version_code(),
            },
            packaging,
            size: bytes.len() as u64,
            sha1: digest.as_ref().try_into().unwrap(),
            url: Uri::try_from("https://download.pureapk.com/b/XAPK/local").unwrap(),
        }
    }

    fn ranged<'a>(
        download: &'a Path,
        opened: &'a RefCell<Vec<Option<u64>>>,
    ) -> impl FnMut(Option<&Resume>) -> Result<Download<File>, ApkPureError> + 'a {
        let length = fs::metadata(download).unwrap().len();
        move |resume| {
            opened.borrow_mut().push(resume.map(|resume| resume.offset));
            let mut file = File::open(download).unwrap();
            Ok(match resume {
                None => whole(file, Some(length)),
                Some(resume) => {
                    file.seek(SeekFrom::Start(resume.offset)).unwrap();
                    Download {
                        body: file,
                        extent: Extent::Partial(https::ContentRange {
                            start: resume.offset,
                            total: Some(length),
                        }),
                        validator: None,
                    }
                }
            })
        }
    }

    fn install_file(
        offer: &Offer,
        store: &Store,
        download: &Path,
    ) -> (Result<ApkSet, ApkPureError>, Vec<Option<u64>>) {
        let opened = RefCell::new(Vec::new());
        let status = StatusSink::terminal();
        let installed = install_with(
            offer,
            store.begin(&status).unwrap(),
            &status,
            ranged(download, &opened),
        );
        (installed, opened.into_inner())
    }

    fn directory_offset(bundle: &Path) -> Option<u64> {
        let length = fs::metadata(bundle).unwrap().len();
        Some(length.saturating_sub(DIRECTORY_TAIL_BYTES))
    }

    #[test]
    fn an_xapk_without_the_host_split_is_skipped_after_reading_only_its_directory() {
        let listed: Vec<_> = offers(ARM64_LIST).unwrap().into_iter().take(2).collect();
        assert!(
            listed
                .iter()
                .all(|offer| offer.version() == VersionCode(18934)
                    && offer.packaging == Packaging::Xapk),
            "APKPure's arm64-v8a list carries two bundles of 2.743.1136 and names neither one's \
             ABI: {listed:?}"
        );
        let dir = temp_dir("xapk-abi");
        let base = dir.join("base.apk");
        let mut writer = ZipWriter::new(File::create(&base).unwrap());
        writer
            .start_file(
                "classes.dex",
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer
            .write_all(&vec![0u8; 2 * DIRECTORY_TAIL_BYTES as usize])
            .unwrap();
        writer.finish().unwrap();
        let split = dir.join("split.apk");
        fs::write(&split, b"native code").unwrap();
        let armeabi = dir.join("armeabi-v7a.xapk");
        write_bundle(
            &armeabi,
            &[
                ("com.roblox.client.apk", &base),
                ("config.armeabi_v7a.apk", &split),
            ],
        );
        let host = dir.join("host.xapk");
        write_bundle(
            &host,
            &[
                ("com.roblox.client.apk", &base),
                (Abi::HOST.xapk_split_entry(), &split),
            ],
        );
        let as_listed = |listed: &Offer, bundle: &Path| {
            let file = offer_of(&fs::read(bundle).unwrap());
            Offer {
                size: file.size,
                sha1: file.sha1,
                ..listed.clone()
            }
        };
        let store = Store::at(dir.join("store"));

        let (installed, opened) = install_file(&as_listed(&listed[0], &armeabi), &store, &armeabi);
        let err = installed.err().expect("an armeabi-v7a bundle is skipped");
        assert!(matches!(err, ApkPureError::NoHostSplit), "{err:?}");
        assert!(
            !err.rejects_offer(),
            "a skipped bundle is not remembered as a rejected download"
        );
        assert_eq!(
            opened,
            [directory_offset(&armeabi)],
            "only the directory at the end of the bundle was fetched"
        );

        let (installed, opened) = install_file(&as_listed(&listed[1], &host), &store, &host);
        let err = installed
            .err()
            .expect("the placeholder APKs fail verification");
        assert!(matches!(err, ApkPureError::Rejected(_)), "{err:?}");
        assert_eq!(
            opened,
            [directory_offset(&host), None],
            "a bundle with the host split is downloaded"
        );

        let fat_base = dir.join("fat-base.apk");
        let mut writer = ZipWriter::new(File::create(&fat_base).unwrap());
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("classes.dex", stored).unwrap();
        writer
            .write_all(&vec![0u8; 2 * DIRECTORY_TAIL_BYTES as usize])
            .unwrap();
        let engine = format!(
            "lib/{}/{}",
            Abi::HOST.android_name(),
            crate::apk::ENGINE_LIB
        );
        writer.start_file(engine, stored).unwrap();
        writer.write_all(b"native code").unwrap();
        writer.finish().unwrap();
        let universal = dir.join("universal.xapk");
        write_bundle(
            &universal,
            &[
                ("com.roblox.client.apk", &fat_base),
                ("config.xxhdpi.apk", &split),
                ("config.en.apk", &split),
            ],
        );
        let (installed, opened) =
            install_file(&as_listed(&listed[1], &universal), &store, &universal);
        let err = installed
            .err()
            .expect("the placeholder base fails verification");
        assert!(
            matches!(err, ApkPureError::Rejected(ref rejected)
                if !matches!(rejected, StoreError::NoNativeSplitGiven)),
            "the store finds the engine in the base APK: {err:?}"
        );
        assert_eq!(
            opened,
            [directory_offset(&universal), None],
            "a bundle that names no ABI's split is downloaded"
        );

        let length = fs::metadata(&armeabi).unwrap().len();
        let mut ignored = Vec::new();
        let status = StatusSink::terminal();
        let err = install_with(
            &as_listed(&listed[0], &armeabi),
            store.begin(&status).unwrap(),
            &status,
            |resume| {
                ignored.push(resume.map(|resume| resume.offset));
                Ok(whole(File::open(&armeabi).unwrap(), Some(length)))
            },
        )
        .err()
        .expect("the store refuses a bundle without the host split");
        assert!(
            matches!(err, ApkPureError::Rejected(StoreError::NoNativeSplitGiven)),
            "a server that ignores the range leaves the decision to the store: {err:?}"
        );
        assert_eq!(ignored, [directory_offset(&armeabi), None]);
        assert_eq!(store.current().unwrap(), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_official_release_installs_from_an_apkpure_bundle_and_nothing_else_does() {
        let Some(paths) = ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
        else {
            eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to install the official Roblox APK set");
            return;
        };
        let Some(split) = paths.native_split.clone() else {
            eprintln!("SKIP: ECLIPSE_ROBLOX_APK is a single APK, not a split set");
            return;
        };
        let official = ApkSet::open(paths.clone()).expect("the official set verifies");
        let dir = temp_dir("official");
        let root = dir.join("store");
        let store = Store::at(root.clone());
        let bundle = dir.join("Roblox.xapk");
        write_bundle(
            &bundle,
            &[
                ("com.roblox.client.apk", &paths.base),
                (Abi::HOST.xapk_split_entry(), &split),
            ],
        );
        let offer = local_offer(&official, Packaging::Xapk, &bundle);

        let mut wrong_version = offer.clone();
        wrong_version.release.version_code = VersionCode(offer.version().0 + 1);
        let err = install_file(&wrong_version, &store, &bundle)
            .0
            .err()
            .expect("a release with another versionCode is refused");
        assert!(
            matches!(
                err,
                ApkPureError::Rejected(StoreError::UnexpectedVersion { .. })
            ),
            "{err:?}"
        );

        let tampered_base = dir.join("tampered-base.apk");
        let mut bytes = fs::read(&paths.base).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        fs::write(&tampered_base, bytes).unwrap();
        let tampered = dir.join("tampered.xapk");
        write_bundle(
            &tampered,
            &[
                ("com.roblox.client.apk", &tampered_base),
                (Abi::HOST.xapk_split_entry(), &split),
            ],
        );
        let err = install_file(
            &local_offer(&official, Packaging::Xapk, &tampered),
            &store,
            &tampered,
        )
        .0
        .err()
        .expect("a modified base APK is refused");
        assert!(
            matches!(
                err,
                ApkPureError::Rejected(StoreError::Set(ApkSetError::Signature { .. }))
            ),
            "a listed SHA-1 never stands in for Roblox's signature: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("APKPure served a file that Roblox did not sign"),
            "{err}"
        );
        assert!(err.rejects_offer());

        let err = install_file(
            &local_offer(&official, Packaging::Apk, &paths.base),
            &store,
            &paths.base,
        )
        .0
        .err()
        .expect("a base APK without native code is refused");
        assert!(
            matches!(err, ApkPureError::Rejected(StoreError::NoNativeSplitGiven)),
            "{err:?}"
        );
        assert_eq!(store.current().unwrap(), None);

        let other_abi = dir.join("armeabi-v7a.xapk");
        write_bundle(
            &other_abi,
            &[
                ("com.roblox.client.apk", &paths.base),
                ("config.armeabi_v7a.apk", &split),
            ],
        );
        let (installed, opened) = install_file(
            &local_offer(&official, Packaging::Xapk, &other_abi),
            &store,
            &other_abi,
        );
        assert!(
            matches!(installed, Err(ApkPureError::NoHostSplit)),
            "a bundle of another ABI is skipped"
        );
        assert_eq!(opened, [directory_offset(&other_abi)]);

        let (installed, opened) = install_file(&offer, &store, &bundle);
        let set = installed.expect("the official bundle installs");
        assert_eq!(opened, [directory_offset(&bundle), None]);
        let version_dir = root.join(offer.version().to_string());
        assert_eq!(set.version_code(), official.version_code());
        assert_eq!(set.base_path(), version_dir.join(BASE_APK));
        assert_eq!(
            set.native_libs_path(),
            version_dir.join(Abi::HOST.split_apk())
        );
        assert!(
            !version_dir.join(BUNDLE_FILE).exists() && !root.join(BUNDLE_FILE).exists(),
            "the downloaded bundle is not kept"
        );
        drop(set);
        fs::remove_dir_all(&dir).ok();
    }
}
