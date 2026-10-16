use std::cmp::Reverse;
use std::fmt;
use std::time::{Instant, SystemTime};

use super::apkpure::{self, ApkPureError, Release};
use super::play::{self, Credentials, PlayError};
use super::store::{
    plan, Candidate, CheckMode, CheckOutcome, DeclaredFile, InstalledVersion, LeftBecause, Plan,
    Rejection, Staging, Store, StoreError, UpdateCheck, UpdateOutcome,
};
use super::{ApkSet, VersionCode, TARGET_ABI};
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
    newer_without_x86_64: Option<Release>,
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

    NoX86_64Build,

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
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unanswered(error) => error.fmt(f),
            Self::NoX86_64Build => write!(f, "it has no {TARGET_ABI} build of Roblox"),
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
    let answers = std::thread::scope(|scope| {
        let pending: Vec<_> = asking
            .into_iter()
            .map(|asked| {
                (
                    asked.source(),
                    scope.spawn(move || asked.list(installed, deadline)),
                )
            })
            .collect();
        pending
            .into_iter()
            .map(|(source, pending)| {
                let answer = pending
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                (source, answer)
            })
            .collect::<Vec<_>>()
    });
    settle(
        store,
        current.map(InstalledVersion::from).as_ref(),
        mode,
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
                    newer_without_x86_64: listing.newer_without_x86_64,
                })
                .map_err(SourceError::ApkPure),
            Self::GooglePlay(credentials) => play::latest(credentials, deadline)
                .map(|version| Listed {
                    offers: vec![Offer::GooglePlay {
                        credentials,
                        version,
                    }],
                    newer_without_x86_64: None,
                })
                .map_err(SourceError::GooglePlay),
        }
    }
}

fn settle<'a>(
    store: &Store,
    installed: Option<&InstalledVersion>,
    mode: CheckMode,
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
            listed.newer_without_x86_64.as_ref(),
            installed,
        ));
        if fresh.is_empty() && installed.is_none() {
            problems.push(SourceProblem {
                source,
                problem: Problem::NoX86_64Build,
            });
        }
        offers.extend(fresh);
    }
    offers.sort_by_key(|offer| (Reverse(offer.version()), offer.source()));
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
                status.warning(format!("{}: {problem}", offer.source()));
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
    newer_without_x86_64: Option<&Release>,
    installed: Option<&InstalledVersion>,
) -> String {
    match (best, newer_without_x86_64, installed) {
        (Some(best), _, _) => format!("{source} offers {}", best.release()),
        (None, Some(release), Some(installed)) => format!(
            "{source} has no {TARGET_ABI} build of {release} yet, so Eclipse keeps the installed \
             Roblox {installed} for now"
        ),
        (None, Some(release), None) => {
            format!("{source} has no {TARGET_ABI} build of {release} or any older Roblox")
        }
        (None, None, Some(installed)) => format!(
            "{source} has no {TARGET_ABI} build newer than the installed Roblox {installed}"
        ),
        (None, None, None) => format!("{source} has no {TARGET_ABI} build of Roblox"),
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
                newer_without_x86_64: None,
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
                newer_without_x86_64: None,
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
            "APKPure has no x86_64 build newer than the installed Roblox 2.741.1061 \
             (versionCode 3212)"
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
    fn a_first_install_without_any_x86_64_build_says_so_for_each_source() {
        let (dir, store) = temp_store("no-build");
        let err = settle(
            &store,
            None,
            CheckMode::Scheduled,
            vec![(
                Source::ApkPure,
                Ok(Listed {
                    offers: Vec::new(),
                    newer_without_x86_64: None,
                }),
            )],
            &StatusSink::terminal(),
            |_, _, _| panic!("nothing is offered"),
        )
        .err()
        .expect("nothing to install");
        assert_eq!(
            err.to_string(),
            "no update source could provide a newer Roblox (APKPure: it has no x86_64 build of \
             Roblox)"
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
