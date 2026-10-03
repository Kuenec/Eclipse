use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, ErrorKind, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use eclipse::graphics::activation::Token;
use eclipse::graphics::launch_window::{WindowCommand, WindowControl, WindowGone};
use eclipse::links::LaunchTarget;
use eclipse::session::Experience;
use rustix::process::Uid;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const CLIENT_LOCK_FILE: &str = "client.lock";
const PROMPT_LOCK_EXTENSION: &str = "prompt";
const HOST_SOCKET_DIR: &str = "eclipse";
const FLATPAK_RUNTIME_DIRS: &str = "app";
const SOCKET_HASH_BYTES: usize = 8;
const SOCKET_PATH_LIMIT: usize = 107;
const PRIVATE_DIR_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;
const SHARED_MODE_BITS: u32 = 0o077;
const PROTOCOL_VERSION: u8 = 1;
const MESSAGE_LIMIT: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const RAISE_TIMEOUT: Duration = Duration::from_secs(10);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(12);
const LOCK_RETRY: Duration = Duration::from_secs(2);
const FIRST_BACKOFF: Duration = Duration::from_millis(25);
const LONGEST_BACKOFF: Duration = Duration::from_millis(400);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(20);
const CLOSE_POLL: Duration = Duration::from_millis(50);
const NO_RUNTIME_DIR: &str =
    "XDG_RUNTIME_DIR is not set to an absolute path; start Eclipse from a desktop session.";
const NO_ANSWER: &str = "Roblox in Eclipse did not answer; close it and try again";
const UNEXPECTED_ANSWER: &str =
    "Roblox in Eclipse gave an answer this Eclipse does not understand; close it and try again";

pub(crate) enum ClientLock {
    Acquired(File),
    Held(PathBuf),
}

pub(crate) fn lock_client(runtime_dir: &Path) -> Result<ClientLock, String> {
    std::fs::create_dir_all(runtime_dir)
        .map_err(|error| format!("cannot create {}: {error}", runtime_dir.display()))?;
    let path = runtime_dir.join(CLIENT_LOCK_FILE);
    Ok(match try_lock(&path)? {
        Some(lock) => ClientLock::Acquired(lock),
        None => ClientLock::Held(path),
    })
}

pub(crate) fn lock_prompt(socket: &Path) -> Result<Option<File>, String> {
    try_lock(&socket.with_extension(PROMPT_LOCK_EXTENSION))
}

fn try_lock(path: &Path) -> Result<Option<File>, String> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(format!("cannot lock {}: {error}", path.display()))
        }
    }
}

pub(crate) fn control_socket(lock_root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(lock_root)
        .map_err(|error| format!("cannot create {}: {error}", lock_root.display()))?;
    let flatpak_app =
        crate::desktop_integration::sandbox_app_id().map_err(|error| error.to_string())?;
    let dir = socket_dir(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        flatpak_app.as_deref(),
    )?;
    socket_path(&dir, lock_root)
}

fn socket_dir(runtime_dir: Option<&OsStr>, flatpak_app: Option<&str>) -> Result<PathBuf, String> {
    let runtime_dir = runtime_dir
        .map(Path::new)
        .filter(|dir| dir.is_absolute())
        .ok_or(NO_RUNTIME_DIR)?;
    let dir = match flatpak_app {
        Some(app) => runtime_dir.join(FLATPAK_RUNTIME_DIRS).join(app),
        None => runtime_dir.join(HOST_SOCKET_DIR),
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR_MODE)
        .create(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let metadata = std::fs::symlink_metadata(&dir)
        .map_err(|error| format!("cannot inspect {}: {error}", dir.display()))?;
    let private = metadata.is_dir()
        && metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & SHARED_MODE_BITS == 0;
    if !private {
        return Err(format!(
            "{} must be a directory that only you can open (yours, mode 0700); fix or remove it",
            dir.display()
        ));
    }
    Ok(dir)
}

fn socket_path(socket_dir: &Path, lock_root: &Path) -> Result<PathBuf, String> {
    let root = lock_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", lock_root.display()))?;
    let digest = Sha256::digest(root.as_os_str().as_bytes());
    let hash: String = digest[..SOCKET_HASH_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let path = socket_dir.join(format!("control-{hash}.sock"));
    if path.as_os_str().len() > SOCKET_PATH_LIMIT {
        return Err(format!(
            "the control socket path {} is longer than the {SOCKET_PATH_LIMIT} bytes a Unix \
             socket allows; set XDG_RUNTIME_DIR to a shorter directory",
            path.display()
        ));
    }
    Ok(path)
}

pub(crate) fn listen(socket: &Path) -> Result<UnixListener, String> {
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot remove the stale control socket {}: {error}",
                socket.display()
            ))
        }
    }
    let listener = UnixListener::bind(socket)
        .map_err(|error| format!("cannot listen on {}: {error}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(SOCKET_MODE))
        .map_err(|error| format!("cannot make {} private: {error}", socket.display()))?;
    Ok(listener)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Request {
    Show {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<Token>,
    },
    Open {
        link: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<Token>,
    },
    Quit,
}

impl Request {
    pub(crate) fn open(target: &LaunchTarget, token: Option<Token>) -> Self {
        Self::Open {
            link: target.android_uri(),
            token,
        }
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Show { .. } => "Show",
            Self::Open { .. } => "Open",
            Self::Quit => "Quit",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Accepted,
    Playing,
    Ended,
    Refused { reason: String },
}

#[derive(Debug)]
enum SlotState {
    Starting(Option<LaunchTarget>),
    Playing,
    Ended,
    Closing,
}

#[derive(Debug)]
pub(crate) struct LaunchSlot(Mutex<SlotState>);

impl LaunchSlot {
    pub(crate) fn new(target: Option<LaunchTarget>) -> Self {
        Self(Mutex::new(SlotState::Starting(target)))
    }

    fn state(&self) -> MutexGuard<'_, SlotState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn begin_play(&self) -> Option<LaunchTarget> {
        let mut state = self.state();
        let SlotState::Starting(target) = &mut *state else {
            return None;
        };
        let target = target.take();
        *state = SlotState::Playing;
        target
    }

    pub(crate) fn end(&self) {
        let mut state = self.state();
        if !matches!(*state, SlotState::Closing) {
            *state = SlotState::Ended;
        }
    }

    pub(crate) fn closing(&self) -> bool {
        matches!(*self.state(), SlotState::Closing)
    }

    fn running(&self) -> bool {
        matches!(*self.state(), SlotState::Starting(_) | SlotState::Playing)
    }

    fn retarget(&self, target: LaunchTarget, experience: Experience) -> Reply {
        match &mut *self.state() {
            SlotState::Starting(slot) => {
                *slot = Some(target);
                Reply::Accepted
            }
            SlotState::Playing => match experience {
                Experience::Joined => Reply::Playing,
                Experience::NotJoined => Reply::Ended,
            },
            SlotState::Ended | SlotState::Closing => Reply::Ended,
        }
    }

    fn close(&self) {
        *self.state() = SlotState::Closing;
    }
}

pub(crate) fn serve_in_background(
    listener: UnixListener,
    slot: Arc<LaunchSlot>,
    window: WindowControl,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("eclipse-control".to_owned())
        .spawn(move || {
            serve(
                &listener,
                &slot,
                |command| window.send(command),
                eclipse::session::experience,
            )
        })
        .map(drop)
}

fn serve(
    listener: &UnixListener,
    slot: &LaunchSlot,
    window: impl Fn(WindowCommand) -> Result<(), WindowGone>,
    experience: impl Fn() -> Experience,
) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                tracing::error!(%error, "Eclipse stopped taking launches from other Eclipse processes");
                return;
            }
        };
        if let Err(error) = serve_connection(stream, slot, &window, &experience) {
            tracing::debug!(%error, "a control connection ended without an answer");
        }
    }
}

fn peer_allowed(peer: Uid, own: Uid) -> bool {
    peer == own
}

fn peer_uid(stream: &UnixStream) -> io::Result<Uid> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let expected = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let mut length = expected;
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut length,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    if length != expected {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("SO_PEERCRED gave {length} bytes, not {expected}"),
        ));
    }
    Ok(Uid::from_raw(cred.uid))
}

fn serve_connection(
    mut stream: UnixStream,
    slot: &LaunchSlot,
    window: &impl Fn(WindowCommand) -> Result<(), WindowGone>,
    experience: &impl Fn() -> Experience,
) -> io::Result<()> {
    let peer = peer_uid(&stream)?;
    if !peer_allowed(peer, rustix::process::geteuid()) {
        tracing::warn!(
            uid = peer.as_raw(),
            "refused a control connection from another user"
        );
        return Ok(());
    }
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    let message = read_within(
        &mut stream,
        MESSAGE_LIMIT + 2,
        Instant::now() + REQUEST_TIMEOUT,
    )?;
    let reply = match parse_request(&message) {
        Ok(request) => answer(request, slot, window, experience),
        Err(reason) => {
            tracing::warn!(%reason, "refused a request from another Eclipse launch");
            Some(Reply::Refused { reason })
        }
    };
    match reply {
        Some(reply) => stream.write_all(&serde_json::to_vec(&reply)?),
        None => Ok(()),
    }
}

fn read_within(stream: &mut UnixStream, limit: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut message = Vec::new();
    let mut chunk = [0; 1024];
    while message.len() < limit {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(ErrorKind::TimedOut)?;
        stream.set_read_timeout(Some(left))?;
        let wanted = chunk.len().min(limit - message.len());
        match stream.read(&mut chunk[..wanted])? {
            0 => break,
            read => message.extend_from_slice(&chunk[..read]),
        }
    }
    Ok(message)
}

fn parse_request(message: &[u8]) -> Result<Request, String> {
    let Some((&version, json)) = message.split_first() else {
        return Err("the request is empty".to_owned());
    };
    if version != PROTOCOL_VERSION {
        return Err(format!(
            "control protocol version {version} is not supported; this Eclipse speaks version \
             {PROTOCOL_VERSION}"
        ));
    }
    if json.len() > MESSAGE_LIMIT {
        return Err(format!(
            "the request is larger than {} KiB",
            MESSAGE_LIMIT / 1024
        ));
    }
    serde_json::from_slice(json)
        .map_err(|_| "this Eclipse does not understand the request".to_owned())
}

fn answer(
    request: Request,
    slot: &LaunchSlot,
    window: &impl Fn(WindowCommand) -> Result<(), WindowGone>,
    experience: &impl Fn() -> Experience,
) -> Option<Reply> {
    match request {
        Request::Show { token } => {
            if !slot.running() {
                return Some(Reply::Ended);
            }
            tracing::info!("another Eclipse launch asked to bring Roblox to the front");
            raise_and_wait(window, token)
        }
        Request::Open { link, token } => {
            let target = match eclipse::links::parse(&link) {
                Ok(target) => target,
                Err(error) => {
                    return Some(Reply::Refused {
                        reason: error.to_string(),
                    })
                }
            };
            let joining = target.to_string();
            let reply = slot.retarget(target, experience());
            if reply == Reply::Accepted {
                tracing::info!(link = %joining, "another Eclipse launch handed its link to the starting Roblox");
                window(WindowCommand::Raise { token, done: None }).ok();
            }
            Some(reply)
        }
        Request::Quit => {
            tracing::info!("another Eclipse launch asked Roblox to close");
            slot.close();
            window(WindowCommand::Close).ok();
            Some(Reply::Accepted)
        }
    }
}

fn raise_and_wait(
    window: &impl Fn(WindowCommand) -> Result<(), WindowGone>,
    token: Option<Token>,
) -> Option<Reply> {
    let (done, raised) = mpsc::channel();
    let raise = WindowCommand::Raise {
        token,
        done: Some(done),
    };
    if window(raise).is_err() {
        return Some(Reply::Ended);
    }
    match raised.recv_timeout(RAISE_TIMEOUT) {
        Ok(()) => Some(Reply::Accepted),
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(Reply::Ended),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                "the window did not come to the front within {} s; the launch that asked \
                 reports Roblox as not responding",
                RAISE_TIMEOUT.as_secs()
            );
            None
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandOff {
    Boot,
    Delivered,
    InExperience,
}

pub(crate) fn hand_off(
    socket: &Path,
    runtime_dir: &Path,
    request: &Request,
    held: impl Fn(&Path) -> String,
) -> Result<HandOff, String> {
    let deadline = Instant::now() + LOCK_RETRY;
    let mut backoff = FIRST_BACKOFF;
    let reply = loop {
        if let Some(reply) = exchange(socket, request)? {
            break reply;
        }
        let lock = match lock_client(runtime_dir)? {
            ClientLock::Acquired(_) => return Ok(HandOff::Boot),
            ClientLock::Held(lock) => lock,
        };
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Err(held(&lock));
        };
        std::thread::sleep(backoff.min(left));
        backoff = (backoff * 2).min(LONGEST_BACKOFF);
    };
    match (reply, request) {
        (Reply::Accepted, _) => Ok(HandOff::Delivered),
        (Reply::Ended, _) => close_running(socket, runtime_dir).map(|()| HandOff::Boot),
        (Reply::Playing, Request::Open { .. }) => Ok(HandOff::InExperience),
        (Reply::Playing, Request::Show { .. } | Request::Quit) => Err(UNEXPECTED_ANSWER.to_owned()),
        (Reply::Refused { reason }, _) => Err(refused(&reason)),
    }
}

pub(crate) fn stay(socket: &Path, token: Option<Token>) -> Result<(), String> {
    match exchange(socket, &Request::Show { token })? {
        None | Some(Reply::Accepted | Reply::Ended) => Ok(()),
        Some(Reply::Playing) => Err(UNEXPECTED_ANSWER.to_owned()),
        Some(Reply::Refused { reason }) => Err(refused(&reason)),
    }
}

pub(crate) fn close_running(socket: &Path, runtime_dir: &Path) -> Result<(), String> {
    if let Some(Reply::Refused { reason }) = exchange(socket, &Request::Quit)? {
        return Err(refused(&reason));
    }
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    loop {
        if let ClientLock::Acquired(_) = lock_client(runtime_dir)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Roblox in Eclipse did not close within {} s; close it and try again",
                CLOSE_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(CLOSE_POLL);
    }
}

fn refused(reason: &str) -> String {
    format!("Roblox in Eclipse refused this launch: {reason}")
}

fn exchange(socket: &Path, request: &Request) -> Result<Option<Reply>, String> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None)
        }
        Err(error) => {
            return Err(format!(
                "cannot reach the running Eclipse through {}: {error}",
                socket.display()
            ))
        }
    };
    let reply = send_request(&mut stream, request).map_err(|_| NO_ANSWER.to_owned())?;
    serde_json::from_slice(&reply)
        .map(Some)
        .map_err(|_| UNEXPECTED_ANSWER.to_owned())
}

fn send_request(stream: &mut UnixStream, request: &Request) -> io::Result<Vec<u8>> {
    let mut message = vec![PROTOCOL_VERSION];
    serde_json::to_writer(&mut message, request)?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    stream.write_all(&message)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let reply = read_within(stream, MESSAGE_LIMIT, Instant::now() + ANSWER_TIMEOUT)?;
    if reply.is_empty() {
        return Err(ErrorKind::UnexpectedEof.into());
    }
    Ok(reply)
}

#[cfg(test)]
const SHORT_TEST_PARENT: &str = "/tmp";

#[cfg(test)]
pub(crate) fn socket_test_root(tag: &str) -> PathBuf {
    let root = Path::new(SHORT_TEST_PARENT).join(format!("ec-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::DirBuilder::new()
        .mode(PRIVATE_DIR_MODE)
        .create(&root)
        .unwrap_or_else(|error| panic!("cannot create {}: {error}", root.display()));
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;
    use std::net::Shutdown;
    use std::process::{Child, Command, Stdio};

    const CONTROL_CHILD: &str = "ECLIPSE_TEST_CONTROL_CHILD";
    const SOCKET: &str = "c.sock";

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().mode() & 0o777
    }

    fn target(link: &str) -> LaunchTarget {
        eclipse::links::parse(link).unwrap()
    }

    fn not_held(lock: &Path) -> String {
        panic!("the lock {} is free", lock.display())
    }

    fn already_running(lock: &Path) -> String {
        format!("already running ({})", lock.display())
    }

    fn once_spawned_children_let_go<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let value = attempt();
            if value.is_some() || Instant::now() >= deadline {
                return value;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn acquired(runtime_dir: &Path) -> File {
        once_spawned_children_let_go(|| match lock_client(runtime_dir).unwrap() {
            ClientLock::Acquired(lock) => Some(lock),
            ClientLock::Held(_) => None,
        })
        .unwrap_or_else(|| panic!("{} stays held", runtime_dir.display()))
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Raise(Option<Token>),
        Close,
    }

    fn token(text: &str) -> Token {
        Token::parse(text.to_owned()).unwrap()
    }

    fn serve_fake(
        socket: &Path,
        slot: Arc<LaunchSlot>,
        mut lock: Option<File>,
        experience: Experience,
    ) -> mpsc::Receiver<Seen> {
        let listener = listen(socket).unwrap();
        let (window, commands) = mpsc::channel();
        let (seen, handled) = mpsc::channel();
        std::thread::spawn(move || {
            for command in commands {
                match command {
                    WindowCommand::Raise { token, done } => {
                        if let Some(done) = done {
                            done.send(()).ok();
                        }
                        seen.send(Seen::Raise(token)).ok();
                    }
                    WindowCommand::Close => {
                        lock.take();
                        seen.send(Seen::Close).ok();
                    }
                }
            }
        });
        std::thread::spawn(move || {
            serve(
                &listener,
                &slot,
                |command: WindowCommand| window.send(command).map_err(|_| WindowGone),
                || experience,
            )
        });
        handled
    }

    fn raw_exchange(socket: &Path, request: &[u8]) -> Reply {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.write_all(request).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        serde_json::from_slice(&reply).unwrap()
    }

    fn message(json: &str) -> Vec<u8> {
        let mut request = vec![PROTOCOL_VERSION];
        request.extend_from_slice(json.as_bytes());
        request
    }

    fn padded(json: &str, len: usize) -> Vec<u8> {
        let mut request = message(json);
        request.resize(len + 1, b' ');
        request
    }

    #[test]
    fn the_socket_is_named_after_the_canonical_lock_root() {
        assert_eq!(
            socket_path(Path::new("/run/user/1000/eclipse"), Path::new("/")).unwrap(),
            Path::new("/run/user/1000/eclipse/control-8a5edab282632443.sock")
        );
        let root = socket_test_root("name");
        let (first, second) = (root.join("first"), root.join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let socket = socket_path(&root, &first).unwrap();
        assert_eq!(
            socket,
            socket_path(&root, &second.join("..").join("first")).unwrap()
        );
        assert_ne!(socket, socket_path(&root, &second).unwrap());

        let long = root.join("x".repeat(SOCKET_PATH_LIMIT));
        let error = socket_path(&long, &first).unwrap_err();
        assert!(
            error.contains(&long.display().to_string()) && error.contains("107 bytes"),
            "{error}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_socket_directory_is_private_to_its_user() {
        let root = socket_test_root("dir");
        let host = socket_dir(Some(root.as_os_str()), None).unwrap();
        assert_eq!(host, root.join(HOST_SOCKET_DIR));
        assert_eq!(mode(&host), 0o700);
        assert_eq!(socket_dir(Some(root.as_os_str()), None).unwrap(), host);

        let app = root.join("app").join("io.github.kuenec.Eclipse");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&app)
            .unwrap();
        assert_eq!(
            socket_dir(Some(root.as_os_str()), Some("io.github.kuenec.Eclipse")).unwrap(),
            app
        );
        let shared = socket_test_root("shared");
        let open = shared.join(HOST_SOCKET_DIR);
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = socket_dir(Some(shared.as_os_str()), None).unwrap_err();
        assert!(
            error.contains(&open.display().to_string()) && error.contains("mode 0700"),
            "{error}"
        );
        std::fs::remove_dir(&open).unwrap();
        std::os::unix::fs::symlink(&host, &open).unwrap();
        assert!(socket_dir(Some(shared.as_os_str()), None).is_err());

        for runtime_dir in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("run/user/1000")),
        ] {
            assert_eq!(socket_dir(runtime_dir, None).unwrap_err(), NO_RUNTIME_DIR);
        }
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&shared).ok();
    }

    #[test]
    fn a_missing_flatpak_runtime_directory_is_created_private() {
        let root = socket_test_root("app");
        let apps = root.join("app");
        let app = apps.join("io.github.kuenec.Eclipse");
        let dir = socket_dir(Some(root.as_os_str()), Some("io.github.kuenec.Eclipse"));
        let modes = [mode(&apps), mode(&app)];
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(dir.unwrap(), app);
        assert_eq!(modes, [0o700, 0o700]);
    }

    #[test]
    fn the_peer_uid_is_read_from_the_connected_socket() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&ours).unwrap(), rustix::process::geteuid());
        drop(theirs);
    }

    #[test]
    fn only_the_same_user_may_send_requests() {
        let own = Uid::from_raw(1000);
        assert!(peer_allowed(own, own));
        assert!(!peer_allowed(Uid::from_raw(1001), own));
        assert!(!peer_allowed(Uid::ROOT, own));
    }

    #[test]
    fn requests_and_replies_have_a_fixed_wire_form() {
        for (request, json) in [
            (Request::Show { token: None }, r#"{"show":{}}"#),
            (
                Request::Show {
                    token: Some(token("t-1")),
                },
                r#"{"show":{"token":"t-1"}}"#,
            ),
            (
                Request::open(&target("1818"), None),
                r#"{"open":{"link":"roblox://placeId=1818"}}"#,
            ),
            (
                Request::open(&target("1818"), Some(token("t-2"))),
                r#"{"open":{"link":"roblox://placeId=1818","token":"t-2"}}"#,
            ),
            (Request::Quit, r#""quit""#),
        ] {
            assert_eq!(serde_json::to_string(&request).unwrap(), json);
            assert_eq!(serde_json::from_str::<Request>(json).unwrap(), request);
        }
        for (reply, json) in [
            (Reply::Accepted, r#""accepted""#),
            (Reply::Playing, r#""playing""#),
            (Reply::Ended, r#""ended""#),
            (
                Reply::Refused {
                    reason: "no".to_owned(),
                },
                r#"{"refused":{"reason":"no"}}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&reply).unwrap(), json);
            assert_eq!(serde_json::from_str::<Reply>(json).unwrap(), reply);
        }
        let access_code = "8f3c2a10-5b6d-4e7f-9a1b-2c3d4e5f6a7b";
        let private = Request::open(
            &target(&format!("roblox://placeId=1818&accessCode={access_code}")),
            None,
        );
        assert_eq!(format!("{private:?}"), "Open");
    }

    #[test]
    fn the_newest_target_written_while_starting_is_the_one_played() {
        let slot = LaunchSlot::new(Some(target("1")));
        assert!(slot.running());
        assert_eq!(
            slot.retarget(target("2"), Experience::NotJoined),
            Reply::Accepted
        );
        assert_eq!(
            slot.retarget(target("3"), Experience::NotJoined),
            Reply::Accepted
        );
        assert_eq!(slot.begin_play(), Some(target("3")));
        assert_eq!(
            slot.retarget(target("4"), Experience::Joined),
            Reply::Playing
        );
        assert_eq!(
            slot.retarget(target("4"), Experience::NotJoined),
            Reply::Ended,
            "outside an experience a link replaces the running Roblox without asking"
        );
        assert_eq!(slot.begin_play(), None);
        slot.end();
        assert!(!slot.running());
        assert_eq!(slot.retarget(target("5"), Experience::Joined), Reply::Ended);
        slot.close();
        slot.end();
        assert!(slot.closing());
        assert_eq!(slot.retarget(target("6"), Experience::Joined), Reply::Ended);
    }

    #[test]
    fn malformed_requests_are_refused_and_leave_the_slot_alone() {
        let root = socket_test_root("refuse");
        let socket = root.join(SOCKET);
        let slot = Arc::new(LaunchSlot::new(Some(target("1818"))));
        let handled = serve_fake(&socket, Arc::clone(&slot), None, Experience::NotJoined);

        let duplicate = "roblox://placeId=1&placeId=2";
        let secret = "roblox://placeId=1818&accessCode=SECRET-0042";
        let link_reason = |link: &str| eclipse::links::parse(link).unwrap_err().to_string();
        for (request, reason) in [
            (b"".to_vec(), "the request is empty".to_owned()),
            (
                [&[2], br#"{"show":{}}"#.as_slice()].concat(),
                "control protocol version 2 is not supported; this Eclipse speaks version 1"
                    .to_owned(),
            ),
            (
                padded(r#"{"show":{}}"#, MESSAGE_LIMIT + 1),
                "the request is larger than 4 KiB".to_owned(),
            ),
            (
                message(r#"{"show":"#),
                "this Eclipse does not understand the request".to_owned(),
            ),
            (
                message(r#"{"open":{"link":1818}}"#),
                "this Eclipse does not understand the request".to_owned(),
            ),
            (
                message(&format!(r#"{{"open":{{"link":"{duplicate}"}}}}"#)),
                link_reason(duplicate),
            ),
            (
                message(&format!(r#"{{"open":{{"link":"{secret}"}}}}"#)),
                link_reason(secret),
            ),
            (
                message(r#"{"show":{"token":"two words"}}"#),
                "this Eclipse does not understand the request".to_owned(),
            ),
            (
                message(r#"{"open":{"link":"roblox://placeId=1","token":""}}"#),
                "this Eclipse does not understand the request".to_owned(),
            ),
        ] {
            assert_eq!(raw_exchange(&socket, &request), Reply::Refused { reason });
        }
        assert!(!link_reason(secret).contains("SECRET"));
        assert_eq!(
            hand_off(
                &socket,
                &root,
                &Request::Open {
                    link: duplicate.to_owned(),
                    token: None,
                },
                not_held
            ),
            Err(refused(&link_reason(duplicate)))
        );
        assert!(handled.try_recv().is_err(), "nothing reached the window");

        assert_eq!(
            raw_exchange(&socket, &padded(r#"{"show":{}}"#, MESSAGE_LIMIT)),
            Reply::Accepted
        );
        assert_eq!(handled.recv().unwrap(), Seen::Raise(None));
        assert_eq!(slot.begin_play(), Some(target("1818")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_silent_client_is_dropped_after_two_seconds_and_the_next_is_served() {
        let root = socket_test_root("silent");
        let socket = root.join(SOCKET);
        serve_fake(
            &socket,
            Arc::new(LaunchSlot::new(None)),
            None,
            Experience::NotJoined,
        );

        let _silent = UnixStream::connect(&socket).unwrap();
        let started = Instant::now();
        let reply = raw_exchange(&socket, &message(r#"{"show":{}}"#));
        let waited = started.elapsed();
        assert_eq!(reply, Reply::Accepted);
        assert!(
            waited >= Duration::from_millis(1900) && waited < Duration::from_secs(4),
            "{waited:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_request_closed_without_an_answer_names_no_wait() {
        let root = socket_test_root("mute");
        let socket = root.join(SOCKET);
        let listener = listen(&socket).unwrap();
        let mute = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
        });

        let started = Instant::now();
        let error = exchange(&socket, &Request::Show { token: None });
        let waited = started.elapsed();
        mute.join().unwrap();
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(error, Err(NO_ANSWER.to_owned()));
        assert!(waited < REQUEST_TIMEOUT, "{waited:?}");
    }

    #[test]
    fn a_stale_socket_with_a_free_lock_boots_and_the_next_holder_rebinds() {
        let root = socket_test_root("stale");
        let (socket, runtime) = (root.join(SOCKET), root.join("runtime"));
        drop(listen(&socket).unwrap());
        assert!(socket.exists());
        assert!(
            once_spawned_children_let_go(|| UnixStream::connect(&socket).err()).is_some(),
            "the stale socket refuses connections"
        );

        let started = Instant::now();
        assert_eq!(
            hand_off(&socket, &runtime, &Request::Show { token: None }, not_held),
            Ok(HandOff::Boot)
        );
        assert!(started.elapsed() < FIRST_BACKOFF * 4);

        let _lock = acquired(&runtime);
        let listener = listen(&socket).unwrap();
        assert_eq!(mode(&socket), 0o600);
        let _client = UnixStream::connect(&socket).unwrap();
        listener.accept().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_held_lock_without_a_socket_gives_up_after_two_seconds() {
        let root = socket_test_root("held");
        let (socket, runtime) = (root.join(SOCKET), root.join("runtime"));
        let _running = acquired(&runtime);

        let started = Instant::now();
        let error = hand_off(
            &socket,
            &runtime,
            &Request::Show { token: None },
            already_running,
        )
        .unwrap_err();
        let waited = started.elapsed();
        assert_eq!(error, already_running(&runtime.join(CLIENT_LOCK_FILE)));
        assert!(
            (1800..=2200).contains(&waited.as_millis()),
            "gave up after {waited:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_client_that_already_ended_is_closed_and_the_launch_boots() {
        let root = socket_test_root("ended");
        let (socket, runtime) = (root.join(SOCKET), root.join("runtime"));
        let slot = Arc::new(LaunchSlot::new(None));
        slot.end();
        let handled = serve_fake(
            &socket,
            slot,
            Some(acquired(&runtime)),
            Experience::NotJoined,
        );

        assert_eq!(
            hand_off(
                &socket,
                &runtime,
                &Request::open(&target("1818"), None),
                not_held
            ),
            Ok(HandOff::Boot)
        );
        assert_eq!(handled.recv().unwrap(), Seen::Close);
        assert!(handled.try_recv().is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_launch_token_reaches_the_window_it_raises() {
        let root = socket_test_root("token");
        let socket = root.join(SOCKET);
        let slot = Arc::new(LaunchSlot::new(None));
        let handled = serve_fake(&socket, Arc::clone(&slot), None, Experience::NotJoined);

        assert_eq!(
            hand_off(
                &socket,
                &root,
                &Request::open(&target("1818"), Some(token("open-token"))),
                not_held
            ),
            Ok(HandOff::Delivered)
        );
        assert_eq!(
            handled.recv().unwrap(),
            Seen::Raise(Some(token("open-token")))
        );
        let show = Request::Show {
            token: Some(token("show-token")),
        };
        assert_eq!(
            hand_off(&socket, &root, &show, not_held),
            Ok(HandOff::Delivered)
        );
        assert_eq!(
            handled.recv().unwrap(),
            Seen::Raise(Some(token("show-token")))
        );
        assert_eq!(slot.begin_play(), Some(target("1818")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_link_while_roblox_is_outside_an_experience_replaces_it_without_asking() {
        let root = socket_test_root("outside");
        let (socket, runtime) = (root.join(SOCKET), root.join("runtime"));
        let slot = Arc::new(LaunchSlot::new(None));
        slot.begin_play();
        let handled = serve_fake(
            &socket,
            Arc::clone(&slot),
            Some(acquired(&runtime)),
            Experience::NotJoined,
        );

        assert_eq!(
            hand_off(
                &socket,
                &runtime,
                &Request::open(&target("1818"), None),
                not_held
            ),
            Ok(HandOff::Boot)
        );
        assert_eq!(handled.recv().unwrap(), Seen::Close);
        assert!(slot.closing());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_link_during_an_experience_asks_and_staying_raises_with_the_prompt_token() {
        let root = socket_test_root("stay");
        let socket = root.join(SOCKET);
        let slot = Arc::new(LaunchSlot::new(None));
        slot.begin_play();
        let handled = serve_fake(&socket, slot, None, Experience::Joined);

        assert_eq!(
            hand_off(
                &socket,
                &root,
                &Request::open(&target("1818"), Some(token("launch-token"))),
                not_held
            ),
            Ok(HandOff::InExperience)
        );
        assert!(
            handled.try_recv().is_err(),
            "the running experience is left alone until the user answers"
        );
        assert_eq!(stay(&socket, Some(token("prompt-token"))), Ok(()));
        assert_eq!(
            handled.recv().unwrap(),
            Seen::Raise(Some(token("prompt-token")))
        );
        assert_eq!(
            stay(&root.join("gone.sock"), None),
            Ok(()),
            "staying in a client that already exited does nothing"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn only_an_open_request_may_be_answered_with_an_experience_in_progress() {
        let root = socket_test_root("playing");
        let socket = root.join(SOCKET);
        let listener = listen(&socket).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                stream.read_to_end(&mut request).unwrap();
                stream.write_all(br#""playing""#).unwrap();
            }
        });

        assert_eq!(
            hand_off(&socket, &root, &Request::Show { token: None }, not_held),
            Err(UNEXPECTED_ANSWER.to_owned())
        );
        assert_eq!(stay(&socket, None), Err(UNEXPECTED_ANSWER.to_owned()));
        server.join().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn only_one_launch_asks_whether_to_leave_at_a_time() {
        let root = socket_test_root("prompt");
        let socket = root.join("control-0123456789abcdef.sock");

        let asking = lock_prompt(&socket).unwrap();
        assert!(asking.is_some());
        assert!(root.join("control-0123456789abcdef.prompt").exists());
        assert!(lock_prompt(&socket).unwrap().is_none());
        drop(asking);
        assert!(once_spawned_children_let_go(|| lock_prompt(&socket).unwrap()).is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    struct ControlChild(Child);

    impl Drop for ControlChild {
        fn drop(&mut self) {
            self.0.kill().ok();
            self.0.wait().ok();
        }
    }

    fn expect_line(lines: &mpsc::Receiver<String>, wanted: &str) {
        loop {
            match lines.recv_timeout(Duration::from_secs(20)) {
                Ok(line) if line == wanted => return,
                Ok(_) => {}
                Err(error) => panic!("the control child never printed {wanted:?}: {error}"),
            }
        }
    }

    fn run_control_child(root: &Path) -> ! {
        let _lock = acquired(&root.join("runtime"));
        let listener = listen(&root.join(SOCKET)).unwrap();
        let slot = Arc::new(LaunchSlot::new(None));
        let (window, commands) = mpsc::channel();
        let served = Arc::clone(&slot);
        std::thread::spawn(move || {
            serve(
                &listener,
                &served,
                |command: WindowCommand| window.send(command).map_err(|_| WindowGone),
                || Experience::Joined,
            )
        });
        println!("listening");
        let mut raises = 0;
        for command in commands {
            match command {
                WindowCommand::Raise { done, .. } => {
                    raises += 1;
                    if let Some(done) = done {
                        done.send(()).ok();
                    }
                    if raises == 2 {
                        let played = slot.begin_play().map(|target| target.android_uri());
                        println!("playing {}", played.unwrap_or_default());
                    }
                }
                WindowCommand::Close => std::process::exit(0),
            }
        }
        std::process::exit(1)
    }

    #[test]
    fn a_running_client_takes_links_until_it_plays_and_exits_when_asked() {
        if let Some(root) = std::env::var_os(CONTROL_CHILD) {
            run_control_child(Path::new(&root));
        }
        let root = socket_test_root("child");
        let (socket, runtime) = (root.join(SOCKET), root.join("runtime"));
        let mut child = ControlChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "instance_control::tests::a_running_client_takes_links_until_it_plays_and_exits_when_asked",
                    "--nocapture",
                ])
                .env(CONTROL_CHILD, &root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (line, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for text in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                line.send(text).ok();
            }
        });
        expect_line(&lines, "listening");

        for place in ["1", "2"] {
            assert_eq!(
                hand_off(
                    &socket,
                    &runtime,
                    &Request::open(&target(place), None),
                    not_held
                ),
                Ok(HandOff::Delivered)
            );
        }
        expect_line(&lines, "playing roblox://placeId=2");
        assert_eq!(
            hand_off(
                &socket,
                &runtime,
                &Request::open(&target("3"), None),
                not_held
            ),
            Ok(HandOff::InExperience)
        );
        assert_eq!(
            hand_off(&socket, &runtime, &Request::Show { token: None }, not_held),
            Ok(HandOff::Delivered)
        );
        assert_eq!(close_running(&socket, &runtime), Ok(()));
        let status = child.0.wait().unwrap();
        assert!(status.success(), "the child exited with {status}");
        std::fs::remove_dir_all(&root).ok();
    }
}
