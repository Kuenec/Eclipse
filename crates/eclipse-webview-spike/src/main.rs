//! Eclipse M1 web-engine spike (docs/web-engine-plan.md §6 M1) — the standalone go/no-go proof
//! that CEF 149 initializes AND renders a live public page on the dev host, in both windowed and
//! windowless (software-OSR `on_paint`) modes. No app wiring, no ART, no APK, no challenge URL.
//!
//! Subcommands (deterministic markers; no retries-until-green, no fake markers):
//!   * `windowed` — CEF opens its own native window, loads https://www.roblox.com, logs
//!     load-start/load-finished, auto-closes ~20 s after load-finished, prints
//!     `ECLIPSE_WEBVIEW_SPIKE_WINDOWED_SUCCESS`.
//!   * `osr`      — windowless software rendering; after load-finished + a short settle, writes
//!     the most recent BGRA frame as `/tmp/eclipse-webview-spike-osr.png`, verifies the buffer is
//!     not uniform (distinct-pixel census), prints `ECLIPSE_WEBVIEW_SPIKE_OSR_SUCCESS` + WxH +
//!     distinct-pixel count.
//!
//! If a required callback never fires, the run times out and prints a
//! `ECLIPSE_WEBVIEW_SPIKE_<MODE>_FAILURE reason=...` line instead.
//!
//! 2026-07-03: API shapes follow the upstream tauri-apps/cef-rs examples at tag
//! export-cef-dir-v149.3.0+149.0.6 (examples/cefsimple + examples/osr) and the docs.rs 149.3.0
//! trait signatures. Extra engine flags (e.g. `--ozone-platform=wayland`, and `--no-sandbox` only
//! if initialization is impossible without it) pass through this binary's own argv: CEF parses
//! the full command line itself via `MainArgs`.
//!
//! Privacy: the absolute URL-redaction rule extends to the spike from day one — every URL that
//! reaches a log line goes through [`scheme_and_host_for_log`] (scheme+host only).

use cef::{args::Args, rc::Rc as _, sys, *};
use std::collections::HashSet;
use std::os::raw::c_int;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The live public page the M1 plan names. Scheme+host only — safe to log verbatim.
const TARGET_URL: &str = "https://www.roblox.com";
const OSR_PNG_PATH: &str = "/tmp/eclipse-webview-spike-osr.png";
/// OSR view size served via `RenderHandler::view_rect`.
const OSR_WIDTH: c_int = 1024;
const OSR_HEIGHT: c_int = 768;
/// Deadline for the main-frame load-finished callback before an honest FAILURE line.
const LOAD_DEADLINE: Duration = Duration::from_secs(90);
/// Windowed: keep the rendered page visible this long after load-finished, then auto-close.
const WINDOWED_LINGER: Duration = Duration::from_secs(20);
/// OSR: settle time after load-finished so late paints (fonts, images) land in the frame.
const OSR_SETTLE: Duration = Duration::from_secs(6);
/// How long a requested browser close may take before we give up waiting for on_before_close.
const CLOSE_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Windowed,
    Osr,
}

impl Mode {
    fn tag(self) -> &'static str {
        match self {
            Mode::Windowed => "WINDOWED",
            Mode::Osr => "OSR",
        }
    }
}

/// Shared spike state, mutated only from CEF UI-thread callbacks (== this process's main thread:
/// multi_threaded_message_loop is off and we pump `do_message_loop_work` from `main`).
#[derive(Default)]
struct SpikeState {
    load_started: bool,
    load_finished_at: Option<Instant>,
    main_frame_error: Option<String>,
    before_close: bool,
    /// Most recent OSR frame: (width, height, BGRA bytes).
    frame: Option<(i32, i32, Vec<u8>)>,
    paint_count: u64,
}

type Shared = Arc<Mutex<SpikeState>>;

fn now_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

/// Absolute URL-redaction rule (AGENTS.md 2026-07-02 record): scheme+host only, never the rest.
fn scheme_and_host_for_log(url: &str) -> String {
    match url.find("://") {
        Some(pos) => {
            let after = &url[pos + 3..];
            let host_end = after.find('/').unwrap_or(after.len());
            format!("{}://{}", &url[..pos], &after[..host_end])
        }
        None => "<non-url>".to_string(),
    }
}

wrap_app! {
    struct SpikeApp;

    impl App {}
}

wrap_load_handler! {
    struct SpikeLoadHandler {
        state: Shared,
        start: Instant,
    }

    impl LoadHandler {
        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            let is_main = frame.map(|f| f.is_main() != 0).unwrap_or(false);
            println!(
                "[{} ms] load-start main_frame={is_main} target={TARGET_URL}",
                now_ms(self.start)
            );
            if is_main {
                self.state.lock().expect("state lock").load_started = true;
            }
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            http_status_code: c_int,
        ) {
            let is_main = frame.map(|f| f.is_main() != 0).unwrap_or(false);
            println!(
                "[{} ms] load-finished main_frame={is_main} http_status={http_status_code} target={TARGET_URL}",
                now_ms(self.start)
            );
            if is_main {
                let mut st = self.state.lock().expect("state lock");
                if st.load_finished_at.is_none() {
                    st.load_finished_at = Some(Instant::now());
                }
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            _error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let is_main = frame.map(|f| f.is_main() != 0).unwrap_or(false);
            let code = sys::cef_errorcode_t::from(error_code) as i32;
            // Redaction: only ever the scheme+host of the failed URL.
            let redacted = failed_url
                .map(|u| scheme_and_host_for_log(&u.to_string()))
                .unwrap_or_else(|| "<none>".to_string());
            println!(
                "[{} ms] load-error main_frame={is_main} code={code} failed_host={redacted}",
                now_ms(self.start)
            );
            // ERR_ABORTED (-3) is navigation churn, not a page failure.
            if is_main && code != sys::cef_errorcode_t::ERR_ABORTED as i32 {
                let mut st = self.state.lock().expect("state lock");
                if st.load_finished_at.is_none() {
                    st.main_frame_error = Some(format!("code={code} host={redacted}"));
                }
            }
        }
    }
}

wrap_life_span_handler! {
    struct SpikeLifeSpanHandler {
        state: Shared,
        start: Instant,
    }

    impl LifeSpanHandler {
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            println!("[{} ms] on_before_close", now_ms(self.start));
            self.state.lock().expect("state lock").before_close = true;
        }
    }
}

wrap_render_handler! {
    struct SpikeRenderHandler {
        state: Shared,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                rect.x = 0;
                rect.y = 0;
                rect.width = OSR_WIDTH;
                rect.height = OSR_HEIGHT;
            }
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> c_int {
            if let Some(screen_info) = screen_info {
                screen_info.device_scale_factor = 1.0;
                return true as c_int;
            }
            false as c_int
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: c_int,
            height: c_int,
        ) {
            // PET_VIEW frames only (the default variant), same filter as the upstream example.
            if type_ != PaintElementType::default() {
                return;
            }
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            // SAFETY: CEF guarantees `buffer` points to width*height BGRA pixels for the
            // duration of this callback (cef_render_handler_t::on_paint contract); we copy out.
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer, (width * height * 4) as usize) };
            let mut st = self.state.lock().expect("state lock");
            st.paint_count += 1;
            st.frame = Some((width, height, bytes.to_vec()));
        }
    }
}

wrap_client! {
    struct SpikeClient {
        load_handler: LoadHandler,
        life_span_handler: LifeSpanHandler,
        render_handler: Option<RenderHandler>,
    }

    impl Client {
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn render_handler(&self) -> Option<RenderHandler> {
            self.render_handler.clone()
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: eclipse-webview-spike <windowed|osr> [extra Chromium flags...]");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    // Initialize the CEF API version before any other CEF call (upstream example order).
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        eprintln!("ECLIPSE_WEBVIEW_SPIKE_FAILURE reason=cannot-parse-command-line");
        return ExitCode::FAILURE;
    };
    let is_browser_process = cmd_line.has_switch(Some(&CefString::from("type"))) != 1;

    let mut app = SpikeApp::new();
    let ret = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );

    if !is_browser_process {
        // A CEF-forked subprocess (renderer/gpu/utility): execute_process ran it to completion.
        return ExitCode::from(ret.clamp(0, 255) as u8);
    }
    if ret != -1 {
        eprintln!("ECLIPSE_WEBVIEW_SPIKE_FAILURE reason=execute-process-consumed-browser-process ret={ret}");
        return ExitCode::FAILURE;
    }

    // Our own subcommand: the first positional argument. Extra flags flow through to CEF.
    let mode = match std::env::args().nth(1).as_deref() {
        Some("windowed") => Mode::Windowed,
        Some("osr") => Mode::Osr,
        _ => return usage(),
    };

    run_spike(mode, &args, &mut app)
}

fn fail(mode: Mode, reason: &str) -> ExitCode {
    println!(
        "ECLIPSE_WEBVIEW_SPIKE_{}_FAILURE reason={reason}",
        mode.tag()
    );
    ExitCode::FAILURE
}

fn run_spike(mode: Mode, args: &Args, app: &mut App) -> ExitCode {
    let start = Instant::now();
    let windowless = (mode == Mode::Osr) as c_int;

    let settings = Settings {
        windowless_rendering_enabled: windowless,
        // Pump the CEF loop ourselves (upstream examples/osr shape): timeouts + auto-close stay
        // on this thread. no_sandbox stays 0 — the real sandbox mode is part of the M1 evidence.
        external_message_pump: 1,
        ..Default::default()
    };
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(app),
        std::ptr::null_mut(),
    ) != 1
    {
        return fail(mode, "cef-initialize-returned-0");
    }
    println!(
        "[{} ms] cef-initialized windowless={windowless}",
        now_ms(start)
    );

    let state: Shared = Arc::new(Mutex::new(SpikeState::default()));
    let load_handler = SpikeLoadHandler::new(state.clone(), start);
    let life_span_handler = SpikeLifeSpanHandler::new(state.clone(), start);
    let render_handler = match mode {
        Mode::Osr => Some(SpikeRenderHandler::new(state.clone())),
        Mode::Windowed => None,
    };
    let mut client = SpikeClient::new(load_handler, life_span_handler, render_handler);

    let window_info = WindowInfo {
        windowless_rendering_enabled: windowless,
        ..Default::default()
    };
    let browser_settings = BrowserSettings {
        windowless_frame_rate: 30,
        ..Default::default()
    };
    let url = CefString::from(TARGET_URL);
    let mut browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&url),
        Some(&browser_settings),
        None,
        None,
    );
    if browser.is_none() {
        shutdown();
        return fail(mode, "browser-create-returned-none");
    }
    println!("[{} ms] browser-created target={TARGET_URL}", now_ms(start));

    let mut close_requested_at: Option<Instant> = None;
    let mut osr_stats: Option<(i32, i32, usize)> = None;
    let mut failure: Option<String> = None;

    loop {
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(10));

        let (load_finished_at, main_frame_error, before_close) = {
            let st = state.lock().expect("state lock");
            (
                st.load_finished_at,
                st.main_frame_error.clone(),
                st.before_close,
            )
        };

        if before_close {
            // Close completed (ours, or the user closed the window early — logged either way).
            if close_requested_at.is_none() {
                println!("[{} ms] browser closed externally", now_ms(start));
                if load_finished_at.is_none() && failure.is_none() {
                    failure = Some("window-closed-before-load-finished".to_string());
                }
            }
            break;
        }

        if let Some(requested) = close_requested_at {
            if requested.elapsed() > CLOSE_DEADLINE {
                println!(
                    "[{} ms] WARN: on_before_close never fired within {CLOSE_DEADLINE:?}; giving up on a clean close",
                    now_ms(start)
                );
                break;
            }
            continue; // Just pump until the close completes.
        }

        // Failure paths first: main-frame hard error, then the load deadline.
        if failure.is_none() {
            if let Some(err) = main_frame_error {
                failure = Some(format!("main-frame-load-error {err}"));
            } else if load_finished_at.is_none() && start.elapsed() > LOAD_DEADLINE {
                failure = Some(format!(
                    "load-finished-never-fired-within {LOAD_DEADLINE:?} (load_started={})",
                    state.lock().expect("state lock").load_started
                ));
            }
            if failure.is_some() {
                request_close(&mut browser, &mut close_requested_at, start);
                continue;
            }
        }

        let Some(finished_at) = load_finished_at else {
            continue;
        };

        match mode {
            Mode::Windowed => {
                if finished_at.elapsed() >= WINDOWED_LINGER {
                    println!(
                        "[{} ms] linger complete ({WINDOWED_LINGER:?} after load-finished); auto-closing",
                        now_ms(start)
                    );
                    request_close(&mut browser, &mut close_requested_at, start);
                }
            }
            Mode::Osr => {
                if finished_at.elapsed() >= OSR_SETTLE {
                    let frame = state.lock().expect("state lock").frame.clone();
                    let paint_count = state.lock().expect("state lock").paint_count;
                    match frame {
                        None => failure = Some("no-on-paint-frame-after-load-finished".to_string()),
                        Some((w, h, bgra)) => {
                            println!(
                                "[{} ms] capturing OSR frame {w}x{h} (paint_count={paint_count})",
                                now_ms(start)
                            );
                            match write_png_and_census(w, h, &bgra) {
                                Ok(distinct) if distinct > 1 => osr_stats = Some((w, h, distinct)),
                                Ok(distinct) => {
                                    failure =
                                        Some(format!("uniform-frame distinct_pixels={distinct}"))
                                }
                                Err(e) => failure = Some(format!("png-write-failed {e}")),
                            }
                        }
                    }
                    request_close(&mut browser, &mut close_requested_at, start);
                }
            }
        }
    }

    // Release the browser reference before shutdown (CEF requires all refs gone), then only
    // shut down if the close actually completed — shutdown() with a live browser aborts.
    let closed_cleanly = state.lock().expect("state lock").before_close;
    drop(browser);
    if closed_cleanly {
        shutdown();
        println!("[{} ms] cef-shutdown complete", now_ms(start));
    } else {
        println!(
            "[{} ms] WARN: skipping cef shutdown() after unclean close",
            now_ms(start)
        );
    }

    if let Some(reason) = failure {
        return fail(mode, &reason.replace(' ', "_"));
    }
    match mode {
        Mode::Windowed => println!("ECLIPSE_WEBVIEW_SPIKE_WINDOWED_SUCCESS"),
        Mode::Osr => {
            let (w, h, distinct) = osr_stats.expect("osr success requires stats");
            println!(
                "ECLIPSE_WEBVIEW_SPIKE_OSR_SUCCESS width={w} height={h} distinct_pixels={distinct} png={OSR_PNG_PATH}"
            );
        }
    }
    if !closed_cleanly {
        // Honest note next to the marker; success criteria (init+load+ink) were still met.
        println!("NOTE: browser close was unclean; exiting without cef shutdown()");
        // Skip destructors that would touch CEF state.
        std::process::exit(0);
    }
    ExitCode::SUCCESS
}

fn request_close(
    browser: &mut Option<Browser>,
    close_requested_at: &mut Option<Instant>,
    start: Instant,
) {
    if close_requested_at.is_some() {
        return;
    }
    if let Some(host) = browser.as_ref().and_then(|b| b.host()) {
        println!("[{} ms] requesting browser close", now_ms(start));
        host.close_browser(1);
        *close_requested_at = Some(Instant::now());
    }
}

/// Writes the BGRA frame as RGBA PNG to [`OSR_PNG_PATH`] and returns the distinct-pixel count.
fn write_png_and_census(w: i32, h: i32, bgra: &[u8]) -> Result<usize, String> {
    let expected = (w as usize) * (h as usize) * 4;
    if bgra.len() != expected {
        return Err(format!(
            "frame-size-mismatch len={} expected={expected}",
            bgra.len()
        ));
    }

    let mut distinct: HashSet<u32> = HashSet::new();
    let mut rgba = Vec::with_capacity(expected);
    for px in bgra.chunks_exact(4) {
        distinct.insert(u32::from_ne_bytes([px[0], px[1], px[2], px[3]]));
        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }

    let file = std::fs::File::create(OSR_PNG_PATH).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(distinct.len())
}
