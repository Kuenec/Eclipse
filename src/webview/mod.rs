//! Challenge-WebView engine bridge — the main-process side of the OUT-OF-PROCESS
//! `eclipse-webview` CEF helper (docs/web-engine-plan.md, owner decision (a) 2026-07-03).
//!
//! 2026-07-03 (plan M2): this module owns everything that crosses the helper process boundary
//! and NOTHING of the engine itself — zero cef/CEF bytes ever appear in the root crate. The
//! helper crate (`crates/eclipse-webview`, workspace-detached) `#[path]`-includes the leaf
//! submodules below verbatim, so there is exactly ONE implementation of the wire protocol, the
//! URL-redaction contract, the fd-passing, the shm mapping guard, and the slot state machine
//! (see `crates/eclipse-webview/src/shared.rs` for the sibling-module-shape invariant).
//!
//! # The helper SPAWN CONTRACT (normative; 2026-07-03)
//!
//! This section is the single normative statement of how a consumer launches the helper. The
//! M2 reference consumer is `crates/eclipse-webview/src/bin/drive.rs`; the M3 main-process
//! client adopts the identical contract at the recorded `framework.rs` WebView-native seams.
//!
//! 1. **Binary resolution:** the consumer resolves the `eclipse-webview` helper binary from,
//!    in order: an explicit caller-provided path (M3: config `webview_helper_path`), else
//!    `$ECLIPSE_WEBVIEW_HELPER`, else a sibling `eclipse-webview` next to the running
//!    executable, else (2026-07-03, tier 4 — a dev-tree convenience added at M3) the
//!    exe-relative checkout builds `<exe_dir>/../../crates/eclipse-webview/target/release/
//!    eclipse-webview` then `.../debug/eclipse-webview`. An EXPLICIT setting (config/env) that
//!    points at a missing file is an actionable error, never a silent fallthrough. No hardcoded
//!    install paths; an unresolvable helper is an actionable error (M3 degrades to the recorded
//!    honest one-shot WARN no-op — never a crash, never a fabricated callback).
//! 2. **Control socket:** the consumer creates a `std::os::unix::net::UnixStream::pair()`
//!    (SOCK_STREAM). One end stays in the consumer; the other end is `dup2`'d onto **fd 3** in
//!    the child (`pre_exec`), and the helper is spawned with the argv flag **`--ipc-fd=3`**.
//!    The helper trusts the flag, not the fd number: no `--ipc-fd` → usage error, nonzero
//!    exit, before any fd use.
//! 3. **Optional argv:** `--ozone-platform=<wayland|x11>` overrides the helper's own explicit
//!    ozone selection (the M1-recorded rule: ozone auto is NEVER trusted).
//!    `--allow-unsandboxed` (2026-07-10, plan M5) forwards the config opt-in
//!    `webview_allow_unsandboxed` — the helper may then run `--no-sandbox` as a LOUD documented
//!    degradation when the host has neither unprivileged userns nor a SUID `chrome-sandbox`
//!    (a boolean flag, argv-safe — no secrets). No other flags are part of the contract; the
//!    helper strips `--enable-logging`/`--no-sandbox` defensively as PASS-THROUGH switches
//!    (its own policy-gated degradation is the one exception, applied via `Settings`, not argv).
//! 4. **No URL ever appears in argv** (token-bearing challenge URLs would leak via
//!    `/proc/*/cmdline`): every load target crosses the control socket only. 2026-07-16: the
//!    operative predicate is **no secrets in argv**, judged BY PROVENANCE, not by shape (the §6
//!    2026-07-16 🌱 ruling). Where a value is genuinely not a secret but is also not required to be
//!    in argv, prefer the child's ENVIRONMENT (§7): `/proc/PID/environ` is owner-only where
//!    `/proc/PID/cmdline` is world-readable, so the tighter channel costs nothing.
//! 5. **Orphan prevention:** the helper exits on control-socket EOF (primary); the consumer
//!    sets `PR_SET_PDEATHSIG(SIGTERM)` in `pre_exec` (secondary — note PDEATHSIG fires when
//!    the spawning THREAD exits, so spawn from a stable thread); the consumer's kill()+wait()
//!    is the backstop.
//! 6. **Handshake:** the first consumer→helper frame MUST be `Hello`; the helper answers
//!    `HelloAck` and exits if no `Hello` arrives within 10 s of spawn ([`proto`] has the full
//!    framing/version rules).
//! 7. **Inherited env (2026-07-10, plan M6):** the spawn does NOT `env_clear`, so
//!    `ECLIPSE_WEBVIEW_CONSOLE=1` reaches the helper. It is a DEV-HOST DIAGNOSTIC ONLY: when set,
//!    the helper logs raw page console TEXT helper-side (page-controlled content — the text never
//!    crosses the frozen text-free wire `Console`; the source stays redacted to scheme+host). Off
//!    by default; a diag-enabled log is by definition a dev-host artifact, never a default boot.
//!    2026-07-16 (plan M6): `ECLIPSE_WEBVIEW_UA_DIAG` reaches the helper the same way. It is a
//!    TEMPORARY DEV-HOST A/B DIAGNOSTIC: a non-empty value replaces the engine-side User-Agent
//!    VERBATIM and OUTRANKS the app's own, and the helper announces it with a loud startup WARN.
//!    8. **`ECLIPSE_WEBVIEW_APP_UA` (2026-07-16, plan M6) — NOT a diagnostic; part of the contract.**
//!    The consumer SETS it on the child (it does not merely inherit) to the User-Agent the app
//!    itself chose via `WebSettings.setUserAgentString`, when the app set one. It is the channel for
//!    the §6 2026-07-16 💥 fix: `CefSettings.user_agent` is global and fixed at `CefInitialize`, so
//!    the spawn is the boot's ONE chance to deliver it. **2026-07-16 (§6 🩹➜⛔) — the ordering rule,
//!    corrected by a live boot:** the claim that stood here (*"the spawn is LAZY — first load-drive,
//!    i.e. after the app configures its WebView — so the ordering works"*) was DISPROVED: a COOKIE
//!    op cold-started the helper 61 s BEFORE `setUserAgentString`. The spawn is now DEFERRED past
//!    the cookie ops that are answerable without an engine (`client::EarlyCookies` — the session
//!    store starts empty, so buffered sets ARE its whole content), to the first op that genuinely
//!    needs CEF — normally the load-drive. Two ops still force an early spawn, and both are LOUD:
//!    a `getCookie` issued after this boot already set a cookie (CEF owns url/domain/path matching),
//!    and the 3-arg `setCookie(url, value, ValueCallback)` (only CEF yields the real success flag).
//!    **2026-07-16 (§6 respawn) — the ordering rule, COMPLETED.** Where an early op genuinely still
//!    forces a spawn before the app's UA exists (the two above), the first load-drive **REPLACES**
//!    that helper: the consumer tears it down (fully REAPING the child — CEF's `root_cache_path`
//!    process singleton forbids two live engines), spawns a replacement with `ECLIPSE_WEBVIEW_APP_UA`
//!    set, and replays the app's ORIGINAL `CookieSet` frames into it. Sound because a helper that
//!    never received a `CreateView` has no browser, so no `Set-Cookie` header ever ran in it and its
//!    store is exactly those frames. `PROTO_VERSION` stays **2** — both helpers speak v2 and the
//!    replay uses existing frames; no message is added.
//!    A forced early spawn is an HONEST DEGRADATION to the fallback UA — the pre-fix behaviour, said
//!    out loud — never a wrong answer. The env, not argv, per §4's provenance test: a UA is neither a URL nor a
//!    secret (the app composes it from ATL's own synthetic `SystemProperties`/`Build` values and
//!    broadcasts it to every server by design), but `/proc/PID/environ` is owner-only while
//!    `/proc/PID/cmdline` is world-readable, so the env channel adds no disclosure at all.
//!    Precedence in the helper (`engine::effective_user_agent`): `ECLIPSE_WEBVIEW_UA_DIAG` >
//!    `ECLIPSE_WEBVIEW_APP_UA` > `engine::ECLIPSE_USER_AGENT` (the fallback, for a WebView the app
//!    never configured). Unset = the fallback literal, byte-for-byte.
//!
//! # M3 adoption note
//!
//! 2026-07-03: IMPLEMENTED at M3 by [`client`] — the two load natives (`framework.rs`
//! `web_view_native_load_url` / `web_view_native_load_data_with_base_url`) are
//! spawn-and-forward per this contract, keyed by the existing `view_registry` widget handle
//! ("integrates, never duplicates" — the protocol's `view` field IS that jlong), the
//! `eclipse-webview-io` socket-reader thread stages frames and hands
//! `WebView.internalLoadChanged(0/3)` to the dedicated `eclipse-webview-upcall` thread as JNI
//! upcalls (2026-07-09/10: the reader itself is JNI-free — see [`client`]'s module docs), and
//! staged helper frames composite at the cached `view_registry` frame rect through
//! the vk-overlay present seam. `client` is MAIN-PROCESS-ONLY and deliberately NOT part of the
//! helper crate's `#[path]` sibling set (`crates/eclipse-webview/src/shared.rs` includes only
//! the five protocol leaf modules below).

pub mod client;
pub mod fdpass;
// 2026-07-10 (plan M5): main-process-only pre-spawn host-lib probe — like `client`, NOT part of
// the helper crate's `#[path]` sibling set; consumed only by `client`, so crate-visible.
pub(crate) mod hostprobe;
pub mod proto;
pub mod redact;
pub mod shm;
pub mod slots;

/// Wire-protocol version (the `Hello`/`HelloAck` version field, the exact-match negotiation
/// channel). 2026-07-09 (plan M4): bumped 1 → 2 for the ADDITIVE v2 extension (JS bridge /
/// cookie-with-result / eval-with-result). The v1 MESSAGE LAYOUTS in [`proto`] stay byte-identical
/// (FROZEN by `proto_roundtrip_encodes_and_decodes_every_v1_message`); only this negotiation
/// integer changes, so a stale v1 peer fails the exact-match handshake loudly-and-honestly rather
/// than mis-decoding a v2 frame (an old helper answers `HelloAck{version:1}`, the consumer's
/// `hello_ack_version_supported` rejects it → the honest one-shot WARN no-op).
pub const PROTO_VERSION: u16 = 2;
