//! `eclipse-webview-drive` — the dev-host M2 verify driver AND the reference consumer
//! implementation of the spawn contract (`src/webview/mod.rs`) + the wire protocol
//! (negotiated at `shared::PROTO_VERSION` — v2 since the 2026-07-09 M4 additive extension).
//!
//! 2026-07-03: spawns the helper per the contract (socketpair end dup2'd to fd 3,
//! `--ipc-fd=3`, `PR_SET_PDEATHSIG(SIGTERM)`, no URL in argv), completes the handshake,
//! creates one 1024x768 view, loads https://www.roblox.com, observes `LoadState` 0→3 over
//! the socket, receives the `FrameBufferNew` + `SCM_RIGHTS` memfd, maps it via the shared
//! `shm` guard, runs a distinct-pixel census on a published frame (must be > 1),
//! sends a MouseMove/MouseClick smoke, round-trips `CookieGet`→`CookieList`,
//! `CloseView`→`ViewClosed`, `Shutdown`, reaps the child, and scans /proc for orphans —
//! then prints the deterministic `ECLIPSE_WEBVIEW_M2_DRIVE_SUCCESS` marker (or an honest
//! `ECLIPSE_WEBVIEW_M2_DRIVE_FAILURE reason=...`; no retries-until-green).
//!
//! Run with `LD_LIBRARY_PATH` including the libcef.so dir (the helper links it); a display
//! session (Wayland or X11) is required. NOT part of `cargo test`; NOT `cargo run -- run`.

// 2026-07-03: the shared src/webview/* surface is compiled per binary; the drive uses the
// consumer-role halves (decode HelperMsg, recv fd, map) — the helper-role halves are
// intentionally unused HERE and exercised by this crate's own `cargo test`.
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
/// Safe to log: scheme+host only (matches the redaction contract by construction).
const TARGET_FOR_LOG: &str = "https://www.roblox.com";

const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
const LOAD_START_DEADLINE: Duration = Duration::from_secs(30);
const LOAD_FINISH_DEADLINE: Duration = Duration::from_secs(90);
/// After load-finished: sample published frames until ink appears (fonts/images settle).
const INK_DEADLINE: Duration = Duration::from_secs(20);
const COOKIE_DEADLINE: Duration = Duration::from_secs(10);
const CLOSE_DEADLINE: Duration = Duration::from_secs(15);
const EXIT_DEADLINE: Duration = Duration::from_secs(15);

fn now_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

/// The mapped frame buffer of the CURRENT generation.
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

/// Resolve the helper binary: `$ECLIPSE_WEBVIEW_HELPER`, else the sibling `eclipse-webview`
/// next to this executable (the spawn-contract resolution rule; no hardcoded paths).
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

fn spawn_helper(helper: &std::path::Path) -> Result<(UnixStream, Child), String> {
    let (parent_end, child_end) =
        UnixStream::pair().map_err(|e| format!("socketpair failed: {e}"))?;
    let mut cmd = Command::new(helper);
    cmd.arg("--ipc-fd=3");
    // Forward an explicit ozone override if the drive was given one (contract flag).
    if let Some(ozone) = std::env::args().find(|a| a.starts_with("--ozone-platform=")) {
        cmd.arg(ozone);
    }
    // 2026-07-10 (plan M5): forward the sandbox-degradation opt-in (spawn contract §3 —
    // reference-consumer parity with the main-process client's config-gated flag).
    if std::env::args().any(|a| a == "--allow-unsandboxed") {
        cmd.arg("--allow-unsandboxed");
    }
    let child_fd = child_end
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| e.to_string())?;
    // SAFETY (pre_exec runs between fork and exec — async-signal-safe calls only):
    // - dup2 moves the socketpair end onto fd 3 (the spawn contract); dup2 does not copy
    //   FD_CLOEXEC, so fd 3 survives exec. If the fd already IS 3, F_SETFD clears CLOEXEC.
    // - prctl(PR_SET_PDEATHSIG, SIGTERM) is the orphan-prevention secondary layer; it fires
    //   when the spawning THREAD exits, so the drive spawns from its stable main thread.
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
    Ok((parent_end, child))
}

impl Drive {
    fn send(&self, msg: &ConsumerMsg) -> DResult<()> {
        let bytes = msg.encode().map_err(|e| format!("encode failed: {e}"))?;
        (&mut &self.stream)
            .write_all(&bytes)
            .map_err(|e| format!("socket write failed: {e}"))?;
        Ok(())
    }

    /// Read the next helper message with an absolute deadline. Handles the cross-cutting
    /// messages every phase must tolerate: `Console` (counted; text is structurally absent),
    /// `Crash` (fatal), `FrameBufferNew` (remap — receives the fd immediately, per the
    /// sentinel adjacency rule). Everything else is returned to the caller.
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
                    // The sentinel + SCM_RIGHTS memfd is the very next unread byte.
                    let fd = fdpass::recv_fd_after_sentinel(&self.stream)
                        .map_err(|e| DriveError::Fail(format!("fd receive failed: {e}")))?;
                    let expected = slot_bytes as usize * usize::from(slot_count);
                    // The previous generation's mapping (if any) is unmapped on replace —
                    // this reader is single-threaded, so no read can be in progress.
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

    /// Census a published slot: distinct BGRA pixel values.
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
    let (stream, child) = spawn_helper(&helper_path).map_err(DriveError::Fail)?;
    let mut drive = Drive {
        stream,
        child,
        helper_path,
        start,
        buffer: None,
        consoles_seen: 0,
    };

    let result = run_protocol(&mut drive);

    // Backstop: whatever happened, never leave the helper running.
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

    // ---- Handshake. ----
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

    // ---- CreateView (the FrameBufferNew + fd arrives via next_msg's handler). ----
    d.send(&ConsumerMsg::CreateView {
        view: VIEW,
        width: WIDTH,
        height: HEIGHT,
    })?;

    // ---- LoadUrl (the URL crosses the wire, never argv; logged scheme+host only). ----
    d.send(&ConsumerMsg::LoadUrl {
        view: VIEW,
        url: TARGET_URL.to_string(),
    })?;
    println!(
        "[{} ms] load-url sent target={TARGET_FOR_LOG}",
        now_ms(start)
    );

    // ---- LoadState 0 then 3; ack every FrameReady on the way. ----
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
                // Decode validated state ∈ {0, 3}.
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
                // Keep the publish/ack flow moving; census comes after load-finished.
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

    // ---- Frame ink: sample published frames until distinct pixels > 1. ----
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
                // Read the slot BETWEEN FrameReady and our FrameAck (the no-torn-frame
                // window), then release it.
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

    // ---- Input smoke: a MouseMove and a full left click at the view center. ----
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

    // ---- CookieGet → CookieList round-trip (correlated by request_id). ----
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
                // Names/domains only in the log — never cookie values.
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

    // ---- CloseView → ViewClosed. ----
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

    // ---- Shutdown (the reap happens in run()). ----
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
    }
}

/// Wait for the child to exit (after `Shutdown` on the success path); on the failure path or
/// a hung child, kill it. Returns the exit code if it exited by itself.
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
    // Failure path (or hung child): kill + reap — never leave the helper running.
    let _ = d.child.kill();
    match d.child.wait() {
        Ok(status) => Ok(status.code()),
        Err(e) => fail(format!("kill+wait failed: {e}")),
    }
}

/// Scan /proc for any surviving process whose argv[0] is the helper binary (CEF children
/// re-exec the same binary). The drive's own pid/path never matches (different basename).
fn orphan_scan(helper: &std::path::Path) -> usize {
    let helper = helper.to_string_lossy();
    let mut count = 0usize;
    // A freshly killed tree can take a moment to unwind: settle briefly, then scan once.
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
