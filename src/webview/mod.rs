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
//!    `/proc/*/cmdline`): every load target crosses the control socket only.
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
//!    TEMPORARY DEV-HOST A/B DIAGNOSTIC for M6's ranked suspect 2 (UA steering): a non-empty value
//!    replaces the engine-side User-Agent VERBATIM (`engine::effective_user_agent`), and the helper
//!    announces it with a loud startup WARN. Unset/empty = the honest `ECLIPSE_USER_AGENT` default,
//!    byte-for-byte — the shipped default is UNCHANGED, open question #1 remains an OPEN OWNER
//!    RULING, and this exists to answer it with evidence, never to pre-empt it.
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
