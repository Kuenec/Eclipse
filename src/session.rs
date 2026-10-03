use std::fmt;
use std::io;
use std::mem;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eclipse_config::{temp_file, CloseOnLeave, Config};
use serde::Serialize;

use crate::client_log::{self, ClientEvent, GameRpc, JobId, PlaceId, Tap, UniverseId};
use crate::framework::lifecycle::{self, QuitReason};
use crate::graphics::title_suffix;
use crate::portal::{self, Notice, NoticeId, PortalError, PortalRequest};
use crate::server_location::{self, Location, Locator, LookupError};

const SESSION_FILE: &str = "session.json";
const THREAD_NAME: &str = "eclipse-session";
const RETURN_AFTER_LEAVING: Duration = Duration::from_secs(10);
const ATTESTATION_TITLE: &str = "Roblox error 318";
const ATTESTATION_EXPLANATION: &str = "This experience requires Android device attestation, \
     which Eclipse cannot pass. Other experiences are not affected.";
const SERVER_LOCATION_TITLE: &str = "Roblox server location";

#[derive(Debug)]
pub enum SessionError {
    Clear { path: PathBuf, source: io::Error },
    Spawn(io::Error),
    AlreadyStarted,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clear { path, source } => {
                write!(f, "cannot remove {}: {source}", path.display())
            }
            Self::Spawn(source) => {
                write!(
                    f,
                    "cannot start the thread that follows Roblox's log: {source}"
                )
            }
            Self::AlreadyStarted => f.write_str("Roblox's log is already followed in this process"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Clear { source, .. } | Self::Spawn(source) => Some(source),
            Self::AlreadyStarted => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchOrigin {
    App,
    Link,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Experience {
    Joined,
    NotJoined,
}

static IN_EXPERIENCE: AtomicBool = AtomicBool::new(false);

pub fn experience() -> Experience {
    if IN_EXPERIENCE.load(Ordering::Acquire) {
        Experience::Joined
    } else {
        Experience::NotJoined
    }
}

fn publish_experience(experience: Experience) {
    IN_EXPERIENCE.store(experience == Experience::Joined, Ordering::Release);
}

pub fn start(
    runtime_dir: &Path,
    config: &Config,
    origin: LaunchOrigin,
) -> Result<(), SessionError> {
    clear(runtime_dir)?;
    let (tap, events) = Tap::new(GameRpc::Ignored);
    let state = SessionState::new(AfterLeaving::new(config.close_on_leave, origin));
    let effects = Effects {
        runtime_dir: runtime_dir.to_path_buf(),
        locator: config
            .server_location_indicator_enabled
            .then(|| Locator::new(server_location::ipinfo())),
        shown: None,
        submit: portal::submit,
        quit: quit_after_leaving,
        title: title_suffix::request,
        experience: publish_experience,
    };
    spawn(state, events, effects).map_err(SessionError::Spawn)?;
    client_log::install(tap).map_err(|_| SessionError::AlreadyStarted)?;
    Ok(())
}

fn quit_after_leaving() {
    lifecycle::request_quit(QuitReason::LeftExperience);
}

pub fn clear(runtime_dir: &Path) -> Result<(), SessionError> {
    let path = runtime_dir.join(SESSION_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SessionError::Clear { path, source }),
    }
}

fn spawn<Fetch, Submit, Quit, Title, Publish>(
    mut state: SessionState,
    events: Receiver<ClientEvent>,
    mut effects: Effects<Fetch, Submit, Quit, Title, Publish>,
) -> io::Result<JoinHandle<()>>
where
    Fetch: FnMut(IpAddr) -> Result<Location, LookupError> + Send + 'static,
    Submit: Fn(PortalRequest) -> Result<(), PortalError> + Send + 'static,
    Quit: Fn() + Send + 'static,
    Title: Fn(Option<String>) + Send + 'static,
    Publish: Fn(Experience) + Send + 'static,
{
    std::thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            for event in events {
                if let Some(action) = state.apply(event, Instant::now()) {
                    effects.perform(action);
                }
            }
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Join {
    place: PlaceId,
    job: JobId,
    universe: Option<UniverseId>,
}

#[derive(Default)]
enum Stage {
    #[default]
    Outside,
    Joining(Join),
    Playing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterLeaving {
    Close,
    Stay,
}

impl AfterLeaving {
    fn new(policy: CloseOnLeave, origin: LaunchOrigin) -> Self {
        match (policy, origin) {
            (CloseOnLeave::Always, _) | (CloseOnLeave::LinkLaunches, LaunchOrigin::Link) => {
                Self::Close
            }
            (CloseOnLeave::Never, _) | (CloseOnLeave::LinkLaunches, LaunchOrigin::App) => {
                Self::Stay
            }
        }
    }
}

struct SessionState {
    stage: Stage,
    after_leaving: AfterLeaving,
    left_at: Option<Instant>,
    teleporting: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Enter { join: Join, server: IpAddr },
    Leave,
    Explain318,
    Quit,
}

impl SessionState {
    fn new(after_leaving: AfterLeaving) -> Self {
        Self {
            stage: Stage::default(),
            after_leaving,
            left_at: None,
            teleporting: false,
        }
    }

    fn apply(&mut self, event: ClientEvent, now: Instant) -> Option<Action> {
        match event {
            ClientEvent::JoiningPlace { place, job } => {
                self.left_at = None;
                let joining = Stage::Joining(Join {
                    place,
                    job,
                    universe: None,
                });
                self.end_stage(joining)
            }
            ClientEvent::UniverseKnown { place, universe } => {
                match &mut self.stage {
                    Stage::Joining(join) if join.place == place => join.universe = Some(universe),
                    Stage::Joining(_) | Stage::Outside | Stage::Playing => {}
                }
                None
            }
            ClientEvent::JoinedServer { addr, .. } => {
                self.teleporting = false;
                match self.stage {
                    Stage::Joining(join) => {
                        self.stage = Stage::Playing;
                        Some(Action::Enter { join, server: addr })
                    }
                    Stage::Outside | Stage::Playing => None,
                }
            }
            ClientEvent::Teleporting => {
                self.teleporting = true;
                None
            }
            ClientEvent::Left => {
                self.left_at = Some(now);
                self.end_stage(Stage::Outside)
            }
            ClientEvent::ReturnedToApp => self.returned_to_app(now),
            ClientEvent::AttestationRequired => Some(Action::Explain318),
            ClientEvent::RpcPending => None,
        }
    }

    fn returned_to_app(&mut self, now: Instant) -> Option<Action> {
        let left_at = self.left_at.take()?;
        let left_for_the_app =
            !self.teleporting && now.duration_since(left_at) <= RETURN_AFTER_LEAVING;
        (left_for_the_app && self.after_leaving == AfterLeaving::Close).then_some(Action::Quit)
    }

    fn end_stage(&mut self, next: Stage) -> Option<Action> {
        match mem::replace(&mut self.stage, next) {
            Stage::Playing => Some(Action::Leave),
            Stage::Outside | Stage::Joining(_) => None,
        }
    }
}

struct Effects<Fetch, Submit, Quit, Title, Publish> {
    runtime_dir: PathBuf,
    locator: Option<Locator<Fetch>>,
    shown: Option<Location>,
    submit: Submit,
    quit: Quit,
    title: Title,
    experience: Publish,
}

impl<Fetch, Submit, Quit, Title, Publish> Effects<Fetch, Submit, Quit, Title, Publish>
where
    Fetch: FnMut(IpAddr) -> Result<Location, LookupError>,
    Submit: Fn(PortalRequest) -> Result<(), PortalError>,
    Quit: Fn(),
    Title: Fn(Option<String>),
    Publish: Fn(Experience),
{
    fn perform(&mut self, action: Action) {
        match action {
            Action::Enter { join, server } => self.enter(join, server),
            Action::Leave => self.leave(),
            Action::Explain318 => explain_attestation(&self.submit),
            Action::Quit => (self.quit)(),
        }
    }

    fn enter(&mut self, join: Join, server: IpAddr) {
        (self.experience)(Experience::Joined);
        let location = self
            .locator
            .as_mut()
            .and_then(|locator| locator.locate(server, Instant::now()));
        if let Err(error) = write(&self.runtime_dir, join, location.as_ref()) {
            tracing::warn!(
                path = %self.runtime_dir.join(SESSION_FILE).display(),
                %error,
                "cannot write the session file, so tools that read it miss this experience"
            );
        }
        let Some(location) = location else {
            return;
        };
        (self.title)(Some(location.as_str().to_owned()));
        let notice = Notice {
            id: NoticeId::ServerLocation,
            title: SERVER_LOCATION_TITLE.to_owned(),
            body: location.as_str().to_owned(),
        };
        if let Err(error) = (self.submit)(PortalRequest::Notify(notice)) {
            tracing::warn!(%error, "could not post a desktop notification");
        }
        self.shown = Some(location);
    }

    fn leave(&mut self) {
        (self.experience)(Experience::NotJoined);
        if let Err(error) = clear(&self.runtime_dir) {
            tracing::warn!(
                %error,
                "the session file still names the experience Roblox left"
            );
        }
        if self.shown.take().is_some() {
            (self.title)(None);
        }
    }
}

#[derive(Serialize)]
struct SessionFile<'a> {
    pid: u32,
    place_id: u64,
    universe_id: Option<u64>,
    job_id: String,
    server_location: Option<&'a str>,
}

fn write(runtime_dir: &Path, join: Join, location: Option<&Location>) -> io::Result<()> {
    let file = SessionFile {
        pid: std::process::id(),
        place_id: join.place.0,
        universe_id: join.universe.map(|universe| universe.0),
        job_id: join.job.to_string(),
        server_location: location.map(Location::as_str),
    };
    let mut json = serde_json::to_vec(&file)?;
    json.push(b'\n');
    temp_file::replace(runtime_dir, SESSION_FILE, &json)
}

fn explain_attestation(submit: &impl Fn(PortalRequest) -> Result<(), PortalError>) {
    tracing::warn!("{ATTESTATION_TITLE}: {ATTESTATION_EXPLANATION}");
    let notice = Notice {
        id: NoticeId::Attestation,
        title: ATTESTATION_TITLE.to_owned(),
        body: ATTESTATION_EXPLANATION.to_owned(),
    };
    if let Err(error) = submit(PortalRequest::Notify(notice)) {
        tracing::warn!(%error, "could not post a desktop notification");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use std::net::Ipv4Addr;
    use std::sync::mpsc;

    const SYNTHETIC_JOINS: &str = include_str!("../tests/fixtures/client-log/synthetic-joins.txt");
    const SYNTHETIC: &str = include_str!("../tests/fixtures/client-log/synthetic.txt");
    const ECLIPSE_STARTUP: &str = include_str!("../tests/fixtures/client-log/2.740.0.931.txt");
    const NO_OP_LEAVE: &str = "2026-09-28T18:25:10.200Z,410.200118,7a1fe6c0,6 \
        [FLog::SingleSurfaceApp] leaveUGCGame: (stage:LuaApp) ... no-op, not in-game";
    const LUA_APP_STAGE: &str = "2026-09-28T18:25:10.219Z,410.219309,7a1fe6c0,6 \
        [FLog::SingleSurfaceApp] setStage: (stage:LuaApp)";
    const USER_ID: &str = "987654321";
    const FIRST_JOB: &str = "00000000-0000-4000-8000-000000000001";
    const FILE_WAIT: Duration = Duration::from_secs(1);
    const START_CHILD: &str = "ECLIPSE_TEST_SESSION_START_CHILD";
    const START_CHILD_LIMIT: Duration = Duration::from_secs(60);
    const IPINFO: &[u8] = include_bytes!("../tests/fixtures/ipinfo.json");
    const SAN_MATEO: &str = "San Mateo, California, US";
    const FIRST_SERVER: IpAddr = IpAddr::V4(Ipv4Addr::new(128, 116, 0, 1));
    const SECOND_SERVER: IpAddr = IpAddr::V4(Ipv4Addr::new(128, 116, 0, 2));

    type Fetch = fn(IpAddr) -> Result<Location, LookupError>;
    type Quiet = Effects<
        Fetch,
        fn(PortalRequest) -> Result<(), PortalError>,
        fn(),
        fn(Option<String>),
        fn(Experience),
    >;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-session-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn fixture_events(log: &str) -> Vec<ClientEvent> {
        let (tap, events) = Tap::new(GameRpc::Ignored);
        client_log::with_test_tap(tap, || {
            for line in log.lines() {
                client_log::offer("Roblox", line);
            }
        });
        events.try_iter().collect()
    }

    fn first_join_events() -> Vec<ClientEvent> {
        let events = fixture_events(SYNTHETIC_JOINS);
        let joined = events
            .iter()
            .position(|event| matches!(event, ClientEvent::JoinedServer { .. }))
            .unwrap();
        events[..=joined].to_vec()
    }

    fn in_app_leave_events() -> Vec<ClientEvent> {
        let events = fixture_events(SYNTHETIC_JOINS);
        let returned = events
            .iter()
            .position(|event| *event == ClientEvent::ReturnedToApp)
            .unwrap();
        events[..=returned].to_vec()
    }

    fn teardown_events() -> Vec<ClientEvent> {
        let events = fixture_events(SYNTHETIC_JOINS);
        events[in_app_leave_events().len()..].to_vec()
    }

    fn quits(events: &[ClientEvent], policy: CloseOnLeave, origin: LaunchOrigin) -> usize {
        let mut state = SessionState::new(AfterLeaving::new(policy, origin));
        let now = Instant::now();
        events
            .iter()
            .filter(|event| state.apply(**event, now) == Some(Action::Quit))
            .count()
    }

    fn jobs(events: &[ClientEvent]) -> Vec<JobId> {
        events
            .iter()
            .filter_map(|event| match event {
                ClientEvent::JoiningPlace { job, .. } => Some(*job),
                _ => None,
            })
            .collect()
    }

    fn actions(events: &[ClientEvent]) -> Vec<Action> {
        let mut state = SessionState::new(AfterLeaving::Stay);
        let now = Instant::now();
        events
            .iter()
            .filter_map(|event| state.apply(*event, now))
            .collect()
    }

    fn wait_for_json(path: &Path) -> serde_json::Value {
        let deadline = Instant::now() + FILE_WAIT;
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                return serde_json::from_str(&text).unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "{} did not appear within {FILE_WAIT:?}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn quiet(_: PortalRequest) -> Result<(), PortalError> {
        Ok(())
    }

    fn stay() {
        panic!("only a leave with close_on_leave in effect closes Eclipse");
    }

    fn untitled(suffix: Option<String>) {
        panic!("only the server location indicator titles the window, not {suffix:?}");
    }

    fn unwatched(_: Experience) {}

    fn located(_: IpAddr) -> Result<Location, LookupError> {
        Location::from_ipinfo(IPINFO)
    }

    fn never_fetched(server: IpAddr) -> Result<Location, LookupError> {
        panic!("{server} is not looked up")
    }

    fn rate_limited(_: IpAddr) -> Result<Location, LookupError> {
        Err(LookupError::Fetch(crate::https::DownloadError::Status(429)))
    }

    fn quiet_effects(dir: &Path) -> Quiet {
        Effects {
            runtime_dir: dir.to_path_buf(),
            locator: None,
            shown: None,
            submit: quiet,
            quit: stay,
            title: untitled,
            experience: unwatched,
        }
    }

    fn first_enter() -> Action {
        actions(&first_join_events()).pop().unwrap()
    }

    fn session_file(dir: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(dir.join(SESSION_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn each_join_writes_the_session_file_and_each_leave_removes_it() {
        let events = fixture_events(SYNTHETIC_JOINS);
        let jobs = jobs(&events);
        assert_eq!(
            actions(&events),
            [
                Action::Enter {
                    join: Join {
                        place: PlaceId(1_000_001),
                        job: jobs[0],
                        universe: Some(UniverseId(2_000_001)),
                    },
                    server: FIRST_SERVER,
                },
                Action::Leave,
                Action::Enter {
                    join: Join {
                        place: PlaceId(1_000_002),
                        job: jobs[1],
                        universe: Some(UniverseId(2_000_002)),
                    },
                    server: SECOND_SERVER,
                },
                Action::Leave,
            ]
        );
    }

    #[test]
    fn a_failed_join_writes_nothing_and_a_new_join_ends_the_last_experience() {
        let events = first_join_events();
        let joining = events[0];
        let joined = *events.last().unwrap();
        let mut state = SessionState::new(AfterLeaving::Stay);
        let now = Instant::now();

        assert_eq!(state.apply(joining, now), None);
        assert_eq!(state.apply(ClientEvent::Left, now), None);
        assert_eq!(state.apply(joined, now), None, "a server without a join");
        assert_eq!(state.apply(joining, now), None);
        assert!(matches!(
            state.apply(joined, now),
            Some(Action::Enter {
                join: Join { universe: None, .. },
                server: FIRST_SERVER,
            })
        ));
        assert_eq!(state.apply(joined, now), None, "a repeated server line");
        assert_eq!(state.apply(joining, now), Some(Action::Leave));
    }

    #[test]
    fn the_session_file_names_the_join_and_never_the_user() {
        let dir = scratch("file");
        let mut effects = quiet_effects(&dir);

        effects.perform(first_enter());

        let text = fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(!text.contains(USER_ID), "{text}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({
                "pid": std::process::id(),
                "place_id": 1_000_001,
                "universe_id": 2_000_001,
                "job_id": FIRST_JOB,
                "server_location": null,
            })
        );
        assert_eq!(names(&dir), [SESSION_FILE], "no temporary remains");

        effects.perform(Action::Leave);
        assert_eq!(names(&dir), Vec::<String>::new());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_experience_counts_as_joined_from_its_join_until_it_is_left() {
        let dir = scratch("experience");
        let published = RefCell::new(Vec::new());
        let mut effects = Effects {
            runtime_dir: dir.clone(),
            locator: None::<Locator<Fetch>>,
            shown: None,
            submit: quiet,
            quit: stay,
            title: untitled,
            experience: |experience| published.borrow_mut().push(experience),
        };

        effects.perform(first_enter());
        assert_eq!(published.borrow().as_slice(), [Experience::Joined]);
        effects.perform(Action::Leave);
        assert_eq!(
            published.borrow().as_slice(),
            [Experience::Joined, Experience::NotJoined]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_the_indicator_no_server_is_looked_up_and_the_title_stays() {
        let dir = scratch("unlocated");
        let mut effects = quiet_effects(&dir);
        for action in actions(&fixture_events(SYNTHETIC_JOINS)) {
            let entering = matches!(action, Action::Enter { .. });
            effects.perform(action);
            if entering {
                assert_eq!(
                    session_file(&dir)["server_location"],
                    serde_json::Value::Null
                );
            }
        }
        assert_eq!(names(&dir), Vec::<String>::new());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_located_server_titles_the_window_fills_the_session_file_and_is_announced() {
        let dir = scratch("located");
        let titles = RefCell::new(Vec::new());
        let posted = RefCell::new(Vec::new());
        let mut effects = Effects {
            runtime_dir: dir.clone(),
            locator: Some(Locator::new(located as Fetch)),
            shown: None,
            submit: |request| {
                posted.borrow_mut().push(request);
                Ok(())
            },
            quit: stay,
            title: |suffix| titles.borrow_mut().push(suffix),
            experience: unwatched,
        };

        effects.perform(first_enter());

        assert_eq!(session_file(&dir)["server_location"], SAN_MATEO);
        assert_eq!(titles.borrow().as_slice(), [Some(SAN_MATEO.to_owned())]);
        let posted = posted.take();
        let [PortalRequest::Notify(notice)] = posted.as_slice() else {
            panic!("one notification is posted, not {}", posted.len());
        };
        assert_eq!(
            notice,
            &Notice {
                id: NoticeId::ServerLocation,
                title: SERVER_LOCATION_TITLE.to_owned(),
                body: SAN_MATEO.to_owned(),
            }
        );

        effects.perform(Action::Leave);
        assert_eq!(names(&dir), Vec::<String>::new());
        assert_eq!(
            titles.borrow().as_slice(),
            [Some(SAN_MATEO.to_owned()), None],
            "leaving clears the location from the title"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unlocated_server_leaves_the_title_alone_and_names_no_location() {
        let dir = scratch("private");
        let Some(Action::Enter { join, .. }) = actions(&first_join_events()).pop() else {
            panic!("the first join enters an experience");
        };
        let private = Action::Enter {
            join,
            server: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        };
        for (fetch, action) in [
            (never_fetched as Fetch, private),
            (rate_limited as Fetch, first_enter()),
        ] {
            let mut effects = Effects {
                locator: Some(Locator::new(fetch)),
                ..quiet_effects(&dir)
            };
            effects.perform(action);
            assert_eq!(
                session_file(&dir)["server_location"],
                serde_json::Value::Null
            );
            effects.perform(Action::Leave);
            assert_eq!(names(&dir), Vec::<String>::new());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn error_318_is_explained_once_in_the_log_and_in_a_notification() {
        let dir = scratch("attestation");
        let mut state = SessionState::new(AfterLeaving::Close);
        let action = state.apply(ClientEvent::AttestationRequired, Instant::now());
        assert_eq!(action, Some(Action::Explain318));

        let posted = RefCell::new(Vec::new());
        let mut effects = Effects {
            runtime_dir: dir.clone(),
            locator: None::<Locator<Fetch>>,
            shown: None,
            submit: |request| {
                posted.borrow_mut().push(request);
                Ok(())
            },
            quit: stay,
            title: untitled,
            experience: unwatched,
        };
        let log = crate::loader::log_capture::formatted_log("info", || {
            effects.perform(action.unwrap());
        });

        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 1, "{log}");
        assert!(
            lines[0].contains(" WARN ")
                && lines[0].contains("318")
                && lines[0].contains("attestation"),
            "{log}"
        );
        let posted = posted.into_inner();
        let [PortalRequest::Notify(notice)] = posted.as_slice() else {
            panic!("one notification is posted");
        };
        assert_eq!(
            notice,
            &Notice {
                id: NoticeId::Attestation,
                title: ATTESTATION_TITLE.to_owned(),
                body: ATTESTATION_EXPLANATION.to_owned(),
            }
        );
        assert_eq!(names(&dir), Vec::<String>::new());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_session_thread_writes_the_file_within_a_second_and_ends_with_its_sender() {
        let dir = scratch("thread");
        let (sender, events) = mpsc::sync_channel(32);
        let thread = spawn(
            SessionState::new(AfterLeaving::Stay),
            events,
            quiet_effects(&dir),
        )
        .unwrap();

        for event in first_join_events() {
            sender.send(event).unwrap();
        }
        let file = wait_for_json(&dir.join(SESSION_FILE));
        assert_eq!(file["job_id"], FIRST_JOB);

        drop(sender);
        let deadline = Instant::now() + FILE_WAIT;
        while !thread.is_finished() {
            assert!(Instant::now() < deadline, "the thread outlived its sender");
            std::thread::sleep(Duration::from_millis(10));
        }
        thread.join().unwrap();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_in_app_leave_closes_eclipse_for_the_policy_and_launch_origin() {
        use CloseOnLeave::{Always, LinkLaunches, Never};
        use LaunchOrigin::{App, Link};

        let leave = in_app_leave_events();
        for (policy, origin, closes) in [
            (Always, App, 1),
            (Always, Link, 1),
            (LinkLaunches, Link, 1),
            (LinkLaunches, App, 0),
            (Never, Link, 0),
            (Never, App, 0),
        ] {
            assert_eq!(
                quits(&leave, policy, origin),
                closes,
                "{policy:?} {origin:?}"
            );
        }
        let mut state = SessionState::new(AfterLeaving::new(Always, App));
        let now = Instant::now();
        let last = leave
            .iter()
            .filter_map(|event| state.apply(*event, now))
            .last();
        assert_eq!(
            last,
            Some(Action::Quit),
            "Eclipse closes after the file is removed"
        );
    }

    #[test]
    fn a_window_close_teardown_never_closes_eclipse() {
        let teardown = teardown_events();
        assert_eq!(teardown.last(), Some(&ClientEvent::Left));
        assert_eq!(
            quits(&teardown, CloseOnLeave::Always, LaunchOrigin::Link),
            0
        );
        assert_eq!(
            quits(
                &fixture_events(SYNTHETIC_JOINS),
                CloseOnLeave::Always,
                LaunchOrigin::Link
            ),
            1,
            "only the in-app leave of the two experiences closes Eclipse"
        );
    }

    #[test]
    fn a_teleport_does_not_close_eclipse_until_the_next_server_is_joined() {
        let joins = fixture_events(SYNTHETIC_JOINS);
        let leave = &joins[3..5];
        assert_eq!(leave, [ClientEvent::Left, ClientEvent::ReturnedToApp]);
        let teleport_line = SYNTHETIC
            .lines()
            .find(|line| line.contains("doTeleport:"))
            .unwrap();
        let teleporting = fixture_events(teleport_line);
        assert_eq!(teleporting, [ClientEvent::Teleporting]);

        let mut teleport = joins[..3].to_vec();
        teleport.extend(teleporting);
        teleport.extend_from_slice(leave);
        teleport.extend_from_slice(&joins[5..8]);
        assert_eq!(
            quits(&teleport, CloseOnLeave::Always, LaunchOrigin::Link),
            0
        );

        teleport.extend_from_slice(leave);
        assert_eq!(
            quits(&teleport, CloseOnLeave::Always, LaunchOrigin::Link),
            1,
            "leaving the server the teleport reached closes Eclipse"
        );
    }

    #[test]
    fn a_leave_belongs_to_the_experience_it_left() {
        let joins = fixture_events(SYNTHETIC_JOINS);
        let mut rejoin = joins[..3].to_vec();
        rejoin.push(ClientEvent::Teleporting);
        rejoin.push(ClientEvent::Left);
        rejoin.extend_from_slice(&joins[5..8]);
        rejoin.push(ClientEvent::ReturnedToApp);
        assert_eq!(
            quits(&rejoin, CloseOnLeave::Always, LaunchOrigin::Link),
            0,
            "a return to the app after the next join is not a return after leaving"
        );
    }

    #[test]
    fn only_a_return_to_the_app_within_ten_seconds_of_leaving_closes_eclipse() {
        let left_at = Instant::now();
        for (after, decision) in [
            (RETURN_AFTER_LEAVING, Some(Action::Quit)),
            (Duration::from_secs(11), None),
        ] {
            let mut state = SessionState::new(AfterLeaving::Close);
            for event in first_join_events() {
                state.apply(event, left_at);
            }
            assert_eq!(state.apply(ClientEvent::Left, left_at), Some(Action::Leave));
            assert_eq!(
                state.apply(ClientEvent::ReturnedToApp, left_at + after),
                decision,
                "{after:?} after leaving"
            );
            assert_eq!(
                state.apply(ClientEvent::ReturnedToApp, left_at + after),
                None,
                "one leave closes Eclipse at most once"
            );
        }
    }

    #[test]
    fn a_return_to_the_app_without_a_leave_does_not_close_eclipse() {
        let startup = fixture_events(ECLIPSE_STARTUP);
        assert_eq!(startup, [ClientEvent::ReturnedToApp]);
        assert_eq!(quits(&startup, CloseOnLeave::Always, LaunchOrigin::Link), 0);

        let no_op = fixture_events(&format!("{NO_OP_LEAVE}\n{LUA_APP_STAGE}"));
        assert_eq!(no_op, [ClientEvent::ReturnedToApp]);
        assert_eq!(quits(&no_op, CloseOnLeave::Always, LaunchOrigin::Link), 0);
    }

    #[test]
    fn the_session_thread_asks_to_close_eclipse_after_an_in_app_leave() {
        let dir = scratch("quit");
        let (sender, events) = mpsc::sync_channel(32);
        let (quit, closed) = mpsc::channel();
        let effects = Effects {
            runtime_dir: dir.clone(),
            locator: None::<Locator<Fetch>>,
            shown: None,
            submit: quiet,
            quit: move || quit.send(()).unwrap(),
            title: untitled,
            experience: unwatched,
        };
        let thread = spawn(SessionState::new(AfterLeaving::Close), events, effects).unwrap();

        for event in in_app_leave_events() {
            sender.send(event).unwrap();
        }
        assert_eq!(closed.recv_timeout(FILE_WAIT), Ok(()));
        assert_eq!(names(&dir), Vec::<String>::new());

        drop(sender);
        thread.join().unwrap();
        assert_eq!(closed.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn start_removes_a_stale_session_file_and_follows_the_installed_tap() {
        const TEST: &str = "start_removes_a_stale_session_file_and_follows_the_installed_tap";
        if let Some(dir) = std::env::var_os(START_CHILD) {
            let dir = PathBuf::from(dir);
            let config = Config {
                close_on_leave: CloseOnLeave::Always,
                ..Config::default()
            };
            start(&dir, &config, LaunchOrigin::App).unwrap();
            assert_eq!(
                names(&dir),
                Vec::<String>::new(),
                "start removed the stale file"
            );
            assert!(matches!(
                start(&dir, &config, LaunchOrigin::App),
                Err(SessionError::AlreadyStarted)
            ));
            let lines: Vec<&str> = SYNTHETIC_JOINS.lines().collect();
            let joined = lines
                .iter()
                .position(|line| line.contains("serverId: "))
                .unwrap();
            for line in &lines[..=joined] {
                client_log::offer("Roblox", line);
            }
            let file = wait_for_json(&dir.join(SESSION_FILE));
            assert_eq!(file["pid"], std::process::id());
            assert_eq!(file["job_id"], FIRST_JOB);

            let returned = lines
                .iter()
                .position(|line| line.ends_with("returnToLuaApp: (stage:LuaApp)."))
                .unwrap();
            for line in &lines[joined + 1..=returned] {
                client_log::offer("Roblox", line);
            }
            let deadline = Instant::now() + FILE_WAIT;
            while lifecycle::take_quit_request() != Some(QuitReason::LeftExperience) {
                assert!(
                    Instant::now() < deadline,
                    "close_on_leave did not ask the event loop to close Eclipse"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            return;
        }

        let dir = scratch("start");
        fs::write(dir.join(SESSION_FILE), "{\"pid\":1}\n").unwrap();
        let output = crate::bounded_child::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("session::tests::{TEST}"),
                    "--test-threads=1",
                ])
                .env(START_CHILD, &dir),
            START_CHILD_LIMIT,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "status={:?}, stdout={stdout}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(&dir).ok();
    }
}
