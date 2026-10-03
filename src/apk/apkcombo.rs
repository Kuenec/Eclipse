use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY, SHA1_OUTPUT_LEN};
use ureq::http::header::USER_AGENT;
use ureq::http::{Request, Response, Uri};

use super::store::{
    CheckOutcome, Committed, InstalledVersion, Release, Staging, Store, StoreError, UpdateCheck,
    UpdateOutcome,
};
use super::{
    ApkSet, ApkSetError, VersionCode, BASE_APK, MAX_APK_BYTES, ROBLOX_PACKAGE, TARGET_ABI,
};
use crate::https::{self, Download, DownloadError, Extent, Host, Resume};
use crate::status::StatusSink;

const SITE: &str = "https://apkcombo.com";
const NEWEST_PAGE_PATH: &str = "/roblox/com.roblox.client/download/apk";
const RELEASE_PAGE_PREFIX: &str = "/roblox/com.roblox.client/download/phone-";
const RELEASE_PAGE_SUFFIX: &str = "-apk";
const RELEASE_LINK: &str = "class=\"ver-item\" href=\"";
const MAX_OLDER_PAGES: usize = 3;
const DOWNLOAD_HOSTS: &[Host] = &[Host::Exact(
    "apks.39b7cb94d40914bac590886981b0ed6e.r2.cloudflarestorage.com",
)];
const VARIANT_LINK: &str = "href=\"/r2?u=";
const ABI_LIST_OPEN: &str = "<code>";
const ABI_LIST_CLOSE: &str = "</code>";
const BUNDLE_FILE: &str = "apkcombo.xapk";
const MAX_PAGE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SIZE_DECIMALS: u32 = 3;
const MEBIBYTE: u64 = 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 256 * 1024;
const EXPIRED_LINK_STATUS: u16 = 403;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Packaging {
    Apk,
    Xapk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvertisedSize {
    bytes: u64,
    tolerance: u64,
}

impl AdvertisedSize {
    fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let (number, unit) = (words.next()?, words.next()?);
        if words.next().is_some() {
            return None;
        }
        let unit_bytes: u64 = match unit {
            "KB" => 1024,
            "MB" => MEBIBYTE,
            "GB" => 1024 * MEBIBYTE,
            _ => return None,
        };
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        let decimals = u32::try_from(fraction.len()).ok()?;
        let all_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        if whole.is_empty() || !all_digits(whole) || !all_digits(fraction) {
            return None;
        }
        if decimals > MAX_SIZE_DECIMALS || (number.contains('.') && fraction.is_empty()) {
            return None;
        }
        let mantissa: u64 = format!("{whole}{fraction}").parse().ok()?;
        let scale = 10u64.pow(decimals);
        Some(Self {
            bytes: mantissa.checked_mul(unit_bytes)? / scale,
            tolerance: unit_bytes / scale,
        })
    }

    fn admits(self, bytes: u64) -> bool {
        bytes.abs_diff(self.bytes) <= self.tolerance
    }

    fn download_limit(self) -> u64 {
        self.bytes.saturating_add(self.tolerance).min(MAX_APK_BYTES)
    }
}

impl fmt::Display for AdvertisedSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "about {} MiB", self.bytes.div_ceil(MEBIBYTE))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Newest,
    Release(String),
}

impl Page {
    fn url(&self) -> String {
        match self {
            Self::Newest => format!("{SITE}{NEWEST_PAGE_PATH}"),
            Self::Release(version_name) => {
                format!("{SITE}{RELEASE_PAGE_PREFIX}{version_name}{RELEASE_PAGE_SUFFIX}")
            }
        }
    }
}

impl fmt::Display for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Newest => f.write_str("APKCombo's Roblox download page"),
            Self::Release(version_name) => {
                write!(f, "APKCombo's download page for Roblox {version_name}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Offer {
    version_name: String,
    version_code: VersionCode,
    packaging: Packaging,
    size: AdvertisedSize,
    base_sha1: [u8; SHA1_OUTPUT_LEN],
    url: Uri,
}

impl Offer {
    fn release(&self) -> Release {
        Release {
            version_code: self.version_code,
            base_sha1: self.base_sha1,
        }
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
            "Roblox {} (versionCode {}, {packaging}, {})",
            self.version_name, self.version_code, self.size
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingBuild {
    version_name: String,
    version_code: VersionCode,
    offered: Vec<String>,
}

impl fmt::Display for MissingBuild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Roblox {} (versionCode {}, offered only for {})",
            self.version_name,
            self.version_code,
            self.offered.join(" or ")
        )
    }
}

fn missing_text(releases: &[MissingBuild]) -> String {
    releases
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" or ")
}

#[derive(Debug, PartialEq, Eq)]
enum Listing {
    Offer(Offer),
    Missing(MissingBuild),
}

impl Listing {
    fn release(&self) -> (&str, VersionCode) {
        match self {
            Self::Offer(offer) => (&offer.version_name, offer.version_code),
            Self::Missing(release) => (&release.version_name, release.version_code),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Choice {
    Offered {
        offer: Offer,
        passed_over: Vec<MissingBuild>,
    },
    Kept {
        installed: InstalledVersion,
        passed_over: Vec<MissingBuild>,
    },
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offered { offer, passed_over } if passed_over.is_empty() => {
                write!(f, "APKCombo offers {offer}")
            }
            Self::Offered { offer, passed_over } => write!(
                f,
                "APKCombo has no {TARGET_ABI} build of {} yet, so Eclipse chose {offer}, the \
                 newest release that has one",
                missing_text(passed_over)
            ),
            Self::Kept {
                installed,
                passed_over,
            } if passed_over.is_empty() => write!(
                f,
                "APKCombo has no {TARGET_ABI} build newer than the installed Roblox {installed}"
            ),
            Self::Kept {
                installed,
                passed_over,
            } => write!(
                f,
                "APKCombo has no {TARGET_ABI} build of {} yet, so Eclipse keeps the installed \
                 Roblox {installed} for now",
                missing_text(passed_over)
            ),
        }
    }
}

pub fn update(
    store: &Store,
    current: Option<&ApkSet>,
    rejected: Option<Release>,
    status: &StatusSink,
) -> Result<UpdateOutcome, ApkComboError> {
    let pages = https::request_agent();
    let downloads = https::download_agent();
    update_from(
        store,
        current,
        rejected,
        status,
        |page| fetch_page(&pages, page),
        |url, resume| https::open_download(&downloads, &url.to_string(), DOWNLOAD_HOSTS, resume),
    )
}

fn update_from<R: Read>(
    store: &Store,
    current: Option<&ApkSet>,
    rejected: Option<Release>,
    status: &StatusSink,
    mut fetch: impl FnMut(&Page) -> Result<String, ApkComboError>,
    mut open: impl FnMut(&Uri, Option<&Resume>) -> Result<Download<R>, DownloadError>,
) -> Result<UpdateOutcome, ApkComboError> {
    let installed = current.map(InstalledVersion::from);
    let choice = choose(installed.as_ref(), &mut fetch)?;
    status.step(choice.to_string());
    let offer = match choice {
        Choice::Offered { offer, .. } => offer,
        Choice::Kept { installed, .. } => return Ok(UpdateOutcome::UpToDate { installed }),
    };
    let own_page = Page::Release(offer.version_name.clone());
    let mut link = offer.url.clone();
    update_with(&offer, store, current, rejected, status, |resume| {
        open_renewing(&offer, &mut link, resume, &mut open, || {
            parse_listing(&fetch(&own_page)?, &own_page)
        })
    })
}

fn choose(
    installed: Option<&InstalledVersion>,
    mut fetch: impl FnMut(&Page) -> Result<String, ApkComboError>,
) -> Result<Choice, ApkComboError> {
    let newest_html = fetch(&Page::Newest)?;
    let newest = match parse_listing(&newest_html, &Page::Newest)? {
        Listing::Offer(offer) => {
            return Ok(Choice::Offered {
                offer,
                passed_over: Vec::new(),
            })
        }
        Listing::Missing(release) => release,
    };
    let newer = |code: VersionCode| installed.is_none_or(|installed| code > installed.version_code);
    let mut passed_over = Vec::new();
    if newer(newest.version_code) {
        let own_page = Page::Release(newest.version_name.clone());
        let mut previous = newest.version_code;
        passed_over.push(newest);
        let older = release_pages(&newest_html)
            .filter(|page| !matches!(page, Ok(page) if *page == own_page))
            .take(MAX_OLDER_PAGES);
        for page in older {
            let page = page?;
            let listing = parse_listing(&fetch(&page)?, &page)?;
            let (version_name, version_code) = listing.release();
            if !matches!(&page, Page::Release(listed) if listed == version_name) {
                return Err(ApkComboError::layout(
                    &page,
                    "a release page lists another release",
                ));
            }
            if version_code >= previous {
                return Err(ApkComboError::layout(
                    &page,
                    "old versions are not listed newest first",
                ));
            }
            previous = version_code;
            match listing {
                Listing::Offer(offer) if newer(version_code) => {
                    return Ok(Choice::Offered { offer, passed_over })
                }
                Listing::Missing(release) if newer(version_code) => passed_over.push(release),
                Listing::Offer(_) | Listing::Missing(_) => break,
            }
        }
    }
    match installed {
        Some(installed) => Ok(Choice::Kept {
            installed: installed.clone(),
            passed_over,
        }),
        None => Err(ApkComboError::NoX86_64 {
            missing: passed_over,
        }),
    }
}

fn open_renewing<R>(
    offer: &Offer,
    link: &mut Uri,
    resume: Option<&Resume>,
    mut open: impl FnMut(&Uri, Option<&Resume>) -> Result<Download<R>, DownloadError>,
    renew: impl FnOnce() -> Result<Listing, ApkComboError>,
) -> Result<Download<R>, ApkComboError> {
    match open(link, resume) {
        Err(DownloadError::Status(EXPIRED_LINK_STATUS)) if resume.is_some() => {}
        opened => return opened.map_err(ApkComboError::Download),
    }
    let renewed = match renew()? {
        Listing::Offer(renewed) if renewed.url.path() == offer.url.path() => renewed,
        Listing::Offer(_) | Listing::Missing(_) => {
            return Err(ApkComboError::Superseded {
                offered: offer.to_string(),
            })
        }
    };
    *link = renewed.url;
    open(link, resume).map_err(ApkComboError::Download)
}

fn update_with<R: Read>(
    offer: &Offer,
    store: &Store,
    current: Option<&ApkSet>,
    rejected: Option<Release>,
    status: &StatusSink,
    open: impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkComboError>,
) -> Result<UpdateOutcome, ApkComboError> {
    let previous = current.map(InstalledVersion::from);
    let staging = store.begin(status)?;
    let recorded = match store.current() {
        Ok(recorded) => recorded,
        Err(error) if error.is_unusable_install() => None,
        Err(error) => return Err(error.into()),
    };
    let unchanged = recorded.as_ref().map(|installed| installed.version_code)
        == previous.as_ref().map(|installed| installed.version_code);
    let (verified, refreshed) = if unchanged {
        (previous.clone(), None)
    } else {
        let set = store.usable_current()?;
        (set.as_ref().map(InstalledVersion::from), set)
    };
    match plan(offer, verified.as_ref(), recorded.as_ref(), rejected)? {
        Plan::UpToDate(installed) => Ok(match refreshed {
            Some(set) => UpdateOutcome::Updated {
                previous,
                committed: Box::new(Committed {
                    set,
                    leftover: None,
                }),
            },
            None => UpdateOutcome::UpToDate { installed },
        }),
        Plan::Skip => Err(remember_rejection(
            store,
            offer,
            ApkComboError::RejectedBefore {
                offered: offer.to_string(),
            },
        )),
        Plan::Download => {
            status.step(format!("Downloading {offer}…"));
            match install(offer, staging, open, status) {
                Ok(committed) => Ok(UpdateOutcome::Updated {
                    previous,
                    committed: Box::new(committed),
                }),
                Err(error) if error.rejects_offer() => Err(remember_rejection(store, offer, error)),
                Err(error) => Err(error),
            }
        }
    }
}

fn remember_rejection(store: &Store, offer: &Offer, rejection: ApkComboError) -> ApkComboError {
    let check = UpdateCheck {
        at: SystemTime::now(),
        rejected: Some(offer.release()),
        outcome: CheckOutcome::Completed,
    };
    match store.record_check(&check) {
        Ok(()) => rejection,
        Err(source) => ApkComboError::NotRemembered {
            rejection: Box::new(rejection),
            source,
        },
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    UpToDate(InstalledVersion),
    Skip,
    Download,
}

fn plan(
    offer: &Offer,
    verified: Option<&InstalledVersion>,
    recorded: Option<&InstalledVersion>,
    rejected: Option<Release>,
) -> Result<Plan, ApkComboError> {
    if let Some(installed) =
        verified.filter(|installed| installed.version_code >= offer.version_code)
    {
        return Ok(Plan::UpToDate(installed.clone()));
    }
    if let Some(installed) =
        recorded.filter(|installed| installed.version_code > offer.version_code)
    {
        return Err(ApkComboError::Older {
            offered: offer.to_string(),
            installed: installed.clone(),
        });
    }
    if rejected == Some(offer.release()) {
        return Ok(Plan::Skip);
    }
    Ok(Plan::Download)
}

fn install<R: Read>(
    offer: &Offer,
    staging: Staging<'_>,
    open: impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkComboError>,
    status: &StatusSink,
) -> Result<Committed, ApkComboError> {
    let base = staging.dir().join(BASE_APK);
    match offer.packaging {
        Packaging::Apk => save_download(open, offer.size, &base, status)?,
        Packaging::Xapk => {
            let bundle = staging.dir().join(BUNDLE_FILE);
            save_download(open, offer.size, &bundle, status)?;
            staging.add_source(&bundle).map_err(store_failure)?;
            fs::remove_file(&bundle).map_err(|source| ApkComboError::Io {
                path: bundle,
                source,
            })?;
        }
    }
    if file_sha1(&base)? != offer.base_sha1 {
        return Err(ApkComboError::BaseHashMismatch);
    }
    status.step("Checking Roblox's signature and installing it…");
    staging
        .commit(Some(offer.version_code))
        .map_err(store_failure)
}

fn store_failure(error: StoreError) -> ApkComboError {
    match error {
        StoreError::NoDataDir
        | StoreError::Io { .. }
        | StoreError::Replace { .. }
        | StoreError::Corrupt { .. }
        | StoreError::MissingInstall { .. }
        | StoreError::Set(ApkSetError::Locate { .. }) => ApkComboError::Store(error),
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
        | StoreError::UnexpectedVersion { .. } => ApkComboError::Rejected(error),
    }
}

fn fetch_page(agent: &ureq::Agent, page: &Page) -> Result<String, ApkComboError> {
    let response = agent
        .run(page_request(page))
        .map_err(|error| ApkComboError::page(page, error))?;
    page_text(page, response)
}

fn page_request(page: &Page) -> Request<()> {
    Request::get(page.url())
        .header(USER_AGENT, format!("Eclipse/{}", crate::VERSION))
        .body(())
        .expect("a GET of an APKCombo page with an ASCII User-Agent is a valid request")
}

fn page_text(page: &Page, response: Response<ureq::Body>) -> Result<String, ApkComboError> {
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(ApkComboError::PageStatus {
            page: page.clone(),
            status,
        });
    }
    let body = response
        .into_body()
        .into_with_config()
        .limit(MAX_PAGE_BYTES)
        .read_to_vec()
        .map_err(|error| ApkComboError::page(page, error))?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn release_pages(html: &str) -> impl Iterator<Item = Result<Page, ApkComboError>> + '_ {
    html.split(RELEASE_LINK).skip(1).map(|piece| {
        piece
            .split_once('"')
            .and_then(|(href, _)| release_page(href))
            .ok_or_else(|| {
                ApkComboError::layout(
                    &Page::Newest,
                    "an old version link does not name a Roblox release page",
                )
            })
    })
}

fn release_page(href: &str) -> Option<Page> {
    let version_name = href
        .strip_prefix(RELEASE_PAGE_PREFIX)?
        .strip_suffix(RELEASE_PAGE_SUFFIX)?;
    version_name
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| Page::Release(version_name.to_owned()))
}

struct VariantLink<'a> {
    abis: &'a str,
    href: &'a str,
    body: &'a str,
}

impl VariantLink<'_> {
    fn has_target_abi(&self) -> bool {
        self.abis.split(',').any(|abi| abi.trim() == TARGET_ABI)
    }
}

fn parse_listing(html: &str, page: &Page) -> Result<Listing, ApkComboError> {
    let (targeted, others): (Vec<_>, Vec<_>) = variant_links(html, page)?
        .into_iter()
        .partition(VariantLink::has_target_abi);
    let offers = targeted
        .iter()
        .map(|link| parse_variant(link, page))
        .collect::<Result<Vec<_>, _>>()?;
    match offers
        .into_iter()
        .max_by_key(|offer| (offer.version_code, offer.packaging == Packaging::Xapk))
    {
        Some(offer) => Ok(Listing::Offer(offer)),
        None => missing_build(&others, page).map(Listing::Missing),
    }
}

fn missing_build(links: &[VariantLink<'_>], page: &Page) -> Result<MissingBuild, ApkComboError> {
    let releases = links
        .iter()
        .map(|link| listed_release(link.body, page))
        .collect::<Result<Vec<_>, _>>()?;
    let (version_name, version_code) = releases
        .into_iter()
        .max_by_key(|&(_, version_code)| version_code)
        .ok_or_else(|| ApkComboError::layout(page, "it has no download links"))?;
    let mut offered: Vec<String> = Vec::new();
    for link in links {
        let abis = link.abis.trim();
        if !offered.iter().any(|listed| listed == abis) {
            offered.push(abis.to_owned());
        }
    }
    Ok(MissingBuild {
        version_name: version_name.to_owned(),
        version_code,
        offered,
    })
}

fn variant_links<'a>(html: &'a str, page: &Page) -> Result<Vec<VariantLink<'a>>, ApkComboError> {
    let mut pieces = html.split(VARIANT_LINK);
    let mut abis = pieces.next().and_then(last_abi_list);
    let mut links = Vec::new();
    for piece in pieces {
        let (anchor, rest) = piece
            .split_once("</a>")
            .ok_or_else(|| ApkComboError::layout(page, "a download link is not closed"))?;
        let (href, body) = anchor
            .split_once('"')
            .ok_or_else(|| ApkComboError::layout(page, "a download link is not quoted"))?;
        links.push(VariantLink {
            abis: abis
                .ok_or_else(|| ApkComboError::layout(page, "a download link has no ABI list"))?,
            href,
            body,
        });
        abis = last_abi_list(rest).or(abis);
    }
    Ok(links)
}

fn last_abi_list(html: &str) -> Option<&str> {
    let start = html.rfind(ABI_LIST_OPEN)? + ABI_LIST_OPEN.len();
    let length = html[start..].find(ABI_LIST_CLOSE)?;
    Some(&html[start..start + length])
}

fn listed_release<'a>(body: &'a str, page: &Page) -> Result<(&'a str, VersionCode), ApkComboError> {
    let version_name = element_text(body, "vername")
        .and_then(|text| text.split_whitespace().last())
        .ok_or_else(|| ApkComboError::layout(page, "a download has no version name"))?;
    let version_code = element_text(body, "vercode")
        .and_then(|text| text.strip_prefix('(')?.strip_suffix(')')?.parse().ok())
        .map(VersionCode)
        .ok_or_else(|| ApkComboError::layout(page, "a download has no versionCode"))?;
    Ok((version_name, version_code))
}

fn parse_variant(link: &VariantLink<'_>, page: &Page) -> Result<Offer, ApkComboError> {
    let href = link.href.replace("&amp;", "&");
    let encoded = href
        .split_once('&')
        .map_or(href.as_str(), |(value, _)| value);
    let url = percent_decode(encoded)
        .ok_or_else(|| ApkComboError::layout(page, "a download link is not percent-encoded"))?;
    let url = https::trusted_uri(&url, DOWNLOAD_HOSTS).map_err(ApkComboError::Download)?;
    let key = ObjectKey::parse(url.path()).ok_or(ApkComboError::UnexpectedLink)?;

    let (version_name, version_code) = listed_release(link.body, page)?;
    let packaging = if link.body.contains("class=\"type-xapk\"") {
        Packaging::Xapk
    } else if link.body.contains("class=\"type-apk\"") {
        Packaging::Apk
    } else {
        return Err(ApkComboError::layout(
            page,
            "a download is neither an APK nor an XAPK",
        ));
    };
    let size = element_text(link.body, "spec ltr")
        .and_then(AdvertisedSize::parse)
        .ok_or_else(|| ApkComboError::layout(page, "a download has no size"))?;

    if key.version_name != version_name || key.version_code != version_code {
        return Err(ApkComboError::Inconsistent {
            listed: format!("{version_name} (versionCode {version_code})"),
            linked: format!("{} (versionCode {})", key.version_name, key.version_code),
        });
    }
    Ok(Offer {
        version_name: version_name.to_owned(),
        version_code,
        packaging,
        size,
        base_sha1: key.base_sha1,
        url,
    })
}

fn element_text<'a>(html: &'a str, class: &str) -> Option<&'a str> {
    let marker = format!("class=\"{class}\">");
    let start = html.find(&marker)? + marker.len();
    let length = html[start..].find('<')?;
    Some(html[start..start + length].trim())
}

struct ObjectKey<'a> {
    version_name: &'a str,
    version_code: VersionCode,
    base_sha1: [u8; SHA1_OUTPUT_LEN],
}

impl<'a> ObjectKey<'a> {
    fn parse(path: &'a str) -> Option<Self> {
        let mut segments = path.strip_prefix('/')?.split('/');
        let (package, version_name, file) = (segments.next()?, segments.next()?, segments.next()?);
        if package != ROBLOX_PACKAGE || version_name.is_empty() || segments.next().is_some() {
            return None;
        }
        let mut parts = file.split('.');
        let (code, sha1, extension) = (parts.next()?, parts.next()?, parts.next()?);
        if extension.is_empty() || parts.next().is_some() {
            return None;
        }
        if code.is_empty() || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(Self {
            version_name,
            version_code: VersionCode(code.parse().ok()?),
            base_sha1: parse_sha1(sha1)?,
        })
    }
}

fn hex_byte(high: u8, low: u8) -> Option<u8> {
    let high = char::from(high).to_digit(16)?;
    let low = char::from(low).to_digit(16)?;
    u8::try_from(high * 16 + low).ok()
}

fn parse_sha1(hex: &str) -> Option<[u8; SHA1_OUTPUT_LEN]> {
    let pairs = hex.as_bytes().as_chunks::<2>();
    if !pairs.1.is_empty() || pairs.0.len() != SHA1_OUTPUT_LEN {
        return None;
    }
    let mut digest = [0u8; SHA1_OUTPUT_LEN];
    for (byte, pair) in digest.iter_mut().zip(pairs.0) {
        *byte = hex_byte(pair[0], pair[1])?;
    }
    Some(digest)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            decoded.push(hex_byte(*bytes.get(index + 1)?, *bytes.get(index + 2)?)?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn save_download<R: Read>(
    mut open: impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkComboError>,
    advertised: AdvertisedSize,
    dest: &Path,
    status: &StatusSink,
) -> Result<(), ApkComboError> {
    let actual = https::save_download(
        |resume| {
            let download = open(resume)?;
            match download.extent {
                Extent::Whole {
                    length: Some(actual),
                } if !advertised.admits(actual) => {
                    Err(ApkComboError::SizeMismatch { advertised, actual })
                }
                _ => Ok(download),
            }
        },
        advertised.download_limit(),
        dest,
        status,
        |_| {},
    )?;
    if !advertised.admits(actual) {
        return Err(ApkComboError::SizeMismatch { advertised, actual });
    }
    Ok(())
}

fn file_sha1(path: &Path) -> Result<[u8; SHA1_OUTPUT_LEN], ApkComboError> {
    let io_error = |source| ApkComboError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(io_error)?;
    let mut context = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(context
        .finish()
        .as_ref()
        .try_into()
        .expect("SHA-1 digests are 20 bytes"))
}

#[derive(Debug)]
pub enum ApkComboError {
    Page {
        page: Page,
        detail: String,
    },

    PageStatus {
        page: Page,
        status: u16,
    },

    PageLayout {
        page: Page,
        problem: &'static str,
    },

    NoX86_64 {
        missing: Vec<MissingBuild>,
    },

    Inconsistent {
        listed: String,
        linked: String,
    },

    UnexpectedLink,

    Download(DownloadError),

    Superseded {
        offered: String,
    },

    SizeMismatch {
        advertised: AdvertisedSize,
        actual: u64,
    },

    BaseHashMismatch,

    Older {
        offered: String,
        installed: InstalledVersion,
    },

    RejectedBefore {
        offered: String,
    },

    Io {
        path: PathBuf,
        source: io::Error,
    },

    Rejected(StoreError),

    Store(StoreError),

    NotRemembered {
        rejection: Box<ApkComboError>,
        source: StoreError,
    },
}

impl ApkComboError {
    fn page(page: &Page, error: ureq::Error) -> Self {
        Self::Page {
            page: page.clone(),
            detail: https::redacted(error),
        }
    }

    fn layout(page: &Page, problem: &'static str) -> Self {
        Self::PageLayout {
            page: page.clone(),
            problem,
        }
    }

    fn rejects_offer(&self) -> bool {
        matches!(
            self,
            Self::Download(DownloadError::TooLarge { .. })
                | Self::SizeMismatch { .. }
                | Self::BaseHashMismatch
                | Self::Rejected(_)
        )
    }
}

impl fmt::Display for ApkComboError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Page { page, detail } => write!(f, "cannot load {page}: {detail}"),
            Self::PageStatus {
                page,
                status: status @ (403 | 429 | 503),
            } => write!(
                f,
                "{page} answered HTTP {status}; it may be refusing automated requests right now, \
                 so try again later or install the APKs with `eclipse install <PATH>`"
            ),
            Self::PageStatus { page, status } => write!(f, "{page} answered HTTP {status}"),
            Self::PageLayout { page, problem } => write!(
                f,
                "{page} has an unexpected layout ({problem}); install the APKs with `eclipse \
                 install <PATH>` until Eclipse supports the new layout"
            ),
            Self::NoX86_64 { missing } => write!(
                f,
                "APKCombo has no {TARGET_ABI} build of {} yet; try again later",
                missing_text(missing)
            ),
            Self::Inconsistent { listed, linked } => write!(
                f,
                "APKCombo lists Roblox {listed} but its download link names {linked}"
            ),
            Self::UnexpectedLink => write!(
                f,
                "APKCombo's download link does not name a {ROBLOX_PACKAGE} release file"
            ),
            Self::Download(error) => {
                write!(f, "the Roblox download from APKCombo failed: {error}")
            }
            Self::Superseded { offered } => write!(
                f,
                "APKCombo's download link for {offered} expired during the download and \
                 APKCombo no longer offers that file; the next update check starts over"
            ),
            Self::SizeMismatch { advertised, actual } => write!(
                f,
                "the Roblox download from APKCombo has {actual} bytes but the page advertised \
                 {advertised}; it was discarded"
            ),
            Self::BaseHashMismatch => write!(
                f,
                "the downloaded {BASE_APK} does not match the SHA-1 in APKCombo's download link; \
                 it was discarded"
            ),
            Self::Older { offered, installed } => write!(
                f,
                "APKCombo offers {offered}, which is older than the installed Roblox \
                 {installed}; Eclipse never installs an older Roblox"
            ),
            Self::RejectedBefore { offered } => write!(
                f,
                "APKCombo still offers {offered}, which Eclipse downloaded and rejected before, \
                 so it is not downloaded again automatically; run `eclipse update` to retry it, \
                 or wait for a newer Roblox"
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Rejected(error) => write!(
                f,
                "the Roblox download from APKCombo failed verification and was discarded: \
                 {error}"
            ),
            Self::Store(error) => error.fmt(f),
            Self::NotRemembered { rejection, source } => write!(
                f,
                "{rejection} (Eclipse could not record the rejection, so it may download the \
                 same file again: {source})"
            ),
        }
    }
}

impl std::error::Error for ApkComboError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Download(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Rejected(error)
            | Self::Store(error)
            | Self::NotRemembered { source: error, .. } => Some(error),
            Self::Page { .. }
            | Self::PageStatus { .. }
            | Self::PageLayout { .. }
            | Self::NoX86_64 { .. }
            | Self::Inconsistent { .. }
            | Self::UnexpectedLink
            | Self::Superseded { .. }
            | Self::SizeMismatch { .. }
            | Self::BaseHashMismatch
            | Self::Older { .. }
            | Self::RejectedBefore { .. } => None,
        }
    }
}

impl From<StoreError> for ApkComboError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<DownloadError> for ApkComboError {
    fn from(error: DownloadError) -> Self {
        Self::Download(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::{ApkSetPaths, NATIVE_SPLIT_APK};
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    const ARM_ONLY_PAGE: &str = include_str!("../../tests/fixtures/apkcombo-2.741.1061.html");
    const LATEST_PAGE: &str = include_str!("../../tests/fixtures/apkcombo-2.740.931.html");
    const OLDER_PAGE: &str = include_str!("../../tests/fixtures/apkcombo-2.739.691.html");
    const MIXED_PAGE: &str = include_str!("../../tests/fixtures/apkcombo-2.738.1397.html");
    const LATEST_BASE_SHA1: &str = "4000331cf5d800c02fc213cc6a51dc823df6ea7c";
    const UNIVERSAL_APK_SHA1: &str = "4ab4e3ce235bd53755d77f4c81939c6c3353f87f";
    const R2_HOST: &str = "apks.39b7cb94d40914bac590886981b0ed6e.r2.cloudflarestorage.com";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-apkcombo-test-{tag}-{:?}",
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create the test directory");
        dir
    }

    fn hex(digest: &[u8]) -> String {
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn installed(code: u32) -> InstalledVersion {
        InstalledVersion {
            version_code: VersionCode(code),
            version_name: Some(format!("2.{code}.0")),
        }
    }

    fn newest_listing(html: &str) -> Result<Listing, ApkComboError> {
        parse_listing(html, &Page::Newest)
    }

    fn offer_on(html: &str) -> Offer {
        match newest_listing(html).unwrap() {
            Listing::Offer(offer) => offer,
            Listing::Missing(release) => panic!("the page has no {TARGET_ABI} build: {release}"),
        }
    }

    fn release(name: &str) -> Page {
        Page::Release(name.to_owned())
    }

    fn page_response(status: u16, body: impl Into<Vec<u8>>) -> Response<ureq::Body> {
        Response::builder()
            .status(status)
            .body(ureq::Body::builder().data(body))
            .unwrap()
    }

    #[test]
    fn page_requests_are_anonymous_https_gets_with_an_honest_user_agent() {
        for (page, url) in [
            (
                Page::Newest,
                "https://apkcombo.com/roblox/com.roblox.client/download/apk",
            ),
            (
                release("2.740.931"),
                "https://apkcombo.com/roblox/com.roblox.client/download/phone-2.740.931-apk",
            ),
        ] {
            let request = page_request(&page);
            assert_eq!(request.method(), "GET");
            assert_eq!(request.uri().to_string(), url);
            assert_eq!(request.uri().scheme_str(), Some("https"));
            assert_eq!(
                request.headers()[USER_AGENT],
                format!("Eclipse/{}", crate::VERSION)
            );
            assert_eq!(
                request.headers().len(),
                1,
                "no cookies, credentials or other headers: {:?}",
                request.headers()
            );
        }
    }

    #[test]
    fn the_page_is_read_only_from_a_success_status_and_below_four_mebibytes() {
        let newest = &Page::Newest;
        assert_eq!(
            page_text(newest, page_response(200, LATEST_PAGE)).unwrap(),
            LATEST_PAGE
        );
        let largest = MAX_PAGE_BYTES as usize - 1;
        assert_eq!(
            page_text(newest, page_response(200, vec![b'a'; largest]))
                .unwrap()
                .len(),
            largest
        );

        let older = &release("2.740.931");
        let err = page_text(older, page_response(200, vec![b'a'; largest + 1])).unwrap_err();
        assert!(
            matches!(&err, ApkComboError::Page { page, .. } if page == older),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("cannot load APKCombo's download page for Roblox 2.740.931: "),
            "{err}"
        );
        for status in [403, 429, 503] {
            let err = page_text(newest, page_response(status, "Just a moment...")).unwrap_err();
            assert!(
                matches!(err, ApkComboError::PageStatus { status: code, .. } if code == status),
                "{err:?}"
            );
            assert!(err.to_string().contains("eclipse install"), "{err}");
        }
        let err = page_text(older, page_response(404, "")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "APKCombo's download page for Roblox 2.740.931 answered HTTP 404"
        );
    }

    #[test]
    fn the_newest_release_page_offers_the_x86_64_xapk() {
        let offer = offer_on(LATEST_PAGE);
        assert_eq!(offer.version_name, "2.740.931");
        assert_eq!(offer.version_code, VersionCode(3170));
        assert_eq!(offer.packaging, Packaging::Xapk);
        assert_eq!(
            offer.size,
            AdvertisedSize {
                bytes: 234 * MEBIBYTE,
                tolerance: MEBIBYTE
            }
        );
        assert!(offer.size.admits(245_807_054));
        assert_eq!(hex(&offer.base_sha1), LATEST_BASE_SHA1);
        assert_eq!(offer.url.host(), Some(R2_HOST));
        assert_eq!(
            offer.url.path(),
            "/com.roblox.client/2.740.931/3170.4000331cf5d800c02fc213cc6a51dc823df6ea7c.apks"
        );
        let query = offer.url.query().expect("a presigned link has a query");
        assert!(query.contains("X-Amz-Expires=14400"), "{query}");
        assert!(
            query.contains("filename%3D%22Roblox_2.740.931_apkcombo.com.xapk%22"),
            "the link is percent-decoded exactly once: {query}"
        );
        assert_eq!(
            offer.to_string(),
            "Roblox 2.740.931 (versionCode 3170, XAPK, about 234 MiB)"
        );
        assert_eq!(
            offer.release(),
            Release {
                version_code: VersionCode(3170),
                base_sha1: parse_sha1(LATEST_BASE_SHA1).unwrap()
            }
        );
    }

    #[test]
    fn a_universal_apk_is_chosen_when_the_xapk_has_no_x86_64_split() {
        let offer = offer_on(MIXED_PAGE);
        assert_eq!(offer.version_code, VersionCode(3092));
        assert_eq!(offer.version_name, "2.738.1397");
        assert_eq!(offer.packaging, Packaging::Apk);
        assert_eq!(hex(&offer.base_sha1), UNIVERSAL_APK_SHA1);
        assert!(offer.size.admits(229_466_269));
    }

    #[test]
    fn an_xapk_is_preferred_over_a_universal_apk_of_the_same_release() {
        let page = MIXED_PAGE.replace(
            "<code>arm64-v8a, armeabi-v7a</code>",
            "<code>arm64-v8a, armeabi-v7a, x86_64</code>",
        );
        let offer = offer_on(&page);
        assert_eq!(offer.packaging, Packaging::Xapk);
        assert_eq!(offer.version_code, VersionCode(3092));
    }

    #[test]
    fn a_page_without_an_x86_64_build_names_its_release_and_abis() {
        let newest = MissingBuild {
            version_name: "2.741.1061".to_owned(),
            version_code: VersionCode(3212),
            offered: vec!["arm64-v8a, armeabi-v7a".to_owned()],
        };
        assert_eq!(
            newest_listing(ARM_ONLY_PAGE).unwrap(),
            Listing::Missing(newest.clone())
        );
        assert_eq!(
            newest.to_string(),
            "Roblox 2.741.1061 (versionCode 3212, offered only for arm64-v8a, armeabi-v7a)"
        );

        let page = MIXED_PAGE.replace(", x86_64</code>", "</code>");
        let Listing::Missing(release) = newest_listing(&page).unwrap() else {
            panic!("the page has no {TARGET_ABI} build");
        };
        assert_eq!(release.version_code, VersionCode(3092));
        assert_eq!(
            release.offered,
            ["arm64-v8a, armeabi-v7a"],
            "each offered ABI list is reported once"
        );
    }

    fn gap_site() -> Vec<(Page, String)> {
        vec![
            (Page::Newest, ARM_ONLY_PAGE.to_owned()),
            (release("2.741.1061"), ARM_ONLY_PAGE.to_owned()),
            (release("2.740.931"), LATEST_PAGE.to_owned()),
            (release("2.739.691"), OLDER_PAGE.to_owned()),
        ]
    }

    fn served_pages<'a>(
        site: &'a [(Page, String)],
        fetched: &'a mut Vec<Page>,
    ) -> impl FnMut(&Page) -> Result<String, ApkComboError> + 'a {
        move |page| {
            fetched.push(page.clone());
            site.iter()
                .find(|(listed, _)| listed == page)
                .map(|(_, html)| html.clone())
                .ok_or_else(|| ApkComboError::PageStatus {
                    page: page.clone(),
                    status: 404,
                })
        }
    }

    fn choose_on(
        site: &[(Page, String)],
        installed: Option<&InstalledVersion>,
    ) -> (Result<Choice, ApkComboError>, Vec<Page>) {
        let mut fetched = Vec::new();
        let choice = choose(installed, served_pages(site, &mut fetched));
        (choice, fetched)
    }

    fn arm_only(version_name: &str, code: u32) -> MissingBuild {
        MissingBuild {
            version_name: version_name.to_owned(),
            version_code: VersionCode(code),
            offered: vec!["arm64-v8a, armeabi-v7a".to_owned()],
        }
    }

    #[test]
    fn older_release_pages_are_read_from_the_old_versions_list() {
        let pages = release_pages(ARM_ONLY_PAGE)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            pages,
            [
                release("2.741.1061"),
                release("2.740.931"),
                release("2.739.691")
            ]
        );
        assert!(release_pages(LATEST_PAGE).next().is_none());
        for href in [
            "/roblox/com.roblox.client/download/phone-2.740.931/../x-apk",
            "/roblox/com.evil.client/download/phone-2.740.931-apk",
            "https://evil.example/roblox/com.roblox.client/download/phone-2.740.931-apk",
            "/roblox/com.roblox.client/download/phone--apk",
            "/roblox/com.roblox.client/download/phone-2..931-apk",
            "/roblox/com.roblox.client/download/phone-2.740.931-apk?u=x",
        ] {
            let page = ARM_ONLY_PAGE.replace(
                "/roblox/com.roblox.client/download/phone-2.740.931-apk",
                href,
            );
            let err = release_pages(&page)
                .collect::<Result<Vec<_>, _>>()
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    ApkComboError::PageLayout {
                        page: Page::Newest,
                        ..
                    }
                ),
                "{href}: {err:?}"
            );
        }
    }

    #[test]
    fn the_newest_release_is_offered_directly_when_it_has_an_x86_64_build() {
        let site = [(Page::Newest, LATEST_PAGE.to_owned())];
        for installed_code in [None, Some(3120), Some(3170)] {
            let (choice, fetched) = choose_on(&site, installed_code.map(installed).as_ref());
            let choice = choice.unwrap();
            assert_eq!(
                choice,
                Choice::Offered {
                    offer: offer_on(LATEST_PAGE),
                    passed_over: Vec::new()
                }
            );
            assert_eq!(fetched, [Page::Newest]);
            assert_eq!(
                choice.to_string(),
                "APKCombo offers Roblox 2.740.931 (versionCode 3170, XAPK, about 234 MiB)"
            );
        }
    }

    #[test]
    fn a_new_user_gets_the_newest_release_that_has_an_x86_64_build() {
        let site = gap_site();
        let (choice, fetched) = choose_on(&site, None);
        let choice = choice.expect("an older release with an x86_64 build is offered");
        assert_eq!(
            choice,
            Choice::Offered {
                offer: offer_on(LATEST_PAGE),
                passed_over: vec![arm_only("2.741.1061", 3212)]
            }
        );
        assert_eq!(
            fetched,
            [Page::Newest, release("2.740.931")],
            "older pages are read only until one has the build"
        );
        assert_eq!(
            choice.to_string(),
            "APKCombo has no x86_64 build of Roblox 2.741.1061 (versionCode 3212, offered only \
             for arm64-v8a, armeabi-v7a) yet, so Eclipse chose Roblox 2.740.931 (versionCode \
             3170, XAPK, about 234 MiB), the newest release that has one"
        );

        let (choice, _) = choose_on(&site, Some(&installed(3120)));
        assert!(
            matches!(choice.unwrap(), Choice::Offered { offer, .. } if offer.version_code == VersionCode(3170)),
            "an older install is updated to it"
        );
    }

    #[test]
    fn an_installed_release_is_kept_while_nothing_newer_has_an_x86_64_build() {
        let site = gap_site();
        let current = InstalledVersion {
            version_code: VersionCode(3170),
            version_name: Some("2.740.931".to_owned()),
        };
        let (choice, fetched) = choose_on(&site, Some(&current));
        let choice = choice.expect("a missing build is no error for an installed release");
        assert_eq!(
            choice,
            Choice::Kept {
                installed: current,
                passed_over: vec![arm_only("2.741.1061", 3212)]
            }
        );
        assert_eq!(fetched, [Page::Newest, release("2.740.931")]);
        assert_eq!(
            choice.to_string(),
            "APKCombo has no x86_64 build of Roblox 2.741.1061 (versionCode 3212, offered only \
             for arm64-v8a, armeabi-v7a) yet, so Eclipse keeps the installed Roblox 2.740.931 \
             (versionCode 3170) for now"
        );

        for code in [3212, 3300] {
            let (choice, fetched) = choose_on(&site, Some(&installed(code)));
            let choice = choice.unwrap();
            assert_eq!(
                choice,
                Choice::Kept {
                    installed: installed(code),
                    passed_over: Vec::new()
                }
            );
            assert_eq!(fetched, [Page::Newest], "no older page can be newer");
            assert_eq!(
                choice.to_string(),
                format!(
                    "APKCombo has no x86_64 build newer than the installed Roblox {}",
                    installed(code)
                )
            );
        }
    }

    #[test]
    fn older_pages_are_read_newest_first_and_at_most_three() {
        let mut site = gap_site();
        site[2].1 = LATEST_PAGE.replace(", x86_64</code>", "</code>");
        let (choice, fetched) = choose_on(&site, None);
        let Choice::Offered { offer, passed_over } = choice.unwrap() else {
            panic!("the third newest release has an x86_64 build");
        };
        assert_eq!(offer.version_code, VersionCode(3120));
        assert_eq!(
            passed_over,
            [arm_only("2.741.1061", 3212), arm_only("2.740.931", 3170)]
        );
        assert_eq!(
            fetched,
            [Page::Newest, release("2.740.931"), release("2.739.691")]
        );

        site[3].1 = OLDER_PAGE.replace(", x86_64</code>", "</code>");
        let (choice, _) = choose_on(&site, None);
        let err = choice.unwrap_err();
        assert!(
            matches!(&err, ApkComboError::NoX86_64 { missing } if missing.len() == 3),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(
            text.starts_with(
                "APKCombo has no x86_64 build of Roblox 2.741.1061 (versionCode 3212, offered \
                 only for arm64-v8a, armeabi-v7a) or Roblox 2.740.931 (versionCode 3170"
            ) && text.ends_with("yet; try again later"),
            "{text}"
        );
        let (choice, _) = choose_on(&site, Some(&installed(3092)));
        assert!(
            matches!(choice.unwrap(), Choice::Kept { passed_over, .. } if passed_over.len() == 3)
        );

        let names = ["2.738.100", "2.737.100", "2.736.100"];
        for (index, name) in (0u32..).zip(names) {
            site[0].1.push_str(&format!(
                "<a class=\"ver-item\" href=\"{RELEASE_PAGE_PREFIX}{name}{RELEASE_PAGE_SUFFIX}\">"
            ));
            site.push((
                release(name),
                ARM_ONLY_PAGE
                    .replace("2.741.1061", name)
                    .replace("(3212)", &format!("({})", 3100 - index)),
            ));
        }
        let (choice, fetched) = choose_on(&site, None);
        let err = choice.unwrap_err();
        assert!(
            matches!(&err, ApkComboError::NoX86_64 { missing } if missing.len() == 1 + MAX_OLDER_PAGES),
            "{err:?}"
        );
        assert_eq!(
            fetched,
            [
                Page::Newest,
                release("2.740.931"),
                release("2.739.691"),
                release("2.738.100")
            ]
        );
    }

    #[test]
    fn older_pages_must_list_their_own_older_release_from_the_pinned_bucket() {
        let mut site = gap_site();
        for (html, problem) in [
            (
                OLDER_PAGE.to_owned(),
                "a release page lists another release",
            ),
            (
                ARM_ONLY_PAGE.replace("2.741.1061", "2.740.931"),
                "old versions are not listed newest first",
            ),
            (
                LATEST_PAGE.replace("234 MB", "big"),
                "a download has no size",
            ),
        ] {
            site[2].1 = html;
            let err = choose_on(&site, None).0.unwrap_err();
            assert!(
                err.to_string().starts_with(&format!(
                    "APKCombo's download page for Roblox 2.740.931 has an unexpected layout \
                     ({problem}); "
                )),
                "{err}"
            );
        }

        site[2].1 = LATEST_PAGE.replace(".r2.cloudflarestorage.com", ".evil.example");
        let err = choose_on(&site, None).0.unwrap_err();
        assert!(
            matches!(
                err,
                ApkComboError::Download(DownloadError::Untrusted { .. })
            ),
            "{err:?}"
        );

        site.remove(2);
        let err = choose_on(&site, Some(&installed(3120))).0.unwrap_err();
        assert!(
            matches!(&err, ApkComboError::PageStatus { page, status: 404 } if *page == release("2.740.931")),
            "{err:?}"
        );
    }

    #[test]
    fn pages_with_an_unexpected_layout_are_refused() {
        let pages = [
            "<html><title>Just a moment...</title></html>".to_owned(),
            String::new(),
            LATEST_PAGE.replace("</a>", "</span>"),
            LATEST_PAGE.replace("<code>", "<kbd>"),
            LATEST_PAGE.replace("234 MB", "big"),
            LATEST_PAGE.replace("type-xapk", "type-zip"),
            LATEST_PAGE.replace("(3170)", "3170"),
            LATEST_PAGE.replace("class=\"vername\"", "class=\"title\""),
            LATEST_PAGE.replace("https%3A%2F", "https%zz%2F"),
        ];
        for page in pages {
            let err = newest_listing(&page).unwrap_err();
            assert!(
                err.to_string()
                    .starts_with("APKCombo's Roblox download page has an unexpected layout ("),
                "{err}"
            );
            assert!(err.to_string().contains("eclipse install"), "{err}");
        }
    }

    #[test]
    fn listed_and_linked_releases_must_agree() {
        for page in [
            LATEST_PAGE.replace("(3170)", "(3171)"),
            LATEST_PAGE.replace("Roblox 2.740.931<", "Roblox 2.740.932<"),
        ] {
            let err = newest_listing(&page).unwrap_err();
            assert!(matches!(err, ApkComboError::Inconsistent { .. }), "{err:?}");
        }
    }

    #[test]
    fn download_links_must_name_a_roblox_release_file_in_apkcombos_bucket() {
        for page in [
            LATEST_PAGE.replace("https%3A%2F%2Fapks.", "http%3A%2F%2Fapks."),
            LATEST_PAGE.replace(".r2.cloudflarestorage.com", ".evil.example"),
            LATEST_PAGE.replace("%2F%2Fapks.39b7cb94", "%2F%2Fother.39b7cb94"),
        ] {
            let err = newest_listing(&page).unwrap_err();
            assert!(
                matches!(
                    err,
                    ApkComboError::Download(DownloadError::Untrusted { .. })
                ),
                "{err:?}"
            );
        }
        for page in [
            LATEST_PAGE.replace("%2Fcom.roblox.client%2F", "%2Fcom.evil.client%2F"),
            LATEST_PAGE.replace("3170.4000331c", "3170.zz00331c"),
            LATEST_PAGE.replace("3170.4000331c", "+3170.4000331c"),
            LATEST_PAGE.replace(
                "3170.4000331cf5d800c02fc213cc6a51dc823df6ea7c.apks",
                "3170.apks",
            ),
        ] {
            let err = newest_listing(&page).unwrap_err();
            assert!(matches!(err, ApkComboError::UnexpectedLink), "{err:?}");
        }
    }

    #[test]
    fn downloads_are_limited_to_apkcombos_r2_bucket_over_https() {
        for url in [
            format!("https://{R2_HOST}/com.roblox.client/2.740.931/3170.x.apks?X-Amz-Expires=1"),
            format!("https://{}:443/x", R2_HOST.to_ascii_uppercase()),
        ] {
            assert!(https::trusted_uri(&url, DOWNLOAD_HOSTS).is_ok(), "{url}");
        }
        for url in [
            format!("http://{R2_HOST}/x"),
            format!("https://evil.{R2_HOST}/x"),
            "https://apks.example.r2.cloudflarestorage.com/x".to_owned(),
            "https://apkcombo.com/r2?u=x".to_owned(),
        ] {
            let err = https::trusted_uri(&url, DOWNLOAD_HOSTS).unwrap_err();
            assert!(matches!(err, DownloadError::Untrusted { .. }), "{url}");
        }
    }

    #[test]
    fn advertised_sizes_are_read_at_their_display_precision() {
        let size = AdvertisedSize::parse(" 234 MB").unwrap();
        assert_eq!(
            size,
            AdvertisedSize {
                bytes: 234 * MEBIBYTE,
                tolerance: MEBIBYTE
            }
        );
        assert!(size.admits(245_807_054));
        assert!(!size.admits(236 * MEBIBYTE));
        assert!(!size.admits(232 * MEBIBYTE));
        assert_eq!(size.download_limit(), 235 * MEBIBYTE);

        assert_eq!(
            AdvertisedSize::parse("1.25 GB"),
            Some(AdvertisedSize {
                bytes: 1280 * MEBIBYTE,
                tolerance: 1024 * MEBIBYTE / 100
            })
        );
        assert_eq!(
            AdvertisedSize::parse("850 KB"),
            Some(AdvertisedSize {
                bytes: 850 * 1024,
                tolerance: 1024
            })
        );
        assert_eq!(
            AdvertisedSize::parse("5 GB").unwrap().download_limit(),
            MAX_APK_BYTES
        );
        for text in [
            "",
            "234",
            "234 TB",
            "2.3.4 MB",
            "234. MB",
            ".5 MB",
            "-1 MB",
            "+1 MB",
            "1.2345 GB",
            "234 MB extra",
            "99999999999999999999 GB",
        ] {
            assert_eq!(AdvertisedSize::parse(text), None, "{text:?}");
        }
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

    const SMALL: AdvertisedSize = AdvertisedSize {
        bytes: 100,
        tolerance: 10,
    };

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
    ) -> impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkComboError> {
        let mut body = Some(body);
        move |resume| {
            assert_eq!(resume, None, "an uninterrupted download is not continued");
            Ok(whole(
                body.take().expect("the download is opened once"),
                length,
            ))
        }
    }

    fn save<R: Read>(
        open: impl FnMut(Option<&Resume>) -> Result<Download<R>, ApkComboError>,
        advertised: AdvertisedSize,
        dest: &Path,
    ) -> Result<(), ApkComboError> {
        save_download(open, advertised, dest, &StatusSink::terminal())
    }

    #[test]
    fn downloads_are_kept_only_within_the_advertised_size() {
        let dir = temp_dir("save");
        let dest = dir.join("download");
        let body = [7u8; 100];

        save(served(&body[..], Some(100)), SMALL, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        save(served(&body[..95], None), SMALL, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), &body[..95]);

        for (length, actual) in [(Some(111), 111), (Some(89), 89), (None, 80)] {
            let err =
                save(served(&[0u8; 111][..actual as usize], length), SMALL, &dest).unwrap_err();
            assert!(
                matches!(err, ApkComboError::SizeMismatch { actual: got, .. } if got == actual),
                "{length:?}: {err:?}"
            );
            assert!(err.rejects_offer(), "{err:?}");
        }
        let err = save(served(&[0u8; 111][..], None), SMALL, &dest).unwrap_err();
        assert!(
            matches!(
                err,
                ApkComboError::Download(DownloadError::TooLarge { limit: 110 })
            ),
            "{err:?}"
        );
        assert!(err.rejects_offer(), "{err:?}");
        let huge = AdvertisedSize::parse("5 GB").unwrap();
        let err = save(served(&body[..], Some(huge.bytes)), huge, &dest).unwrap_err();
        assert!(
            matches!(
                err,
                ApkComboError::Download(DownloadError::TooLarge {
                    limit: MAX_APK_BYTES
                })
            ),
            "{err:?}"
        );

        let err = save(served(&body[..95], Some(100)), SMALL, &dest).unwrap_err();
        assert!(
            matches!(
                err,
                ApkComboError::Download(DownloadError::LengthMismatch { .. })
            ),
            "{err:?}"
        );
        assert!(!err.rejects_offer(), "{err:?}");
        let err = save(served(FailingBody, None), SMALL, &dest).unwrap_err();
        assert!(
            matches!(err, ApkComboError::Download(DownloadError::Interrupted(_))),
            "{err:?}"
        );
        assert!(!err.rejects_offer(), "{err:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dropped_download_continues_from_where_it_stopped() {
        let dir = temp_dir("resume");
        let dest = dir.join("download");
        let body: Vec<u8> = (0..100u8).collect();
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
            SMALL,
            &dest,
        )
        .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(offsets, [None, Some(60)]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_expired_link_is_renewed_only_to_continue_the_same_file() {
        let offer = small_offer();
        let renewed_url = |path: &str| {
            Uri::try_from(format!("https://{R2_HOST}{path}?X-Amz-Signature=renewed")).unwrap()
        };
        let resume = Resume {
            offset: 60,
            validator: Some("\"v1\"".to_owned()),
        };
        let answer = |url: &Uri| -> Result<Download<&'static [u8]>, DownloadError> {
            if url == &offer.url {
                return Err(DownloadError::Status(EXPIRED_LINK_STATUS));
            }
            Ok(Download {
                body: &[],
                extent: Extent::Partial(https::ContentRange {
                    start: 60,
                    total: Some(100),
                }),
                validator: None,
            })
        };

        let mut link = offer.url.clone();
        let mut opened = Vec::new();
        let download = open_renewing(
            &offer,
            &mut link,
            Some(&resume),
            |url, resume| {
                opened.push((url.clone(), resume.cloned()));
                answer(url)
            },
            || {
                Ok(Listing::Offer(Offer {
                    url: renewed_url(offer.url.path()),
                    ..small_offer()
                }))
            },
        )
        .expect("the download continues from the renewed link");
        assert!(matches!(download.extent, Extent::Partial(_)));
        assert_eq!(link, renewed_url(offer.url.path()));
        assert_eq!(
            opened,
            [
                (offer.url.clone(), Some(resume.clone())),
                (link.clone(), Some(resume.clone()))
            ]
        );

        let other_file = Listing::Offer(Offer {
            url: renewed_url("/com.roblox.client/2.741.1/3171.aa.apks"),
            ..small_offer()
        });
        let withdrawn = newest_listing(ARM_ONLY_PAGE).unwrap();
        for renewed in [other_file, withdrawn] {
            let mut link = offer.url.clone();
            let err = open_renewing(
                &offer,
                &mut link,
                Some(&resume),
                |url, _| answer(url),
                || Ok(renewed),
            )
            .err()
            .expect("a different file is not appended");
            assert!(matches!(err, ApkComboError::Superseded { .. }), "{err:?}");
            assert_eq!(link, offer.url);
        }

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
            matches!(
                err,
                ApkComboError::Download(DownloadError::Status(EXPIRED_LINK_STATUS))
            ),
            "{err:?}"
        );
    }

    #[test]
    fn an_expired_link_is_renewed_from_the_chosen_releases_own_page() {
        let newest_offered = vec![
            (Page::Newest, LATEST_PAGE.to_owned()),
            (release("2.740.931"), LATEST_PAGE.to_owned()),
        ];
        let path =
            "/com.roblox.client/2.740.931/3170.4000331cf5d800c02fc213cc6a51dc823df6ea7c.apks";
        for (site, choice_pages) in [
            (newest_offered, vec![Page::Newest]),
            (gap_site(), vec![Page::Newest, release("2.740.931")]),
        ] {
            let dir = temp_dir("renew");
            let store = Store::at(dir.join("store"));
            let mut fetched = Vec::new();
            let mut opened = Vec::new();
            let err = update_from(
                &store,
                None,
                None,
                &StatusSink::terminal(),
                served_pages(&site, &mut fetched),
                |url: &Uri, resume: Option<&Resume>| {
                    opened.push((url.path().to_owned(), resume.map(|resume| resume.offset)));
                    match opened.len() {
                        1 => Ok(whole(
                            Box::new((&[0u8; 60][..]).chain(FailingBody)) as Box<dyn Read>,
                            Some(234 * MEBIBYTE),
                        )),
                        2 => Err(DownloadError::Status(EXPIRED_LINK_STATUS)),
                        _ => Err(DownloadError::Status(404)),
                    }
                },
            )
            .err()
            .expect("the renewed link is refused");
            assert!(
                matches!(err, ApkComboError::Download(DownloadError::Status(404))),
                "{err:?}"
            );
            assert_eq!(
                fetched,
                [choice_pages, vec![release("2.740.931")]].concat(),
                "the expired link is renewed from the page of the release being downloaded, \
                 which keeps listing its file after the main page moves on"
            );
            assert_eq!(
                opened,
                [
                    (path.to_owned(), None),
                    (path.to_owned(), Some(60)),
                    (path.to_owned(), Some(60))
                ]
            );
            assert_eq!(store.last_check().unwrap(), None);
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn sha1_digests_match_the_standard_test_vector_and_parse_from_hex() {
        let dir = temp_dir("sha1");
        let path = dir.join("abc");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            hex(&file_sha1(&path).unwrap()),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            parse_sha1(LATEST_BASE_SHA1).map(|digest| hex(&digest)),
            Some(LATEST_BASE_SHA1.to_owned())
        );
        let too_long = format!("{LATEST_BASE_SHA1}00");
        for text in ["", "4000331c", &LATEST_BASE_SHA1[1..], &too_long] {
            assert_eq!(parse_sha1(text), None, "{text}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_a_newer_release_is_downloaded_and_roblox_is_never_downgraded() {
        let offer = offer_on(LATEST_PAGE);
        assert_eq!(plan(&offer, None, None, None).unwrap(), Plan::Download);
        assert_eq!(
            plan(&offer, Some(&installed(3056)), Some(&installed(3056)), None).unwrap(),
            Plan::Download
        );
        for code in [3170, 3200] {
            assert_eq!(
                plan(&offer, Some(&installed(code)), Some(&installed(code)), None).unwrap(),
                Plan::UpToDate(installed(code))
            );
        }
        assert_eq!(
            plan(&offer, None, Some(&installed(3170)), None).unwrap(),
            Plan::Download,
            "a damaged install of the offered release is repaired"
        );
        let err = plan(&offer, None, Some(&installed(3200)), None).unwrap_err();
        assert!(matches!(err, ApkComboError::Older { .. }), "{err:?}");
        assert!(err.to_string().contains("never installs an older"), "{err}");
    }

    #[test]
    fn a_rejected_release_is_skipped_but_an_installed_or_other_one_is_not() {
        let offer = offer_on(LATEST_PAGE);
        let rejected = Some(offer.release());
        assert_eq!(
            plan(
                &offer,
                Some(&installed(3056)),
                Some(&installed(3056)),
                rejected
            )
            .unwrap(),
            Plan::Skip
        );
        assert_eq!(plan(&offer, None, None, rejected).unwrap(), Plan::Skip);
        assert_eq!(
            plan(
                &offer,
                Some(&installed(3170)),
                Some(&installed(3170)),
                rejected
            )
            .unwrap(),
            Plan::UpToDate(installed(3170)),
            "an installed release is up to date whatever was rejected"
        );
        let mut other_file = offer.release();
        other_file.base_sha1[0] ^= 1;
        let mut older = offer.release();
        older.version_code = VersionCode(3120);
        for other in [other_file, older] {
            assert_eq!(
                plan(&offer, None, None, Some(other)).unwrap(),
                Plan::Download
            );
        }
    }

    fn small_offer() -> Offer {
        Offer {
            size: SMALL,
            ..offer_on(LATEST_PAGE)
        }
    }

    #[test]
    fn a_rejected_release_is_not_downloaded_again_by_the_next_scheduled_check() {
        let dir = temp_dir("rejected");
        let store = Store::at(dir.join("store"));
        let offer = small_offer();

        let err = update_with(
            &offer,
            &store,
            None,
            None,
            &StatusSink::terminal(),
            served(&[0u8; 150][..], Some(150)),
        )
        .err()
        .expect("a download of another size is rejected");
        assert!(
            matches!(err, ApkComboError::SizeMismatch { actual: 150, .. }),
            "{err:?}"
        );
        let check = store
            .last_check()
            .unwrap()
            .expect("the rejection completes the check");
        assert_eq!(check.rejected, Some(offer.release()));

        let mut opened = false;
        let err = update_with(
            &offer,
            &store,
            None,
            check.rejected,
            &StatusSink::terminal(),
            |_| {
                opened = true;
                Ok(whole(&[][..], None))
            },
        )
        .err()
        .expect("the rejected release is not installed");
        assert!(!opened, "the rejected release is not downloaded again");
        assert!(
            matches!(err, ApkComboError::RejectedBefore { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("eclipse update"), "{err}");
        let skipped = store.last_check().unwrap().unwrap();
        assert_eq!(skipped.rejected, Some(offer.release()));
        assert!(skipped.at >= check.at, "skipping completes the check too");

        let mut newer = small_offer();
        newer.version_code = VersionCode(3171);
        for (offer, rejected) in [(&newer, check.rejected), (&offer, None)] {
            let mut opened = false;
            let err = update_with(
                offer,
                &store,
                None,
                rejected,
                &StatusSink::terminal(),
                |_| {
                    opened = true;
                    Ok(whole(&[0u8; 150][..], Some(150)))
                },
            )
            .err()
            .expect("the download is rejected");
            assert!(
                opened,
                "a newer release, or an explicit update, downloads again: {err:?}"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rejection_that_cannot_be_recorded_still_says_why_it_was_rejected() {
        let dir = temp_dir("unrecorded");
        let root = dir.join("store");
        fs::create_dir_all(root.join("last-update-check.json").join("taken")).unwrap();
        let store = Store::at(root);

        let err = update_with(
            &small_offer(),
            &store,
            None,
            None,
            &StatusSink::terminal(),
            served(&[0u8; 150][..], Some(150)),
        )
        .err()
        .expect("a download of another size is rejected");
        let ApkComboError::NotRemembered { rejection, source } = &err else {
            panic!("the failed record is reported: {err:?}");
        };
        assert!(
            matches!(**rejection, ApkComboError::SizeMismatch { actual: 150, .. }),
            "{err:?}"
        );
        assert!(matches!(source, StoreError::Replace { .. }), "{err:?}");
        let text = err.to_string();
        assert!(
            text.contains("has 150 bytes") && text.contains("could not record the rejection"),
            "{text}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_transfer_is_retried_at_the_next_check() {
        let dir = temp_dir("transfer");
        let store = Store::at(dir.join("store"));
        let offer = small_offer();

        let err = update_with(
            &offer,
            &store,
            None,
            None,
            &StatusSink::terminal(),
            served(FailingBody, None),
        )
        .err()
        .expect("an interrupted download fails");
        assert!(
            matches!(err, ApkComboError::Download(DownloadError::Interrupted(_))),
            "{err:?}"
        );
        let err = update_with(
            &offer,
            &store,
            None,
            None,
            &StatusSink::terminal(),
            served(&[0u8; 95][..], Some(100)),
        )
        .err()
        .expect("a short download fails");
        assert!(
            matches!(
                err,
                ApkComboError::Download(DownloadError::LengthMismatch { .. })
            ),
            "{err:?}"
        );
        let err = update_with(
            &offer,
            &store,
            None,
            None,
            &StatusSink::terminal(),
            |_| -> Result<Download<&[u8]>, _> {
                Err(ApkComboError::Download(DownloadError::Status(403)))
            },
        )
        .err()
        .expect("a refused download fails");
        assert!(
            matches!(err, ApkComboError::Download(DownloadError::Status(403))),
            "{err:?}"
        );
        assert_eq!(
            store.last_check().unwrap(),
            None,
            "nothing is recorded, so the next launch tries again"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_failures_after_verification_are_not_called_discarded() {
        let replace = store_failure(StoreError::Replace {
            temp: PathBuf::from("/store/current.json.1.tmp"),
            path: PathBuf::from("/store/current.json"),
            source: io::Error::other("busy"),
        });
        assert!(matches!(replace, ApkComboError::Store(_)), "{replace:?}");
        assert!(!replace.to_string().contains("discarded"), "{replace}");
        assert!(!replace.rejects_offer());

        let unreadable = store_failure(StoreError::Set(ApkSetError::Locate {
            path: PathBuf::from("/store/3170.partial/base.apk"),
            source: io::Error::other("read failed"),
        }));
        assert!(
            matches!(unreadable, ApkComboError::Store(_)),
            "{unreadable:?}"
        );
        assert!(
            !unreadable.to_string().contains("discarded"),
            "{unreadable}"
        );
        assert!(!unreadable.rejects_offer());

        let rejected = store_failure(StoreError::UnexpectedVersion {
            expected: VersionCode(3170),
            found: VersionCode(3120),
        });
        assert!(
            matches!(rejected, ApkComboError::Rejected(_)),
            "{rejected:?}"
        );
        assert!(rejected.to_string().contains("discarded"), "{rejected}");
        assert!(rejected.rejects_offer());
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

    fn local_offer(set: &ApkSet, packaging: Packaging, download: &Path, base: &Path) -> Offer {
        let version_name = set.version_name().expect("Roblox names its versions");
        let base_sha1 = file_sha1(base).unwrap();
        let url = format!(
            "https://{R2_HOST}/{ROBLOX_PACKAGE}/{version_name}/{}.{}.apks",
            set.version_code(),
            hex(&base_sha1)
        );
        Offer {
            version_name: version_name.to_owned(),
            version_code: set.version_code(),
            packaging,
            size: AdvertisedSize {
                bytes: fs::metadata(download).unwrap().len(),
                tolerance: MEBIBYTE,
            },
            base_sha1,
            url: https::trusted_uri(&url, DOWNLOAD_HOSTS).unwrap(),
        }
    }

    fn install_file(
        offer: &Offer,
        store: &Store,
        download: &Path,
    ) -> Result<ApkSet, ApkComboError> {
        let length = fs::metadata(download).unwrap().len();
        let status = StatusSink::terminal();
        install(
            offer,
            store.begin(&status).unwrap(),
            served(File::open(download).unwrap(), Some(length)),
            &status,
        )
        .map(|committed| committed.set)
    }

    fn store_entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name != "install.lock")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn official_release_installs_from_an_apkcombo_bundle_and_nothing_else_does() {
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
                ("config.x86_64.apk", &split),
            ],
        );
        let offer = local_offer(&official, Packaging::Xapk, &bundle, &paths.base);

        let mut wrong_hash = offer.clone();
        wrong_hash.base_sha1[0] ^= 1;
        let err = install_file(&wrong_hash, &store, &bundle)
            .err()
            .expect("a base APK with another SHA-1 is refused");
        assert!(matches!(err, ApkComboError::BaseHashMismatch), "{err:?}");

        let mut wrong_version = offer.clone();
        wrong_version.version_code = VersionCode(offer.version_code.0 + 1);
        let err = install_file(&wrong_version, &store, &bundle)
            .err()
            .expect("a release with another versionCode is refused");
        assert!(
            matches!(
                err,
                ApkComboError::Rejected(StoreError::UnexpectedVersion { .. })
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
                ("config.x86_64.apk", &split),
            ],
        );
        let tampered_offer = local_offer(&official, Packaging::Xapk, &tampered, &tampered_base);
        let err = install_file(&tampered_offer, &store, &tampered)
            .err()
            .expect("a modified base APK is refused");
        assert!(
            matches!(
                err,
                ApkComboError::Rejected(StoreError::Set(ApkSetError::Signature { .. }))
            ),
            "a matching SHA-1 never stands in for Roblox's signature: {err:?}"
        );
        assert!(err.to_string().contains("discarded"), "{err}");
        assert!(err.rejects_offer());

        let base_only = local_offer(&official, Packaging::Apk, &paths.base, &paths.base);
        let err = install_file(&base_only, &store, &paths.base)
            .err()
            .expect("a base APK without x86_64 code is refused");
        assert!(
            matches!(err, ApkComboError::Rejected(StoreError::NoNativeSplitGiven)),
            "{err:?}"
        );

        assert_eq!(store.current().unwrap(), None);
        assert!(
            store_entries(&root).is_empty(),
            "{:?}",
            store_entries(&root)
        );

        let set = install_file(&offer, &store, &bundle).expect("the official bundle installs");
        let version_dir = root.join(offer.version_code.to_string());
        assert_eq!(set.version_code(), official.version_code());
        assert_eq!(set.base_path(), version_dir.join(BASE_APK));
        assert_eq!(set.native_libs_path(), version_dir.join(NATIVE_SPLIT_APK));
        assert_eq!(
            store.current().unwrap(),
            Some(InstalledVersion::from(&official))
        );
        assert_eq!(
            store_entries(&version_dir),
            [BASE_APK, NATIVE_SPLIT_APK, "verified.json"]
        );
        assert_eq!(
            plan(
                &offer,
                Some(&InstalledVersion::from(&set)),
                store.current().unwrap().as_ref(),
                None
            )
            .unwrap(),
            Plan::UpToDate(InstalledVersion::from(&official))
        );

        let outcome = update_with(
            &offer,
            &store,
            Some(&set),
            None,
            &StatusSink::terminal(),
            |_| -> Result<Download<&[u8]>, _> {
                panic!("an up-to-date install is not downloaded again")
            },
        )
        .expect("the installed release is up to date");
        assert!(
            matches!(&outcome, UpdateOutcome::UpToDate { installed } if installed.version_code == offer.version_code)
        );

        let installed = InstalledVersion::from(&set);
        let newer_code = (installed.version_code.0 + 1).to_string();
        let site = [
            (Page::Newest, ARM_ONLY_PAGE.replace("3212", &newer_code)),
            (
                release("2.740.931"),
                LATEST_PAGE.replace("3170", &installed.version_code.to_string()),
            ),
        ];
        let mut fetched = Vec::new();
        let outcome = update_from(
            &store,
            Some(&set),
            None,
            &StatusSink::terminal(),
            served_pages(&site, &mut fetched),
            |_: &Uri, _: Option<&Resume>| -> Result<Download<&[u8]>, DownloadError> {
                panic!("the installed release is kept, not downloaded again")
            },
        )
        .expect("a newer release without an x86_64 build is no error for an installed one");
        assert!(
            matches!(&outcome, UpdateOutcome::UpToDate { installed: kept } if *kept == installed),
            "the installed release is kept"
        );
        assert_eq!(fetched, [Page::Newest, release("2.740.931")]);
        assert_eq!(
            store.last_check().unwrap(),
            None,
            "keeping the installed release records no check of its own"
        );
        drop(set);

        let outcome = update_with(
            &offer,
            &store,
            None,
            None,
            &StatusSink::terminal(),
            |_| -> Result<Download<&[u8]>, _> {
                panic!("a release another launch installed meanwhile is not downloaded again")
            },
        )
        .expect("the release another launch installed is used");
        let UpdateOutcome::Updated {
            previous: None,
            committed,
        } = outcome
        else {
            panic!("the release installed meanwhile is handed back as the update");
        };
        let set = committed.set;
        assert_eq!(set.version_code(), offer.version_code);
        assert_eq!(set.base_path(), version_dir.join(BASE_APK));
        drop(set);
        fs::remove_dir_all(&dir).ok();
    }
}
