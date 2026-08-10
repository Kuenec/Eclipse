
mod engine;
mod logging;

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
use std::time::{Duration, Instant};

const COMPONENT: &str = "helper";

const HELLO_WATCHDOG: Duration = Duration::from_secs(10);

const CONTEXT_INIT_DEADLINE: Duration = Duration::from_secs(10);

const PUMP_INTERVAL: Duration = Duration::from_millis(10);

const OUT_QUEUE_HIGH_WATER: usize = 1024;

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

            return Err("--ipc-fd requires the --ipc-fd=<fd> form".to_string());
        }
    }
    found.ok_or_else(|| {
        "missing required --ipc-fd=<fd> (the spawn contract in src/webview/mod.rs)".to_string()
    })
}

fn parse_ozone_override<I: Iterator<Item = String>>(args: I) -> Option<String> {
    args.filter_map(|a| a.strip_prefix("--ozone-platform=").map(str::to_string))
        .last()
}

fn probe_userns() -> bool {

    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return false;
        }
        if pid == 0 {

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

fn suid_sandbox_stat_ok(is_file: bool, uid: u32, mode: u32) -> bool {
    is_file && uid == 0 && mode & 0o4000 != 0 && mode & 0o001 != 0
}

fn probe_suid_sandbox(exe_dir: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    let path = exe_dir.join("chrome-sandbox");
    let meta = std::fs::metadata(&path).ok()?;
    if !suid_sandbox_stat_ok(meta.is_file(), meta.uid(), meta.mode()) {
        return None;
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;

    (unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } == 0).then_some(path)
}

wrap_browser_process_handler! {
    struct HelperBrowserProcessHandler {
        context_initialized: Arc<AtomicBool>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            self.context_initialized.store(true, Ordering::Release);
            log::info(COMPONENT, "persistent global request context initialized");
        }
    }
}

wrap_app! {
    struct HelperApp {
        ozone: Arc<Mutex<Option<String>>>,
        render_handler: RenderProcessHandler,
        degraded_sandbox: Arc<AtomicBool>,
        browser_handler: BrowserProcessHandler,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(self.browser_handler.clone())
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(self.render_handler.clone())
        }

        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {

            let is_browser = process_type
                .map(|p| p.to_string().is_empty())
                .unwrap_or(true);
            let Some(cmd) = command_line else { return };
            if !is_browser {
                return;
            }

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

wrap_render_process_handler! {
    struct HelperRenderProcessHandler {
        router: Arc<RendererSideRouter>,
        inventory: Arc<Mutex<HashMap<String, Vec<String>>>>,
        bridge_diag: Option<LoadHandler>,
    }

    impl RenderProcessHandler {
        fn load_handler(&self) -> Option<LoadHandler> {

            self.bridge_diag.clone()
        }

        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            let frame_owned: Option<Frame> = frame.map(|f| f.clone());

            let ctx_for_eval: Option<V8Context> = context.as_deref().cloned();

            self.router.on_context_created(
                browser.map(|b| b.clone()),
                frame_owned.clone(),
                context.map(|c| c.clone()),
            );

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

                                if let Some(frame) = frame {
                                    let js =
                                        generate_stub_js(&iface, &methods, self.bridge_diag_on());
                                    frame.execute_java_script(
                                        Some(&CefString::from(js.as_str())),
                                        None,
                                        0,
                                    );

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

    fn bridge_diag_on(&self) -> bool {
        self.bridge_diag.is_some()
    }

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

fn format_stub_apply_line(mode: &str, ifaces: usize, methods: usize) -> String {
    format!("bridge stubs applied mode={mode} ifaces={ifaces} methods={methods}")
}

fn format_context_ready_line(main_frame: bool) -> String {
    format!(
        "context ready (requested bridge inventory) main_frame={}",
        u8::from(main_frame)
    )
}

const STUB_TRACE_PREFIX: &str = "ECLIPSE-STUB";

fn generate_stub_js(iface: &str, methods: &[String], diag: bool) -> String {
    let name = js_escape(iface);
    let methods_arr = methods
        .iter()
        .map(|m| format!("\"{}\"", js_escape(m)))
        .collect::<Vec<_>>()
        .join(",");

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

            let ifaces: Vec<String> = match self.inventory.lock() {

                Ok(inv) => {
                    let mut names: Vec<String> = inv.keys().cloned().collect();
                    names.sort();
                    names
                }
                Err(_) => return,
            };

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

        _ => (true, "null".to_string()),
    }
}

enum Command {
    Msg(ConsumerMsg),

    Dead,
}

fn write_helper_msg(stream: &UnixStream, msg: &HelperMsg) -> std::io::Result<()> {
    match msg.encode() {
        Ok(bytes) => (&mut &*stream).write_all(&bytes),
        Err(e) => {

            log::error(COMPONENT, &format!("outbound frame encode failed: {e}"));
            Ok(())
        }
    }
}

fn set_cloexec(stream: &UnixStream) {

    let ok = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    if ok != 0 {
        log::warn(COMPONENT, "could not set FD_CLOEXEC on the control socket");
    }
}

fn main() -> ExitCode {

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        log::error(COMPONENT, "cannot parse the command line");
        return ExitCode::FAILURE;
    };
    let is_browser_process = cmd_line.has_switch(Some(&CefString::from("type"))) != 1;

    let ozone_slot: Arc<Mutex<Option<String>>> = Arc::default();

    let degraded_sandbox: Arc<AtomicBool> = Arc::default();

    let bridge_diag =
        engine::bridge_diag_enabled(std::env::var("ECLIPSE_WEBVIEW_BRIDGE_DIAG").ok().as_deref());

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

    let inventory: Arc<Mutex<HashMap<String, Vec<String>>>> = Arc::new(Mutex::new(HashMap::new()));
    let render_handler = HelperRenderProcessHandler::new(
        RendererSideRouter::new(MessageRouterConfig::default()),
        inventory.clone(),

        bridge_diag.then(|| BridgeDiagLoadHandler::new(inventory)),
    );
    let context_initialized: Arc<AtomicBool> = Arc::default();
    let browser_handler = HelperBrowserProcessHandler::new(context_initialized.clone());
    let mut app = HelperApp::new(
        ozone_slot.clone(),
        render_handler,
        degraded_sandbox.clone(),
        browser_handler,
    );
    let ret = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if !is_browser_process {

        return ExitCode::from(ret.clamp(0, 255) as u8);
    }
    if ret != -1 {
        log::error(
            COMPONENT,
            &format!("execute_process consumed the browser process (ret={ret})"),
        );
        return ExitCode::FAILURE;
    }

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

    let stream = unsafe { UnixStream::from_raw_fd(ipc_fd) };
    set_cloexec(&stream);

    if stream.set_read_timeout(Some(HELLO_WATCHDOG)).is_err() {
        log::error(COMPONENT, "cannot arm the handshake watchdog");
        return ExitCode::from(2);
    }
    match proto::read_consumer_msg(&mut &stream) {
        Ok(ConsumerMsg::Hello { version }) if version == shared::PROTO_VERSION => {}
        Ok(ConsumerMsg::Hello { version }) => {

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
                    kind: 1,
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
                        kind: 1,
                        code: 2,
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

    let profile_paths = match std::env::var_os("ECLIPSE_WEBVIEW_DATA_DIR") {
        Some(root) if Path::new(&root).is_dir() => {
            match engine::persistent_profile_paths(Path::new(&root)) {
                Ok(paths) => paths,
                Err(reason) => {
                    log::error(COMPONENT, reason);
                    let _ = write_helper_msg(
                        &stream,
                        &HelperMsg::Crash {
                            view: 0,
                            kind: 1,
                            code: 3,
                        },
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
        Some(_) => {
            log::error(
                COMPONENT,
                "ECLIPSE_WEBVIEW_DATA_DIR is not an existing directory; the consumer must create \
                 the private persistent CEF root before spawning the helper",
            );
            let _ = write_helper_msg(
                &stream,
                &HelperMsg::Crash {
                    view: 0,
                    kind: 1,
                    code: 3,
                },
            );
            return ExitCode::FAILURE;
        }
        None => {
            log::error(
                COMPONENT,
                "missing ECLIPSE_WEBVIEW_DATA_DIR in the helper spawn environment; persistent \
                 cookies are required by Android CookieManager",
            );
            let _ = write_helper_msg(
                &stream,
                &HelperMsg::Crash {
                    view: 0,
                    kind: 1,
                    code: 3,
                },
            );
            return ExitCode::FAILURE;
        }
    };

    let ua_diag = std::env::var("ECLIPSE_WEBVIEW_UA_DIAG").ok();
    let app_ua = std::env::var("ECLIPSE_WEBVIEW_APP_UA").ok();
    let user_agent = engine::effective_user_agent(ua_diag.as_deref(), app_ua.as_deref());

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

    let mut settings = engine::build_settings_with_ua(user_agent);
    engine::apply_persistent_profile(&mut settings, &profile_paths);
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

    let context_deadline = Instant::now() + CONTEXT_INIT_DEADLINE;
    while !context_initialized.load(Ordering::Acquire) && Instant::now() < context_deadline {
        do_message_loop_work();
        std::thread::sleep(PUMP_INTERVAL);
    }
    if !context_initialized.load(Ordering::Acquire) {
        log::error(
            COMPONENT,
            "persistent global request context did not initialize within 10 seconds",
        );
        let _ = write_helper_msg(
            &stream,
            &HelperMsg::Crash {
                view: 0,
                kind: 1,
                code: 3,
            },
        );
        shutdown();
        return ExitCode::FAILURE;
    }

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

                    log::warn(COMPONENT, "consumer closed the control socket (EOF)");
                    let _ = cmd_tx.send(Command::Dead);
                    break;
                }
                Err(e) => {

                    log::error(COMPONENT, &format!("malformed control stream: {e}"));
                    let _ = cmd_tx.send(Command::Dead);
                    break;
                }
            }
        }
    });

    let console_text =
        engine::console_text_diag_enabled(std::env::var("ECLIPSE_WEBVIEW_CONSOLE").ok().as_deref());
    if console_text {
        log::warn(
            COMPONENT,
            "ECLIPSE_WEBVIEW_CONSOLE=1 — page console TEXT will be logged (dev-host diagnostic \
             only; page-controlled content; never a default)",
        );
    }

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

        std::process::exit(exit_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_fd_argument_is_required_and_validated() {

        let args = |v: &[&str]| {
            v.iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .into_iter()
        };

        let err = parse_ipc_fd(args(&["eclipse-webview"])).unwrap_err();
        assert!(err.contains("--ipc-fd"));

        assert!(parse_ipc_fd(args(&["x", "--ipc-fd=abc"])).is_err());
        assert!(parse_ipc_fd(args(&["x", "--ipc-fd=-1"])).is_err());
        assert!(parse_ipc_fd(args(&["x", "--ipc-fd", "3"])).is_err());

        assert!(parse_ipc_fd(args(&["x", "--ipc-fd=3", "--ipc-fd=4"])).is_err());

        assert_eq!(parse_ipc_fd(args(&["x", "--ipc-fd=3"])), Ok(3));
        assert_eq!(
            parse_ipc_fd(args(&["x", "--ozone-platform=x11", "--ipc-fd=7"])),
            Ok(7)
        );

        assert_eq!(
            parse_ozone_override(args(&["x", "--ozone-platform=wayland"])).as_deref(),
            Some("wayland")
        );
        assert_eq!(parse_ozone_override(args(&["x"])), None);
    }

    #[test]
    fn generate_stub_js_emits_the_promise_shape_guarded_by_cefquery_and_escapes_identifiers() {

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

        assert!(
            js.contains("Iface\\\"x"),
            "interface name not escaped: {js}"
        );
        assert!(js.contains("m\\\\1"), "method name not escaped: {js}");
    }

    #[test]
    fn generate_stub_js_gate_off_is_byte_identical_to_the_pre_diag_stub() {

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

        let js = generate_stub_js("Iface\"x", &["m\\1".to_string()], true);

        assert!(js.contains(STUB_TRACE_PREFIX), "{js}");
        assert!(
            js.contains("iface=Iface\\\"x"),
            "iface not bound/escaped: {js}"
        );
        assert!(js.contains("method=\"+m"), "method not bound: {js}");

        assert!(
            js.contains("argc=\"+arguments.length"),
            "arg count not bound: {js}"
        );

        for event in ["no-cefQuery", "invoke", "sent", "success", "failure"] {
            assert!(
                js.contains(&format!("d(\"{event} ")),
                "missing {event} trace: {js}"
            );
        }

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

        assert!(js.contains("[\"m\\\\1\"]"), "method name not escaped: {js}");

        assert!(js.contains("var C=window.console,L=C&&C.log"), "{js}");
        assert!(js.contains("try{L.call(C,"), "{js}");
        assert!(
            js.contains("catch(e){}"),
            "the trace must never throw into the page: {js}"
        );
    }

    #[test]
    fn build_bridge_introspection_js_reads_only_our_inventory_and_never_scans_the_page() {

        let js = build_bridge_introspection_js(&["Iface\"x".to_string(), "B\\1".to_string()]);

        assert!(
            js.contains("[\"Iface\\\"x\",\"B\\\\1\"]"),
            "must iterate exactly the inventory's escaped iface names: {js}"
        );

        assert!(js.contains("typeof window.cefQuery"), "{js}");
        assert!(js.contains("Object.getOwnPropertyNames"), "{js}");
        assert!(js.contains("typeof v[p]"), "{js}");

        assert!(js.contains("v!==null&&(typeof v===\"object\""), "{js}");

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

        let empty = build_bridge_introspection_js(&[]);
        assert!(empty.contains("[].forEach"), "{empty}");
        assert!(empty.contains("typeof window.cefQuery"), "{empty}");
    }

    #[test]
    fn format_bridge_diag_line_keeps_the_frame_url_redacted() {

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

        assert_eq!(
            format_context_ready_line(true),
            "context ready (requested bridge inventory) main_frame=1"
        );
        assert_eq!(
            format_context_ready_line(false),
            "context ready (requested bridge inventory) main_frame=0"
        );

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

        assert!(suid_sandbox_stat_ok(true, 0, 0o104755));

        assert!(!suid_sandbox_stat_ok(true, 0, 0o104750));
        assert!(!suid_sandbox_stat_ok(true, 0, 0o104700));

        assert!(!suid_sandbox_stat_ok(true, 0, 0o100755));

        assert!(!suid_sandbox_stat_ok(true, 1000, 0o104755));

        assert!(!suid_sandbox_stat_ok(false, 0, 0o104755));
    }
}
