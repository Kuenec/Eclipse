use std::fs::File;
use std::io::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use eclipse_config::temp_file;
use jni::objects::JObject;
use jni::refs::Global;
use jni::sys::jint;
use jni::vm::JavaVM;
use jni::{jni_sig, Env};
use serde::{Deserialize, Serialize};

use super::{
    checked, FrameworkError, MainLooperDue, TrackedActivity, ACTIVITY_FINISHING_FIELD_NAME,
    TRACKED_ACTIVITIES,
};
use crate::apk::store::Store;
use crate::apk::VersionCode;
use crate::runtime::Vm;

const CLIENT_EXIT_HANDOFF: Duration = Duration::from_secs(5);
const CLIENT_EXIT_WEB_DEADLINE: Duration = Duration::from_secs(2);
const LAST_ACTIVITY_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const LAST_ACTIVITY_GRACE: Duration = Duration::from_secs(1);
const THREAD_NAME_PATH: &str = "/proc/thread-self/comm";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientEnd {
    Played,
    WindowClosed,
    ClosedForAnotherLaunch,
    FailureShown,
    ClientExited { status: i32 },
}

impl ClientEnd {
    fn status(self) -> i32 {
        match self {
            Self::Played | Self::ClosedForAnotherLaunch => 0,
            Self::WindowClosed | Self::FailureShown => 1,
            Self::ClientExited { status } => status,
        }
    }
}

static EXIT_RECORD: Mutex<Option<File>> = Mutex::new(None);

pub fn report_exit_to(pipe: File) {
    *EXIT_RECORD.lock().unwrap_or_else(PoisonError::into_inner) = Some(pipe);
}

pub fn finish_android_process(end: ClientEnd) -> ! {
    let mut record = EXIT_RECORD.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(pipe) = record.take() {
        if let Err(error) = write_exit_record(pipe, end) {
            eprintln!("eclipse: cannot tell Eclipse's supervisor how Roblox ended: {error}");
        }
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    unsafe { libc::_exit(end.status()) }
}

fn write_exit_record(mut pipe: File, end: ClientEnd) -> std::io::Result<()> {
    let mut record = serde_json::to_vec(&end)?;
    record.push(b'\n');
    pipe.write_all(&record)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuitReason {
    ClientExit { status: jint },
    AnotherLaunch,
    LeftExperience,
}

static QUIT_REQUEST: Mutex<Option<QuitReason>> = Mutex::new(None);

pub(crate) fn request_quit(reason: QuitReason) {
    *QUIT_REQUEST.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    super::wake_main_looper();
}

pub(crate) fn take_quit_request() -> Option<QuitReason> {
    QUIT_REQUEST
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

pub(crate) struct UnsavedFile {
    pub(crate) dir: PathBuf,
    pub(crate) name: &'static str,
    pub(crate) contents: Vec<u8>,
}

static UNSAVED_AT_CLIENT_EXIT: Mutex<Option<UnsavedFile>> = Mutex::new(None);

pub(crate) fn save_at_client_exit(file: Option<UnsavedFile>) {
    *UNSAVED_AT_CLIENT_EXIT
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = file;
}

static NORMAL_CLOSE_AT_CLIENT_EXIT: Mutex<Option<(Store, VersionCode)>> = Mutex::new(None);

pub fn record_normal_close_at_client_exit(store: Store, version: VersionCode) {
    *NORMAL_CLOSE_AT_CLIENT_EXIT
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some((store, version));
}

static EVENT_LOOP_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);

pub(crate) struct EventLoopThread(());

impl EventLoopThread {
    pub(crate) fn enter() -> Self {
        *EVENT_LOOP_THREAD
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(std::thread::current().id());
        Self(())
    }
}

impl Drop for EventLoopThread {
    fn drop(&mut self) {
        *EVENT_LOOP_THREAD
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitRoute {
    Inline,
    HandOff,
}

fn exit_route(current: ThreadId, event_loop: Option<ThreadId>) -> ExitRoute {
    match event_loop {
        Some(event_loop) if event_loop != current => ExitRoute::HandOff,
        Some(_) | None => ExitRoute::Inline,
    }
}

pub(crate) extern "C" fn client_exit_hook(status: jint) -> ! {
    if std::panic::catch_unwind(|| end_after_client_exit(status)).is_err() {
        eprintln!("eclipse: ending Roblox's System.exit({status}) panicked; exiting at once");
    }
    finish_android_process(ClientEnd::ClientExited { status })
}

fn end_after_client_exit(status: jint) {
    tracing::info!(
        status,
        thread = %current_thread_name(),
        "Roblox called System.exit; Eclipse ends the process with Roblox's status"
    );
    let event_loop = *EVENT_LOOP_THREAD
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match exit_route(std::thread::current().id(), event_loop) {
        ExitRoute::Inline => finish_after_client_exit(status),
        ExitRoute::HandOff => {
            request_quit(QuitReason::ClientExit { status });
            std::thread::sleep(CLIENT_EXIT_HANDOFF);
            tracing::warn!(
                status,
                "the host event loop did not end the process within {} s of System.exit; \
                 exiting without retiring the web engine",
                CLIENT_EXIT_HANDOFF.as_secs()
            );
            settle_client_exit(status);
        }
    }
}

fn current_thread_name() -> String {
    std::fs::read_to_string(THREAD_NAME_PATH).map_or_else(
        |error| format!("unknown ({error})"),
        |name| name.trim_end().to_owned(),
    )
}

pub(crate) fn finish_after_client_exit(status: jint) -> ! {
    if let Some(vm) = Vm::booted() {
        retire_web_engine(&vm, CLIENT_EXIT_WEB_DEADLINE);
    }
    settle_client_exit(status);
    finish_android_process(ClientEnd::ClientExited { status })
}

fn settle_client_exit(status: jint) {
    save_unsaved_file();
    record_normal_close(status);
}

fn save_unsaved_file() {
    let unsaved = UNSAVED_AT_CLIENT_EXIT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let Some(UnsavedFile {
        dir,
        name,
        contents,
    }) = unsaved
    else {
        return;
    };
    if let Err(error) = temp_file::replace(&dir, name, &contents) {
        tracing::warn!(
            path = %dir.join(name).display(),
            %error,
            "could not save this file before Roblox's System.exit ended Eclipse"
        );
    }
}

fn record_normal_close(status: jint) {
    if status != 0 || !crate::first_frame::shown() {
        return;
    }
    let armed = NORMAL_CLOSE_AT_CLIENT_EXIT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let Some((store, version)) = armed else {
        return;
    };
    if let Err(error) = store.record_normal_close(version) {
        tracing::warn!(
            %version,
            %error,
            "could not record that Roblox was played and closed, so Eclipse keeps the version \
             before it for now"
        );
        return;
    }
    if let Err(error) = store.prune() {
        tracing::warn!(
            %error,
            "could not remove the Roblox versions Eclipse no longer keeps"
        );
    }
}

pub(crate) fn retire_web_engine(vm: &Vm, deadline: Duration) {
    if crate::webview::client::needs_cookie_flush_before_shutdown() {
        if let Err(error) = super::cookie_manager_flush(vm) {
            tracing::warn!(
                error = %error,
                "CookieManager.flush dispatch failed; continuing the web engine helper's teardown"
            );
        }
    }
    let report = crate::webview::client::shutdown(vm, deadline);
    tracing::info!(
        helper_exit = report.helper_exit,
        reader_joined = report.reader_joined,
        "web engine retired"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityState {
    Running,
    Finishing,
    Finished,
}

fn last_activity_gone(activities: &[ActivityState]) -> bool {
    !activities.is_empty() && !activities.contains(&ActivityState::Running)
}

#[derive(Default)]
pub(crate) struct LastActivityCheck {
    next: Option<Instant>,
    gone_since: Option<Instant>,
}

impl LastActivityCheck {
    pub(crate) fn client_finished(
        &mut self,
        vm: &Vm,
        main_looper: MainLooperDue,
        now: Instant,
    ) -> bool {
        if !self.due(main_looper, now) {
            return false;
        }
        match activity_states(vm) {
            Ok(activities) => self.observe(last_activity_gone(&activities), now),
            Err(error) => {
                tracing::warn!(%error, "the running Android activities could not be read");
                false
            }
        }
    }

    fn due(&mut self, main_looper: MainLooperDue, now: Instant) -> bool {
        if main_looper == MainLooperDue::Now || self.next.is_some_and(|next| now < next) {
            return false;
        }
        self.next = Some(now + LAST_ACTIVITY_CHECK_INTERVAL);
        true
    }

    fn observe(&mut self, gone: bool, now: Instant) -> bool {
        if !gone {
            self.gone_since = None;
            return false;
        }
        let since = *self.gone_since.get_or_insert(now);
        now.duration_since(since) >= LAST_ACTIVITY_GRACE
    }
}

fn activity_states(vm: &Vm) -> Result<Vec<ActivityState>, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }

    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| tracked_activity_states(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn tracked_activity_states(env: &mut Env) -> Result<Vec<ActivityState>, FrameworkError> {
    let tracker = TRACKED_ACTIVITIES
        .lock()
        .map_err(|_| FrameworkError::ActivityTrackerPoisoned)?;
    tracker
        .iter()
        .map(|entry| match entry {
            TrackedActivity::Live(activity) => live_activity_state(env, activity),
            TrackedActivity::Finished(_) => Ok(ActivityState::Finished),
        })
        .collect()
}

fn live_activity_state(
    env: &mut Env,
    activity: &Global<JObject<'static>>,
) -> Result<ActivityState, FrameworkError> {
    let finishing = checked(env, "Activity.finishing", |env| {
        env.get_field(
            activity.as_obj(),
            ACTIVITY_FINISHING_FIELD_NAME,
            jni_sig!("Z"),
        )?
        .z()
    })?;
    Ok(if finishing {
        ActivityState::Finishing
    } else {
        ActivityState::Running
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::{fake_jvm, mark_activity_finished_once, track_activity};
    use std::process::Command;

    const RAW_EXIT_CHILD: &str = "ECLIPSE_TEST_RAW_ANDROID_EXIT_CHILD";
    const EXIT_HOOK_CHILD: &str = "ECLIPSE_TEST_CLIENT_EXIT_HOOK_CHILD";
    const EXIT_STATUS_CHILD: &str = "ECLIPSE_TEST_CLIENT_EXIT_STATUS";
    const FIRST_FRAME_CHILD: &str = "ECLIPSE_TEST_CLIENT_EXIT_FIRST_FRAME";
    const CLIENT_STATUS: jint = 7;
    const UPDATED_VERSION: VersionCode = VersionCode(3212);

    extern "C" fn abort_if_atexit_runs() {
        std::process::abort();
    }

    fn register_atexit_canary() {
        let registered = unsafe { libc::atexit(abort_if_atexit_runs) };
        assert_eq!(registered, 0, "the child must register its atexit canary");
    }

    fn child(test: &str, env: &str, value: &std::ffi::OsStr) -> Command {
        let mut command = Command::new(
            std::env::current_exe().expect("the test harness executable must have a path"),
        );
        command
            .args([
                "--exact",
                &format!("framework::lifecycle::tests::{test}"),
                "--test-threads=1",
            ])
            .env(env, value);
        command
    }

    fn record_path(test: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-lifecycle-{test}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create the exit record dir");
        dir.join("exit-record")
    }

    fn report_exit_to_file(path: &std::ffi::OsStr) {
        report_exit_to(File::create(path).expect("create the exit record"));
    }

    fn exit_record(path: &std::path::Path) -> String {
        let record = std::fs::read_to_string(path).expect("the child wrote an exit record");
        std::fs::remove_dir_all(path.parent().expect("record dir")).ok();
        record
    }

    #[test]
    fn android_process_exit_skips_unsafe_foreign_atexit_handlers() {
        if std::env::var_os(RAW_EXIT_CHILD).is_some() {
            register_atexit_canary();
            report_exit_to(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .expect("open /dev/null"),
            );
            finish_android_process(ClientEnd::Played);
        }

        let output = crate::bounded_child::output(
            &mut child(
                "android_process_exit_skips_unsafe_foreign_atexit_handlers",
                RAW_EXIT_CHILD,
                "1".as_ref(),
            ),
            Duration::from_secs(60),
        );

        assert!(
            output.status.success(),
            "the raw-exit child ran an atexit handler: status={:?}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn system_exit_on_the_event_loop_thread_ends_with_the_client_status_and_record() {
        const TEST: &str =
            "system_exit_on_the_event_loop_thread_ends_with_the_client_status_and_record";
        if let Some(path) = std::env::var_os(EXIT_HOOK_CHILD) {
            register_atexit_canary();
            report_exit_to_file(&path);
            let _event_loop = EventLoopThread::enter();
            client_exit_hook(CLIENT_STATUS);
        }

        let path = record_path("event-loop-thread");
        let output = crate::bounded_child::output(
            &mut child(TEST, EXIT_HOOK_CHILD, path.as_os_str()),
            Duration::from_secs(60),
        );

        assert_eq!(
            output.status.code(),
            Some(CLIENT_STATUS),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(exit_record(&path), "{\"client_exited\":{\"status\":7}}\n");
    }

    #[test]
    fn a_clean_system_exit_after_the_first_frame_counts_the_version_as_played() {
        use crate::apk::store::Proof;
        const TEST: &str = "a_clean_system_exit_after_the_first_frame_counts_the_version_as_played";
        if let Some(root) = std::env::var_os(EXIT_HOOK_CHILD) {
            let status = std::env::var(EXIT_STATUS_CHILD)
                .expect("the parent passes the exit status")
                .parse()
                .expect("the exit status is a number");
            record_normal_close_at_client_exit(Store::at(PathBuf::from(root)), UPDATED_VERSION);
            if std::env::var_os(FIRST_FRAME_CHILD).is_some() {
                crate::first_frame::presented();
            }
            let _event_loop = EventLoopThread::enter();
            client_exit_hook(status);
        }

        for (status, first_frame, proof) in [
            (0, true, None),
            (CLIENT_STATUS, true, Some(Proof::NormalClose)),
            (0, false, Some(Proof::NormalClose)),
        ] {
            let root = record_path(&format!("played-{status}-{first_frame}"))
                .parent()
                .expect("record dir")
                .to_path_buf();
            std::fs::create_dir_all(root.join(UPDATED_VERSION.to_string()))
                .expect("create the version dir");
            std::fs::write(
                root.join("current.json"),
                format!(r#"{{"version_code":{UPDATED_VERSION}}}"#),
            )
            .expect("write current.json");
            let store = Store::at(root.clone());
            store
                .record_first_frame(UPDATED_VERSION)
                .expect("record the first frame");

            let mut command = child(TEST, EXIT_HOOK_CHILD, root.as_os_str());
            command.env(EXIT_STATUS_CHILD, status.to_string());
            if first_frame {
                command.env(FIRST_FRAME_CHILD, "1");
            }
            let output = crate::bounded_child::output(&mut command, Duration::from_secs(60));

            assert_eq!(
                output.status.code(),
                Some(status),
                "stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                store.proof_needed(UPDATED_VERSION).expect("read the store"),
                proof,
                "status {status}, first frame shown: {first_frame}"
            );
            std::fs::remove_dir_all(&root).ok();
        }
    }

    #[test]
    fn system_exit_on_a_worker_is_ended_by_the_event_loop_thread() {
        const TEST: &str = "system_exit_on_a_worker_is_ended_by_the_event_loop_thread";
        if let Some(path) = std::env::var_os(EXIT_HOOK_CHILD) {
            report_exit_to_file(&path);
            let _event_loop = EventLoopThread::enter();
            std::thread::spawn(|| client_exit_hook(CLIENT_STATUS));
            loop {
                if let Some(QuitReason::ClientExit { status }) = take_quit_request() {
                    finish_after_client_exit(status);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        let path = record_path("worker-served");
        let output = crate::bounded_child::output(
            &mut child(TEST, EXIT_HOOK_CHILD, path.as_os_str()),
            CLIENT_EXIT_HANDOFF - Duration::from_secs(1),
        );

        assert_eq!(
            output.status.code(),
            Some(CLIENT_STATUS),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(exit_record(&path), "{\"client_exited\":{\"status\":7}}\n");
    }

    #[test]
    fn system_exit_on_a_worker_ends_the_process_when_the_event_loop_does_not_answer() {
        const TEST: &str =
            "system_exit_on_a_worker_ends_the_process_when_the_event_loop_does_not_answer";
        if std::env::var_os(EXIT_HOOK_CHILD).is_some() {
            let _event_loop = EventLoopThread::enter();
            std::thread::spawn(|| client_exit_hook(CLIENT_STATUS));
            loop {
                std::thread::park();
            }
        }

        let started = Instant::now();
        let output = crate::bounded_child::output(
            &mut child(TEST, EXIT_HOOK_CHILD, "1".as_ref()),
            CLIENT_EXIT_HANDOFF + Duration::from_secs(1),
        );

        assert_eq!(
            output.status.code(),
            Some(CLIENT_STATUS),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            started.elapsed() >= CLIENT_EXIT_HANDOFF,
            "the hook waited for the event loop before ending the process"
        );
    }

    #[test]
    fn only_a_thread_other_than_the_running_event_loop_hands_the_exit_off() {
        let current = std::thread::current().id();
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("the other thread ran");

        assert_eq!(exit_route(current, Some(current)), ExitRoute::Inline);
        assert_eq!(exit_route(current, Some(other)), ExitRoute::HandOff);
        assert_eq!(exit_route(current, None), ExitRoute::Inline);
    }

    #[test]
    fn the_client_ends_only_when_no_tracked_activity_is_still_running() {
        use ActivityState::{Finished, Finishing, Running};

        for (activities, gone) in [
            (&[Finished][..], true),
            (&[Finishing][..], true),
            (&[Finished, Finishing][..], true),
            (&[Running][..], false),
            (&[Finished, Running][..], false),
            (&[Finishing, Running][..], false),
            (&[][..], false),
        ] {
            assert_eq!(last_activity_gone(activities), gone, "{activities:?}");
        }
    }

    #[test]
    fn the_last_activity_check_skips_a_main_looper_backlog_and_runs_every_250_ms() {
        let start = Instant::now();
        let mut check = LastActivityCheck::default();

        assert!(!check.due(MainLooperDue::Now, start));
        assert!(check.due(MainLooperDue::At(start), start));
        assert!(!check.due(
            MainLooperDue::WhenWoken,
            start + LAST_ACTIVITY_CHECK_INTERVAL - Duration::from_millis(1)
        ));
        assert!(check.due(
            MainLooperDue::WhenWoken,
            start + LAST_ACTIVITY_CHECK_INTERVAL
        ));
    }

    #[test]
    fn the_client_ends_once_no_activity_has_run_for_a_second() {
        let start = Instant::now();
        let mut check = LastActivityCheck::default();
        let at = |millis| start + Duration::from_millis(millis);

        assert!(!check.observe(false, at(0)));
        assert!(!check.observe(true, at(250)));
        assert!(!check.observe(true, at(1_000)));
        assert!(
            !check.observe(false, at(1_250)),
            "an activity started within the second keeps the client"
        );
        assert!(!check.observe(true, at(1_500)));
        assert!(!check.observe(true, at(2_250)));
        assert!(check.observe(true, at(2_500)));
    }

    #[test]
    fn tracked_activities_report_running_finishing_and_finished() {
        let _lock = crate::framework::ACTIVITY_TRACKER_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        fake_jvm::with_env(|env| {
            TRACKED_ACTIVITIES.lock().expect("tracker").clear();

            let only = fake_jvm::new_object(env);
            track_activity(env, &only);
            assert_eq!(
                tracked_activity_states(env).expect("states"),
                [ActivityState::Running]
            );
            assert!(mark_activity_finished_once(env, &only));
            let states = tracked_activity_states(env).expect("states");
            assert_eq!(states, [ActivityState::Finished]);
            assert!(last_activity_gone(&states));

            TRACKED_ACTIVITIES.lock().expect("tracker").clear();
            let first = fake_jvm::new_object(env);
            let second = fake_jvm::new_object(env);
            track_activity(env, &first);
            track_activity(env, &second);
            assert!(mark_activity_finished_once(env, &first));
            let states = tracked_activity_states(env).expect("states");
            assert_eq!(states, [ActivityState::Finished, ActivityState::Running]);
            assert!(!last_activity_gone(&states));

            TRACKED_ACTIVITIES.lock().expect("tracker").clear();
            let recreated = fake_jvm::new_object(env);
            let replacement = fake_jvm::new_object(env);
            track_activity(env, &recreated);
            track_activity(env, &replacement);
            fake_jvm::set_boolean_field(&recreated, "finishing", true);
            let states = tracked_activity_states(env).expect("states");
            assert_eq!(states, [ActivityState::Finishing, ActivityState::Running]);
            assert!(!last_activity_gone(&states));

            assert!(mark_activity_finished_once(env, &replacement));
            let states = tracked_activity_states(env).expect("states");
            assert_eq!(states, [ActivityState::Finishing, ActivityState::Finished]);
            assert!(last_activity_gone(&states));

            TRACKED_ACTIVITIES.lock().expect("tracker").clear();
        });
    }
}
