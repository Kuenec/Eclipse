//! `eclipse-webview` — the out-of-process CEF helper for the Eclipse challenge WebView
//! (docs/web-engine-plan.md M2; spawned per the contract in the root crate's
//! `src/webview/mod.rs`).
//!
//! Process shape (the M1-spike-proven cefsimple pattern): `api_hash` → `Args` →
//! `execute_process` early-exit for CEF-forked subprocesses → strict `--ipc-fd` parse (no
//! fd-3 assumption without the flag) → protocol handshake FIRST with a 10 s watchdog (a
//! version mismatch or missing `Hello` exits before paying any engine-init cost) → explicit
//! ozone selection (never Chromium's auto — the M1 designed-failure rule) → `CefInitialize`
//! (windowless, external pump, sandbox ON, engine logging DISABLED) → a 10 ms
//! `do_message_loop_work` pump loop draining a std `mpsc` command channel.
//!
//! Threads: a READER (blocking exact-length framed reads; EOF/malformed → one loud
//! payload-free line → quit path) and a WRITER (owns the socket write half behind a bounded
//! `sync_channel`, so CEF callbacks never block on a stalled consumer). No tokio, no async
//! runtime — std only (AGENTS.md §2.5).
//!
//! Exit codes: 0 clean (`Shutdown` message), 2 malformed input / unexpected EOF / handshake
//! failure (the protocol's symmetric malformed-input contract), 1 usage / engine-init
//! failure.

mod engine;
mod logging;
// 2026-07-03: the shared src/webview/* surface is compiled per binary; the helper decodes
// ConsumerMsg / encodes HelperMsg, so the opposite-direction halves (and the consumer-role
// fdpass/shm functions) are intentionally unused HERE — they are exercised by this crate's
// own `cargo test` via the shared `#[cfg(test)]` units (the two-gate parity insurance).
#[allow(dead_code)]
mod shared;

use cef::wrapper::message_router::{
    MessageRouterConfig, MessageRouterRendererSide, MessageRouterRendererSideHandlerCallbacks,
    RendererSideRouter,
};
use cef::{args::Args, sys, *};
use engine::{Engine, Out, Outbox};
use logging as log;
use shared::fdpass;
use shared::proto::{self, ConsumerMsg, HelperMsg, ProtoError};
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, RawFd};
use std::os::raw::c_int;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const COMPONENT: &str = "helper";
/// No `Hello` within this window after spawn → exit (the handshake watchdog).
const HELLO_WATCHDOG: Duration = Duration::from_secs(10);
/// The M1-spike-proven external-pump cadence.
const PUMP_INTERVAL: Duration = Duration::from_millis(10);
/// Outbound queue high-water mark: FrameReady flow control bounds pixel traffic to one
/// in-flight frame, so only console/load events accumulate — a queue this deep means the
/// consumer stopped reading while alive (treated as dead; challenge sessions are ~60 s).
const OUT_QUEUE_HIGH_WATER: usize = 1024;

/// Strict spawn-contract argv parse: exactly one `--ipc-fd=<non-negative int>`.
fn parse_ipc_fd<I: Iterator<Item = String>>(args: I) -> Result<RawFd, String> {
    let mut found: Option<RawFd> = None;
    for arg in args {
        if let Some(value) = arg.strip_prefix("--ipc-fd=") {
            match value.parse::<RawFd>() {
                Ok(fd) if fd >= 0 => {
                    if found.is_some() {
                        return Err("duplicate --ipc-fd argument".to_string());
                    }
                    found = Some(fd);
                }
                _ => {
                    return Err(format!(
                        "invalid --ipc-fd value {value:?} (expected a non-negative integer)"
                    ));
                }
            }
        } else if arg == "--ipc-fd" {
            // The contract is the `=` form only; a split form is a spawner bug.
            return Err("--ipc-fd requires the --ipc-fd=<fd> form".to_string());
        }
    }
    found.ok_or_else(|| {
        "missing required --ipc-fd=<fd> (the spawn contract in src/webview/mod.rs)".to_string()
    })
}

/// The optional explicit ozone override from the spawn contract (`--ozone-platform=<x>`).
fn parse_ozone_override<I: Iterator<Item = String>>(args: I) -> Option<String> {
    args.filter_map(|a| a.strip_prefix("--ozone-platform=").map(str::to_string))
        .last()
}

/// Live unprivileged-userns capability TEST (plan M5, 2026-07-10) — an honest measurement, not
/// knob-file reading: the knob names/locations vary per distro (Debian/Arch
/// `kernel.unprivileged_userns_clone`, Ubuntu 23.10+'s AppArmor gate, plain mainline none), so
/// the only distro-agnostic answer is to fork a child that actually exercises the capability.
///
/// The child tests the predicate the sandbox NEEDS — a USABLE namespace, not a merely
/// CREATABLE one (2026-07-10 review fix): on stock Ubuntu 24.04+ the default
/// `kernel.apparmor_restrict_unprivileged_userns=1` is permit-then-confine — a bare
/// `unshare(CLONE_NEWUSER)` from an unconfined process SUCCEEDS and the task is transitioned
/// into the `unprivileged_userns` AppArmor profile, which denies every capability inside the
/// new namespace (uid_map writes, `CLONE_NEWPID`, chroot); Chromium's own in-CefInitialize
/// viability check then correctly judges the namespace sandbox unusable and LOG(FATAL)s "No
/// usable sandbox!" AFTER HelloAck — past the designed pre-init refusal. So after a successful
/// `unshare(CLONE_NEWUSER)` the child must additionally perform one capability-gated raw
/// syscall inside the new namespace — `unshare(CLONE_NEWPID)` (ns_capable `CAP_SYS_ADMIN`, the
/// exact capability the sandboxed zygote's `CLONE_NEWPID|CLONE_NEWNET` launch needs; the
/// namespace creator holds the full capability set on unrestricted kernels, so a capable host
/// can never be false-negatived).
///
/// INVARIANT: this MUST run before this process spawns any thread (it runs in the
/// post-handshake / pre-`initialize` region, which uses only the main thread — the
/// reader/writer threads spawn after `initialize`), because `fork` from a multithreaded
/// process may only run async-signal-safe code in the child. The child here calls only
/// `unshare` (twice) + `_exit` (all raw syscalls), so it is safe even against a hidden
/// thread — the single-thread region keeps it trivially sound.
fn probe_userns() -> bool {
    // SAFETY: raw fork/unshare/_exit/waitpid. The child executes nothing but three
    // async-signal-safe syscalls and never returns into Rust; the parent reaps it
    // unconditionally. A fork failure reads as "not verified" (false) — the SUID tier and the
    // config opt-in remain, so a false here can only move DOWN the tier ladder, never crash.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return false;
        }
        if pid == 0 {
            // Create, then USE (see the fn doc): the second unshare is the in-namespace
            // capability test that fails EPERM under Ubuntu's `unprivileged_userns` profile.
            let ok =
                libc::unshare(libc::CLONE_NEWUSER) == 0 && libc::unshare(libc::CLONE_NEWPID) == 0;
            libc::_exit(if ok { 0 } else { 1 });
        }
        let mut status: libc::c_int = 0;
        if libc::waitpid(pid, &mut status, 0) != pid {
            return false;
        }
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }
}

/// The pure stat half of Chromium's SUID-helper acceptance predicate
/// (`sandbox/linux/suid/client/setuid_sandbox_host.cc`): root-owned regular file, setuid
/// (`S_ISUID`), AND world-executable (`S_IXOTH`) — the source of Chromium's canonical
/// "owned by root and has mode 4755" FATAL. 2026-07-10 review fix: the earlier
/// uid==0 && S_ISUID subset accepted a root:root 4750/4700 hardening chmod that Chromium
/// rejects with a post-HelloAck LOG(FATAL), mis-selecting the Suid tier past the designed
/// pre-init refusal (whose text names exactly the mode-4755 fix).
fn suid_sandbox_stat_ok(is_file: bool, uid: u32, mode: u32) -> bool {
    is_file && uid == 0 && mode & 0o4000 != 0 && mode & 0o001 != 0
}

/// Measure the SUID `chrome-sandbox` tier: a root-owned setuid world-executable regular file
/// beside the helper binary (the CEF dist ships it 0755 — an admin `chown root:root && chmod
/// 4755` enables this tier; the packaged-layout README documents it). Returns the path only
/// when Chromium's own acceptance predicate holds — [`suid_sandbox_stat_ok`] plus a live
/// `access(X_OK)` for the invoking user (Chromium requires both) — a measured stat, never an
/// assumption.
fn probe_suid_sandbox(exe_dir: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    let path = exe_dir.join("chrome-sandbox");
    let meta = std::fs::metadata(&path).ok()?;
    if !suid_sandbox_stat_ok(meta.is_file(), meta.uid(), meta.mode()) {
        return None;
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: plain libc::access on a valid NUL-terminated path; no memory is handed over.
    (unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } == 0).then_some(path)
}

// The `ozone` field is the explicit ozone selection, filled in before `initialize` (browser
// process only; `None` until the handshake completes). `render_handler` is the renderer-side
// JS-bridge/eval handler (plan M4) — active only in the CEF-forked renderer subprocess.
// `degraded_sandbox` (plan M5, 2026-07-10) records the helper's own policy-gated --no-sandbox
// degradation so the strip loop keeps banning the switch as a PASS-THROUGH while never
// desyncing Chromium from Settings.no_sandbox=1. (Field doc comments are not accepted by the
// wrap_app! macro grammar.)
wrap_app! {
    struct HelperApp {
        ozone: Arc<Mutex<Option<String>>>,
        render_handler: RenderProcessHandler,
        degraded_sandbox: Arc<AtomicBool>,
    }

    impl App {
        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(self.render_handler.clone())
        }

        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            // Browser process only (empty/absent process type). Subprocess command lines
            // are constructed by CEF itself and inherit the browser's switches.
            let is_browser = process_type
                .map(|p| p.to_string().is_empty())
                .unwrap_or(true);
            let Some(cmd) = command_line else { return };
            if !is_browser {
                return;
            }
            // Strip the forbidden pass-through switches (the M1 stderr-URL-leak channel and
            // the never---no-sandbox rule) no matter how they arrived. 2026-07-10 (plan M5):
            // routed through switch_should_be_stripped — when the helper ITSELF entered the
            // policy-gated degradation, CEF propagates Settings.no_sandbox=1 onto this command
            // line and stripping that copy would desync Chromium's sandbox decision from the
            // settings; the strip bans pass-through, not the helper's own deliberate act.
            let degraded = self.degraded_sandbox.load(Ordering::Acquire);
            for name in engine::FORBIDDEN_PASSTHROUGH_SWITCHES {
                if !engine::switch_should_be_stripped(name, degraded) {
                    continue;
                }
                let key = CefString::from(*name);
                if cmd.has_switch(Some(&key)) == 1 {
                    log::warn(
                        COMPONENT,
                        &format!("stripping forbidden pass-through switch --{name}"),
                    );
                    cmd.remove_switch(Some(&key));
                }
            }
            // Explicit ozone platform: an override already present in argv wins; otherwise
            // append our explicit selection (NEVER leave it to ozone auto).
            let ozone_key = CefString::from("ozone-platform");
            if cmd.has_switch(Some(&ozone_key)) != 1 {
                let selected = self.ozone.lock().ok().and_then(|s| s.clone());
                if let Some(platform) = selected {
                    cmd.append_switch_with_value(
                        Some(&ozone_key),
                        Some(&CefString::from(platform.as_str())),
                    );
                }
            }
        }
    }
}

// === Renderer-side JS bridge + evaluateJavascript (plan M4, 2026-07-09) ===========================
//
// Active ONLY in the CEF-forked renderer subprocess (execute_process runs the same binary; App's
// render_process_handler is called there). Holds the renderer-side cefQuery router + a per-render-
// process bridge inventory (interface → method names). Injects `window.<name>` stubs on each main-
// frame context creation, and services `eclipse.bridge.register` / `eclipse.eval` process messages.

wrap_render_process_handler! {
    struct HelperRenderProcessHandler {
        router: Arc<RendererSideRouter>,
        inventory: Arc<Mutex<HashMap<String, Vec<String>>>>,
    }

    impl RenderProcessHandler {
        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            let frame_owned: Option<Frame> = frame.map(|f| f.clone());
            // (1) Register cefQuery/cefQueryCancel on the new context (the router contract).
            self.router.on_context_created(
                browser.map(|b| b.clone()),
                frame_owned.clone(),
                context.map(|c| c.clone()),
            );
            // (2) On the MAIN frame: inject any already-known stubs, then ask the browser to (re-)send
            //     the bridge inventory for THIS fresh context (the pull model — a browser→renderer
            //     send before the renderer was connected is dropped, and each navigation is a new
            //     context). The browser answers with "eclipse.bridge.register", injecting the stubs.
            if let Some(frame) = frame_owned {
                if frame.is_main() != 0 {
                    self.inject_all_stubs(&frame);
                    if let Some(mut ready) =
                        process_message_create(Some(&CefString::from("eclipse.bridge.ready")))
                    {
                        frame.send_process_message(ProcessId::BROWSER, Some(&mut ready));
                    }
                    log::info("render", "main-frame context ready (requested bridge inventory)");
                }
            }
        }

        fn on_context_released(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            self.router.on_context_released(
                browser.map(|b| b.clone()),
                frame.map(|f| f.clone()),
                context.map(|c| c.clone()),
            );
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> c_int {
            let name = message
                .as_ref()
                .map(|m| CefString::from(&m.name()).to_string())
                .unwrap_or_default();
            match name.as_str() {
                "eclipse.bridge.register" => {
                    if let Some(msg) = message {
                        if let Some(args) = msg.argument_list() {
                            let n = args.size();
                            if n >= 1 {
                                let iface = CefString::from(&args.string(0)).to_string();
                                let methods: Vec<String> = (1..n)
                                    .map(|i| CefString::from(&args.string(i)).to_string())
                                    .collect();
                                if let Ok(mut inv) = self.inventory.lock() {
                                    inv.insert(iface.clone(), methods.clone());
                                }
                                // Inject immediately if the main-frame context already exists.
                                if let Some(frame) = frame {
                                    if frame.is_main() != 0 {
                                        let js = generate_stub_js(&iface, &methods);
                                        frame.execute_java_script(
                                            Some(&CefString::from(js.as_str())),
                                            None,
                                            0,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    1
                }
                "eclipse.eval" => {
                    if let (Some(msg), Some(frame)) = (message, frame) {
                        if let Some(args) = msg.argument_list() {
                            let request_id = args.int(0);
                            let script = CefString::from(&args.string(1)).to_string();
                            let (ok, value_json) = eval_in_frame(frame, &script);
                            if let Some(mut out) = process_message_create(Some(&CefString::from(
                                "eclipse.eval.result",
                            ))) {
                                if let Some(oargs) = out.argument_list() {
                                    oargs.set_size(3);
                                    oargs.set_int(0, request_id);
                                    oargs.set_bool(1, c_int::from(ok));
                                    oargs.set_string(
                                        2,
                                        Some(&CefString::from(value_json.as_str())),
                                    );
                                }
                                frame.send_process_message(ProcessId::BROWSER, Some(&mut out));
                            }
                        }
                    }
                    1
                }
                // Not ours: let the renderer router handle cefQuery replies from the browser.
                _ => {
                    if self.router.on_process_message_received(
                        browser.map(|b| b.clone()),
                        frame.map(|f| f.clone()),
                        Some(source_process),
                        message.map(|m| m.clone()),
                    ) {
                        1
                    } else {
                        0
                    }
                }
            }
        }
    }
}

impl HelperRenderProcessHandler {
    /// Inject every stored interface's `window.<name>` stubs into `frame`.
    fn inject_all_stubs(&self, frame: &Frame) {
        if let Ok(inv) = self.inventory.lock() {
            for (iface, methods) in inv.iter() {
                let js = generate_stub_js(iface, methods);
                frame.execute_java_script(Some(&CefString::from(js.as_str())), None, 0);
            }
        }
    }
}

/// Escape a Java identifier defensively for embedding inside a JS double-quoted string literal
/// (interface/method names are Java identifiers — no real injection risk, but escape anyway).
fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Generate the `window.<name>` bridge stub JS for one interface. Each method returns a Promise
/// (the async bridge shape — see the M4 design §0.2 documented sync-return divergence): it
/// serializes `{iface, method, args}` and sends it through `cefQuery`; onSuccess resolves with the
/// JSON-parsed result, onFailure rejects with an Error.
fn generate_stub_js(iface: &str, methods: &[String]) -> String {
    let name = js_escape(iface);
    let methods_arr = methods
        .iter()
        .map(|m| format!("\"{}\"", js_escape(m)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "(function(){{var q=window.cefQuery;if(!q){{return;}}var o=window[\"{name}\"]=window[\"{name}\"]||{{}};\
         [{methods_arr}].forEach(function(m){{o[m]=function(){{\
         var a=Array.prototype.slice.call(arguments);\
         var r=JSON.stringify({{iface:\"{name}\",method:m,args:a}});\
         return new Promise(function(res,rej){{q({{request:r,\
         onSuccess:function(s){{res(s===\"\"?undefined:JSON.parse(s));}},\
         onFailure:function(c,msg){{rej(new Error(msg));}}}});}});}};}});}})();"
    )
}

/// Evaluate `script` in `frame`'s V8 context and return `(ok, value_json)`. The wrapper
/// `JSON.stringify((function(){return (<script>);})())` yields the JSON-encoded result string
/// (Android's `evaluateJavascript` contract); `undefined`/non-string → `"null"`.
fn eval_in_frame(frame: &Frame, script: &str) -> (bool, String) {
    let wrapper = format!("JSON.stringify((function(){{return ({script});}})())");
    let Some(ctx) = frame.v8_context() else {
        return (false, "null".to_string());
    };
    let mut retval: Option<V8Value> = None;
    let mut exception: Option<V8Exception> = None;
    let code = ctx.eval(
        Some(&CefString::from(wrapper.as_str())),
        None,
        0,
        Some(&mut retval),
        Some(&mut exception),
    );
    if code == 0 {
        return (false, "null".to_string());
    }
    match retval {
        Some(v) if v.is_string() != 0 => (true, CefString::from(&v.string_value()).to_string()),
        // undefined / non-string result → "null" (the Android evaluateJavascript contract).
        _ => (true, "null".to_string()),
    }
}

/// Commands from the reader thread into the pump loop.
enum Command {
    Msg(ConsumerMsg),
    /// The stream ended (wire violation or plain EOF — both take the same quit path, exit 2;
    /// they differ only in the log severity the reader thread already emitted).
    Dead,
}

fn write_helper_msg(stream: &UnixStream, msg: &HelperMsg) -> std::io::Result<()> {
    match msg.encode() {
        Ok(bytes) => (&mut &*stream).write_all(&bytes),
        Err(e) => {
            // An encode failure is a helper bug (oversized outbound frame): log loudly,
            // skip the message, keep the connection (payload-free line).
            log::error(COMPONENT, &format!("outbound frame encode failed: {e}"));
            Ok(())
        }
    }
}

fn set_cloexec(stream: &UnixStream) {
    // SAFETY: plain fcntl flag set on a live fd we own — keeps the control socket from
    // leaking into CEF's forked subprocesses (2026-07-03).
    let ok = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    if ok != 0 {
        log::warn(COMPONENT, "could not set FD_CLOEXEC on the control socket");
    }
}

fn main() -> ExitCode {
    // Initialize the CEF API version before any other CEF call (upstream example order).
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        log::error(COMPONENT, "cannot parse the command line");
        return ExitCode::FAILURE;
    };
    let is_browser_process = cmd_line.has_switch(Some(&CefString::from("type"))) != 1;

    let ozone_slot: Arc<Mutex<Option<String>>> = Arc::default();
    // 2026-07-10 (plan M5): set only by the browser process's own policy-gated degradation
    // (never by argv) — consumed by the strip loop via switch_should_be_stripped.
    let degraded_sandbox: Arc<AtomicBool> = Arc::default();
    // The renderer-side JS-bridge/eval handler (plan M4). The router + inventory are cheap to
    // construct here; the handler is only USED in the CEF-forked renderer subprocess (App's
    // render_process_handler is called there, never in the browser process).
    let render_handler = HelperRenderProcessHandler::new(
        RendererSideRouter::new(MessageRouterConfig::default()),
        Arc::new(Mutex::new(HashMap::new())),
    );
    let mut app = HelperApp::new(ozone_slot.clone(), render_handler, degraded_sandbox.clone());
    let ret = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if !is_browser_process {
        // A CEF-forked subprocess (renderer/gpu/utility): execute_process ran it fully.
        return ExitCode::from(ret.clamp(0, 255) as u8);
    }
    if ret != -1 {
        log::error(
            COMPONENT,
            &format!("execute_process consumed the browser process (ret={ret})"),
        );
        return ExitCode::FAILURE;
    }

    // Spawn contract: the control socket arrives as the fd named by --ipc-fd (strict).
    let ipc_fd = match parse_ipc_fd(std::env::args()) {
        Ok(fd) => fd,
        Err(msg) => {
            log::error(COMPONENT, &msg);
            eprintln!(
                "usage: eclipse-webview --ipc-fd=<fd> [--ozone-platform=<wayland|x11>] \
                 [--allow-unsandboxed]"
            );
            return ExitCode::FAILURE;
        }
    };
    // SAFETY: the spawn contract (src/webview/mod.rs) gives this process exclusive ownership
    // of the fd named by --ipc-fd — the consumer dup2'd its socketpair end there before
    // exec, and nothing else in this process has touched it. Taking ownership once is sound.
    let stream = unsafe { UnixStream::from_raw_fd(ipc_fd) };
    set_cloexec(&stream);

    // ---- Handshake FIRST, under the watchdog — before any engine-init cost. ----
    if stream.set_read_timeout(Some(HELLO_WATCHDOG)).is_err() {
        log::error(COMPONENT, "cannot arm the handshake watchdog");
        return ExitCode::from(2);
    }
    match proto::read_consumer_msg(&mut &stream) {
        Ok(ConsumerMsg::Hello { version }) if version == shared::PROTO_VERSION => {}
        Ok(ConsumerMsg::Hello { version }) => {
            // Unsupported version: answer with OUR version so the consumer can raise an
            // actionable mismatch error, then close.
            let _ = write_helper_msg(
                &stream,
                &HelperMsg::HelloAck {
                    version: shared::PROTO_VERSION,
                    engine: engine::engine_id(),
                },
            );
            log::error(
                COMPONENT,
                &format!(
                    "protocol version mismatch: consumer v{version}, helper v{} — closing",
                    shared::PROTO_VERSION
                ),
            );
            return ExitCode::from(2);
        }
        Ok(_) => {
            log::error(
                COMPONENT,
                "handshake-order violation: first frame was not Hello",
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            log::error(
                COMPONENT,
                &format!("no valid Hello within {HELLO_WATCHDOG:?}: {e}"),
            );
            return ExitCode::from(2);
        }
    }
    if write_helper_msg(
        &stream,
        &HelperMsg::HelloAck {
            version: shared::PROTO_VERSION,
            engine: engine::engine_id(),
        },
    )
    .is_err()
    {
        log::error(COMPONENT, "cannot write HelloAck — consumer gone");
        return ExitCode::from(2);
    }
    let _ = stream.set_read_timeout(None);

    // ---- Explicit ozone selection (never auto — the M1 designed-failure rule). ----
    let override_flag = parse_ozone_override(std::env::args());
    let selection = match engine::select_ozone(
        override_flag.as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
    ) {
        Ok(platform) => platform,
        Err(e) => {
            log::error(COMPONENT, &e.to_string());
            let _ = write_helper_msg(
                &stream,
                &HelperMsg::Crash {
                    view: 0,
                    kind: 1, // engine-init-failed
                    code: 0,
                },
            );
            return ExitCode::FAILURE;
        }
    };
    log::info(
        COMPONENT,
        &format!(
            "ozone platform selected explicitly: {selection}{}",
            if override_flag.is_some() {
                " (override)"
            } else {
                ""
            }
        ),
    );
    if let Ok(mut slot) = ozone_slot.lock() {
        *slot = Some(selection);
    }

    // ---- Sandbox-mode selection (plan M5, 2026-07-10 — the dated owner-revisable policy). ----
    // Runs in the pre-`initialize` single-thread region (the probe_userns invariant — see its
    // doc). Both inputs are MEASURED capabilities: a live userns USABILITY probe (create + an
    // in-namespace capability use) and Chromium's own chrome-sandbox acceptance predicate
    // against the file beside the helper binary; refusal requires BOTH tiers to have measurably
    // failed AND the config opt-in to be absent — a capable host can never be false-negatived
    // into degradation by knob guessing.
    let allow_unsandboxed = std::env::args().any(|a| a == "--allow-unsandboxed");
    let suid_path = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
        .and_then(probe_suid_sandbox);
    let sandbox_mode =
        match engine::select_sandbox_mode(probe_userns(), suid_path.is_some(), allow_unsandboxed) {
            Ok(mode) => mode,
            Err(e) => {
                log::error(COMPONENT, &e.to_string());
                let _ = write_helper_msg(
                    &stream,
                    &HelperMsg::Crash {
                        view: 0,
                        kind: 1, // engine-init-failed
                        code: 2, // 2026-07-10 (M5): sandbox-policy refusal (0 = no display)
                    },
                );
                return ExitCode::FAILURE;
            }
        };
    match &sandbox_mode {
        engine::SandboxMode::Userns => log::info(
            COMPONENT,
            "sandbox mode selected: userns (unprivileged user namespaces verified USABLE by a \
             live unshare + in-namespace capability probe)",
        ),
        engine::SandboxMode::Suid => {
            let path = suid_path.as_deref().unwrap_or(Path::new("chrome-sandbox"));
            // 2026-07-10: Chromium's documented SUID-helper path override. set_var runs in the
            // same pre-threads region as the fork probe (env mutation is thread-unsafe).
            std::env::set_var("CHROME_DEVEL_SANDBOX", path);
            log::info(
                COMPONENT,
                &format!(
                    "sandbox mode selected: suid (chrome-sandbox setuid root at {})",
                    path.file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ),
            );
        }
        engine::SandboxMode::Degraded => {
            degraded_sandbox.store(true, Ordering::Release);
            log::warn(
                COMPONENT,
                "sandbox mode selected: DEGRADED --no-sandbox by explicit config opt-in \
                 (webview_allow_unsandboxed) — hostile web content will render UNSANDBOXED",
            );
        }
    }

    // ---- Render-path detection (plan M5, 2026-07-10): LOG-ONLY by design — Chromium's own
    // GPU-process fallback is the mechanism; the shipped SwiftShader/ANGLE set (pinned into
    // the packaged payload by tools/webview-dist/package-webview.sh) makes the no-GPU branch a
    // working degradation. A missing /dev/dri simply reads as no render nodes.
    let mut render_nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|name| name.starts_with("renderD"))
                .collect()
        })
        .unwrap_or_default();
    render_nodes.sort();
    let nvidia_ctl_present = Path::new("/dev/nvidiactl").exists();
    match engine::classify_render_path(&render_nodes, nvidia_ctl_present) {
        engine::RenderPathVerdict::GpuCandidates(devices) => log::info(
            COMPONENT,
            &format!(
                "render path: gpu candidate devices present ({}) — Chromium selects the GPU \
                 path; the bundled SwiftShader remains the automatic fallback",
                devices.join(", ")
            ),
        ),
        engine::RenderPathVerdict::SoftwareFallback => log::info(
            COMPONENT,
            "render path: no GPU render nodes detected — Chromium will use the bundled \
             SwiftShader software renderer",
        ),
    }

    // ---- Engine init: windowless, external pump, sandbox per the selected mode (ON except
    // the policy-gated Degraded opt-in), engine logging OFF. ----
    let mut settings = engine::build_settings();
    engine::apply_sandbox_mode(&mut settings, &sandbox_mode);
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) != 1
    {
        log::error(COMPONENT, "CefInitialize failed (engine-init-failed)");
        let _ = write_helper_msg(
            &stream,
            &HelperMsg::Crash {
                view: 0,
                kind: 1,
                code: 0,
            },
        );
        return ExitCode::FAILURE;
    }
    log::info(COMPONENT, &format!("initialized {}", engine::engine_id()));

    // ---- Writer thread: owns the socket write half behind a bounded queue. ----
    let (out_tx, out_rx) = mpsc::sync_channel::<Out>(OUT_QUEUE_HIGH_WATER);
    let outbox = Outbox::new(out_tx);
    let writer_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::error(COMPONENT, &format!("cannot clone the control socket: {e}"));
            shutdown();
            return ExitCode::from(2);
        }
    };
    let writer = std::thread::spawn(move || {
        for out in out_rx {
            let result = match out {
                Out::Msg(msg) => write_helper_msg(&writer_stream, &msg),
                Out::MsgWithFd(msg, fd) => write_helper_msg(&writer_stream, &msg).and_then(|()| {
                    // The FrameBufferNew adjacency contract: the sentinel+SCM_RIGHTS memfd
                    // follows the frame bytes immediately (same writer, same order).
                    fdpass::send_fd_with_sentinel(&writer_stream, fd.as_fd())
                        .map_err(|e| std::io::Error::other(e.to_string()))
                }),
            };
            if let Err(e) = result {
                log::error(COMPONENT, &format!("control-socket write failed: {e}"));
                break;
            }
        }
    });

    // ---- Reader thread: blocking exact-length framed reads → command channel. ----
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::error(COMPONENT, &format!("cannot clone the control socket: {e}"));
            shutdown();
            return ExitCode::from(2);
        }
    };
    std::thread::spawn(move || {
        let mut r = &reader_stream;
        loop {
            match proto::read_consumer_msg(&mut r) {
                Ok(msg) => {
                    if cmd_tx.send(Command::Msg(msg)).is_err() {
                        break;
                    }
                }
                Err(ProtoError::Eof) => {
                    // Unexpected EOF: same quit path as malformed, lower log severity.
                    log::warn(COMPONENT, "consumer closed the control socket (EOF)");
                    let _ = cmd_tx.send(Command::Dead);
                    break;
                }
                Err(e) => {
                    // ONE loud payload-free line (error kind + type byte/declared len only).
                    log::error(COMPONENT, &format!("malformed control stream: {e}"));
                    let _ = cmd_tx.send(Command::Dead);
                    break;
                }
            }
        }
    });

    // ---- The pump loop (CEF UI thread == this thread). ----
    let engine = Engine::new(outbox.clone());
    let (exit_code, clean_close) = loop {
        do_message_loop_work();
        loop {
            match cmd_rx.try_recv() {
                Ok(Command::Msg(msg)) => engine.handle(msg),
                Ok(Command::Dead) => engine.begin_shutdown(2),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    engine.begin_shutdown(2);
                    break;
                }
            }
        }
        engine.poll();
        if engine.outbox_dead() {
            engine.begin_shutdown(2);
        }
        if let Some(done) = engine.shutdown_state() {
            break done;
        }
        std::thread::sleep(PUMP_INTERVAL);
    };

    // Flush the outbound queue (ViewClosed and friends must reach the consumer), then shut
    // CEF down — only after a clean close (shutdown() with live browser refs aborts).
    drop(engine);
    drop(outbox);
    let _ = writer.join();
    if clean_close {
        shutdown();
        log::info(COMPONENT, &format!("clean shutdown, exit={exit_code}"));
        ExitCode::from(exit_code.clamp(0, 255) as u8)
    } else {
        log::warn(
            COMPONENT,
            &format!("browsers did not close in time; skipping cef shutdown, exit={exit_code}"),
        );
        // Skip destructors that would touch CEF state (the spike's unclean-close lesson).
        std::process::exit(exit_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_fd_argument_is_required_and_validated() {
        // 2026-07-03: pins spawn-contract strictness — no blind fd-3 assumption. A missing
        // or garbage --ipc-fd is a usage error (nonzero exit in main) BEFORE any fd use.
        let args = |v: &[&str]| {
            v.iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .into_iter()
        };

        // Missing → error naming the flag.
        let err = parse_ipc_fd(args(&["eclipse-webview"])).unwrap_err();
        assert!(err.contains("--ipc-fd"));
        // Garbage / negative / split-form → error.
        assert!(parse_ipc_fd(args(&["x", "--ipc-fd=abc"])).is_err());
        assert!(parse_ipc_fd(args(&["x", "--ipc-fd=-1"])).is_err());
        assert!(parse_ipc_fd(args(&["x", "--ipc-fd", "3"])).is_err());
        // Duplicate → error.
        assert!(parse_ipc_fd(args(&["x", "--ipc-fd=3", "--ipc-fd=4"])).is_err());
        // The contract form parses.
        assert_eq!(parse_ipc_fd(args(&["x", "--ipc-fd=3"])), Ok(3));
        assert_eq!(
            parse_ipc_fd(args(&["x", "--ozone-platform=x11", "--ipc-fd=7"])),
            Ok(7)
        );

        // The ozone override parse (the only other contract flag).
        assert_eq!(
            parse_ozone_override(args(&["x", "--ozone-platform=wayland"])).as_deref(),
            Some("wayland")
        );
        assert_eq!(parse_ozone_override(args(&["x"])), None);
    }

    #[test]
    fn suid_sandbox_stat_predicate_byte_matches_chromiums_acceptance() {
        // 2026-07-10 (M5 review fix): Chromium's setuid_sandbox_host.cc requires st_uid==0 &&
        // S_ISUID && S_IXOTH (+ access(X_OK), checked live at the call site) — the canonical
        // "owned by root and has mode 4755". A subset predicate mis-selected the Suid tier for
        // root:root 4750/4700 files that Chromium then rejects with a post-HelloAck
        // LOG(FATAL), bypassing the designed pre-init SandboxUnavailable refusal.
        // The documented remedy shape passes.
        assert!(suid_sandbox_stat_ok(true, 0, 0o104755));
        // Group/owner-restricted hardening chmods Chromium rejects (no S_IXOTH).
        assert!(!suid_sandbox_stat_ok(true, 0, 0o104750));
        assert!(!suid_sandbox_stat_ok(true, 0, 0o104700));
        // The as-shipped 0755 (no S_ISUID) stays tier-unavailable.
        assert!(!suid_sandbox_stat_ok(true, 0, 0o100755));
        // Non-root ownership never qualifies, even at 4755.
        assert!(!suid_sandbox_stat_ok(true, 1000, 0o104755));
        // Not a regular file never qualifies.
        assert!(!suid_sandbox_stat_ok(false, 0, 0o104755));
    }
}
