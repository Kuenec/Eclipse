#[path = "../src/bounded_child.rs"]
mod bounded_child;
#[path = "support/mock_portal.rs"]
mod mock_portal;

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use dbus::arg::{PropMap, Variant};
use eclipse::portal::{
    ExternalUri, IdleInhibit, Notice, NoticeId, PortalError, PortalRequest, PortalSession,
    PortalWorker, SteamKeyboard, SteamKeyboardMode,
};
use mock_portal::{MockPortal, Replies, TestBus, INHIBIT_HANDLE, PORTAL_OBJECT};

const FLUSH_WAIT: Duration = Duration::from_secs(10);
const PLATFORM_TEST_LIMIT: Duration = Duration::from_secs(300);

fn answered(replies: Replies) -> (TestBus, MockPortal, PortalSession) {
    let bus = TestBus::start();
    let portal = MockPortal::serve(&bus, replies);
    let session = PortalSession::open(&bus.address()).expect("open the portal session");
    (bus, portal, session)
}

fn answering() -> (TestBus, MockPortal, PortalSession) {
    answered(Replies::Answer { game_mode: 0 })
}

fn notice(id: NoticeId) -> Notice {
    Notice {
        id,
        title: "Title".to_owned(),
        body: "Body".to_owned(),
    }
}

fn pidfd_process(pidfd: &OwnedFd) -> u32 {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))
        .expect("read the received descriptor's fdinfo");
    info.lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .unwrap_or_else(|| panic!("the received descriptor is not a pidfd:\n{info}"))
        .trim()
        .parse()
        .expect("a pid in the pidfd's fdinfo")
}

fn flushed(worker: &PortalWorker) {
    let (done, acknowledged) = mpsc::sync_channel(1);
    worker
        .submit(PortalRequest::Flush(done))
        .expect("queue the flush");
    acknowledged
        .recv_timeout(FLUSH_WAIT)
        .expect("the worker acknowledges the flush");
}

#[test]
fn open_uri_sends_one_open_uri_call() {
    let (_bus, portal, mut session) = answering();
    let uri = ExternalUri::parse("https://example.org/a?b=c").expect("a web link");

    session
        .handle(PortalRequest::OpenUri(uri))
        .expect("OpenURI succeeds");

    let call = portal.next_call();
    assert_eq!(
        (
            call.path.as_str(),
            call.interface.as_str(),
            call.member.as_str()
        ),
        (PORTAL_OBJECT, "org.freedesktop.portal.OpenURI", "OpenURI")
    );
    assert_eq!(call.signature, "ssa{sv}");
    let (parent, uri, options): (&str, &str, PropMap) = call.message.read3().expect("OpenURI args");
    assert_eq!((parent, uri), ("", "https://example.org/a?b=c"));
    assert!(options.is_empty());
    portal.assert_no_call();
}

#[test]
fn steam_keyboard_requests_open_only_their_steam_uris() {
    let (_bus, portal, mut session) = answering();

    for (keyboard, sent) in [
        (
            SteamKeyboard::Open {
                x: 40,
                y: 620,
                width: 900,
                height: 52,
                mode: SteamKeyboardMode::SingleLine,
            },
            "steam://open/keyboard?XPosition=40&YPosition=620&Width=900&Height=52&Mode=0",
        ),
        (
            SteamKeyboard::Open {
                x: 0,
                y: 360,
                width: 1280,
                height: 300,
                mode: SteamKeyboardMode::MultipleLines,
            },
            "steam://open/keyboard?XPosition=0&YPosition=360&Width=1280&Height=300&Mode=1",
        ),
        (SteamKeyboard::Close, "steam://close/keyboard"),
    ] {
        session
            .handle(PortalRequest::SteamKeyboard(keyboard))
            .expect("OpenURI succeeds");

        let call = portal.next_call();
        assert_eq!(
            (
                call.path.as_str(),
                call.interface.as_str(),
                call.member.as_str()
            ),
            (PORTAL_OBJECT, "org.freedesktop.portal.OpenURI", "OpenURI")
        );
        assert_eq!(call.signature, "ssa{sv}");
        let (parent, uri, options): (&str, &str, PropMap) =
            call.message.read3().expect("OpenURI args");
        assert_eq!((parent, uri), ("", sent));
        assert!(options.is_empty());
    }
    portal.assert_no_call();
}

#[test]
fn register_game_sends_this_process_once() {
    let (_bus, portal, mut session) = answering();

    session
        .handle(PortalRequest::RegisterGame)
        .expect("RegisterGame succeeds");
    session
        .handle(PortalRequest::RegisterGame)
        .expect("a second RegisterGame is a no-op");

    let call = portal.next_call();
    assert_eq!(
        (call.interface.as_str(), call.member.as_str()),
        ("org.freedesktop.portal.GameMode", "RegisterGameByPIDFd")
    );
    assert_eq!(call.signature, "hh");
    let (target, requester): (OwnedFd, OwnedFd) =
        call.message.read2().expect("RegisterGameByPIDFd pidfds");
    assert_eq!(pidfd_process(&target), std::process::id());
    assert_eq!(pidfd_process(&requester), std::process::id());
    portal.assert_no_call();
}

#[test]
fn a_refused_game_registration_is_an_error_and_is_not_retried() {
    let (_bus, portal, mut session) = answered(Replies::Answer { game_mode: -1 });

    let refused = session.handle(PortalRequest::RegisterGame);
    assert!(
        matches!(refused, Err(PortalError::GameModeRejected(-1))),
        "{refused:?}"
    );
    session
        .handle(PortalRequest::RegisterGame)
        .expect("a second RegisterGame is a no-op");

    assert_eq!(portal.next_call().member, "RegisterGameByPIDFd");
    portal.assert_no_call();
}

#[test]
fn a_game_mode_portal_without_gamemoded_is_not_an_error_and_is_not_retried() {
    let (_bus, portal, mut session) = answered(Replies::Answer { game_mode: -2 });

    session
        .handle(PortalRequest::RegisterGame)
        .expect("GameMode being unavailable is not a failure");
    session
        .handle(PortalRequest::RegisterGame)
        .expect("a second RegisterGame is a no-op");

    assert_eq!(portal.next_call().member, "RegisterGameByPIDFd");
    portal.assert_no_call();
}

#[test]
fn idle_inhibit_holds_once_and_closes_the_returned_request_once() {
    let (_bus, portal, mut session) = answering();

    for step in [
        IdleInhibit::Hold,
        IdleInhibit::Hold,
        IdleInhibit::Release,
        IdleInhibit::Release,
    ] {
        session
            .handle(PortalRequest::IdleInhibit(step))
            .expect("the inhibit step succeeds");
    }

    let inhibit = portal.next_call();
    assert_eq!(
        (
            inhibit.path.as_str(),
            inhibit.interface.as_str(),
            inhibit.member.as_str()
        ),
        (PORTAL_OBJECT, "org.freedesktop.portal.Inhibit", "Inhibit")
    );
    assert_eq!(inhibit.signature, "sua{sv}");
    let (window, flags, options): (&str, u32, HashMap<String, Variant<String>>) =
        inhibit.message.read3().expect("Inhibit args");
    assert_eq!((window, flags), ("", 8));
    assert_eq!(
        options,
        HashMap::from([("reason".to_owned(), Variant("Playing Roblox".to_owned()))])
    );
    let close = portal.next_call();
    assert_eq!(
        (
            close.path.as_str(),
            close.interface.as_str(),
            close.member.as_str()
        ),
        (INHIBIT_HANDLE, "org.freedesktop.portal.Request", "Close")
    );
    assert_eq!(close.signature, "");
    portal.assert_no_call();
}

#[test]
fn notices_are_added_and_withdrawn_by_their_wire_id() {
    let (_bus, portal, mut session) = answering();

    for (id, wire_id) in [
        (NoticeId::Client(7), "client-7"),
        (NoticeId::Share, "eclipse-share"),
        (NoticeId::Link, "eclipse-link"),
    ] {
        session
            .handle(PortalRequest::Notify(notice(id)))
            .expect("AddNotification succeeds");
        session
            .handle(PortalRequest::Withdraw(id))
            .expect("RemoveNotification succeeds");

        let add = portal.next_call();
        assert_eq!(
            (add.interface.as_str(), add.member.as_str()),
            ("org.freedesktop.portal.Notification", "AddNotification")
        );
        assert_eq!(add.signature, "sa{sv}");
        let (added, fields): (&str, HashMap<String, Variant<String>>) =
            add.message.read2().expect("AddNotification args");
        assert_eq!(added, wire_id);
        assert_eq!(
            fields,
            HashMap::from([
                ("title".to_owned(), Variant("Title".to_owned())),
                ("body".to_owned(), Variant("Body".to_owned())),
            ])
        );
        let remove = portal.next_call();
        assert_eq!(remove.member, "RemoveNotification");
        assert_eq!(remove.signature, "s");
        let removed: &str = remove.message.read1().expect("RemoveNotification args");
        assert_eq!(removed, wire_id);
    }
    portal.assert_no_call();
}

#[test]
fn without_a_portal_the_error_names_the_call_and_the_bus_error() {
    let bus = TestBus::start();
    let mut session = PortalSession::open(&bus.address()).expect("open the portal session");

    let failed = session
        .handle(PortalRequest::RegisterGame)
        .expect_err("nobody owns the portal name");

    let message = failed.to_string();
    assert!(
        message.contains("org.freedesktop.portal.GameMode.RegisterGameByPIDFd failed"),
        "{message}"
    );
    assert!(
        message.contains("org.freedesktop.DBus.Error.ServiceUnknown"),
        "{message}"
    );
}

#[test]
fn the_worker_handles_requests_in_order_and_acknowledges_a_flush() {
    let bus = TestBus::start();
    let portal = MockPortal::serve(&bus, Replies::Answer { game_mode: 0 });
    let worker = PortalWorker::spawn(Ok(bus.address())).expect("start the worker");

    worker
        .submit(PortalRequest::Notify(notice(NoticeId::Link)))
        .expect("queue the notice");
    worker
        .submit(PortalRequest::Withdraw(NoticeId::Link))
        .expect("queue the withdrawal");
    flushed(&worker);

    assert_eq!(portal.next_call().member, "AddNotification");
    assert_eq!(portal.next_call().member, "RemoveNotification");
    portal.assert_no_call();
}

#[test]
fn a_worker_without_a_bus_still_answers_flushes() {
    let unreachable = eclipse::portal::session_bus_address(
        Some("unix:path=/nonexistent/eclipse-test-bus".as_ref()),
        None,
    )
    .expect("a well-formed address");
    for address in [Err(PortalError::NoSessionBus), Ok(unreachable)] {
        let worker = PortalWorker::spawn(address).expect("start the worker");
        worker
            .submit(PortalRequest::RegisterGame)
            .expect("queue a request");
        flushed(&worker);
    }
}

#[test]
fn the_queue_refuses_the_request_after_32_waiting_ones() {
    let bus = TestBus::start();
    let portal = MockPortal::serve(&bus, Replies::Never);
    let worker = PortalWorker::spawn(Ok(bus.address())).expect("start the worker");
    worker
        .submit(PortalRequest::RegisterGame)
        .expect("queue the request the worker blocks on");
    assert_eq!(portal.next_call().member, "RegisterGameByPIDFd");

    for waiting in 1..=32 {
        worker
            .submit(PortalRequest::Withdraw(NoticeId::Client(waiting)))
            .unwrap_or_else(|error| panic!("request {waiting} should wait in the queue: {error}"));
    }
    let refused = worker.submit(PortalRequest::Withdraw(NoticeId::Client(33)));
    assert!(matches!(refused, Err(PortalError::Busy)), "{refused:?}");
}

struct PlatformTestRoot(PathBuf);

impl PlatformTestRoot {
    fn create(tag: &str) -> Self {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("portal-platform-test-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(path.join("config")).expect("create the config dir");
        Self(path)
    }
}

impl Drop for PlatformTestRoot {
    fn drop(&mut self) {
        let removed = std::fs::remove_dir_all(&self.0);
        if !std::thread::panicking() {
            removed.expect("remove the platform test dir");
        }
    }
}

fn platform_test_can_boot() -> bool {
    let apk = eclipse::apk::ApkSetPaths::from_env()
        .expect("ECLIPSE_ROBLOX_APK must name an APK or a directory holding base.apk")
        .is_some();
    if !apk {
        eprintln!(
            "SKIP: Roblox APK absent (set ECLIPSE_ROBLOX_APK to an APK file or to a directory \
             holding base.apk and split_config.x86_64.apk)"
        );
        return false;
    }
    let runtime = eclipse::runtime::find_libart().is_ok()
        && eclipse::runtime::find_framework().is_ok()
        && eclipse::runtime::find_boot_image().is_ok();
    if !runtime {
        eprintln!(
            "SKIP: Android runtime absent (ART, its boot image or the patched framework; set \
             ECLIPSE_LIBART and ECLIPSE_ANDROID_FRAMEWORK_DIR)"
        );
        return false;
    }
    true
}

fn assert_added_notice(call: &mock_portal::PortalCall, [id, title, body]: [&str; 3], text: &str) {
    assert_eq!(
        (call.interface.as_str(), call.member.as_str()),
        ("org.freedesktop.portal.Notification", "AddNotification"),
        "{text}"
    );
    let (added, fields): (&str, HashMap<String, Variant<String>>) =
        call.message.read2().expect("AddNotification args");
    assert_eq!(added, id, "{text}");
    assert_eq!(
        fields,
        HashMap::from([
            ("title".to_owned(), Variant(title.to_owned())),
            ("body".to_owned(), Variant(body.to_owned())),
        ]),
        "{text}"
    );
}

#[test]
fn platform_test_sends_client_notifications_links_and_shares_to_the_portal() {
    if !platform_test_can_boot() {
        return;
    }
    let bus = TestBus::start();
    let portal = MockPortal::serve(&bus, Replies::Answer { game_mode: 0 });
    let root = PlatformTestRoot::create("notifications");

    let out = bounded_child::output(
        Command::new(env!("CARGO_BIN_EXE_eclipse"))
            .arg("__platform-test")
            .env("DBUS_SESSION_BUS_ADDRESS", bus.address().as_str())
            .env("XDG_CONFIG_HOME", root.0.join("config"))
            .env("ECLIPSE_APP_DATA_DIR", root.0.join("app-data")),
        PLATFORM_TEST_LIMIT,
    );
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "__platform-test exited non-zero ({:?}).\n{text}",
        out.status.code()
    );
    for line in [
        "NotificationManager natives returned without an exception; nativeInitBuilder returned 0",
        "Activity.nativeOpenURI(\"https://example.org/x\") returned",
        "Activity.nativeOpenURI(\"roblox://placeId=1\") threw \
         android.content.ActivityNotFoundException",
        "Context.nativeShareFile(\"hi https://www.roblox.com/share?code=X\", -1) returned",
        "host clipboard slot holds \"hi https://www.roblox.com/share?code=X\"",
    ] {
        assert!(
            text.lines()
                .any(|output| output.strip_prefix("__platform-test: ") == Some(line)),
            "missing {line:?}.\n{text}"
        );
    }

    assert_added_notice(&portal.next_call(), ["client-7", "Title", "Body"], &text);
    let remove = portal.next_call();
    assert_eq!(
        (remove.interface.as_str(), remove.member.as_str()),
        ("org.freedesktop.portal.Notification", "RemoveNotification"),
        "{text}"
    );
    let removed: &str = remove.message.read1().expect("RemoveNotification args");
    assert_eq!(removed, "client-7");

    let open = portal.next_call();
    assert_eq!(
        (open.interface.as_str(), open.member.as_str()),
        ("org.freedesktop.portal.OpenURI", "OpenURI"),
        "{text}"
    );
    let (parent, uri, options): (&str, &str, PropMap) = open.message.read3().expect("OpenURI args");
    assert_eq!((parent, uri), ("", "https://example.org/x"));
    assert!(options.is_empty());
    assert_added_notice(
        &portal.next_call(),
        [
            "eclipse-link",
            "Can't open this link",
            "Eclipse opens web and email links only.",
        ],
        &text,
    );
    assert_added_notice(
        &portal.next_call(),
        [
            "eclipse-share",
            "Copied to clipboard",
            "Paste it wherever you want to share it.",
        ],
        &text,
    );
    portal.assert_no_call();
}
