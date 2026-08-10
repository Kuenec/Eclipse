
#[allow(dead_code)]
#[path = "../shared.rs"]
mod shared;

use shared::proto::{self, ConsumerMsg, HelperMsg};
use shared::{fdpass, shm};
use std::collections::HashSet;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitCode};
use std::time::{Duration, Instant};

const VIEW: i64 = 1;
const WIDTH: u16 = 1024;
const HEIGHT: u16 = 768;
const TARGET_URL: &str = "https://www.roblox.com";

const TARGET_FOR_LOG: &str = "https://www.roblox.com";

const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
const LOAD_START_DEADLINE: Duration = Duration::from_secs(30);
const LOAD_FINISH_DEADLINE: Duration = Duration::from_secs(90);

const INK_DEADLINE: Duration = Duration::from_secs(20);
const COOKIE_DEADLINE: Duration = Duration::from_secs(10);
const CLOSE_DEADLINE: Duration = Duration::from_secs(15);
const EXIT_DEADLINE: Duration = Duration::from_secs(15);

fn now_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

struct Buffer {
    mapping: shm::FrameMapping,
    generation: u32,
    slot_bytes: u32,
}

struct Drive {
    stream: UnixStream,
    child: Child,
    helper_path: std::path::PathBuf,
    start: Instant,
    buffer: Option<Buffer>,
    consoles_seen: u32,
}

enum DriveError {
    Fail(String),
}

impl From<String> for DriveError {
    fn from(s: String) -> Self {
        DriveError::Fail(s)
    }
}

type DResult<T> = Result<T, DriveError>;

fn fail<T>(reason: impl Into<String>) -> DResult<T> {
    Err(DriveError::Fail(reason.into()))
}

fn main() -> ExitCode {
    match run() {
        Ok(summary) => {
            println!("ECLIPSE_WEBVIEW_M2_DRIVE_SUCCESS {summary}");
            ExitCode::SUCCESS
        }
        Err(DriveError::Fail(reason)) => {
            println!(
                "ECLIPSE_WEBVIEW_M2_DRIVE_FAILURE reason={}",
                reason.replace(' ', "_")
            );
            ExitCode::FAILURE
        }
    }
}

fn resolve_helper() -> Result<std::path::PathBuf, String> {
    if let Ok(explicit) = std::env::var("ECLIPSE_WEBVIEW_HELPER") {
        let p = std::path::PathBuf::from(explicit);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "ECLIPSE_WEBVIEW_HELPER points at a missing file: {}",
            p.display()
        ));
    }
    let me = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let sibling = me.with_file_name("eclipse-webview");
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err(format!(
        "helper binary not found: set ECLIPSE_WEBVIEW_HELPER or build the sibling {}",
        sibling.display()
    ))
}

struct TempProfile {
    root: std::path::PathBuf,
}

impl TempProfile {
    fn create() -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt as _;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("system clock before Unix epoch: {e}"))?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "eclipse-webview-drive-profile-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root)
            .map_err(|e| format!("create temporary CEF profile failed: {e}"))?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("restrict temporary CEF profile failed: {e}"))?;
        Ok(Self { root })
    }
}

impl Drop for TempProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn spawn_helper(helper: &std::path::Path) -> Result<(UnixStream, Child, TempProfile), String> {
    let (parent_end, child_end) =
        UnixStream::pair().map_err(|e| format!("socketpair failed: {e}"))?;
    let mut cmd = Command::new(helper);
    cmd.arg("--ipc-fd=3");
    let profile = TempProfile::create()?;
    cmd.env("ECLIPSE_WEBVIEW_DATA_DIR", &profile.root);

    if let Some(ozone) = std::env::args().find(|a| a.starts_with("--ozone-platform=")) {
        cmd.arg(ozone);
    }

    if std::env::args().any(|a| a == "--allow-unsandboxed") {
        cmd.arg("--allow-unsandboxed");
    }
    let child_fd = child_end
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| e.to_string())?;

    unsafe {
        use std::os::fd::AsRawFd;
        let raw = child_fd.as_raw_fd();
        cmd.pre_exec(move || {
            if raw == 3 {
                if libc::fcntl(3, libc::F_SETFD, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(raw, 3) != 3 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("spawn {} failed: {e}", helper.display()))?;
    drop(child_fd);
    drop(child_end);
    Ok((parent_end, child, profile))
}

impl Drive {
    fn send(&self, msg: &ConsumerMsg) -> DResult<()> {
        let bytes = msg.encode().map_err(|e| format!("encode failed: {e}"))?;
        (&mut &self.stream)
            .write_all(&bytes)
            .map_err(|e| format!("socket write failed: {e}"))?;
        Ok(())
    }

    fn next_msg(&mut self, deadline: Instant) -> DResult<HelperMsg> {
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| DriveError::Fail("timeout waiting for helper message".into()))?;
            self.stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                .map_err(|e| DriveError::Fail(format!("set_read_timeout: {e}")))?;
            let msg = match proto::read_helper_msg(&mut &self.stream) {
                Ok(m) => m,
                Err(proto::ProtoError::Io(kind))
                    if kind == std::io::ErrorKind::WouldBlock
                        || kind == std::io::ErrorKind::TimedOut =>
                {
                    return fail("timeout waiting for helper message");
                }
                Err(e) => return fail(format!("protocol error from helper: {e}")),
            };
            match msg {
                HelperMsg::Console { view, console } => {
                    self.consoles_seen += 1;
                    println!(
                        "[{} ms] console view={view} severity={} source={} line={} len={}",
                        now_ms(self.start),
                        console.severity(),
                        console.source(),
                        console.line(),
                        console.message_len()
                    );
                }
                HelperMsg::Crash { view, kind, code } => {
                    return fail(format!("helper crash view={view} kind={kind} code={code}"));
                }
                HelperMsg::FrameBufferNew {
                    view,
                    generation,
                    width,
                    height,
                    stride,
                    slot_bytes,
                    slot_count,
                } => {
                    println!(
                        "[{} ms] frame-buffer-new view={view} generation={generation} \
                         {width}x{height} stride={stride} slot_bytes={slot_bytes} slots={slot_count}",
                        now_ms(self.start)
                    );

                    let fd = fdpass::recv_fd_after_sentinel(&self.stream)
                        .map_err(|e| DriveError::Fail(format!("fd receive failed: {e}")))?;
                    let expected = slot_bytes as usize * usize::from(slot_count);

                    let mapping = shm::map_frame_buffer(fd.as_fd(), expected)
                        .map_err(|e| DriveError::Fail(format!("memfd map rejected: {e}")))?;
                    self.buffer = Some(Buffer {
                        mapping,
                        generation,
                        slot_bytes,
                    });
                }
                other => return Ok(other),
            }
        }
    }

    fn census(&self, generation: u32, slot: u8) -> DResult<usize> {
        let Some(buf) = self.buffer.as_ref() else {
            return fail("frame ready before any frame buffer");
        };
        if buf.generation != generation {
            return fail("census on a stale generation");
        }
        let offset = buf.slot_bytes as usize * usize::from(slot);
        let bytes = buf
            .mapping
            .slice(offset, buf.slot_bytes as usize)
            .ok_or_else(|| DriveError::Fail("slot out of mapping bounds".into()))?;
        let mut distinct: HashSet<u32> = HashSet::new();
        for px in bytes.chunks_exact(4) {
            distinct.insert(u32::from_ne_bytes([px[0], px[1], px[2], px[3]]));
        }
        Ok(distinct.len())
    }

    fn ack(&self, generation: u32, seq: u32) -> DResult<()> {
        self.send(&ConsumerMsg::FrameAck {
            view: VIEW,
            generation,
            seq,
        })
    }
}

fn run() -> DResult<String> {
    let start = Instant::now();
    let helper_path = resolve_helper().map_err(DriveError::Fail)?;
    println!("[0 ms] helper={}", helper_path.display());
    let (stream, child, _profile) = spawn_helper(&helper_path).map_err(DriveError::Fail)?;
    let mut drive = Drive {
        stream,
        child,
        helper_path,
        start,
        buffer: None,
        consoles_seen: 0,
    };

    let result = run_protocol(&mut drive);

    let exit_status = reap(&mut drive, result.is_ok())?;
    let orphans = orphan_scan(&drive.helper_path);

    let summary = result?;
    if exit_status != Some(0) {
        return fail(format!("helper exit status {exit_status:?} (expected 0)"));
    }
    if orphans > 0 {
        return fail(format!(
            "orphan scan found {orphans} surviving helper process(es)"
        ));
    }
    println!(
        "[{} ms] child reaped exit=0, orphan scan clean",
        now_ms(start)
    );
    Ok(format!("{summary} helper_exit=0 orphans=0"))
}

fn run_protocol(d: &mut Drive) -> DResult<String> {
    let start = d.start;

    d.send(&ConsumerMsg::Hello {
        version: shared::PROTO_VERSION,
    })?;
    let engine = match d.next_msg(Instant::now() + HANDSHAKE_DEADLINE)? {
        HelperMsg::HelloAck { version, engine } => {
            if !proto::hello_ack_version_supported(version) {
                return fail(format!(
                    "protocol version mismatch: helper v{version}, drive v{}",
                    shared::PROTO_VERSION
                ));
            }
            engine
        }
        other => return fail(format!("expected HelloAck, got {}", name_of(&other))),
    };
    println!("[{} ms] hello-ack engine={engine}", now_ms(start));

    d.send(&ConsumerMsg::CreateView {
        view: VIEW,
        width: WIDTH,
        height: HEIGHT,
    })?;

    d.send(&ConsumerMsg::LoadUrl {
        view: VIEW,
        url: TARGET_URL.to_string(),
    })?;
    println!(
        "[{} ms] load-url sent target={TARGET_FOR_LOG}",
        now_ms(start)
    );

    let mut load_started_ms: Option<u128> = None;
    let mut load_finished: Option<(u128, i32)> = None;
    let started_deadline = Instant::now() + LOAD_START_DEADLINE;
    let finished_deadline = Instant::now() + LOAD_FINISH_DEADLINE;
    while load_finished.is_none() {
        let deadline = if load_started_ms.is_none() {
            started_deadline
        } else {
            finished_deadline
        };
        match d.next_msg(deadline)? {
            HelperMsg::LoadState {
                view,
                state,
                http_status,
            } if view == VIEW => {
                println!(
                    "[{} ms] load-state state={state} http_status={http_status} target={TARGET_FOR_LOG}",
                    now_ms(start)
                );

                if state == 0 {
                    if load_started_ms.is_none() {
                        load_started_ms = Some(now_ms(start));
                    }
                } else if load_finished.is_none() {
                    load_finished = Some((now_ms(start), http_status));
                }
            }
            HelperMsg::FrameReady {
                view,
                generation,
                slot: _,
                seq,
            } if view == VIEW => {

                if d.buffer
                    .as_ref()
                    .is_some_and(|b| b.generation == generation)
                {
                    d.ack(generation, seq)?;
                }
            }
            other => println!(
                "[{} ms] (ignored while loading: {})",
                now_ms(start),
                name_of(&other)
            ),
        }
    }
    let Some(started_ms) = load_started_ms else {
        return fail("load-finished arrived without load-started");
    };
    let (finished_ms, http_status) = load_finished.unwrap_or((0, 0));

    let ink_deadline = Instant::now() + INK_DEADLINE;
    let census = loop {
        match d.next_msg(ink_deadline)? {
            HelperMsg::FrameReady {
                view,
                generation,
                slot,
                seq,
            } if view == VIEW => {
                let stale = d.buffer.as_ref().is_none_or(|b| b.generation != generation);
                if stale {
                    continue;
                }

                let count = d.census(generation, slot)?;
                d.ack(generation, seq)?;
                if count > 1 {
                    println!(
                        "[{} ms] frame-ready slot={slot} distinct_pixels={count}",
                        now_ms(start)
                    );
                    break count;
                }
                println!(
                    "[{} ms] frame-ready slot={slot} distinct_pixels={count} (settling)",
                    now_ms(start)
                );
            }
            other => println!(
                "[{} ms] (ignored while sampling ink: {})",
                now_ms(start),
                name_of(&other)
            ),
        }
    };

    d.send(&ConsumerMsg::MouseMove {
        view: VIEW,
        x: i32::from(WIDTH) / 2,
        y: i32::from(HEIGHT) / 2,
        modifiers: 0,
        leave: false,
    })?;
    for down in [true, false] {
        d.send(&ConsumerMsg::MouseClick {
            view: VIEW,
            x: i32::from(WIDTH) / 2,
            y: i32::from(HEIGHT) / 2,
            button: 0,
            down,
            click_count: 1,
            modifiers: 0,
        })?;
    }
    println!("[{} ms] input smoke sent (move + click)", now_ms(start));

    const COOKIE_REQ: u32 = 7;
    d.send(&ConsumerMsg::CookieGet {
        request_id: COOKIE_REQ,
        url: format!("{TARGET_URL}/"),
    })?;
    let cookie_count = loop {
        let deadline = Instant::now() + COOKIE_DEADLINE;
        match d.next_msg(deadline)? {
            HelperMsg::CookieList {
                request_id,
                cookies,
            } => {
                if request_id != COOKIE_REQ {
                    return fail(format!(
                        "cookie list correlation mismatch: got request_id={request_id}"
                    ));
                }

                for c in &cookies {
                    println!(
                        "[{} ms] cookie name={} domain={} secure={} http_only={}",
                        now_ms(start),
                        c.name,
                        c.domain,
                        c.secure,
                        c.http_only
                    );
                }
                break cookies.len();
            }
            HelperMsg::FrameReady {
                view,
                generation,
                slot: _,
                seq,
            } if view == VIEW => {
                if d.buffer
                    .as_ref()
                    .is_some_and(|b| b.generation == generation)
                {
                    d.ack(generation, seq)?;
                }
            }
            other => println!(
                "[{} ms] (ignored while awaiting cookies: {})",
                now_ms(start),
                name_of(&other)
            ),
        }
    };
    println!(
        "[{} ms] cookie round-trip complete count={cookie_count}",
        now_ms(start)
    );

    d.send(&ConsumerMsg::CloseView { view: VIEW })?;
    let close_deadline = Instant::now() + CLOSE_DEADLINE;
    loop {
        match d.next_msg(close_deadline)? {
            HelperMsg::ViewClosed { view } if view == VIEW => break,
            HelperMsg::FrameReady {
                view,
                generation,
                slot: _,
                seq,
            } if view == VIEW => {
                if d.buffer
                    .as_ref()
                    .is_some_and(|b| b.generation == generation)
                {
                    d.ack(generation, seq)?;
                }
            }
            other => println!(
                "[{} ms] (ignored while closing: {})",
                now_ms(start),
                name_of(&other)
            ),
        }
    }
    println!("[{} ms] view-closed", now_ms(start));

    d.send(&ConsumerMsg::Shutdown)?;

    Ok(format!(
        "target={TARGET_FOR_LOG} load_started_ms={started_ms} load_finished_ms={finished_ms} \
         http_status={http_status} distinct_pixels={census} cookies={cookie_count} consoles={}",
        d.consoles_seen
    ))
}

fn name_of(msg: &HelperMsg) -> &'static str {
    match msg {
        HelperMsg::HelloAck { .. } => "HelloAck",
        HelperMsg::LoadState { .. } => "LoadState",
        HelperMsg::FrameBufferNew { .. } => "FrameBufferNew",
        HelperMsg::FrameReady { .. } => "FrameReady",
        HelperMsg::Console { .. } => "Console",
        HelperMsg::Crash { .. } => "Crash",
        HelperMsg::CookieList { .. } => "CookieList",
        HelperMsg::ViewClosed { .. } => "ViewClosed",
        HelperMsg::BridgeCall { .. } => "BridgeCall",
        HelperMsg::EvaluateJsResult { .. } => "EvaluateJsResult",
        HelperMsg::CookieSetResult { .. } => "CookieSetResult",
        HelperMsg::CookieFlushDone { .. } => "CookieFlushDone",
        HelperMsg::CookiesClearDone { .. } => "CookiesClearDone",
    }
}

fn reap(d: &mut Drive, expect_clean: bool) -> DResult<Option<i32>> {
    let deadline = Instant::now() + EXIT_DEADLINE;
    if expect_clean {
        while Instant::now() < deadline {
            match d.child.try_wait() {
                Ok(Some(status)) => return Ok(status.code()),
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return fail(format!("wait failed: {e}")),
            }
        }
        println!(
            "[{} ms] WARN: helper did not exit within {EXIT_DEADLINE:?}; killing",
            now_ms(d.start)
        );
    }

    let _ = d.child.kill();
    match d.child.wait() {
        Ok(status) => Ok(status.code()),
        Err(e) => fail(format!("kill+wait failed: {e}")),
    }
}

fn orphan_scan(helper: &std::path::Path) -> usize {
    let helper = helper.to_string_lossy();
    let mut count = 0usize;

    std::thread::sleep(Duration::from_millis(500));
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv0 = cmdline.split(|b| *b == 0).next().unwrap_or(&[]);
        if argv0 == helper.as_bytes() {
            println!("orphan: pid={pid}");
            count += 1;
        }
    }
    count
}
