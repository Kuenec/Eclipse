# Eclipse — Web-Engine Plan for the Challenge WebView

> **Status:** Locked direction / decision record + phased plan. Owner decision **(a)** made
> **2026-07-03**. Companion to [`dependency-plan.md`](./dependency-plan.md) (the dependency
> justification) and `AGENTS.md` §6 (2026-07-03 🧭, the decision-log entry). Every milestone
> gates the next. **M1 is DONE — GO (2026-07-03, verified);** implementation continues at **M2**.

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

### M6 — Live challenge boot: the challenge page renders in the app, then the human completes it

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
