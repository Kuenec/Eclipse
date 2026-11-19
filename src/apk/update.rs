use std::cmp::Reverse;
use std::fmt;
use std::time::{Instant, SystemTime};

use super::apkpure::{self, ApkPureError, Release};
use super::play::{self, Credentials, PlayError};
use super::store::{
    plan, Candidate, CheckMode, CheckOutcome, DeclaredFile, InstalledVersion, LeftBecause, Plan,
    Rejection, Staging, Store, StoreError, UpdateCheck, UpdateOutcome,
};
use super::tested::{self, EclipseVersion, ListError, Standing, TestedVersions};
use super::{ApkSet, VersionCode};
use crate::abi::Abi;
use crate::status::StatusSink;

const MAX_DOWNLOADS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    ApkPure,
    GooglePlay,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ApkPure => "APKPure",
            Self::GooglePlay => "Google Play",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sources {
    Every,
    GooglePlay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Offer<'a> {
    ApkPure(apkpure::Offer),
    GooglePlay {
        credentials: &'a Credentials,
        version: VersionCode,
    },
}

impl Offer<'_> {
    fn source(&self) -> Source {
        match self {
            Self::ApkPure(_) => Source::ApkPure,
            Self::GooglePlay { .. } => Source::GooglePlay,
        }
    }

    fn version(&self) -> VersionCode {
        match self {
            Self::ApkPure(offer) => offer.version(),
            Self::GooglePlay { version, .. } => *version,
        }
    }

    fn file(&self) -> Option<DeclaredFile> {
        match self {
            Self::ApkPure(offer) => Some(offer.declared_file()),
            Self::GooglePlay { .. } => None,
        }
    }

    fn candidate(&self) -> Candidate {
        Candidate {
            version: self.version(),
            file: self.file(),
        }
    }

    fn release(&self) -> String {
        match self {
            Self::ApkPure(offer) => offer.to_string(),
            Self::GooglePlay { version, .. } => format!("Roblox versionCode {version}"),
        }
    }
}

impl fmt::Display for Offer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} from {}", self.release(), self.source())
    }
}

struct Listed<'a> {
    offers: Vec<Offer<'a>>,
    newer_without_build: Option<Release>,
}

#[derive(Debug)]
pub enum SourceError {
    ApkPure(ApkPureError),
    GooglePlay(PlayError),
}

impl SourceError {
    fn rejects_offer(&self) -> bool {
        match self {
            Self::ApkPure(error) => error.rejects_offer(),
            Self::GooglePlay(_) => false,
        }
    }

    fn lacks_host_build(&self) -> bool {
        matches!(self, Self::ApkPure(ApkPureError::NoHostSplit))
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApkPure(error) => error.fmt(f),
            Self::GooglePlay(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ApkPure(error) => Some(error),
            Self::GooglePlay(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub enum Problem {
    Unanswered(SourceError),

    NoHostBuild,

    Failed {
        offered: String,
        error: SourceError,
    },

    RejectedBefore {
        offered: String,
        rejection: Rejection,
    },

    Older {
        offered: String,
        installed: InstalledVersion,
    },

    NotForHost {
        offered: String,
    },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unanswered(error) => error.fmt(f),
            Self::NoHostBuild => {
                write!(f, "it has no {} build of Roblox", Abi::HOST.android_name())
            }
            Self::Failed { error, .. } => error.fmt(f),
            Self::RejectedBefore {
                offered,
                rejection: Rejection::FailedVerification,
            } => write!(
                f,
                "it still offers {offered}, which Eclipse downloaded and rejected before, so it \
                 is not downloaded again automatically; run `eclipse update` to retry it"
            ),
            Self::RejectedBefore {
                offered,
                rejection: Rejection::Left(LeftBecause::FailedToStart),
            } => write!(
                f,
                "it still offers {offered}, which failed to start twice with this Eclipse, so it \
                 is not installed again automatically; run `eclipse update` to retry it"
            ),
            Self::RejectedBefore {
                offered,
                rejection: Rejection::Left(LeftBecause::RolledBack),
            } => write!(
                f,
                "it still offers {offered}, which you went back from with `eclipse rollback`, so \
                 it is not installed again automatically; run `eclipse update` to install it"
            ),
            Self::Older { offered, installed } => write!(
                f,
                "it offers {offered}, which is older than the installed Roblox {installed}; \
                 Eclipse never installs an older Roblox"
            ),
            Self::NotForHost { offered } => write!(
                f,
                "{offered} holds no {} build of Roblox, so Eclipse skips it",
                Abi::HOST.android_name()
            ),
        }
    }
}

#[derive(Debug)]
pub struct SourceProblem {
    pub source: Source,

    pub problem: Problem,
}

impl fmt::Display for SourceProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.source, self.problem)
    }
}

#[derive(Debug)]
pub enum UpdateError {
    NotSignedIn,

    Account(PlayError),

    NothingInstalled(Vec<SourceProblem>),

    Store(StoreError),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSignedIn => {
                f.write_str("not signed in to Google Play; run `eclipse play-login` first")
            }
            Self::Account(error) => write!(f, "cannot read the Google Play sign-in: {error}"),
            Self::NothingInstalled(problems) => {
                f.write_str("no update source could provide a newer Roblox (")?;
                for (index, problem) in problems.iter().enumerate() {
                    if index > 0 {
                        f.write_str("; ")?;
                    }
                    problem.fmt(f)?;
                }
                f.write_str(")")
            }
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for UpdateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Account(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::NotSignedIn | Self::NothingInstalled(_) => None,
        }
    }
}

impl From<StoreError> for UpdateError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

pub fn update(
    store: &Store,
    current: Option<&ApkSet>,
    sources: Sources,
    mode: CheckMode,
    deadline: Option<Instant>,
    status: &StatusSink,
) -> Result<UpdateOutcome, UpdateError> {
    let credentials = match (
        sources,
        play::Account::open().and_then(|account| account.credentials()),
    ) {
        (_, Ok(credentials)) => credentials,
        (Sources::GooglePlay, Err(error)) => return Err(UpdateError::Account(error)),
        (Sources::Every, Err(error)) => {
            status.warning(format!(
                "Eclipse cannot read the Google Play sign-in, so it checks APKPure only: {error}"
            ));
            None
        }
    };
    let asking = match (sources, &credentials) {
        (Sources::GooglePlay, None) => return Err(UpdateError::NotSignedIn),
        (Sources::GooglePlay, Some(credentials)) => vec![Asking::GooglePlay(credentials)],
        (Sources::Every, None) => vec![Asking::ApkPure],
        (Sources::Every, Some(credentials)) => {
            vec![Asking::ApkPure, Asking::GooglePlay(credentials)]
        }
    };
    let names = asking
        .iter()
        .map(|asked| asked.source().to_string())
        .collect::<Vec<_>>()
        .join(" and ");
    status.step(format!("Checking {names} for the newest Roblox client…"));
    let installed = current.map(ApkSet::version_code);
    let waits = waits_for_tests(store, Abi::HOST, installed, mode)?;
    let (answers, tests) = std::thread::scope(|scope| {
        let tests = waits.then(|| scope.spawn(move || tested::fetch(deadline)));
        let pending: Vec<_> = asking
            .into_iter()
            .map(|asked| {
                (
                    asked.source(),
                    scope.spawn(move || asked.list(installed, deadline)),
                )
            })
            .collect();
        let answers = pending
            .into_iter()
            .map(|(source, pending)| {
                let answer = pending
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                (source, answer)
            })
            .collect::<Vec<_>>();
        let tests = tests.map(|pending| {
            pending
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
        (answers, tests)
    });
    settle(
        store,
        current.map(InstalledVersion::from).as_ref(),
        mode,
        tests,
        answers,
        status,
        |offer, staging, status| match offer {
            Offer::ApkPure(offer) => {
                apkpure::install(offer, staging, status).map_err(SourceError::ApkPure)
            }
            Offer::GooglePlay {
                credentials,
                version,
            } => play::install(credentials, *version, staging, status)
                .map_err(SourceError::GooglePlay),
        },
    )
}

fn waits_for_tests(
    store: &Store,
    abi: Abi,
    installed: Option<VersionCode>,
    mode: CheckMode,
) -> Result<bool, StoreError> {
    match mode {
        CheckMode::Scheduled if abi == tested::LISTED_ABI => {
            Ok(installed.is_none() || store.upgrade_required()? != installed)
        }
        CheckMode::Scheduled | CheckMode::Explicit => Ok(false),
    }
}

fn tested_offers<'a>(
    mut offers: Vec<Offer<'a>>,
    tests: Result<TestedVersions, ListError>,
    installed: Option<&InstalledVersion>,
    status: &StatusSink,
) -> Vec<Offer<'a>> {
    let tests = match tests {
        Ok(tests) => tests,
        Err(error) => {
            status.warning(format!(
                "could not load the list of Roblox versions tested with Eclipse, so this check \
                 goes on without it: {error}"
            ));
            return offers;
        }
    };
    let standing = |offer: &Offer<'_>| tests.standing(offer.version(), EclipseVersion::RUNNING);
    let Some(installed) = installed else {
        offers.sort_by_key(|offer| standing(offer) != Standing::Tested);
        return offers;
    };
    let mut tested = Vec::new();
    let mut waiting = None;
    for offer in offers {
        match standing(&offer) {
            Standing::Tested => tested.push(offer),
            Standing::TestedWithNewer(eclipse) => {
                waiting.get_or_insert((offer, Some(eclipse)));
            }
            Standing::Untested => {
                waiting.get_or_insert((offer, None));
            }
        }
    }
    match waiting {
        Some((offer, Some(eclipse))) => status.warning(format!(
            "{} is tested with Eclipse {eclipse} and newer, and this is Eclipse {}, so Eclipse \
             keeps the installed Roblox {installed}; update Eclipse to get it",
            offer.release(),
            EclipseVersion::RUNNING
        )),
        Some((offer, None)) => status.step(format!(
            "{} has not been tested with Eclipse yet, so Eclipse keeps the installed Roblox \
             {installed} for now; run `eclipse update` to install it anyway",
            offer.release()
        )),
        None => {}
    }
    tested
}

#[derive(Clone, Copy)]
enum Asking<'a> {
    ApkPure,
    GooglePlay(&'a Credentials),
}

impl<'a> Asking<'a> {
    fn source(self) -> Source {
        match self {
            Self::ApkPure => Source::ApkPure,
            Self::GooglePlay(_) => Source::GooglePlay,
        }
    }

    fn list(
        self,
        installed: Option<VersionCode>,
        deadline: Option<Instant>,
    ) -> Result<Listed<'a>, SourceError> {
        match self {
            Self::ApkPure => apkpure::list(installed, deadline)
                .map(|listing| Listed {
                    offers: listing.offers.into_iter().map(Offer::ApkPure).collect(),
                    newer_without_build: listing.newer_without_build,
                })
                .map_err(SourceError::ApkPure),
            Self::GooglePlay(credentials) => play::latest(credentials, deadline)
                .map(|version| Listed {
                    offers: vec![Offer::GooglePlay {
                        credentials,
                        version,
                    }],
                    newer_without_build: None,
                })
                .map_err(SourceError::GooglePlay),
        }
    }
}

fn settle<'a>(
    store: &Store,
    installed: Option<&InstalledVersion>,
    mode: CheckMode,
    tests: Option<Result<TestedVersions, ListError>>,
    answers: Vec<(Source, Result<Listed<'a>, SourceError>)>,
    status: &StatusSink,
    mut install: impl FnMut(&Offer<'a>, Staging<'_>, &StatusSink) -> Result<ApkSet, SourceError>,
) -> Result<UpdateOutcome, UpdateError> {
    let newer =
        |version: VersionCode| installed.is_none_or(|installed| version > installed.version_code);
    let mut problems = Vec::new();
    let mut offers = Vec::new();
    let mut answered = false;
    for (source, answer) in answers {
        let listed = match answer {
            Ok(listed) => listed,
            Err(error) => {
                status.warning(format!("could not check {source}: {error}"));
                problems.push(SourceProblem {
                    source,
                    problem: Problem::Unanswered(error),
                });
                continue;
            }
        };
        answered = true;
        let fresh: Vec<_> = listed
            .offers
            .into_iter()
            .filter(|offer| newer(offer.version()))
            .collect();
        status.step(summary(
            source,
            fresh.first(),
            listed.newer_without_build.as_ref(),
            installed,
        ));
        if fresh.is_empty() && installed.is_none() {
            problems.push(SourceProblem {
                source,
                problem: Problem::NoHostBuild,
            });
        }
        offers.extend(fresh);
    }
    offers.sort_by_key(|offer| (Reverse(offer.version()), offer.source()));
    let offers = match tests {
        Some(tests) if !offers.is_empty() => tested_offers(offers, tests, installed, status),
        Some(_) | None => offers,
    };
    let mut downloads = 0;
    let mut failed_transfers: Vec<(Source, VersionCode)> = Vec::new();
    for offer in &offers {
        if downloads == MAX_DOWNLOADS {
            break;
        }
        if failed_transfers
            .iter()
            .any(|&(source, version)| source == offer.source() && offer.version() < version)
        {
            continue;
        }
        match attempt(store, installed, mode, offer, status, &mut install)? {
            Attempted::Done(outcome) => return Ok(outcome),
            Attempted::Problem(problem) => {
                if let Problem::Failed { error, .. } = &problem {
                    downloads += 1;
                    if !error.rejects_offer() {
                        failed_transfers.push((offer.source(), offer.version()));
                    }
                }
                let line = format!("{}: {problem}", offer.source());
                match problem {
                    Problem::NotForHost { .. } => status.step(line),
                    Problem::Unanswered(_)
                    | Problem::NoHostBuild
                    | Problem::Failed { .. }
                    | Problem::RejectedBefore { .. }
                    | Problem::Older { .. } => status.warning(line),
                }
                problems.push(SourceProblem {
                    source: offer.source(),
                    problem,
                });
            }
        }
    }
    match installed {
        Some(installed) if answered && offers.is_empty() => Ok(UpdateOutcome::UpToDate {
            installed: installed.clone(),
        }),
        _ => Err(UpdateError::NothingInstalled(problems)),
    }
}

fn summary(
    source: Source,
    best: Option<&Offer<'_>>,
    newer_without_build: Option<&Release>,
    installed: Option<&InstalledVersion>,
) -> String {
    let abi = Abi::HOST.android_name();
    match (best, newer_without_build, installed) {
        (Some(best), _, _) => format!("{source} offers {}", best.release()),
        (None, Some(release), Some(installed)) => format!(
            "{source} has no {abi} build of {release} yet, so Eclipse keeps the installed Roblox \
             {installed} for now"
        ),
        (None, Some(release), None) => {
            format!("{source} has no {abi} build of {release} or any older Roblox")
        }
        (None, None, Some(installed)) => {
            format!("{source} has no {abi} build newer than the installed Roblox {installed}")
        }
        (None, None, None) => format!("{source} has no {abi} build of Roblox"),
    }
}

enum Attempted {
    Done(UpdateOutcome),
    Problem(Problem),
}

fn attempt<'a>(
    store: &Store,
    previous: Option<&InstalledVersion>,
    mode: CheckMode,
    offer: &Offer<'a>,
    status: &StatusSink,
    install: &mut impl FnMut(&Offer<'a>, Staging<'_>, &StatusSink) -> Result<ApkSet, SourceError>,
) -> Result<Attempted, StoreError> {
    let previous = previous.cloned();
    let staging = store.begin(status)?;
    let recorded = match store.current() {
        Ok(recorded) => recorded,
        Err(error) if error.is_unusable_install() => None,
        Err(error) => return Err(error),
    };
    let unchanged = recorded.as_ref().map(|installed| installed.version_code)
        == previous.as_ref().map(|installed| installed.version_code);
    let (verified, refreshed) = if unchanged {
        (previous.clone(), None)
    } else {
        let set = store.usable_current()?;
        (set.as_ref().map(InstalledVersion::from), set)
    };
    let rejections = store.rejections(crate::VERSION)?;
    let offered = offer.to_string();
    Ok(
        match plan(
            offer.candidate(),
            verified.as_ref(),
            recorded.as_ref(),
            &rejections,
            mode,
        ) {
            Plan::UpToDate(installed) => Attempted::Done(match refreshed {
                Some(set) => UpdateOutcome::Updated {
                    previous,
                    set: Box::new(set),
                },
                None => UpdateOutcome::UpToDate { installed },
            }),
            Plan::Older(installed) => Attempted::Problem(Problem::Older { offered, installed }),
            Plan::Rejected(rejection) => {
                remember_rejection(store, None, status);
                Attempted::Problem(Problem::RejectedBefore { offered, rejection })
            }
            Plan::Download => {
                status.step(format!("Downloading {offer}…"));
                match install(offer, staging, status) {
                    Ok(set) => Attempted::Done(UpdateOutcome::Updated {
                        previous,
                        set: Box::new(set),
                    }),
                    Err(error) if error.lacks_host_build() => {
                        Attempted::Problem(Problem::NotForHost { offered })
                    }
                    Err(error) => {
                        if error.rejects_offer() {
                            remember_rejection(store, offer.file(), status);
                        }
                        Attempted::Problem(Problem::Failed { offered, error })
                    }
                }
            }
        },
    )
}

fn remember_rejection(store: &Store, rejected_file: Option<DeclaredFile>, status: &StatusSink) {
    let check = UpdateCheck {
        at: SystemTime::now(),
        outcome: CheckOutcome::Completed,
    };
    let recorded = rejected_file
        .map_or(Ok(()), |file| store.reject_file(file))
        .and_then(|()| store.record_check(&check));
    if let Err(error) = recorded {
        status.warning(format!(
            "Eclipse could not record the rejection, so it may download the same file again: \
             {error}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::ApkSetPaths;
    use crate::status::StatusUpdate;
    use std::cell::RefCell;
    use std::fs;
    use std::path::PathBuf;

    const X86_64_LIST: &[u8] = include_bytes!("../../tests/fixtures/apkpure-x86_64.bin");

    fn temp_store(tag: &str) -> (PathBuf, Store) {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-update-test-{tag}-{:?}",
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        let store = Store::at(dir.join("store"));
        (dir, store)
    }

    fn installed(store: &Store, code: u32) -> InstalledVersion {
        fs::create_dir_all(store.root()).unwrap();
        fs::write(
            store.root().join("current.json"),
            format!("{{\"version_code\": {code}}}"),
        )
        .unwrap();
        InstalledVersion {
            version_code: VersionCode(code),
            version_name: None,
        }
    }

    fn credentials() -> Credentials {
        serde_json::from_value(serde_json::json!({
            "email": "player@example.com",
            "aas_token": "aas_et/secret",
            "gsf_id": 42,
            "device_config_token": "device",
            "checkin_consistency_token": "consistency",
        }))
        .unwrap()
    }

    fn apkpure_offers() -> Vec<Offer<'static>> {
        apkpure::offers(X86_64_LIST)
            .unwrap()
            .into_iter()
            .map(Offer::ApkPure)
            .collect()
    }

    fn apkpure_answer() -> (Source, Result<Listed<'static>, SourceError>) {
        (
            Source::ApkPure,
            Ok(Listed {
                offers: apkpure_offers(),
                newer_without_build: None,
            }),
        )
    }

    fn play_answer(
        credentials: &Credentials,
        code: u32,
    ) -> (Source, Result<Listed<'_>, SourceError>) {
        (
            Source::GooglePlay,
            Ok(Listed {
                offers: vec![Offer::GooglePlay {
                    credentials,
                    version: VersionCode(code),
                }],
                newer_without_build: None,
            }),
        )
    }

    fn unreachable_apkpure() -> (Source, Result<Listed<'static>, SourceError>) {
        (
            Source::ApkPure,
            Err(SourceError::ApkPure(ApkPureError::Listing {
                detail: "io: failed to lookup address information".to_owned(),
            })),
        )
    }

    fn tried(offers: &RefCell<Vec<(Source, u32)>>, offer: &Offer<'_>) {
        offers
            .borrow_mut()
            .push((offer.source(), offer.version().0));
    }

    fn unavailable(offer: &Offer<'_>) -> SourceError {
        match offer {
            Offer::ApkPure(_) => SourceError::ApkPure(ApkPureError::Download(
                crate::https::DownloadError::Status(503),
            )),
            Offer::GooglePlay { .. } => SourceError::GooglePlay(PlayError::Delivery(9)),
        }
    }

    #[test]
    fn each_answer_names_its_source_once() {
        let credentials = credentials();
        let newest = apkpure_offers().remove(0);
        let play = Offer::GooglePlay {
            credentials: &credentials,
            version: VersionCode(3300),
        };
        let installed = InstalledVersion {
            version_code: VersionCode(3212),
            version_name: Some("2.741.1061".to_owned()),
        };
        assert_eq!(
            summary(Source::ApkPure, Some(&newest), None, Some(&installed)),
            "APKPure offers Roblox 2.742.1059 (versionCode 3222) (APK, 224.0 MiB)"
        );
        assert_eq!(
            summary(Source::GooglePlay, Some(&play), None, Some(&installed)),
            "Google Play offers Roblox versionCode 3300"
        );
        assert_eq!(
            summary(Source::ApkPure, None, None, Some(&installed)),
            format!(
                "APKPure has no {} build newer than the installed Roblox 2.741.1061 \
                 (versionCode 3212)",
                Abi::HOST.android_name()
            )
        );
        assert_eq!(
            newest.to_string(),
            "Roblox 2.742.1059 (versionCode 3222) (APK, 224.0 MiB) from APKPure"
        );
        assert_eq!(play.to_string(), "Roblox versionCode 3300 from Google Play");
    }

    #[test]
    fn a_source_that_does_not_answer_is_named_and_the_next_one_is_tried() {
        let (dir, store) = temp_store("next-source");
        let credentials = credentials();
        let attempts = RefCell::new(Vec::new());
        let err = settle(
            &store,
            Some(&installed(&store, 3212)),
            CheckMode::Scheduled,
            None,
            vec![unreachable_apkpure(), play_answer(&credentials, 3222)],
            &StatusSink::terminal(),
            |offer, _, _| {
                tried(&attempts, offer);
                Err(unavailable(offer))
            },
        )
        .err()
        .expect("no source installs anything");
        assert_eq!(attempts.into_inner(), [(Source::GooglePlay, 3222)]);
        assert_eq!(
            err.to_string(),
            "no update source could provide a newer Roblox (APKPure: cannot load APKPure's list \
             of Roblox releases: io: failed to lookup address information; Google Play: Google \
             Play says com.roblox.client is not available for this device)"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_source_whose_newest_release_fails_to_download_is_not_asked_for_older_ones() {
        let (dir, store) = temp_store("newest-per-source");
        let credentials = credentials();
        for (play, order) in [
            (
                3300,
                vec![(Source::GooglePlay, 3300), (Source::ApkPure, 3222)],
            ),
            (
                3222,
                vec![(Source::ApkPure, 3222), (Source::GooglePlay, 3222)],
            ),
        ] {
            let attempts = RefCell::new(Vec::new());
            let err = settle(
                &store,
                None,
                CheckMode::Scheduled,
                None,
                vec![apkpure_answer(), play_answer(&credentials, play)],
                &StatusSink::terminal(),
                |offer, _, _| {
                    tried(&attempts, offer);
                    Err(unavailable(offer))
                },
            )
            .err()
            .expect("every download fails");
            let attempts = attempts.into_inner();
            assert_eq!(
                attempts, order,
                "the newest first, APKPure before the account at equal versions, and no older \
                 release of a source after its newest one failed"
            );
            let UpdateError::NothingInstalled(problems) = err else {
                panic!("the failures are listed");
            };
            assert_eq!(problems.len(), attempts.len());
        }
        assert_eq!(
            store.last_check().unwrap(),
            None,
            "transfer failures are retried"
        );
        let attempts = RefCell::new(Vec::new());
        settle(
            &store,
            None,
            CheckMode::Scheduled,
            None,
            vec![apkpure_answer()],
            &StatusSink::terminal(),
            |offer, _, _| {
                tried(&attempts, offer);
                match offer.version() {
                    VersionCode(3222) => Err(SourceError::ApkPure(ApkPureError::HashMismatch)),
                    _ => Err(unavailable(offer)),
                }
            },
        )
        .err()
        .expect("every download fails");
        assert_eq!(
            attempts.into_inner(),
            [(Source::ApkPure, 3222), (Source::ApkPure, 3092)],
            "a discarded file is passed over like a rejected one, a failed transfer is not"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn offers_skipped_without_a_download_leave_the_download_limit_to_the_next_ones() {
        let (dir, store) = temp_store("skipped-offers");
        let credentials = credentials();
        for offer in &apkpure_offers()[..2] {
            store.reject_file(offer.file().unwrap()).unwrap();
        }
        let attempts = RefCell::new(Vec::new());
        let err = settle(
            &store,
            None,
            CheckMode::Scheduled,
            None,
            vec![apkpure_answer(), play_answer(&credentials, 3084)],
            &StatusSink::terminal(),
            |offer, _, _| {
                tried(&attempts, offer);
                Err(unavailable(offer))
            },
        )
        .err()
        .expect("every download fails");
        assert_eq!(
            attempts.into_inner(),
            [(Source::ApkPure, 3084), (Source::GooglePlay, 3084)],
            "the rejected 3222 and 3092 files are skipped without counting as downloads"
        );
        let UpdateError::NothingInstalled(problems) = err else {
            panic!("the skipped and failed offers are listed");
        };
        let listed: Vec<_> = problems
            .iter()
            .map(|listed| match &listed.problem {
                Problem::RejectedBefore { .. } => (listed.source, "rejected"),
                Problem::Failed { .. } => (listed.source, "failed"),
                other => panic!("{other}"),
            })
            .collect();
        assert_eq!(
            listed,
            [
                (Source::ApkPure, "rejected"),
                (Source::ApkPure, "rejected"),
                (Source::ApkPure, "failed"),
                (Source::GooglePlay, "failed"),
            ]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bundle_for_another_abi_is_passed_over_quietly_without_using_a_download() {
        let (dir, store) = temp_store("other-abi");
        let credentials = credentials();
        let attempts = RefCell::new(Vec::new());
        let (updates, shown) = std::sync::mpsc::channel();
        let err = settle(
            &store,
            None,
            CheckMode::Scheduled,
            None,
            vec![apkpure_answer(), play_answer(&credentials, 3000)],
            &StatusSink::with_window(updates),
            |offer, _, _| {
                tried(&attempts, offer);
                match offer {
                    Offer::ApkPure(_) => Err(SourceError::ApkPure(ApkPureError::NoHostSplit)),
                    Offer::GooglePlay { .. } => Err(unavailable(offer)),
                }
            },
        )
        .err()
        .expect("nothing installs");
        assert_eq!(
            attempts.into_inner(),
            [
                (Source::ApkPure, 3222),
                (Source::ApkPure, 3092),
                (Source::ApkPure, 3084),
                (Source::GooglePlay, 3000)
            ],
            "a skipped bundle is neither a download nor a failed transfer"
        );
        let UpdateError::NothingInstalled(problems) = err else {
            panic!("the skipped and failed offers are listed");
        };
        assert!(
            matches!(
                problems.as_slice(),
                [
                    SourceProblem {
                        problem: Problem::NotForHost { .. },
                        ..
                    },
                    SourceProblem {
                        problem: Problem::NotForHost { .. },
                        ..
                    },
                    SourceProblem {
                        problem: Problem::NotForHost { .. },
                        ..
                    },
                    SourceProblem {
                        source: Source::GooglePlay,
                        problem: Problem::Failed { .. },
                    },
                ]
            ),
            "{problems:?}"
        );
        let warnings: Vec<_> = shown
            .try_iter()
            .filter(|update| matches!(update, StatusUpdate::Warning(_)))
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "only the failed download warns: {warnings:?}"
        );
        assert_eq!(
            store.last_check().unwrap(),
            None,
            "a skipped bundle is not remembered as rejected"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_newer_from_any_source_that_answered_keeps_the_installed_roblox() {
        let (dir, store) = temp_store("up-to-date");
        let credentials = credentials();
        let current = installed(&store, 3222);
        let never =
            |_: &Offer<'_>, _: Staging<'_>, _: &StatusSink| -> Result<ApkSet, SourceError> {
                panic!("nothing is newer than the installed Roblox")
            };
        for answers in [
            vec![apkpure_answer(), play_answer(&credentials, 3222)],
            vec![
                apkpure_answer(),
                (
                    Source::GooglePlay,
                    Err(SourceError::GooglePlay(PlayError::NotOffered)),
                ),
            ],
        ] {
            let outcome = settle(
                &store,
                Some(&current),
                CheckMode::Scheduled,
                None,
                answers,
                &StatusSink::terminal(),
                never,
            )
            .unwrap_or_else(|error| panic!("{error}"));
            assert!(
                matches!(&outcome, UpdateOutcome::UpToDate { installed } if *installed == current)
            );
        }
        let err = settle(
            &store,
            Some(&current),
            CheckMode::Scheduled,
            None,
            vec![unreachable_apkpure()],
            &StatusSink::terminal(),
            never,
        )
        .err()
        .expect("no answer is no up-to-date");
        assert!(matches!(err, UpdateError::NothingInstalled(_)), "{err:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_first_install_without_any_host_build_says_so_for_each_source() {
        let (dir, store) = temp_store("no-build");
        let err = settle(
            &store,
            None,
            CheckMode::Scheduled,
            None,
            vec![(
                Source::ApkPure,
                Ok(Listed {
                    offers: Vec::new(),
                    newer_without_build: None,
                }),
            )],
            &StatusSink::terminal(),
            |_, _, _| panic!("nothing is offered"),
        )
        .err()
        .expect("nothing to install");
        assert_eq!(
            err.to_string(),
            format!(
                "no update source could provide a newer Roblox (APKPure: it has no {} build of \
                 Roblox)",
                Abi::HOST.android_name()
            )
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_that_failed_verification_is_skipped_until_an_explicit_update() {
        let (dir, store) = temp_store("rejected");
        let current = installed(&store, 3212);
        let attempts = RefCell::new(Vec::new());
        let mut install = |offer: &Offer<'_>, _: Staging<'_>, _: &StatusSink| {
            tried(&attempts, offer);
            Err(SourceError::ApkPure(ApkPureError::HashMismatch))
        };
        let check = |mode,
                     install: &mut dyn FnMut(
            &Offer<'_>,
            Staging<'_>,
            &StatusSink,
        ) -> Result<ApkSet, SourceError>| {
            settle(
                &store,
                Some(&current),
                mode,
                None,
                vec![apkpure_answer()],
                &StatusSink::terminal(),
                install,
            )
            .err()
            .expect("nothing installs")
        };
        check(CheckMode::Scheduled, &mut install);
        assert_eq!(attempts.borrow().as_slice(), [(Source::ApkPure, 3222)]);
        assert_eq!(
            store.last_check().unwrap().map(|check| check.outcome),
            Some(CheckOutcome::Completed),
            "a rejected file completes the check"
        );
        let err = check(CheckMode::Scheduled, &mut install);
        assert_eq!(
            attempts.borrow().len(),
            1,
            "the rejected file is not downloaded again"
        );
        let UpdateError::NothingInstalled(problems) = &err else {
            panic!("{err:?}");
        };
        assert!(
            matches!(
                problems.as_slice(),
                [SourceProblem {
                    source: Source::ApkPure,
                    problem: Problem::RejectedBefore {
                        rejection: Rejection::FailedVerification,
                        ..
                    }
                }]
            ),
            "{err}"
        );
        assert!(err.to_string().contains("`eclipse update`"), "{err}");
        check(CheckMode::Explicit, &mut install);
        assert_eq!(
            attempts.borrow().len(),
            2,
            "an explicit update downloads it again"
        );
        fs::remove_dir_all(&dir).ok();
    }

    fn tested(entries: &[(u32, &str)]) -> Option<Result<TestedVersions, ListError>> {
        let roblox: Vec<_> = entries
            .iter()
            .map(|(code, eclipse)| serde_json::json!({"version_code": code, "eclipse": eclipse}))
            .collect();
        let list = serde_json::json!({"schema": 1, "roblox": roblox}).to_string();
        Some(TestedVersions::parse(list.as_bytes()))
    }

    fn scheduled(
        store: &Store,
        installed: Option<&InstalledVersion>,
        tests: Option<Result<TestedVersions, ListError>>,
    ) -> (Vec<(Source, u32)>, bool, Vec<StatusUpdate>) {
        let attempts = RefCell::new(Vec::new());
        let (updates, shown) = std::sync::mpsc::channel();
        let outcome = settle(
            store,
            installed,
            CheckMode::Scheduled,
            tests,
            vec![apkpure_answer()],
            &StatusSink::with_window(updates),
            |offer, _, _| {
                tried(&attempts, offer);
                Err(unavailable(offer))
            },
        );
        let up_to_date = matches!(
            (&outcome, installed),
            (Ok(UpdateOutcome::UpToDate { installed: kept }), Some(installed)) if kept == installed
        );
        (
            attempts.into_inner(),
            up_to_date,
            shown.try_iter().collect(),
        )
    }

    #[test]
    fn a_newer_roblox_waits_until_it_is_tested_with_this_eclipse() {
        let (dir, store) = temp_store("waits");
        let current = installed(&store, 3212);
        let offered = "Roblox 2.742.1059 (versionCode 3222) (APK, 224.0 MiB)";

        let (attempts, up_to_date, shown) =
            scheduled(&store, Some(&current), tested(&[(3212, crate::VERSION)]));
        assert!(attempts.is_empty(), "{attempts:?}");
        assert!(up_to_date, "a waiting release is no failed check");
        let waits = format!(
            "{offered} has not been tested with Eclipse yet, so Eclipse keeps the installed \
             Roblox versionCode 3212 for now; run `eclipse update` to install it anyway"
        );
        assert!(shown.contains(&StatusUpdate::Step(waits)), "{shown:?}");

        let (attempts, up_to_date, shown) =
            scheduled(&store, Some(&current), tested(&[(3222, "999.0.0")]));
        assert!(attempts.is_empty() && up_to_date, "{attempts:?}");
        let needs = format!(
            "{offered} is tested with Eclipse 999.0.0 and newer, and this is Eclipse {}, so \
             Eclipse keeps the installed Roblox versionCode 3212; update Eclipse to get it",
            crate::VERSION
        );
        assert!(shown.contains(&StatusUpdate::Warning(needs)), "{shown:?}");

        let (attempts, up_to_date, _) =
            scheduled(&store, Some(&current), tested(&[(3222, crate::VERSION)]));
        assert_eq!(attempts, [(Source::ApkPure, 3222)]);
        assert!(
            !up_to_date,
            "the tested release was tried and its download failed"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_newest_tested_release_is_installed_while_a_newer_one_waits() {
        let (dir, store) = temp_store("newest-tested");
        let current = installed(&store, 3084);
        let (attempts, _, _) = scheduled(&store, Some(&current), tested(&[(3092, "0.1.0")]));
        assert_eq!(attempts, [(Source::ApkPure, 3092)]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_first_install_prefers_a_tested_release_and_still_tries_the_newest() {
        let (dir, store) = temp_store("first-tested");
        let (attempts, _, _) = scheduled(&store, None, tested(&[(3092, "0.1.0")]));
        assert_eq!(
            attempts,
            [(Source::ApkPure, 3092), (Source::ApkPure, 3222)],
            "with nothing installed there is no Roblox to keep, so an untested one is tried too"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_list_that_cannot_be_read_holds_nothing_back() {
        let (dir, store) = temp_store("unreadable-list");
        let current = installed(&store, 3212);
        let warned = |shown: &[StatusUpdate]| {
            shown.iter().any(|update| {
                matches!(
                    update,
                    StatusUpdate::Warning(text) if text.starts_with(
                        "could not load the list of Roblox versions tested with Eclipse"
                    )
                )
            })
        };
        let (attempts, _, shown) =
            scheduled(&store, Some(&current), Some(Err(ListError::Schema(2))));
        assert_eq!(attempts, [(Source::ApkPure, 3222)]);
        assert!(warned(&shown), "{shown:?}");

        let newest = installed(&store, 3222);
        let (attempts, up_to_date, shown) =
            scheduled(&store, Some(&newest), Some(Err(ListError::Schema(2))));
        assert!(attempts.is_empty() && up_to_date, "{attempts:?}");
        assert!(!warned(&shown), "nothing newer needed the list: {shown:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_a_scheduled_check_of_a_roblox_that_still_joins_games_waits_for_tests() {
        let (dir, store) = temp_store("asks-tests");
        let current = installed(&store, 3212).version_code;
        let waits =
            |installed, mode| waits_for_tests(&store, Abi::X86_64, installed, mode).unwrap();
        assert!(waits(Some(current), CheckMode::Scheduled));
        assert!(waits(None, CheckMode::Scheduled));
        assert!(!waits(Some(current), CheckMode::Explicit));
        assert!(
            !waits_for_tests(&store, Abi::Arm64V8a, Some(current), CheckMode::Scheduled).unwrap(),
            "only x86-64 builds of Roblox are tested and listed"
        );
        store.record_upgrade_required(current).unwrap();
        assert!(
            !waits(Some(current), CheckMode::Scheduled),
            "Roblox refuses this version, so the newest one is installed untested"
        );
        assert!(waits(Some(VersionCode(3222)), CheckMode::Scheduled));
        assert!(waits(None, CheckMode::Scheduled));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_newest_verified_release_is_installed_when_an_older_source_also_offers_one() {
        let Some(paths) = ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
        else {
            eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to install the official Roblox APK set");
            return;
        };
        let official = ApkSet::open(paths.clone()).expect("the official set verifies");
        let (dir, store) = temp_store("installs");
        let credentials = credentials();
        let version = official.version_code();
        let attempts = RefCell::new(Vec::new());
        let outcome = settle(
            &store,
            None,
            CheckMode::Scheduled,
            None,
            vec![apkpure_answer(), play_answer(&credentials, version.0 + 100)],
            &StatusSink::terminal(),
            |offer, staging, _| {
                tried(&attempts, offer);
                match offer {
                    Offer::GooglePlay { .. } => Err(unavailable(offer)),
                    Offer::ApkPure(_) => {
                        staging.add_source(&paths.base).unwrap();
                        if let Some(split) = &paths.native_split {
                            staging.add_source(split).unwrap();
                        }
                        staging
                            .commit(Some(version))
                            .map_err(|error| SourceError::ApkPure(ApkPureError::Rejected(error)))
                    }
                }
            },
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let UpdateOutcome::Updated {
            previous: None,
            set,
        } = outcome
        else {
            panic!("the official set is installed");
        };
        assert_eq!(set.version_code(), version);
        assert_eq!(
            attempts.into_inner().first(),
            Some(&(Source::GooglePlay, version.0 + 100)),
            "the newest offer was tried first"
        );
        assert_eq!(
            store.current().unwrap().map(|current| current.version_code),
            Some(version)
        );
        drop(set);
        fs::remove_dir_all(&dir).ok();
    }
}
