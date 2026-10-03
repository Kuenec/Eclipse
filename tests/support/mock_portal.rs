use std::ffi::CString;
use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use dbus::arg::{ArgType, PropMap};
use dbus::blocking::stdintf::org_freedesktop_dbus::RequestNameReply;
use dbus::blocking::Connection;
use dbus::channel::{MatchingReceiver as _, Sender as _};
use dbus::message::MatchRule;
use dbus::Message;

pub(crate) const PORTAL_OBJECT: &str = "/org/freedesktop/portal/desktop";
pub(crate) const OPEN_URI_HANDLE: &str = "/org/freedesktop/portal/desktop/request/mock/open_uri";
pub(crate) const INHIBIT_HANDLE: &str = "/org/freedesktop/portal/desktop/request/mock/inhibit";

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const CALL_WAIT: Duration = Duration::from_secs(10);
const QUIET_WAIT: Duration = Duration::from_millis(300);
const SERVE_POLL: Duration = Duration::from_millis(20);

static STARTED: AtomicUsize = AtomicUsize::new(0);

struct BusDir(PathBuf);

impl Drop for BusDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

pub(crate) struct TestBus {
    daemon: Child,
    address: String,
    _dir: BusDir,
}

impl TestBus {
    pub(crate) fn start() -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "bus{}-{}",
            STARTED.fetch_add(1, Ordering::Relaxed),
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create the test bus directory");
        let dir = BusDir(dir);
        let config = dir.0.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<busconfig>\n  <type>session</type>\n  <listen>unix:dir={}</listen>\n  \
                 <auth>EXTERNAL</auth>\n  <policy context=\"default\">\n    \
                 <allow send_destination=\"*\" eavesdrop=\"true\"/>\n    \
                 <allow eavesdrop=\"true\"/>\n    <allow own=\"*\"/>\n  </policy>\n\
                 </busconfig>\n",
                dir.0.display()
            ),
        )
        .expect("write the test bus configuration");
        let mut daemon = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| match error.kind() {
                std::io::ErrorKind::NotFound => {
                    panic!("the portal tests need a private bus: install dbus-daemon")
                }
                _ => panic!("cannot start dbus-daemon: {error}"),
            });
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().expect("dbus-daemon stdout"))
            .read_line(&mut address)
            .expect("read the test bus address");
        let address = address.trim().to_owned();
        assert!(
            !address.is_empty(),
            "dbus-daemon exited without printing its address"
        );
        Self {
            daemon,
            address,
            _dir: dir,
        }
    }

    pub(crate) fn address(&self) -> eclipse::portal::BusAddress {
        eclipse::portal::session_bus_address(Some(self.address.as_ref()), None)
            .expect("the test bus address is usable")
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        self.daemon.kill().ok();
        self.daemon.wait().ok();
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Replies {
    Answer { game_mode: i32 },
    Never,
}

pub(crate) struct PortalCall {
    pub(crate) path: String,
    pub(crate) interface: String,
    pub(crate) member: String,
    pub(crate) signature: String,
    pub(crate) message: Message,
}

impl PortalCall {
    fn record(message: Message) -> Self {
        let mut args = message.iter_init();
        let mut signature = String::new();
        while args.arg_type() != ArgType::Invalid {
            signature.push_str(&args.signature());
            args.next();
        }
        Self {
            path: message
                .path()
                .map(|path| path.to_string())
                .unwrap_or_default(),
            interface: message
                .interface()
                .map(|interface| interface.to_string())
                .unwrap_or_default(),
            member: message
                .member()
                .map(|member| member.to_string())
                .unwrap_or_default(),
            signature,
            message,
        }
    }
}

pub(crate) struct MockPortal {
    calls: Receiver<PortalCall>,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl MockPortal {
    pub(crate) fn serve(bus: &TestBus, replies: Replies) -> Self {
        let address = bus.address.clone();
        let (recorded, calls) = mpsc::channel();
        let (ready, serving) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let connection = Connection::new_address(&address).expect("connect the mock portal");
            let owned = connection
                .request_name(PORTAL_NAME, false, true, true)
                .expect("ask for the portal name");
            assert_eq!(owned, RequestNameReply::PrimaryOwner);
            connection.start_receive(
                MatchRule::new_method_call(),
                Box::new(move |call, connection| {
                    answer(call, connection, replies, &recorded);
                    true
                }),
            );
            ready.send(()).expect("report the mock portal ready");
            while !stopping.load(Ordering::Acquire) {
                connection
                    .process(SERVE_POLL)
                    .expect("serve the mock portal");
            }
        });
        serving
            .recv_timeout(CALL_WAIT)
            .expect("the mock portal did not start");
        Self {
            calls,
            stop,
            server: Some(server),
        }
    }

    pub(crate) fn next_call(&self) -> PortalCall {
        self.calls
            .recv_timeout(CALL_WAIT)
            .expect("the mock portal received no call")
    }

    pub(crate) fn assert_no_call(&self) {
        if let Ok(call) = self.calls.recv_timeout(QUIET_WAIT) {
            panic!(
                "unexpected portal call {}.{} on {}",
                call.interface, call.member, call.path
            );
        }
    }
}

impl Drop for MockPortal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            let stopped = server.join();
            if !std::thread::panicking() {
                stopped.expect("the mock portal panicked");
            }
        }
    }
}

fn answer(
    call: Message,
    connection: &Connection,
    replies: Replies,
    recorded: &mpsc::Sender<PortalCall>,
) {
    let reply = match replies {
        Replies::Never => None,
        Replies::Answer { game_mode } => Some(reply_to(&call, game_mode)),
    };
    let response = (reply.is_some() && call.member().as_deref() == Some("OpenURI")).then(|| {
        let mut signal = Message::new_signal(
            OPEN_URI_HANDLE,
            "org.freedesktop.portal.Request",
            "Response",
        )
        .expect("build the Response signal")
        .append2(0_u32, PropMap::new());
        signal.set_destination(call.sender());
        signal
    });
    recorded
        .send(PortalCall::record(call))
        .expect("record the portal call");
    for message in reply.into_iter().chain(response) {
        connection
            .send(message)
            .expect("send the mock portal's answer");
    }
}

fn reply_to(call: &Message, game_mode: i32) -> Message {
    let interface = call.interface().map(|interface| interface.to_string());
    let member = call.member().map(|member| member.to_string());
    match (interface.as_deref(), member.as_deref()) {
        (Some("org.freedesktop.portal.OpenURI"), Some("OpenURI")) => {
            call.return_with_args((dbus::Path::from(OPEN_URI_HANDLE),))
        }
        (Some("org.freedesktop.portal.GameMode"), Some("RegisterGameByPIDFd")) => {
            call.return_with_args((game_mode,))
        }
        (Some("org.freedesktop.portal.Inhibit"), Some("Inhibit")) => {
            call.return_with_args((dbus::Path::from(INHIBIT_HANDLE),))
        }
        (
            Some("org.freedesktop.portal.Notification"),
            Some("AddNotification" | "RemoveNotification"),
        )
        | (Some("org.freedesktop.portal.Request"), Some("Close")) => call.method_return(),
        _ => call.error(
            &"org.freedesktop.DBus.Error.UnknownMethod".into(),
            &CString::new("the mock portal does not implement this method")
                .expect("a C string without NUL"),
        ),
    }
}
