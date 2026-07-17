# Eclipse — Web-Engine Plan for the Challenge WebView

> **Status:** Locked direction / decision record + phased plan. Owner decision **(a)** made
> **2026-07-03**. Companion to [`dependency-plan.md`](./dependency-plan.md) (the dependency
> justification) and `AGENTS.md` §6 (2026-07-03 🧭, the decision-log entry). Every milestone
> gates the next. **M1 is DONE — GO; M2 is DONE (2026-07-03, drive-verified); M3 is DONE
> (2026-07-03, `__webview-test`-verified); M4 is DONE (2026-07-10, `__webview-test`-verified);
> M5 is DONE (2026-07-10, packaged + four-leg live-verified);** implementation continues at
> **M6** (2026-07-16: the M6 live boots RAN — the 2026-07-10 pass validated + a deeper
> Looper-affinity root cause found and fixed; the milestone is NOT complete — the bridge is still
> silent, now proven an independent defect ranked to UA-steering / open question #1).

## 1. Why a web engine at all (the recorded ceiling)

2026-07-03: the challenge15 boot proved the challenge fragment's lifecycle completes with
**zero** remaining native failures and **zero** new `UnsatisfiedLinkError`s — the
native-binding ceiling on the login-challenge path is REACHED (AGENTS.md §6 2026-07-03 ✅🔘).
Eclipse has no web engine, so the Arkose/FunCaptcha-style **web** challenge renders nothing
inside the constructed-but-empty `ChallengeHybridWebView` and times out (~60 s) to the
recovery LoginV2 every boot. The boundary is behavioral, not a missing binding (§6 2026-07-02
🌐): `internalLoadChanged` never fires, `WebViewClient.onPageStarted/onPageFinished` never
run, and login cannot proceed past the 403 challenge.

**The owner decision (2026-07-03):** option **(a)** — integrate a real non-GTK web engine for
the challenge WebView (rejecting option (b), a non-web login/challenge path).

## 2. Chosen direction

**CEF/Chromium (the tauri-apps [`cef`](https://crates.io/crates/cef) crate, currently
v149.x as of 2026-07-03), embedded in an Eclipse-owned OUT-OF-PROCESS `eclipse-webview` Rust
helper binary:** windowless OSR, BGRA frames over memfd into the existing vk-overlay present
seam, input/load-events/cookies/JS bridge over a small std-only owned Unix-socket protocol —
**zero engine bytes ever mapped into the ART main process.**

**Runner-up (the recorded fallback):** Servo — the `servo` crate (pinned to its half-yearly
LTS stream) in the identical out-of-process helper shape; triggered only if the verification
vendor's server-side scoring refuses CEF-shaped clients at M6, or the owner rejects the CEF
footprint/release treadmill. It must itself be gated on a days-scale servoshell spike against
the real challenge URL before any integration work is committed to it.

## 3. Rubric justification (stability > pure-Rust > no-bloat, AGENTS.md §3)

AGENTS.md §3 orders the rubric **stability > pure-Rust > no-bloat** and states #2/#3 never
override #1, so a three-lens engineering review (stability / purity-bloat / integration
judges) resolves by rubric, not averaging: the stability and integration lenses both rank CEF
first; the purity-bloat lens's Servo-first ordering is precisely the ordering §3
subordinates.

- **Stability (the dominant axis, and the one Eclipse can never patch itself):** will a
  commercial anti-bot widget render AND complete — and keep doing so for years as the vendor
  updates it — on the chosen engine? CEF is the only candidate that fully outsources that
  axis: it IS current Chromium, the engine family the widget vendor builds and tests against
  (Arkose documents Android-WebView and Chrome support), shipped as prebuilt bundled binaries
  identical on every distro, with the best GPU degradation story (Mesa, NVIDIA, bundled
  SwiftShader software fallback). Servo carries a measured ~19.8%-of-Baseline web-compat gap
  that structurally widens, zero public evidence of Arkose-class widgets running on it, and
  monthly breaking embedder churn — a compat break in year two would be terminal.
- **Integration:** every recorded Eclipse seam maps 1:1 onto decade-mature CEF APIs —
  `internalLoadChanged(0/3)` ← `OnLoadStart`/`OnLoadEnd`; `CookieManager` ←
  `CefCookieManager` (incl. the overlay 3-arg `setCookie` / `.ROBLOSECURITY` handoff);
  `addJavascriptInterface` ← renderer-side V8 handler + `CefMessageRouter` (incl. synchronous
  returns); OSR `OnPaint` ← the proven memfd + vk-overlay precedents.
- **Pure-Rust (§2.1) survives where the charter measures it — "every line we own":** the
  helper, IPC, compositing, and JNI plumbing are all Rust; the `cef` bindings are
  machine-generated from CEF's stable C API and loaded via `libloading` (regenerable if
  tauri-apps stalls). `libcef` becomes the **second vendored non-Rust black box** under the
  recorded libsqlite3-sys precedent ("no pure-Rust X is production-grade — stability >
  purity", `dependency-plan.md`), written up there and logged in AGENTS.md §6 before the
  dependency lands, audited with `cargo tree`/`cargo bloat`, with a pinned SHA1-verified
  artifact download and the Chromium third-party license aggregate shipped.
- **No-bloat (§2.5) is honored by confinement, not denial:** the ~320–400 MB stripped/
  locale-pruned payload never touches the main `eclipse` binary, its unit-test gate, or the
  gameplay hot path (§2.4). The helper is spawned lazily on the first real load-drive of a
  challenge WebView and killed after completion/timeout — dead weight on disk otherwise, zero
  per-frame/per-event cost when absent, **no async runtime** (std `UnixStream` only).

## 4. The non-negotiables (they shape the architecture, not the choice)

- **NO engine maps into the main process.** In-process CEF (1.375 GB unstripped `libcef`,
  V8's ~1 TB virtual reservation, crashpad signal handlers beside ART's fault handler, plus
  `DT_NEEDED` glib/atk/cairo/pango) is exactly the large-native-stack-at-startup class the
  recorded Step-3.5 low_4gb thesis banned GTK for (`art-and-runtime.md`, `src/graphics.rs`).
  The helper-process split is **mandatory**: the ART process gains only a Unix socket plus a
  memfd frame mapping (kernel places both high; near-zero low-4GB footprint),
  renderer/GPU processes fork from the helper never from Eclipse, and engine crashes are
  isolated from ART and the game.
- **Off the gameplay hot path (§2.4):** the engine exists only for the login challenge; no
  per-frame or per-event cost when no WebView is live.
- **"Integrates, never duplicates":** the helper's per-view identity is the existing
  `view_registry` widget handle returned by the (already bound) shared
  `native_constructor` — no new webview_registry.
- **The URL-redaction rule is absolute and extends across the new process boundary from day
  one** (2026-07-02 record): all URL logging routes through the
  `url_scheme_and_host_for_log` contract (scheme+host only); load payloads are never bound to
  any log macro at any level, in the main process or the helper.
- **Distro-agnostic, detect-don't-assume (§2.9):** bundled identical engine everywhere (no
  distro packages CEF — bundling is the portable answer), runtime probes with actionable
  errors for the `DT_NEEDED` host-lib set, the sandbox mode (SUID `chrome-sandbox` vs
  unprivileged userns vs loud documented degradation), Wayland (XWayland /
  `--ozone-platform`) and X11, GPU vs SwiftShader.
- **Faithful rendering only:** Eclipse renders the challenge page for a real human to
  complete and hands the callback back through the app's own WebView contract — it never
  automates or engineers around vendor scoring.

## 5. Rejected candidates (honest summaries)

- **WPE WebKit (helper):** viable engine, rejected as primary — zero Rust bindings exist
  (Eclipse would own a new hand-rolled unsafe GObject FFI surface); the embedder API is
  mid-sunset (libwpe dies at 2.54, WPEPlatform a may-change preview until ~Sept 2026);
  host-provided coverage fails Ubuntu 24.04+ and Fedora outright; WPEPlatform has no X11
  platform; and its practical fallback is out-of-process WebKitGTK, which needs an explicit
  owner ruling against the recorded GTK ban's letter.
- **Pure-Rust (Blitz/Stylo + Boa):** non-viable — Blitz executes no JavaScript by design and
  Boa has no DOM; the missing JS↔DOM web platform is ~5–15 engineer-years of owned browser
  code before the widget's first script could run, and the finished artifact would still face
  vendor environment-screening as an alien engine.
- **External browser (xdg-open / portal):** non-viable for the stated contract — the widget's
  completion token is delivered to in-page JS with no redirect-URI step, so it dies in the
  foreign tab (cookie jars severed; the app's challenge fragment still times out at ~60 s
  exactly as challenge13/14/15 record). Every working reformulation is option (b), which the
  owner rejected — plus a privacy regression (token-bearing URL in argv/`/proc/cmdline`).
- **In-process embedding of ANY engine (CEF, Servo, or WPE inside the `eclipse` binary):**
  rejected by the recorded low_4gb mechanism behind the GTK ban — mapping a 100 MB–1.4 GB
  engine, JS-engine address-space reservations, dozens of threads, and foreign signal
  handlers into the process whose low-4GB window ART/LOS needs is exactly the Step-3.5 class
  the boot thesis proved fatal. The helper process is mandatory, not optional.

## 6. Phased milestones (each gates the next; per-milestone verify checks)

### M1 — Standalone CEF spike on the dev host (no app wiring; the go/no-go proof) — **DONE: GO (2026-07-03)**

New scratch crate outside the `eclipse` binary (e.g. `crates/eclipse-webview-spike` or the
session scratchpad): `cef` crate + `download-cef` fetches the pinned CEF 149 `linux64_minimal`
artifact (SHA1-verified); `CefInitialize` and render a live public HTTPS page (e.g.
`https://www.roblox.com`) in CEF's own window; repeat windowless (OSR software `OnPaint`)
dumping BGRA frames to PNG. Exercise both a Wayland session (XWayland; note
`--ozone-platform` behavior) and an X11 session; record which sandbox mode initializes (SUID
`chrome-sandbox` / unprivileged userns / `--no-sandbox`), the `DT_NEEDED` host-lib probe
results, GPU vs SwiftShader path, and the measured on-disk footprint after strip +
locale-prune. NOT `cargo run -- run`; no ART, no APK, no challenge URL — a public page only.

**Verify:** the spike binary runs on the dev host under both `WAYLAND_DISPLAY` and X11
`DISPLAY` sessions showing the rendered live page, and the OSR run writes a PNG with nonzero
ink; sandbox mode, host-lib set, render path, and footprint numbers are captured in the spike
notes for the §6 record. Failure here (engine cannot initialize/render on this host) stops
the plan before any integration cost.

**Status — DONE, GO (2026-07-03; independently re-run and verified).** Spike crate
`crates/eclipse-webview-spike` (standalone, workspace-detached — the libm-shim pattern; `cef`
pinned `=149.3.0` → CEF 149.0.6 / Chromium 149.0.7827.201 `linux64_minimal`, SHA1-verified
`d46ec0d5723771bd1c9678c429e1cdb1f1ef0a72`). Measured on the dev host:

- **Display paths** (windowed AND OSR each SUCCESS): (a) session default — ozone AUTO-selected
  **native Wayland**, not XWayland; (b) explicit `--ozone-platform=wayland` — **answers
  open-question #6 positively on this host** (windowed included, libX11 merely present);
  (c) X11 via `XDG_SESSION_TYPE=x11` + `DISPLAY`; (d) explicit `--ozone-platform=x11`. The one
  designed failure: `WAYLAND_DISPLAY` unset while `XDG_SESSION_TYPE=wayland` — ozone auto
  still picks Wayland and cannot connect, so the M2 helper must select/probe the ozone
  platform explicitly, never trust auto.
- **Sandbox mode:** unprivileged user-namespace sandbox engaged **by default** — `--no-sandbox`
  never passed (`Settings.no_sandbox=0` every run). Kernel-level probe of the live process
  tree: zygote+renderer forks in a new user/PID/net namespace, `NoNewPrivs=1`,
  renderer/utility `Seccomp=2` (filter mode). Shipped `chrome-sandbox` is mode 0755 (not
  SUID), so the SUID mode is impossible as-shipped; host userns availability was verified,
  not assumed (`kernel.unprivileged_userns_clone=1`).
- **Host-lib probe:** `ldd` on `libcef.so` — **0 "not found"** (32 direct `DT_NEEDED`
  incl. nss/nspr, atk/atspi, cups, gbm, xkbcommon, cairo/pango, asound, libX11; full closure
  ~70 libs, all resolved on this host).
- **Render path:** hardware GPU (NVIDIA 610.43.02 via CEF's bundled ANGLE EGL/GL — Chromium
  disables Vulkan under ozone-wayland); `libvk_swiftshader.so` never mapped in any run.
- **OSR ink proof:** 1024x768 (exactly the served view_rect) BGRA→RGBA PNG of the live
  rendered roblox.com signup page; distinct-pixel census 122,859–123,156 across runs;
  load-start ~0.36–0.71 s, load-finished http 200 ~0.7–1.4 s; clean
  `close_browser`→`on_before_close`→`shutdown`, exit 0, no orphan Chromium processes.
- **Footprint:** as-downloaded extracted 1452 MB (archive 297 MB); `strip` of `libcef.so`
  1,375,259,784 → 256,322,688 bytes (dir total 385 MB); + locale prune to en-US (220 files,
  50 MB → 0.56 MB) = **336 MB** — inside the plan's ~320–400 MB envelope (~8 MB of that is
  SDK sources that would not ship).
- **M2-carried findings:** Chromium's `--enable-logging=stderr` console forwarding prints
  FULL page URLs — the helper must keep engine stderr logging off/filtered (the absolute
  redaction rule); the roblox.com captcha bootstrap ran on CEF 149 (early positive signal for
  open-question #1).

Full record: `AGENTS.md` §6 2026-07-03 🧪 (M1 verdict entry).

### M2 — Dependency-justification cycle + real `eclipse-webview` helper + owned IPC protocol

Write the full stability>pure-Rust>no-bloat justification (`cef`, `cef-dll-sys`,
`download-cef`; pinning + SHA1 + MOZJS-free supply chain; strip/prune plan; the Chromium
license-attribution obligation; the libsqlite3-sys/ART precedent) into `dependency-plan.md`
and append the decision to AGENTS.md §6 / update §5. Promote the spike into the helper crate:
one windowless browser per view handle, software `OnPaint` BGRA → memfd, lazy spawn/kill
lifecycle, and a small owned std-only Unix-socket protocol (in: loadUrl /
loadDataWithBaseURL / input / evaluateJs / cookie ops; out: load-state 0/3, frame-ready,
console/crash) — no tokio, no new async runtime. Helper logging inherits the absolute
redaction rule: scheme+host only via the `url_scheme_and_host_for_log` contract, payloads
never bound to any log macro.

**Verify:** full quality gate green (fmt / build --all-targets / clippy -D warnings /
cargo test / release) with new plain-`cargo test` units for protocol framing and helper-side
redaction (no CEF or display needed); a dev-host standalone helper run loads a public page
headless and emits load-started/finished events plus a nonzero-ink frame over the socket;
`dependency-plan.md` + AGENTS.md §5/§6 entries exist before the dependency merges.

**Status — DONE (2026-07-03; gate + dev-host drive run verified).**

- **Protocol v1 FROZEN.** `src/webview/proto.rs` is the normative message-set/framing spec
  (this doc keeps only the summary): one `SOCK_STREAM` UnixStream (socketpair end at fd 3
  per the spawn contract in `src/webview/mod.rs`), frames `[len u32 LE][type u8][body]`,
  16 consumer→helper types (0x01–0x10) + 8 helper→consumer (0x81–0x88), global 8 MiB cap
  before allocation + per-type caps, `Hello`/`HelloAck` exact-version handshake (10 s
  watchdog), symmetric typed malformed-input contract (close loudly + payload-free; helper
  exit 2). Frame transport: one sealed memfd per (view × size) generation, 2 BGRA slots,
  `SCM_RIGHTS` on a `0xF5` sentinel byte right after `FrameBufferNew`; torn frames impossible
  by the `SlotTracker` publish/ack invariant (never write a published-unacked slot;
  latest-wins coalescing into the spare).
- **Crate layout.** Root gains `src/webview/` (mod/proto/redact/fdpass/shm/slots — std+libc
  only, zero cef); the REAL helper `crates/eclipse-webview` (workspace-DETACHED, `cef
  =149.3.0` + `libc`, committed `Cargo.lock`) `#[path]`-includes those files verbatim via
  `src/shared.rs` (sibling-module-shape invariant), so the shared units run under BOTH
  `cargo test` gates. Binaries: `eclipse-webview` (helper) + `eclipse-webview-drive` (the M2
  verify driver / reference consumer).
- **Measured drive run (dev-host, Wayland session, ozone selected explicitly `wayland`):**
  handshake → CreateView 1024x768 → LoadUrl <https://www.roblox.com> → `LoadState` 0 at
  ~354 ms and 3 at ~867 ms `http_status=200` over the socket → `FrameBufferNew` + memfd
  received/verified/mapped → `FrameReady` census **122,925 distinct pixels** (the M1 range
  122,859–123,156 — the rendered live page) → mouse move/click smoke → `CookieGet`→
  `CookieList` round-trip (12 cookies, names/domains only) → `CloseView`→`ViewClosed` →
  `Shutdown` → helper exit 0, child reaped, /proc orphan scan clean →
  `ECLIPSE_WEBVIEW_M2_DRIVE_SUCCESS`. LoadState fidelity finding fixed during M2: the
  `CreateView` about:blank bootstrap navigation's 0/3 events are suppressed (the Android
  `internalLoadChanged` contract fires only for DRIVEN loads) — pinned by
  `load_state_suppresses_the_about_blank_bootstrap_but_never_driven_loads`.
- **Redaction (the absolute rule, across the boundary by construction):** engine logging OFF
  at the source (`build_settings` pins `log_severity=DISABLE`, no log file, sandbox ON;
  `--enable-logging`/`--no-sandbox` stripped from any pass-through command line) +
  `on_console_message` returns 1 (suppressing the M1-measured console-to-stderr URL leak) +
  the `Console` wire message is STRUCTURALLY text-free (`Console::from_raw` redacts the
  source and keeps only the text length). One canonical `url_scheme_and_host_for_log`
  (moved verbatim `framework.rs` → `src/webview/redact.rs`, call sites/test unchanged);
  same-pattern audit fixed the spike's weaker local copy (leaked query text on path-less
  URLs) by switching it to the shared module. Drive-log privacy grep: scheme+host only.
- **Dependency cycle complete:** `dependency-plan.md` (real dated `cef = "=149.3.0"` +
  `download-cef`-as-tooling entries, vendored-table M2 note) and the AGENTS.md §5/§6 records
  were authored BEFORE/WITH the dependency (the §2.1 enforcement order); the root
  `Cargo.toml`/`Cargo.lock` verified free of cef\*.
- Implementation continues at **M3**.

### M3 — Main-process wiring at the recorded seams, validated against a public page via a hidden dev-host subcommand

Replace the bodies of the two validated no-op load natives (`src/framework.rs`
`web_view_native_load_url` ~:9325, `web_view_native_load_data_with_base_url` ~:9389, 2026-07-03
coordinates) with spawn-and-forward keyed by the existing `view_registry` widget handle
("integrates, never duplicates" — no new webview_registry); a socket-reader thread attaches
to the VM and fires the dead seam `WebView.internalLoadChanged(0/3)` as JNI upcalls
(`EnvUnowned`/catch_unwind house shape) so `WebViewClient.onPageStarted/onPageFinished` run
for the first time; composite helper frames at the `view_registry` frame rect through the
existing vk-overlay present seam; route winit mouse/key events to the helper while a WebView
is live; an absent/failed helper degrades to the current honest one-shot WARN no-op with an
actionable error (never a crash, never a fabricated callback). Add the hidden dev-host
diagnostic subcommand `__webview-test` (house pattern of `__gl-test`) printing a
deterministic SUCCESS marker; extend the regression pins
(`web_view_native_names_sigs_and_class_match_the_installed_dex` unchanged, the redaction test
extended to the IPC boundary, new protocol pins; the live `bound=3` registration line
preserved).

**Verify:** quality gate green; dev-host `cargo run -- __webview-test` renders a live public
page inside Eclipse's own window via the full natives→socket→helper→memfd→vk-overlay path,
fires `internalLoadChanged` 0 and 3, and prints the SUCCESS marker; a self-skipping guard for
that marker added to `tests/engine_milestones.rs` per the existing convention.

**Status — DONE (2026-07-03; gate + dev-host `__webview-test` run verified).**

- **Main-process client shipped.** NEW `src/webview/client.rs` (1624 lines; std-only, zero
  cef): 4-tier helper resolver (config `webview_helper_path` → `ECLIPSE_WEBVIEW_HELPER` env →
  beside-the-eclipse-binary → dev-tree target), io-thread spawn + handshake per the
  `src/webview/mod.rs` spawn contract, a pure dispatch state machine, staged frames, a
  one-way failure latch, and the input/compositor/lifecycle surface. `shm::FrameMapping`'s
  `!Send` is confined to the io thread via the `SendMapping` newtype (SAFETY-commented).
- **The two load natives are LIVE.** `framework.rs` `web_view_native_load_url`/
  `web_view_native_load_data_with_base_url` are spawn-and-forward keyed by the existing
  `view_registry` widget handle ("integrates, never duplicates" — no new webview_registry);
  the one-shot-WARN fallback is preserved — an absent/failed helper degrades honestly (never
  a crash, never a fabricated callback). `fire_web_view_internal_load_changed` fires the
  previously-dead `WebView.internalLoadChanged(0/3)` as real JNI upcalls (local ref taken
  under the registry lock, dispatch OUTSIDE it — app code may re-enter). Teardown:
  `view_native_destructor` → `client::notify_view_freed`.
- **Composite + input.** `vk_overlay.rs` gains `WEB_COMPOSITE` + `composite_webview_frame`
  with pure unit-pinned `classify_swapchain_format`/`bgra_rows_into`/`clamp_webview_rect`
  behind an `active_view()` atomic fast gate (§2.4: zero per-frame cost when no WebView is
  live); `graphics.rs` routes winit mouse/key events to the helper in the four `handed_off`
  arms (cached-registry-rect-only capture); `view_registry::absolute_frame()` supplies the
  composite rect; `main.rs` gains `__webview-test`/`run_webview_test`.
- **Measured `__webview-test` run (dev-host main thread, 2026-07-03, log
  `/tmp/eclipse-webview-m3-test.log`, 42 lines):** `timeout 180 cargo run --release --
  __webview-test` → EXIT=0 with the SUCCESS marker `WebView engine pipeline OK:
  internalLoadChanged upcalls 2/2 (state 0 @ 350ms, state 3 @ 700ms, http 200), frame
  1024x768 122925 distinct pixels, ViewClosed, helper exit 0`. Booted REAL ART from the
  default APK (framework classpath; no libroblox preload, no window), drove `WebView.loadUrl`
  through the production native path (`handle=4294967296`), helper resolved tier-4 dev-tree,
  ozone `wayland` explicit; ink census 122,925 EXACTLY inside the M1/M2 range
  122,859–123,156; clean ViewClosed → shutdown → helper exit 0; privacy grep of the log:
  0 full-URL-shaped lines (scheme+host only); /proc orphan scan clean.
- **HONEST PLAN-WORDING DIVERGENCE (recorded, not papered over):** this section's verify
  sentence says the page renders "inside Eclipse's own window via … vk-overlay" — the
  composite executes only under an engine `vkQueuePresentKHR`, so it first runs ON-SCREEN at
  the M6 live boot. `__webview-test` proves natives→socket→helper→memfd→main-process staging
  + real upcalls + `bound=3`; the composite's pure parts are unit-pinned. The GL-path
  composite is documented as deferred (seam noted in `vk_overlay.rs`).
- **Gate:** root fmt / build --all-targets / clippy -D warnings / test / release ALL clean —
  **613 unit + 6 integ + 2 doctest** (was 604+4+2; +11 new webview/overlay/registry pins
  incl. the ALWAYS-RUN `root_lockfile_stays_cef_free` cef-freedom pin and the self-skipping
  `webview_test_fires_load_upcalls_and_stages_frames` guard — which executed FOR REAL on this
  host: all 6 `engine_milestones` guards ran their real paths, zero SKIP). Helper crate gate
  fmt --check / build / clippy -D warnings / test **17 + 11** clean (`CEF_PATH` = the reused
  M1 dist). Root `Cargo.toml`/`Cargo.lock` verified cef-free (the 4 `cef` substring hits in
  Cargo.lock are hex-checksum digits).
- **Adversarial review (3 dimensions, high effort): 0 confirmed findings.** privacy-redaction
  0; portability-scope 0; jni-threading 1 alleged (CLIENT mutex held across a blocking
  up-to-8-MiB socket write while the reader needs CLIENT for FrameAck → deadlock under helper
  backpressure) REFUTED by the skeptic with mechanism: the helper's consumer-socket reader
  thread drains unconditionally into an UNBOUNDED mpsc queue independent of its writer, so
  the consumer `write_all` always completes — the backpressure premise is false. Nothing
  changed post-review.
- **Carried to M6:** the ATL 2-arg `onPageStarted` vs AOSP 3-arg reconciliation; pre-driven
  self-navigation silence (the driven-loads-only contract); input routing vs the
  centered-fallback composite rect (the fallback rect deliberately does NOT capture input —
  dated note in `webview_relative_point`); challenge-rect fidelity / `ResizeView` wiring
  deferred to M6 per the recalibration recipe.
- Implementation continues at **M4**.

### M4 — JS bridge + cookies + UA: the completion-handoff surface (overlay pass)

Back `addJavascriptInterface` for real (renderer-side V8 handler + `CefMessageRouter`
synthesizing the exact `window.<name>` objects, forwarded helper→Eclipse→JNI into the app's
`@JavascriptInterface` methods; test the synchronous-return corner), `evaluateJavascript` →
`CefFrame::ExecuteJavaScript` with the result routed to the overlay-patched `ValueCallback`,
`CookieManager` get/set incl. the overlay 3-arg `setCookie` (the
CookieProtocol/`.ROBLOSECURITY` handoff) via `CefCookieManager` with a private session-scoped
store, and replace the recorded "GDPR VIOLATION" UA with an honest deliberate UA. All
Java-surface changes go through `tools/framework-overlay/patch-framework.sh` (never stub a
static-final constant — the 2026-07-02 include-id lesson); overlay-patch the known
`javascript:`-println full-URL leak channel now that `javascript:` URLs become live and
secret-bearing.

**Verify:** overlay rebuilds green through its own §-guards and installs; quality gate green;
`__webview-test` extended with a first-party local test page proving bridge round-trip (page
JS → injected object → JNI method → evaluateJavascript back), cookie set/get round-trip
through `CookieManager`, and the honest UA — deterministic SUCCESS marker; a privacy grep of
the run shows scheme+host-only URL lines and zero payload text.

**Status — DONE (2026-07-10; gate + dev-host `__webview-test` run verified).**

- **Protocol v2 shipped as a versioned ADDITIVE extension — v1 layouts stay FROZEN.**
  `PROTO_V1` → `PROTO_VERSION = 2` with the existing exact-match handshake as the negotiation
  channel (no capability bits — the helper ships from the same build tree, so consumer and
  helper are always one version; a skewed peer answers `HelloAck{its version}` and closes,
  and the consumer latches the M3 honest one-shot-WARN no-op degradation). New wire types
  ct `0x11–0x14` (`BridgeRegister`/`BridgeResult`/`EvaluateJsForResult`/`CookieSetForResult`)
  + ht `0x89–0x8B` (`BridgeCall`/`EvaluateJsResult`/`CookieSetResult`), all with caps
  enforced BEFORE allocation in the total-decoder shape; the FROZEN v1 round-trip pin is
  untouched (still asserts exactly 24 messages) and a separate v2 pin covers the 7 new ones.
  Bridge/eval/cookie payloads are OPAQUE to the helper — forwarded unparsed, logged as
  lengths/counts only.
- **JS bridge = async Promise (the DOCUMENTED DIVERGENCE from AOSP's synchronous
  `@JavascriptInterface` contract).** CEF's only cross-process JS↔native primitive
  (`CefMessageRouter`/`cefQuery`) is asynchronous; a synchronous return would block the
  sandboxed renderer main thread on renderer→browser→socket→ART→back, which CEF does not
  offer and which risks renderer deadlock. Renderer-side V8 stubs make every
  `window.<name>.<method>()` return a Promise; browser-side router → `BridgeCall` over the
  socket → ART reflect-invoke of the retained `@JavascriptInterface` methods (the annotation,
  RUNTIME-retained via the new overlay class, is the security gate; overloads resolved by
  ARGUMENT COUNT per the Chromium gin java-bridge reference) → `BridgeResult` → the page's
  Promise. The renderer main thread is NEVER blocked; a page expecting a synchronous return
  gets a Promise — re-validate against `RBHybridWebView`'s real shape at M6 (open question
  #3). Inventory delivery is a PULL model (the renderer signals `eclipse.bridge.ready` per
  main-frame context and the browser re-sends the per-view inventory — the design's push was
  live-observed dropped by CEF).
- **Cookies = ONE session-scoped private in-memory `RequestContext`** (empty `cache_path`,
  `persist_session_cookies=0`) shared by every browser and every cookie op — the
  `.ROBLOSECURITY` handoff lands in the store the challenge WebView reads. The overlay
  `CookieManager` is native-backed end-to-end (get / 2-arg set / 3-arg set / removeAll /
  removeSession / flush-no-op, NEW registrar `bound=6`), replacing the fabricated
  `Boolean.TRUE` with the real async result; `getCookie` is a bounded blocking round-trip
  (5 s, honest-empty degrade).
- **Honest deliberate UA** (`Mozilla/5.0 (X11; Linux x86_64) … Chrome/149.0.0.0 …
  Eclipse-WebView/149.0.6`) set helper-side in `build_settings()` AND returned by the overlay
  `WebSettings` (`getUserAgentString` + `getDefaultUserAgent`) — byte-matched and pinned in
  both units; the recorded "GDPR VIOLATION" literal is GONE. It is genuinely Chromium 149 on
  Linux x86_64 and deliberately identifying — never impersonating a device; the M6 owner
  caveat on vendor reception stays open question #1.
- **The `javascript:`-println full-URL leak channel is PATCHED**: the overlay `loadUrl` now
  routes `javascript:` URLs to `native_evaluateJavascript` and the println is gone — landed
  BEFORE such URLs become secret-bearing, exactly as this section requires (M6 must
  overlay-validate it against the real challenge flow). WebView natives grew `bound=3` →
  **`bound=5`** (`native_evaluateJavascript` + `native_addJavascriptInterface`); the
  `@JavascriptInterface` runtime annotation + the inert `EclipseBridgeProbe` test class are
  new javac classes (classes.dex); WebView + WebSettings are shadowed into classes2.dex
  behind the established anchor/pristine/back-check §-guards.
- **Threading (the post-review hardening):** a dedicated `eclipse-webview-upcall` thread owns
  ALL app-facing JNI (internalLoadChanged, bridge reflect-invokes, every ValueCallback, the
  deferred closed-view bridge drop) — the socket reader is fully JNI-free; era-gated drains
  give exactly-once, honest-failure callback delivery on every loss path (helper crash/EOF,
  shutdown, renderer death, view close incl. the close+re-drive corner); reader exit wakes
  parked `getCookie` waiters immediately.
- **Measured `__webview-test` run (dev-host main thread, 2026-07-10, log
  `/tmp/eclipse-webview-m4-test.log`, 52 lines):** `timeout 180 ./target/release/eclipse
  __webview-test` → EXIT=0 with the SUCCESS marker `WebView engine pipeline OK:
  internalLoadChanged upcalls 2/2 (state 0 @ 150ms, state 3 @ 150ms, http 200), frame
  1024x768 237 distinct pixels, bridge round-trip OK, evaluateJavascript OK, honest UA OK,
  cookie set/get OK, cookie callback OK, ViewClosed, helper exit 0, bound=5` plus the
  CookieManager `bound=6` registration line. The driven page is now an OFFLINE first-party
  loopback page (`http://127.0.0.1:<port>` — a real http origin for cookies/bridge; the M3
  live-roblox.com dependency moves to M6), hence the small ink census (237 distinct pixels —
  the solid-color test page, not the M1–M3 roblox.com range 122,859–123,156); the handshake
  logged `engine=cef/149.0.6+g0d0eeb6+chromium-149.0.7827.201 protocol=2`; helper resolved to
  the release binary; privacy greps: 0 full-URL-shaped lines (scheme+host only), 0
  ROBLOSECURITY-shaped strings — bridge method names, cookie values, the UA and eval results
  are deliberately never logged; /proc orphan scan clean. An earlier identical run on the
  pre-close-out tree also passed EXIT=0 (state 0/3 @ 200 ms).
- **Gate:** root fmt / build --all-targets / clippy -D warnings / test / release ALL clean —
  **629 unit + 6 integ + 2 doctest, ZERO SKIP** (was 613+6+2 at M3; the `__webview-test`
  guard drove the full natives→socket→helper→memfd pipeline through REAL ART); helper crate
  **23 + 15** clean (was 17+11; `CEF_PATH` = the reused M1 dist); root
  `Cargo.toml`/`Cargo.lock` stay cef-free.
- **Review ledger (three-workflow chain):** main workflow (recon ×3 → design → implement →
  independent gate → adversarial review ×3 dimensions, every finding skeptic-verified) — 7
  alleged / 7 CONFIRMED / ALL FIXED (ValueCallback JNI-global leaks across
  teardown/shutdown/latch; the reader-thread reentrancy self-deadlock class — a synchronous
  `getCookie` from any upcall stalled the io thread 5 s and returned a wrong empty; the eval
  degrade dropping its callback unfired; `reader_fatal` leaving getCookie waiters parked;
  `@JavascriptInterface` overload collapse). Post-fix review — 4 alleged: 2 CONFIRMED + fixed
  (never-driven-view bridge leak; stale queued ViewClosedDrain vs close+re-drive), 1 REFUTED
  with mechanism (the GC-finalize trigger is unreachable — Eclipse's own per-view jobject
  global pins every constructed View), 1 unadjudicated (skeptic died) → re-adjudicated by the
  close-out pass: CONFIRMED (the reader's ViewClosed bridge drop did hidden scoped JNI
  attach/detach per global) + fixed (the drop moved to the upcall thread, era-gated). Stale
  protocol-v1 strings fixed tree-wide (the handshake log binds the negotiated `protocol=2`).
- **Carried:** the M3 carried-to-M6 list stays carried (ATL 2-arg `onPageStarted`
  reconciliation; driven-loads-only contract; centered-fallback rect no-input;
  challenge-rect/`ResizeView` at M6). TWO new owner-flagged unconfirmed nuances (bounded,
  recorded not fixed): an in-flight eval callback of a finalized (notify-path) view whose
  result raced the finalize is held until the helper-teardown drain; the pre-existing
  `pending_bridges` inventory-loss nuance on close+re-drive.
- Implementation continues at **M5**.

### M5 — Distro-agnostic hardening + packaging

Runtime detect-don't-assume with actionable errors: `DT_NEEDED` host-lib probe (nss/nspr,
atk/atspi, cups, gbm, xkbcommon, cairo/pango, alsa, libX11), sandbox-mode selection (SUID
`chrome-sandbox` vs unprivileged userns vs loud, documented degradation — per the M1/M6 owner
ruling), Wayland (XWayland required or `--ozone-platform=wayland` validated) vs X11, GPU vs
bundled SwiftShader fallback. Packaging: `download-cef` pinned + SHA1-verified at package
time, `libcef` stripped (1.375 GB → ~256 MB), locales pruned per the owner decision, the
Chromium third-party license aggregate shipped, no hardcoded paths, Flatpak
(`org.freedesktop.Platform`) layout sketched.

**Verify:** quality gate green from a clean checkout at a different path; `__webview-test`
passes on both a Wayland and an X11 session on the dev host with the detection lines logged;
a simulated missing-host-lib run (bwrap/env harness) produces the actionable error, not a
crash; the measured shipped-payload size recorded in §6.

**Status — DONE (2026-07-10; gate + clean-path cold gate + FOUR dev-host `__webview-test`
live legs verified).**

- **Sandbox-mode selection = the three-tier ladder** (the implemented answer to open question
  #2; the full dated policy record is AGENTS.md §6 2026-07-10 🔒): (1) unprivileged user
  namespaces, verified USABLE by a live fork-child probe (`unshare(CLONE_NEWUSER)` + one
  capability-gated syscall INSIDE the new namespace — bare creation is NOT the predicate;
  catches Ubuntu 24.04's permit-then-confine AppArmor default); (2) SUID `chrome-sandbox`
  beside libcef.so, accepted by Chromium's OWN predicate (root-owned, `S_ISUID`, `S_IXOTH`,
  `access(X_OK)`); (3) neither → REFUSE pre-init with the typed `SandboxUnavailable`
  actionable error — unless the user explicitly set `webview_allow_unsandboxed = true`, which
  selects a LOUD logged `--no-sandbox` degradation (never a default). Both capability inputs
  are MEASURED live, never knob-file guesses.
- **`DT_NEEDED` host-lib probe + enriched spawn post-mortem:** NEW `src/webview/hostprobe.rs`
  resolves the 26 direct DT_NEEDED of libcef.so pre-spawn; a miss WARNs with the exact soname
  + per-distro package names (apt/dnf/pacman), and the consumer's handshake failure
  distinguishes the dynamic-linker exit 127 from a normal usage failure, folding the probe
  result into ONE actionable error. The simulated missing-lib run (bwrap masking
  `/usr/lib/libnss3.so`) produced exactly that error — controlled EXIT=1, no panic/crash
  (log `/tmp/eclipse-webview-m5-test-missinglib.log`).
- **Display + render path:** Wayland and X11 detected and the ozone platform set EXPLICITLY
  (`# display: wayland (WAYLAND_DISPLAY set)` / `# display: x11 (DISPLAY set, WAYLAND_DISPLAY
  unset)`); GPU vs bundled SwiftShader is LOG-ONLY detection by design (Chromium owns the
  selection; this NVIDIA host takes the GPU path; SwiftShader ships as the automatic
  fallback, not separately exercised).
- **Packaging (`tools/webview-dist/package-webview.sh`, exit 0):** pin verified
  `cef_binary_149.0.6+g0d0eeb6+chromium-149.0.7827.201_linux64_minimal.tar.bz2` (sha1
  `d46ec0d5723771bd1c9678c429e1cdb1f1ef0a72`, sha256
  `f90dec4c5c42a7bbd4f2bd80a7a77e0ac6aacfc6627bb43572d803e77f26dfbc`); the build-input
  libcef.so sha256-matches the verified tarball and the ship-set is extracted ONLY from it;
  libcef.so stripped 1,375,259,784 → 256,322,688 bytes (byte-identical to the M1 reference);
  locales pruned to en-US (en-US.pak 588,985 B + 3 18-byte grammatical-gender stubs);
  CREDITS.html (19,678,258 B) + LICENSE.txt shipped; RUNPATH=`$ORIGIN` (the helper
  `.cargo/config.toml` contract) re-verified; no-display packaged-layout smoke OK.
  **PACKAGED PAYLOAD: 355428085 bytes (340M)** at `dist/eclipse-linux-x86_64` (§7 #5 —
  recorded in AGENTS.md §6). Flatpak stays a SKETCH (`tools/webview-dist/README.md`; §7 #7 —
  full validation trails).
- **Gate:** root fmt --check / build --all-targets / clippy -D warnings / test / release ALL
  exit 0 — **638 unit + 6 integ + 2 doctest, ZERO SKIP** (was 629+6+2 at M4); helper crate
  **28 + 15** clean (was 23+15). Clean-path portability: pristine rsync copy (no
  target/dist/.git) at `$HOME/eclipse-m5-cleanpath` — the FULL cold gate green in BOTH crates
  (root 153 crates cold, helper 87 cold), same counts, 0 SKIP verified with `--nocapture`,
  the detection needles verified there too.
- **The four live legs (dev-host main thread, 2026-07-10, all zero-orphan):** Wayland EXIT=0
  (`/tmp/eclipse-webview-m5-test-wayland.log`); X11 EXIT=0 via Xwayland with WAYLAND_DISPLAY
  unset (`/tmp/eclipse-webview-m5-test-x11.log`) — HONEST caveat: a pure-X11-server host was
  not available, so §7 #6 (Wayland-only hosts without XWayland / bare-X11 breadth) stays OPEN
  for M6+; packaged layout EXIT=0 against the dist payload, probe 26/26, handshake
  `engine=cef/149.0.6+g0d0eeb6+chromium-149.0.7827.201 protocol=2`
  (`/tmp/eclipse-webview-m5-test-packaged.log`); the missing-host-lib sim controlled EXIT=1
  (above). All three EXIT=0 legs end in the unchanged M4 SUCCESS marker.
- **Carried to M6:** the M4 carried list unchanged (the ATL 2-arg `onPageStarted`
  reconciliation; the driven-loads-only contract; centered-fallback rect no-input;
  challenge-rect/`ResizeView`; the two owner-flagged eval/bridge nuances; overlay-validate
  the `javascript:`-URL leak fix against the real challenge flow); the §7 #4 release-cadence
  ruling stays OPEN (pin-pair mechanics landed — AGENTS.md §6 2026-07-10 🔒); §7 #6 and #7
  stay open as above.
- Implementation continues at **M6**.

### M6 — Live challenge boot: the challenge page renders in the app, then the human completes it

**Status (2026-07-16) — THE LIVE BOOTS RAN. The 2026-07-10 pass is VALIDATED, a deeper root cause
was found and FIXED, and the milestone is NOT yet complete: the bridge is still silent, now proven
to be an INDEPENDENT defect (full record: AGENTS.md §6 2026-07-16 🧵).** Three live boots
(`/tmp/eclipse-challenge17.log`, `…18-consolediag.log`, `…19-looper.log`; dev-host main thread,
EXIT=124, zero-orphan). **Validated:** (C) the 3-arg `onPageStarted` fix is CONFIRMED — the app's
real override ran for the first time ever (suspect 3 dead); (D) eager CloseView is CONFIRMED —
teardown 2 ms + 29 ms `ViewClosed` vs challenge16's ~40 s stale composite; (A) the console
diagnostic worked and was decisive. **(B) did NOT engage** — both boots log `mode=refresh`, never
`mode=sync` (a fresh renderer's inventory is empty at `on_context_created`), but it did not matter:
stubs land ~490 ms before the page's wrapper speaks, so **the injection race is ruled out**.
**NEW ROOT CAUSE, FIXED:** every app-facing WebView callback was delivered on the Looper-less
`eclipse-webview-upcall` thread, so ATL `Handler.<init>` threw *"Can't create handler inside thread
that has not called Looper.prepare()"* inside BOTH real page callbacks — exactly 2×/boot, every
boot (via `SwipeRefreshLayout.setRefreshing` → `View.startAnimation` → `new Handler()`). AOSP
delivers these on the UI thread and never on a Looper-less thread. They now run on Eclipse's ART
main thread via a pure-Rust job slot drained by the existing main-Looper pump; the poster blocks so
the `upcalls 2/2` marker keeps its exact meaning and global FIFO is preserved. `@JavascriptInterface`
deliberately stays on the upcall thread (AOSP's own thread identity; deferred + instrumented).
**Live verdict:** Looper throws 2→0, `internalLoadChanged` failures 2→0, lifecycle failures 3→1
(only the known upgrade-dialog), the app's own `onPageFinished. url=` log 0→1, zero new ULE/NPE.
The new overlay `EclipseWebViewClientProbe` gives the pinned `__webview-test` marker teeth for the
first time (pre-fix it FAILS 0/2; post-fix 2/2). **Frontier:** the page's console fingerprint is
byte-identical pre/post fix — the bridge silence is a separate bug. The page fires all four hybrid
calls but every one goes `to origin: undefined` (postMessage vocabulary, not Android-bridge
vocabulary). Ranked suspect was **UA steering (open question #1)**.

**UA A/B RAN 2026-07-16 — SUSPECT 2 CONFIRMED; open question #1 is now an OWNER RULING WITH
EVIDENCE (record: AGENTS.md §6 2026-07-16 🕵️).** A temporary env-gated `ECLIPSE_WEBVIEW_UA_DIAG`
override (default UNCHANGED, its `!contains("Android")` pin still green) gave a clean
3-control/1-treatment result: `challengeCompleted` appears **0×** in challenge17/18/19 (honest
desktop UA) and **1×** in challenge20 (the app's genuine Android-WebView UA). The
`generic-challenge-type=proofofwork` challenge **genuinely completes** under the Android-WebView
context and genuinely does not under a desktop-Linux Chromium UA. **But it does not unblock login:**
`bridge call received` is 0 under BOTH UAs — the wrapper still says `to origin: undefined`, so the
completed challenge is never delivered and the ~60 s `Load generic challenge failed` → LoginV2
recovery is unchanged. **The bridge is a THIRD, independent defect** (injection race ruled out;
callback delivery fixed; UA ruled IN for the challenge path, OUT as the bridge's cause). Methodology
caveat now recorded: the console `len=` fingerprint MISSED this (`challengeDisplayed` and
`challengeCompleted` are both 18 chars) — equal lengths can never prove sameness.

**⇐ THE LAST BLOCKER — the bridge. Live suspect: §7 #3 / suspect 4, the async-Promise shape.**
`generate_stub_js` makes every `@JavascriptInterface` method return a Promise; AOSP's
`addJavascriptInterface` is SYNCHRONOUS. Next step is an env-gated dev-host eval introspecting
Eclipse's OWN injected stub as the page sees it — evidence before any rewrite.

**Historical status (2026-07-10) — the pass the boots above validated:**
A first live boot (challenge16, log `/tmp/eclipse-challenge16.log`) reached the FIRST-EVER app-side
`onPageFinished` (http 200) on the real ChallengeHybridWebView but the page NEVER called the bridge,
and GC-only teardown left the stale full-window composite over LoginV2 for ~40 s with no
`ViewClosed`. This pass landed (all ADDITIVE; the socket wire protocol untouched, PROTO_VERSION
stays 2): (A) observability making the blocker READABLE — a default-visible (info-level) page
console surface + an env-gated (`ECLIPSE_WEBVIEW_CONSOLE=1`) raw-text helper diagnostic, a
renderer `bridge stubs applied mode=sync|refresh` timestamp, a consumer `bridge call received`
receipt (shape-gated identifiers+lengths only — pre-validation iface/method are page-controlled,
so URL/token-shaped strings bind as `<non-identifier>`), and a cookie-rejection predicate
classifier; (B) the root fix
for the injection race — a reliable `on_render_view_ready` inventory re-push + truly-synchronous
`V8Context::eval` stub injection before page scripts; (C) overlay — WebView.`internalLoadChanged`
now dispatches AOSP's **3-arg** `onPageStarted(WebView,String,Bitmap)` at state 0 (the confirmed
suspect-3 fix), plus a WebViewClient shadow with the AOSP base 3-arg onPageStarted +
`shouldOverrideUrlLoading`→false (base surface only; **dispatch DEFERRED** — it cannot be wired
honestly under the driven-loads-only/frozen-protocol contract; rationale in §6); (D) eager
`CloseView` on the active WebView's detach from the view tree (subtree-tested). Residual: if the
real blocker is UA-steering (open question #1) the bridge may stay silent even with sync injection
— this pass makes that verdict readable rather than guaranteeing a bridge call.

End-to-end validation against the real app flow: the 403→ChallengeHybridWebView fragment
load-drives the real challenge URL into the helper; watch for the first-ever
`internalLoadChanged(0)`→`onPageStarted` and `(3)`→`onPageFinished`, nonzero composited ink
inside the WebView frame rect, zero new ULEs, ART booting with no low_4gb regression, the
privacy absolute held (the only `target=` lines are scheme+host; no payload anywhere incl.
helper logs), and helper-crash isolation (the app survives a killed helper). Then an
owner-interactive boot where the real human completes the rendered challenge and the
completion callback/cookies hand back to the app so login proceeds past the 403 (may exceed
the 180 s automated envelope — owner-driven). If the vendor's scoring refuses/loops
CEF-shaped sessions despite faithful rendering and a live human, record the evidence and
trigger the runner-up path (Servo, behind its own servoshell spike).

**Verify (orchestrator live-boot per the AGENTS.md recipe — NEVER a subagent; live boots run
only on the dev host's main thread via `cargo run`):** stage-0 tap `400,413` with the
`/tmp/eclipse_field_probe.png` re-calibration technique if Landing drifted; 180 s EXIT=124;
do NOT wipe `/tmp/atl_cache` (proven red herring 2026-07-03). Grep verdicts for
onPageStarted/onPageFinished firing, WebView-rect ink present, zero `No implementation
found`/`UnsatisfiedLinkError`, the `bound=3` line intact, privacy greps clean — followed by
the owner-interactive completion boot whose success criterion is the app advancing past the
challenge (no ~60 s timeout-to-LoginV2 recovery); results recorded in §5/§6.

## 7. Open questions (2026-07-03)

1. **Vendor reception of CEF-shaped clients:** DataDome publicly documents
   fingerprinting/blocking CEF while Arkose officially supports embedded WebViews — only the
   M6 live boot answers whether the challenge is served and completable for a real human on
   genuine Chromium 149; if refused, the recorded fallback is the runner-up (Servo behind its
   own servoshell real-challenge-URL spike).
2. **Sandbox degradation policy (owner ruling needed):** is `--no-sandbox` acceptable as a
   loud, documented degradation on hosts with neither a SUID `chrome-sandbox` nor
   unprivileged user namespaces, given this component renders hostile web content beside the
   user's session?
3. **RBHybridWebView's actual bridge shape** (interface names, synchronous-return usage,
   `javascript:` URL channel vs postMessage vs scheme interception) is only discoverable at
   the first live boot — M4's bridge design must be re-validated against what the app really
   calls.
4. **Release-tracking policy:** Chromium moves to a 2-week stable cadence in September 2026 —
   pin CEF's LTC/LTS builds (slower, ages against the vendor's supported-browser matrix) or
   track stable (bump work every ~2 weeks)? Needs a dated §6 policy either way, since this
   engine renders hostile content and accrues CVEs when lagged.
5. **Footprint acceptance:** ~320–400 MB shipped payload (order ~120 MB compressed) is a
   10×+ distribution growth — owner sign-off on the size and on pruning locales to en-US (or
   shipping all).
6. **Wayland-only hosts without XWayland:** libX11 is a hard `DT_NEEDED` of `libcef` and
   official builds are X11-first — validate `--ozone-platform=wayland` initialization under
   OSR or document XWayland as a hard requirement with an actionable error.
7. **Flatpak story:** the SUID sandbox is impossible inside Flatpak and userns availability
   varies — the `org.freedesktop.Platform` packaging needs its own sandbox-mode validation
   (M5 sketches it; full validation may trail).
8. **Audio-challenge accessibility path:** does CEF's bundled audio (alsa `DT_NEEDED`) route
   correctly through Pulse AND PipeWire on real hosts, and is the widget's audio alternative
   required for the product to be considered complete?
9. **Helper lifecycle vs the app's own ~60 s challenge timeout:** exact spawn/kill and
   re-load semantics when the app retries or the fragment is torn down mid-render — must
   never leave orphan Chromium process trees.
10. **cef-rs binding durability:** tauri-apps could deprioritize the crate — the mitigation
    is regenerating the machine-generated bindings against CEF's stable C API, but that is
    C-API-level ownership Eclipse should budget for in the §6 record.
