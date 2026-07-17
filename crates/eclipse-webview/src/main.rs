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
use logging::RedactedTarget;
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
        bridge_diag: Option<LoadHandler>,
    }

    impl RenderProcessHandler {
        fn load_handler(&self) -> Option<LoadHandler> {
            // 2026-07-16 (plan M6): the bridge SELF-INTROSPECTION seam. The pinned binding's
            // `_cef_render_process_handler_t::get_load_handler` — "Return the handler for browser
            // load status events" — installs a LoadHandler whose callbacks CEF delivers on "the
            // browser process UI thread OR render process main thread (TID_RENDERER)"
            // (`cef_load_handler_t`'s own doc); returned from the RENDER process handler it is the
            // renderer's TID_RENDERER, which is the ONLY thread V8 may be touched from and exactly
            // where `eval_in_frame` already works. That makes `on_load_end` the correct seam: CEF's
            // own "this frame is done loading" notification, delivered in-process to the thread that
            // owns the V8 context — no browser→renderer IPC hop and no ordering question about
            // whether the context still exists.
            //
            // Diagnostic OFF (the default) returns None: CEF gets a NULL load handler and the
            // renderer pays nothing — no handler, no eval, no log.
            self.bridge_diag.clone()
        }

        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            let frame_owned: Option<Frame> = frame.map(|f| f.clone());
            // 2026-07-10 (plan M6): clone the handed-in context BEFORE the router consumes it, so we
            // can eval the stubs synchronously on THIS same context after cefQuery is registered.
            let ctx_for_eval: Option<V8Context> = context.as_deref().cloned();
            // (1) Register cefQuery/cefQueryCancel on the new context FIRST (the router contract) so
            //     window.cefQuery exists before the stub eval references it.
            self.router.on_context_created(
                browser.map(|b| b.clone()),
                frame_owned.clone(),
                context.map(|c| c.clone()),
            );
            // (2) On EVERY frame: inject any already-known stubs SYNCHRONOUSLY via V8Context::eval
            //     (guaranteed to run before on_context_created returns — i.e. before any page script
            //     executes in this new context; CefFrame::ExecuteJavaScript is NOT contractually
            //     synchronous during context creation). Then ask the browser to (re-)send the bridge
            //     inventory for THIS fresh context (the pull model / backstop — a browser→renderer
            //     send before the renderer was connected is dropped, and each navigation is a new
            //     context). The browser answers with "eclipse.bridge.register" addressed to the
            //     frame that asked (engine.rs), so a site-isolated subframe in its own renderer
            //     process — whose per-process inventory starts empty — pulls its own copy.
            //
            // 2026-07-16 (plan M6, divergence #1): NOT main-frame-gated. This is AOSP's contract,
            // not a choice. `WebView.addJavascriptInterface`'s normative javadoc: "The object is
            // injected into all frames of the web page, including all the iframes" — and Chromium
            // implements exactly that (gin_java_bridge_dispatcher_host.cc fans AddNamedObject out
            // over every live frame; the renderer's GinJavaBridgeDispatcher is a per-RenderFrame
            // RenderFrameObserver binding on each frame's DidClearWindowObject). The developer
            // guide is explicit that the legacy API "is available to every frame within the
            // WebView, including iframes" and "lacks origin-based access control". The old gate
            // made Eclipse measurably MORE restrictive than the platform it stands in for
            // (§6 2026-07-16 🔬 measured three arkose subframes at "type":"undefined").
            //
            // SECURITY CONSEQUENCE OF UNGATING: none — there is no delta. The cefQuery router is
            // registered on EVERY context above (step 1), and the browser-side dispatcher
            // (engine.rs `BridgeHandler::on_query_str`) resolves the view from the BROWSER identity
            // and ignores the frame entirely. Any subframe can therefore ALREADY invoke the app's
            // method by hand-building the cefQuery payload this stub would have sent: the gate
            // withheld the ergonomic wrapper, never the transport, so it was never a security
            // boundary. AOSP hands the platform exactly one enforcement lever — the
            // @JavascriptInterface annotation gate — and Eclipse pulls it unconditionally in
            // `framework.rs::reflect_javascript_interface_methods`. Do not "harden" this back into
            // a main-frame gate: it would buy nothing and re-open the conformance defect.
            if let Some(frame) = frame_owned {
                let main_frame = frame.is_main() != 0;
                let (ifaces, methods) = self.inject_all_stubs(ctx_for_eval.as_ref());
                if ifaces > 0 {
                    log::info("render", &format_stub_apply_line("sync", ifaces, methods));
                }
                if let Some(mut ready) =
                    process_message_create(Some(&CefString::from("eclipse.bridge.ready")))
                {
                    frame.send_process_message(ProcessId::BROWSER, Some(&mut ready));
                }
                log::info("render", &format_context_ready_line(main_frame));
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
                                // Inject immediately into the frame this register arrived on. This
                                // REFRESH path keeps frame.execute_java_script — the proven M4
                                // round-trip path for an already-running page (the sync-eval path is
                                // on_context_created, before page scripts).
                                //
                                // 2026-07-16 (plan M6, divergence #1): NOT main-frame-gated, for the
                                // same AOSP contract as the on_context_created path above — the
                                // object belongs in "all frames of the web page, including all the
                                // iframes". The browser addresses each register to the frame whose
                                // "eclipse.bridge.ready" it answers, so injecting into `frame` is
                                // both correct and sufficient: every frame pulls for itself.
                                if let Some(frame) = frame {
                                    let js =
                                        generate_stub_js(&iface, &methods, self.bridge_diag_on());
                                    frame.execute_java_script(
                                        Some(&CefString::from(js.as_str())),
                                        None,
                                        0,
                                    );
                                    // 2026-07-10 (plan M6): the A2 evidence line (counts only).
                                    log::info(
                                        "render",
                                        &format_stub_apply_line("refresh", 1, methods.len()),
                                    );
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
    /// Whether the dev-host `ECLIPSE_WEBVIEW_BRIDGE_DIAG=1` gate is on in THIS renderer process
    /// (plan M6, 2026-07-16). Derived from the single field the gate already materializes into —
    /// `bridge_diag` is `Some` iff `engine::bridge_diag_enabled` said so in `main` — rather than
    /// copied into a second field that could drift out of step with it.
    fn bridge_diag_on(&self) -> bool {
        self.bridge_diag.is_some()
    }

    /// Inject every stored interface's `window.<name>` stubs into `ctx` SYNCHRONOUSLY via
    /// `V8Context::eval` (plan M6, 2026-07-10) and return `(ifaces, total_methods)` for the A2
    /// evidence line. Eval is the same primitive `eval_in_frame` proves against this binding and is
    /// guaranteed to run before `on_context_created` returns (before page scripts). An eval failure
    /// is logged once (payload-free) and does not abort the remaining interfaces.
    fn inject_all_stubs(&self, ctx: Option<&V8Context>) -> (usize, usize) {
        let Some(ctx) = ctx else {
            return (0, 0);
        };
        let Ok(inv) = self.inventory.lock() else {
            return (0, 0);
        };
        let mut ifaces = 0usize;
        let mut methods_total = 0usize;
        for (iface, methods) in inv.iter() {
            let js = generate_stub_js(iface, methods, self.bridge_diag_on());
            let mut retval: Option<V8Value> = None;
            let mut exception: Option<V8Exception> = None;
            let code = ctx.eval(
                Some(&CefString::from(js.as_str())),
                None,
                0,
                Some(&mut retval),
                Some(&mut exception),
            );
            if code == 0 {
                log::warn(
                    "render",
                    "bridge stub injection failed (context eval error)",
                );
            }
            ifaces += 1;
            methods_total += methods.len();
        }
        (ifaces, methods_total)
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

/// The renderer stub-injection evidence line (plan M6, 2026-07-10): `mode` is `sync` (the
/// on_context_created eval, before any page script runs) or `refresh` (a late
/// `eclipse.bridge.register` re-inject for an already-running page). COUNTS only — no interface or
/// method names (matches the "names not logged" stance). The A2 timestamp makes the injection
/// race measurable against the engine's "load started" line. Pure/unit-pinned.
fn format_stub_apply_line(mode: &str, ifaces: usize, methods: usize) -> String {
    format!("bridge stubs applied mode={mode} ifaces={ifaces} methods={methods}")
}

/// The renderer per-frame "context ready, inventory requested" evidence line (plan M6).
///
/// 2026-07-16: was the bare literal `"main-frame context ready (requested bridge inventory)"`, which
/// only ever fired for the main frame. Now that stub injection follows AOSP's all-frames contract
/// this fires per FRAME, so the line binds `main_frame=1|0` instead of asserting "main-frame" — a
/// subframe pull is exactly what the all-frames fix is expected to produce, and a line that still
/// said "main-frame" would be a stale lie. Boolean only: the frame URL is deliberately NOT logged
/// (the privacy absolute — frame URLs reach the log only through `RedactedTarget`). Pure/unit-pinned.
fn format_context_ready_line(main_frame: bool) -> String {
    format!(
        "context ready (requested bridge inventory) main_frame={}",
        u8::from(main_frame)
    )
}

/// The static, greppable prefix every stub-invocation trace line carries (plan M6, 2026-07-16).
/// Static by construction so a live-boot grep is one fixed literal; see [`generate_stub_js`].
const STUB_TRACE_PREFIX: &str = "ECLIPSE-STUB";

/// Generate the `window.<name>` bridge stub JS for one interface. Each method returns a Promise
/// (the async bridge shape — see the M4 design §0.2 documented sync-return divergence): it
/// serializes `{iface, method, args}` and sends it through `cefQuery`; onSuccess resolves with the
/// JSON-parsed result, onFailure rejects with an Error.
///
/// `diag` is the dev-host `ECLIPSE_WEBVIEW_BRIDGE_DIAG=1` gate (`engine::bridge_diag_enabled`, read
/// once in `main`). 2026-07-16 (plan M6) — WHY THIS EXISTS: across SEVEN live boots the consumer's
/// `bridge call received` is ZERO, and the record has repeatedly INFERRED "the page never calls the
/// bridge" from that. The inference is UNSOUND: the chain is stub body → `q(...)` (`window.cefQuery`)
/// → `CefMessageRouter` → `BridgeHandler::on_query_str` (engine.rs) → `HelperMsg::BridgeCall` → ART.
/// If the page DID call the stub and ANY link dropped it, the observable is IDENTICAL to the page
/// never calling — yet the two have opposite next steps (an Eclipse bug we can fix vs the page's own
/// transport selection, which we cannot). `__webview-test` proves the chain only for a local
/// same-origin SINGLE-FRAME page; the challenge page is cross-origin with cross-origin subframes.
/// This trace makes the stub announce its OWN invocation, so the two worlds separate by measurement.
///
/// The trace announces at five points: the installer's cefQuery bail (otherwise INVISIBLE — the
/// `bridge stubs applied` line is logged by the Rust caller either way, so a bail would look exactly
/// like a page that never called), then per call `invoke` (FIRST statement in the body — before
/// `JSON.stringify`, which genuinely throws on circular/BigInt args, and before `q`), `sent` (q
/// accepted the query), and each terminal (`success`/`failure`).
///
/// PAYLOAD RULE: iface name, method name, and ARGUMENT COUNT only — never an argument VALUE and
/// never a URL, matching `framework.rs`'s bridge receipt (which binds `arg_lens` only). Identifiers
/// are js-escaped exactly as the stub body escapes them.
///
/// `diag == false` (the default) emits BYTE-IDENTICAL JS to the pre-diagnostic stub: every fragment
/// below is empty, so the `format!` collapses to exactly the original text (pinned by
/// `generate_stub_js_gate_off_is_byte_identical_to_the_pre_diag_stub`). The gate-on body keeps the
/// stub's semantics unchanged — same cefQuery capture + guard, same JSON payload, same Promise
/// shape, same terminals; the trace only observes.
///
/// The console is captured AT INJECTION TIME (`window.console` + its `log`, called back with the
/// console as `this`) rather than read per call: on the sync path that capture happens before any
/// page script runs, so a page that later replaces `window.console` cannot silence the trace — a
/// silenced trace would read as "absent", i.e. the exact false negative this diagnostic exists to
/// prevent. Every trace call is try/caught (a missing console collapses into the same catch), so the
/// diagnostic can never throw into the page's own call path.
fn generate_stub_js(iface: &str, methods: &[String], diag: bool) -> String {
    let name = js_escape(iface);
    let methods_arr = methods
        .iter()
        .map(|m| format!("\"{}\"", js_escape(m)))
        .collect::<Vec<_>>()
        .join(",");
    // `C`/`L`/`d` live inside the stub's own IIFE — no page global is touched or shadowed (the
    // onFailure param `c` is case-distinct from `C`).
    let trace_prelude = if diag {
        format!(
            "var C=window.console,L=C&&C.log,\
             d=function(t){{try{{L.call(C,\"{STUB_TRACE_PREFIX} \"+t);}}catch(e){{}}}};"
        )
    } else {
        String::new()
    };
    let trace = |event: &str, suffix: &str| {
        if diag {
            format!("d(\"{event} iface={name} method=\"+m{suffix});")
        } else {
            String::new()
        }
    };
    // The bail fires before any method is known, so it binds the iface alone.
    let trace_no_query = if diag {
        format!("d(\"no-cefQuery iface={name}\");")
    } else {
        String::new()
    };
    let trace_invoke = trace("invoke", "+\" argc=\"+arguments.length");
    let trace_sent = trace("sent", "");
    let trace_success = trace("success", "");
    let trace_failure = trace("failure", "");
    format!(
        "(function(){{{trace_prelude}var q=window.cefQuery;if(!q){{{trace_no_query}return;}}var o=window[\"{name}\"]=window[\"{name}\"]||{{}};\
         [{methods_arr}].forEach(function(m){{o[m]=function(){{{trace_invoke}\
         var a=Array.prototype.slice.call(arguments);\
         var r=JSON.stringify({{iface:\"{name}\",method:m,args:a}});\
         return new Promise(function(res,rej){{q({{request:r,\
         onSuccess:function(s){{{trace_success}res(s===\"\"?undefined:JSON.parse(s));}},\
         onFailure:function(c,msg){{{trace_failure}rej(new Error(msg));}}}});{trace_sent}}});}};}});}})();"
    )
}

// === Bridge self-introspection diagnostic (plan M6, 2026-07-16) ==================================
//
// DEV-HOST ONLY, env-gated by ECLIPSE_WEBVIEW_BRIDGE_DIAG=1 (engine::bridge_diag_enabled); OFF is a
// structural no-op (HelperRenderProcessHandler::load_handler returns None → CEF gets no handler).
//
// WHY: the M6 frontier (AGENTS.md §5 🕵️ / §6 2026-07-16). The app registers
// `addJavascriptInterface iface=__globalRobloxAndroidBridge__ method_count=1`, the renderer logs
// `bridge stubs applied ifaces=1 methods=1` ~470 ms before the page's bundle first speaks, the page
// demonstrably runs and fires all four hybrid calls — and yet ZERO `bridge call received` have EVER
// reached Eclipse across five live boots. The injection race is ruled out, callback delivery is
// fixed, and UA steering is ruled out AS THE BRIDGE'S CAUSE (silent under both UAs). So the open
// question is narrow and first-party: what does ECLIPSE'S OWN INJECTED STUB actually look like, in
// the frame the page runs in, once the page has finished running?
//
// SCOPE — FIRST-PARTY ONLY, and enforced by construction: the iface list comes from the RENDERER'S
// OWN inventory (what Eclipse itself injected, from the app's own registration), never from
// scanning the page. The script touches exactly two things: `window.cefQuery` (OUR router) and
// `window[<our iface>]` (the global OUR stub claimed). It reads no page-authored global, no
// document, no location, no storage, and no page state.

/// Build the bridge SELF-INTROSPECTION expression from Eclipse's OWN bridge inventory (plan M6,
/// 2026-07-16). `ifaces` MUST be the renderer's own inventory keys — the interfaces Eclipse injected
/// — never names scraped from the page.
///
/// Emits an expression (the shape [`eval_in_frame`] wraps in `JSON.stringify`) reporting, for OUR
/// injection only: `typeof window.cefQuery` (is OUR router present in this frame?), and per iface
/// its `typeof` plus each own property name with that property's `typeof` (did OUR method land, and
/// as what?). `getOwnPropertyNames` is called ONLY on an object/function (it throws on
/// undefined/null), and each property read is try/caught, so a live getter that throws degrades to
/// `<throws>` instead of failing the whole eval — an honest partial answer beats no answer.
/// Identifiers are js-escaped exactly as [`generate_stub_js`] escapes them. Pure/unit-pinned.
fn build_bridge_introspection_js(ifaces: &[String]) -> String {
    let names = ifaces
        .iter()
        .map(|n| format!("\"{}\"", js_escape(n)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "(function(){{var r={{cefQuery:typeof window.cefQuery,ifaces:[]}};\
         [{names}].forEach(function(n){{var v=window[n];\
         var e={{name:n,type:typeof v,props:[]}};\
         if(v!==null&&(typeof v===\"object\"||typeof v===\"function\")){{\
         Object.getOwnPropertyNames(v).forEach(function(p){{var t;\
         try{{t=typeof v[p];}}catch(err){{t=\"<throws>\";}}\
         e.props.push([p,t]);}});}}\
         r.ifaces.push(e);}});return r;}})()"
    )
}

/// The bridge self-introspection evidence line (plan M6, 2026-07-16). The frame is identified by the
/// EXISTING redaction contract — a [`RedactedTarget`] (scheme+host, unforgeable from a raw URL), so
/// the absolute URL-redaction rule holds in the diagnostic exactly as everywhere else. `main_frame`
/// is CEF's own `is_main()` fact, not a page-observable. `result` is the JSON of OUR OWN
/// introspection script — Eclipse's own injected identifiers, never page content. Pure/unit-pinned.
fn format_bridge_diag_line(
    main_frame: bool,
    frame: &RedactedTarget,
    ok: bool,
    result_json: &str,
) -> String {
    format!(
        "bridge-introspect(diag) main_frame={main_frame} frame={} ok={ok} result={result_json}",
        frame.as_str()
    )
}

wrap_load_handler! {
    struct BridgeDiagLoadHandler {
        inventory: Arc<Mutex<HashMap<String, Vec<String>>>>,
    }

    impl LoadHandler {
        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: c_int,
        ) {
            let Some(frame) = frame else { return };
            // Deliberately NOT main-frame-gated (unlike the injection paths, which only ever inject
            // into the main frame): the challenge page has cross-origin subframes (console events
            // arrive from arkoselabs.roblox.com as well as js.rbxcdn.com), so "which frames did OUR
            // stub actually reach" is precisely the fact in question. main_frame is reported per
            // line instead.
            let ifaces: Vec<String> = match self.inventory.lock() {
                // Sorted so the line is stable across runs (HashMap iteration order is not).
                Ok(inv) => {
                    let mut names: Vec<String> = inv.keys().cloned().collect();
                    names.sort();
                    names
                }
                Err(_) => return,
            };
            // An EMPTY inventory is still worth evaluating: it reports whether OUR cefQuery router
            // reached this frame, and an empty list is itself the answer (the renderer's inventory
            // was empty here) — the same fact the mode=refresh/mode=sync split already measures.
            let script = build_bridge_introspection_js(&ifaces);
            let main_frame = frame.is_main() != 0;
            let raw_url = CefString::from(&frame.url()).to_string();
            let (ok, result_json) = eval_in_frame(frame, &script);
            log::warn(
                "render",
                &format_bridge_diag_line(
                    main_frame,
                    &RedactedTarget::from_raw_url(&raw_url),
                    ok,
                    &result_json,
                ),
            );
        }
    }
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
    // 2026-07-16 (plan M6): read the dev-host bridge SELF-INTROSPECTION gate ONCE. It MUST be read
    // HERE — before execute_process — and not beside the other diag reads below: those run in the
    // browser-only tail, but this diagnostic acts in the CEF-forked RENDERER (V8 lives on
    // TID_RENDERER), and the renderer returns from execute_process without ever reaching that tail.
    // This region is still single-threaded (the probe_userns invariant), so the env read is sound.
    // The var reaches the renderer the same way LD_LIBRARY_PATH already must for it to start at all
    // (Chromium launches subprocesses with the inherited environment); if that ever failed, the WARN
    // below would print with no bridge-introspect line following it — the log stays self-evidencing
    // rather than silently lying.
    let bridge_diag =
        engine::bridge_diag_enabled(std::env::var("ECLIPSE_WEBVIEW_BRIDGE_DIAG").ok().as_deref());
    // Announce from the BROWSER process only — exactly one loud line per boot (each renderer
    // process would otherwise repeat it).
    if bridge_diag && is_browser_process {
        log::warn(
            COMPONENT,
            "ECLIPSE_WEBVIEW_BRIDGE_DIAG=1 — the renderer will evaluate a bridge \
             SELF-INTROSPECTION script on every frame load-end and log what ECLIPSE'S OWN injected \
             stub looks like, AND the injected stubs themselves will trace their own invocation to \
             the page console with the ECLIPSE-STUB prefix (dev-host diagnostic only; first-party — \
             it reads only the globals Eclipse injected plus our own cefQuery router, never page \
             content; never a default). 2026-07-16: the stub trace reaches this log ONLY with \
             ECLIPSE_WEBVIEW_CONSOLE=1 as well — without it the console line carries severity+len \
             but no text, so the trace is emitted and NOT readable. Set BOTH.",
        );
    }
    // The renderer-side JS-bridge/eval handler (plan M4). The router + inventory are cheap to
    // construct here; the handler is only USED in the CEF-forked renderer subprocess (App's
    // render_process_handler is called there, never in the browser process).
    let inventory: Arc<Mutex<HashMap<String, Vec<String>>>> = Arc::new(Mutex::new(HashMap::new()));
    let render_handler = HelperRenderProcessHandler::new(
        RendererSideRouter::new(MessageRouterConfig::default()),
        inventory.clone(),
        // OFF → None → the renderer installs no load handler at all (a structural no-op).
        bridge_diag.then(|| BridgeDiagLoadHandler::new(inventory)),
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

    // 2026-07-16 (plan M6): resolve the User-Agent ONCE from the two inherited env channels (the
    // consumer's spawn does not env_clear). They MUST be read here rather than beside the
    // ECLIPSE_WEBVIEW_CONSOLE read below: the UA is an input to `Settings`, which `initialize`
    // consumes below, and `CefSettings.user_agent` is GLOBAL and fixed at CefInitialize.
    //   * ECLIPSE_WEBVIEW_APP_UA — THE UA THE APP SET via WebSettings.setUserAgentString, forwarded
    //     by the consumer at spawn. 2026-07-16 (§6 respawn): the parenthetical that stood here ("the
    //     ordering works because the helper spawns lazily on the first load-drive") was DISPROVED —
    //     a cookie op cold-started the helper 61 s earlier. The consumer now guarantees the ordering
    //     on its side: it defers the spawn where it can, and REPLACES a helper that booted on the
    //     wrong UA at the first load-drive. Nothing changes here: this process reads its env once and
    //     initializes once, which is all CefSettings.user_agent permits. This is the SHIPPED path:
    //     the app's own configuration, honored (§6 2026-07-16 💥) — not a diagnostic, so it is INFO.
    //   * ECLIPSE_WEBVIEW_UA_DIAG — the dev-host A/B override, which outranks it. WARN when in
    //     force: a forced UA means the boot is a measurement and NEVER a default.
    // Both values are the app's/operator's own public product token — not user data — so logging
    // them in full is in-policy (and a byte count could not be reproduced; see the overlay note).
    let ua_diag = std::env::var("ECLIPSE_WEBVIEW_UA_DIAG").ok();
    let app_ua = std::env::var("ECLIPSE_WEBVIEW_APP_UA").ok();
    let user_agent = engine::effective_user_agent(ua_diag.as_deref(), app_ua.as_deref());
    // Report which rung of the ladder actually won, so a boot log can never leave it ambiguous.
    if ua_diag.as_deref().is_some_and(|v| !v.is_empty()) {
        log::warn(
            COMPONENT,
            &format!(
                "ECLIPSE_WEBVIEW_UA_DIAG set — a FORCED diagnostic User-Agent is in force and \
                 OUTRANKS the app's own (dev-host A/B diagnostic only; never a default boot; the \
                 overlay's Java getUserAgentString() does NOT consult it and still reports the \
                 app's/fallback UA, so the two deliberately disagree): ua={user_agent}"
            ),
        );
    } else if app_ua.as_deref().is_some_and(|v| !v.is_empty()) {
        log::info(
            COMPONENT,
            &format!(
                "honoring the User-Agent the app set via WebSettings.setUserAgentString \
                 (ECLIPSE_WEBVIEW_APP_UA): ua={user_agent}"
            ),
        );
    } else {
        log::info(
            COMPONENT,
            "the app set no User-Agent — using Eclipse's fallback literal",
        );
    }

    // ---- Engine init: windowless, external pump, sandbox per the selected mode (ON except
    // the policy-gated Degraded opt-in), engine logging OFF. ----
    let mut settings = engine::build_settings_with_ua(user_agent);
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

    // 2026-07-10 (plan M6): read the dev-host page-console-TEXT diagnostic gate ONCE
    // (ECLIPSE_WEBVIEW_CONSOLE=1, inherited via the consumer's no-env_clear spawn). Announce it
    // loudly when ON — the page console text is page-controlled content, so a diag-enabled log is
    // by definition a dev-host artifact and NEVER a default boot.
    let console_text =
        engine::console_text_diag_enabled(std::env::var("ECLIPSE_WEBVIEW_CONSOLE").ok().as_deref());
    if console_text {
        log::warn(
            COMPONENT,
            "ECLIPSE_WEBVIEW_CONSOLE=1 — page console TEXT will be logged (dev-host diagnostic \
             only; page-controlled content; never a default)",
        );
    }

    // ---- The pump loop (CEF UI thread == this thread). ----
    let engine = Engine::new(outbox.clone(), console_text);
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
    fn generate_stub_js_emits_the_promise_shape_guarded_by_cefquery_and_escapes_identifiers() {
        // 2026-07-10 (plan M6): the bridge stub must (a) guard on window.cefQuery (so a page
        // without the router injected sees no throw), (b) return a Promise per method (the async
        // bridge shape), and (c) js_escape hostile identifier characters into the string literals.
        let js = generate_stub_js("Iface\"x", &["m\\1".to_string()], false);
        assert!(
            js.contains("window.cefQuery"),
            "must reference cefQuery: {js}"
        );
        assert!(
            js.contains("if(!q)"),
            "must guard on cefQuery presence: {js}"
        );
        assert!(
            js.contains("new Promise"),
            "each method returns a Promise: {js}"
        );
        assert!(js.contains("onSuccess") && js.contains("onFailure"), "{js}");
        // The hostile `"` and `\` are escaped, never emitted raw into the literal.
        assert!(
            js.contains("Iface\\\"x"),
            "interface name not escaped: {js}"
        );
        assert!(js.contains("m\\\\1"), "method name not escaped: {js}");
    }

    #[test]
    fn generate_stub_js_gate_off_is_byte_identical_to_the_pre_diag_stub() {
        // 2026-07-16 (plan M6): the stub-invocation trace must be STRUCTURALLY impossible with the
        // dev-host gate off. This pins the DEFAULT-boot stub byte-for-byte against the literal the
        // pre-diagnostic `generate_stub_js` emitted (commit e17390a), so any future edit that leaks
        // diagnostic text — or any other change — into the shipped stub fails here rather than in a
        // live boot. The trace is a measurement instrument; the default page must never see it.
        let js = generate_stub_js(
            "__globalRobloxAndroidBridge__",
            &["executeRoblox".to_string()],
            false,
        );
        assert_eq!(
            js,
            "(function(){var q=window.cefQuery;if(!q){return;}\
             var o=window[\"__globalRobloxAndroidBridge__\"]=window[\"__globalRobloxAndroidBridge__\"]||{};\
             [\"executeRoblox\"].forEach(function(m){o[m]=function(){\
             var a=Array.prototype.slice.call(arguments);\
             var r=JSON.stringify({iface:\"__globalRobloxAndroidBridge__\",method:m,args:a});\
             return new Promise(function(res,rej){q({request:r,\
             onSuccess:function(s){res(s===\"\"?undefined:JSON.parse(s));},\
             onFailure:function(c,msg){rej(new Error(msg));}});});};});})();"
        );
        // The prefix is the grep the live boot runs — it must not exist at all when gated off.
        assert!(
            !js.contains(STUB_TRACE_PREFIX),
            "trace leaked into the default stub: {js}"
        );
        assert!(
            !js.contains("console"),
            "console leaked into the default stub: {js}"
        );
    }

    #[test]
    fn generate_stub_js_gate_on_traces_the_invocation_and_never_binds_arg_values() {
        // 2026-07-16 (plan M6): THE GAP THIS CLOSES — across seven live boots the consumer's
        // `bridge call received` is ZERO, and "the page never calls the bridge" was INFERRED from
        // that. A page that DID call the stub while Eclipse dropped the call somewhere in
        // stub → cefQuery → router → on_query_str → BridgeCall → ART is INDISTINGUISHABLE from a
        // page that never called. This trace separates them, so it must actually bind the facts the
        // decision turns on — and must never bind an argument VALUE (the recorded no-arg-values
        // rule; framework.rs's bridge receipt binds arg_lens only).
        let js = generate_stub_js("Iface\"x", &["m\\1".to_string()], true);

        // (a) The static greppable prefix, plus the iface and method (`m` is the forEach binding,
        //     concatenated at call time — the runtime method name).
        assert!(js.contains(STUB_TRACE_PREFIX), "{js}");
        assert!(
            js.contains("iface=Iface\\\"x"),
            "iface not bound/escaped: {js}"
        );
        assert!(js.contains("method=\"+m"), "method not bound: {js}");
        // (b) The ARGUMENT COUNT — never the arguments themselves.
        assert!(
            js.contains("argc=\"+arguments.length"),
            "arg count not bound: {js}"
        );
        // (c) Every announcement point: the invisible installer bail, the invocation, the
        //     q-accepted bisect point, and both terminals.
        for event in ["no-cefQuery", "invoke", "sent", "success", "failure"] {
            assert!(
                js.contains(&format!("d(\"{event} ")),
                "missing {event} trace: {js}"
            );
        }
        // (d) NO ARG VALUES: the trace must never concatenate the argument array (`a`), an element
        //     of it, or the serialized payload (`r`) into a console line.
        for banned in [
            "\"+a+\"",
            "\"+a)",
            "+a+",
            "+JSON.stringify(a)",
            "\"+r+\"",
            "\"+r)",
            "argv",
            "args=\"+",
        ] {
            assert!(
                !js.contains(banned),
                "arg value token {banned:?} in the trace: {js}"
            );
        }
        // (e) The stub's SEMANTICS are unchanged — same guard, same payload, same Promise shape.
        assert!(js.contains("var q=window.cefQuery;if(!q){"), "{js}");
        assert!(
            js.contains("return;}"),
            "the cefQuery guard must still bail: {js}"
        );
        assert!(
            js.contains("var r=JSON.stringify({iface:\"Iface\\\"x\",method:m,args:a});"),
            "{js}"
        );
        assert!(
            js.contains("new Promise") && js.contains("onSuccess") && js.contains("onFailure"),
            "{js}"
        );
        // (f) js_escape still applies to the method-name array literal.
        assert!(js.contains("[\"m\\\\1\"]"), "method name not escaped: {js}");
        // (g) The trace can never throw into the page's own call path, and the console is captured
        //     at injection time (a page that later replaces window.console cannot silence it —
        //     a silenced trace would read as "absent", the exact false negative this must avoid).
        assert!(js.contains("var C=window.console,L=C&&C.log"), "{js}");
        assert!(js.contains("try{L.call(C,"), "{js}");
        assert!(
            js.contains("catch(e){}"),
            "the trace must never throw into the page: {js}"
        );
    }

    #[test]
    fn build_bridge_introspection_js_reads_only_our_inventory_and_never_scans_the_page() {
        // 2026-07-16 (plan M6): the bridge self-introspection diagnostic is FIRST-PARTY BY
        // CONSTRUCTION. (a) The iface list is exactly the inventory handed in — what ECLIPSE
        // injected — and each name is js-escaped like generate_stub_js escapes it. (b) It reports
        // OUR router + each iface's own property names/types. (c) It must never scan or read the
        // page: no window enumeration, no document/location/storage/navigator, no page state.
        let js = build_bridge_introspection_js(&["Iface\"x".to_string(), "B\\1".to_string()]);

        // (a) Exactly our inventory's ifaces, escaped — never a name scraped from the page.
        assert!(
            js.contains("[\"Iface\\\"x\",\"B\\\\1\"]"),
            "must iterate exactly the inventory's escaped iface names: {js}"
        );
        // (b) Our own router + our own object's shape.
        assert!(js.contains("typeof window.cefQuery"), "{js}");
        assert!(js.contains("Object.getOwnPropertyNames"), "{js}");
        assert!(js.contains("typeof v[p]"), "{js}");
        // getOwnPropertyNames throws on undefined/null — the guard is what keeps a missing stub a
        // reportable answer instead of a failed eval.
        assert!(js.contains("v!==null&&(typeof v===\"object\""), "{js}");

        // (c) No page scanning, no page-authored surface — the scope rule, asserted.
        for banned in [
            "Object.keys(window)",
            "getOwnPropertyNames(window)",
            "document",
            "location",
            "cookie",
            "navigator",
            "localStorage",
            "sessionStorage",
            "for(var k in window)",
        ] {
            assert!(
                !js.contains(banned),
                "page-scanning token {banned:?} in: {js}"
            );
        }

        // An EMPTY inventory still reports our router (and an empty iface list) — never a page scan.
        let empty = build_bridge_introspection_js(&[]);
        assert!(empty.contains("[].forEach"), "{empty}");
        assert!(empty.contains("typeof window.cefQuery"), "{empty}");
    }

    #[test]
    fn format_bridge_diag_line_keeps_the_frame_url_redacted() {
        // 2026-07-16 (plan M6): the diagnostic never relaxes the absolute URL-redaction rule — the
        // frame is bound ONLY through RedactedTarget (scheme+host), even though the challenge frame
        // URL carries a token. The result JSON is our own introspection output.
        let frame =
            RedactedTarget::from_raw_url("https://apps.roblox.com/challenge?token=SECRETTOKEN");
        let line = format_bridge_diag_line(true, &frame, true, "{\"cefQuery\":\"function\"}");
        assert!(line.contains("frame=https://apps.roblox.com"), "{line}");
        assert!(!line.contains("SECRETTOKEN"), "frame token leaked: {line}");
        assert!(!line.contains("/challenge"), "frame path leaked: {line}");
        assert!(
            line.contains("main_frame=true") && line.contains("ok=true"),
            "{line}"
        );
        assert!(
            line.contains("result={\"cefQuery\":\"function\"}"),
            "{line}"
        );
    }

    #[test]
    fn format_stub_apply_line_shape_is_counts_only() {
        // 2026-07-10 (plan M6): the A2 evidence line binds mode + counts ONLY (never names).
        assert_eq!(
            format_stub_apply_line("sync", 1, 3),
            "bridge stubs applied mode=sync ifaces=1 methods=3"
        );
        assert_eq!(
            format_stub_apply_line("refresh", 2, 5),
            "bridge stubs applied mode=refresh ifaces=2 methods=5"
        );
    }

    #[test]
    fn format_context_ready_line_binds_the_frame_kind_and_never_a_url() {
        // 2026-07-16 (plan M6, divergence #1): stub injection now follows AOSP's all-frames
        // contract, so this line fires per FRAME. It must (a) distinguish the frames rather than
        // claim "main-frame" for all of them — a subframe pull is the fix's expected signal — and
        // (b) stay URL-free: frame URLs reach the log only via `RedactedTarget`.
        assert_eq!(
            format_context_ready_line(true),
            "context ready (requested bridge inventory) main_frame=1"
        );
        assert_eq!(
            format_context_ready_line(false),
            "context ready (requested bridge inventory) main_frame=0"
        );
        // The two frame kinds must not collapse to the same line (that is what makes the
        // all-frames fix observable in a live log).
        assert_ne!(
            format_context_ready_line(true),
            format_context_ready_line(false)
        );
        for line in [
            format_context_ready_line(true),
            format_context_ready_line(false),
        ] {
            assert!(
                !line.contains("://"),
                "frame URL must never be logged: {line}"
            );
        }
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
