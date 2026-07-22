# Current Goal

Get the owner logged into Roblox **Home** under Eclipse through a direct password sign-in. If Roblox
shows 2SV, the owner types it in the app window; never read or store the code. Completion means:

1. Observe `onAppReady: Home` after the interactive sign-in.
2. Close cleanly so cookies flush.
3. Relaunch and observe Home without another `/v2/login` POST.
4. Remind the owner to rotate the temporary password.

No credential, verification code, session token, challenge identifier, or cookie belongs in this
file. Do not inspect shell history or environment secrets. No forged integrity response, patched auth
handler, suppressed auth response, or imported session is acceptable.

# Current State

- A normal (no `ECLIPSE_WEB_LOGIN`) Eclipse/Roblox process is currently alive on the native Home
  screen. The owner typed credentials only into Roblox's first-party page; Eclipse never logged or
  read them.
- Branch: `main`, HEAD `3d29cc1` (`docs: record direct Sober login comparison`). Nothing has been
  committed or pushed. The owner confirmed that the official page did show 2SV, they entered the
  code themselves, pressed Continue, and then reached Home.
- The intended new runtime change is the real per-queue MessageQueue implementation in
  `src/framework/message_queue.rs`, registered from `src/framework.rs`. Other modified/deleted/
  untracked files predate or are independent of this investigation and must be preserved.
- The official-login route in `src/framework.rs`/`src/main.rs` opens
  `https://www.roblox.com/login` in the existing persistent WebView profile and registers no test
  JavaScript interface. `crates/eclipse-webview/src/engine.rs` now explicitly focuses the
  windowless CEF browser before mouse-down or keyboard delivery; without that required CEF focus
  notification the page rendered but its fields were inert.
- Root fmt, clippy, 684 unit tests, 1 main test, 8 integration tests, 2 doc tests, and release build
  passed before the CEF focus change. The WebView crate's strict clippy and all 59 tests passed after
  it; its release helper was rebuilt. `git diff --check` is clean.

# Verified Official Login and Native Handoff

Interactive login log: `/home/kue/eclipse-web-login-focus-20260722.log`.

- The official Roblox login form loaded through the production WebView path.
- After the explicit windowless-CEF focus fix, the fields accepted real pointer/keyboard input.
- Roblox displayed 2SV. The owner entered the code themselves and pressed Continue; Eclipse did not
  inspect or record the code. Roblox then authenticated the owner and navigated to web Home.
- Eclipse closed cleanly; the helper confirmed `ViewClosed` and exited 0.

Normal relaunch log: `/home/kue/eclipse-post-web-auth-20260722.log`.

- Relaunched with no `ECLIPSE_WEB_LOGIN` override.
- The native app reported `onAppReady: Home` at `17:37:34.563` and Home was visually confirmed.
- The relaunch log contains no `/v2/login` POST.
- Do not log out, clear the trusted browser profile, or erase the working session merely to force a
  2SV prompt without explicit owner approval.

# Confirmed Runtime Fix: MessageQueue

The former single nonzero sentinel made every Android MessageQueue behave like Eclipse's externally
pumped main queue. A HandlerThread therefore exited whenever its queue was briefly empty. That lost
the Play Core Standard Integrity warm-up posted just after `getLooper()` returned.

The current implementation gives each queue a unique opaque handle and distinguishes:

- Main queue: nonblocking/yielding so winit can pump it.
- Worker queue: condition-variable wait, wake, timed poll, pending-wake bit, idling state, and safe
  destruction.

All five ATL static natives are registered. Unit tests cover handle uniqueness, worker blocking and
wake, early wake, timed poll, pending wake, destroy, and idling. Live evidence confirms Play Core's
worker now runs and reaches `Context.bindService`; this is a real general Android runtime fix.

# Last Credentialed Attempt

Log: `/home/kue/eclipse-queue-only-login-20260722.log`.

- `16:27:33.690` — `onAppReady: LoginV2`.
- `16:27:38.619` — owner submits.
- `16:27:38.738794` — `/v2/login` returns the expected 403 challenge response.
- `16:27:38.739175` — `Rendering native challenge`.
- `16:27:38.757094` — `onAppReady: ChallengeNativeWrapper`.
- `16:27:38.857761` — back to `onAppReady: LoginV2` after about 100.7 ms.

There was no hybrid WebView and no 2SV screen, even after leaving the app open. The process later
closed cleanly.

# Confirmed Failure Mechanism

## Provider warm-up

With worker queues fixed, the app's real Java/Play Core path runs:

- Standard Integrity warm-up starts.
- It tries to bind action
  `com.google.android.play.core.expressintegrityservice.BIND_EXPRESS_INTEGRITY_SERVICE` in package
  `com.android.vending`.
- Eclipse has no Play Store package or Express Integrity service, so `Context.bindService` accurately
  returns false.
- The provider setup fails with Standard Integrity `PLAY_STORE_NOT_FOUND (-2)`.
- The optional token provider remains empty.

The APK's `km.i.e(JSONObject)` then returns immediately with a nonempty JSON object containing an
empty token and result `TOKEN_PROVIDER_UNINITIALIZED`. It cannot request a token or enter the 5-second
inner timeout because no provider exists.

## Roblox challenge wrapper

Both Eclipse's exact shipped `UniversalApp.rbxm` and Sober's cached UniversalApp were decoded from
their production Luau bytecode. The opcode multiplier is 227; all 67k/66k prototypes parse. The
relevant `DeviceIntegrityTokenChallenge.init` implementations are instruction-for-instruction the
same (only source locations/obfuscated module names differ).

The wrapper does not inspect the Java `result` enum. Its behavior is:

- A nil or empty return calls `onRetrieveTokenFailure`, which completes the native challenge with an
  empty redemption token and no refresh callback.
- Any nonempty return is JSON-decoded as success. The wrapper reads `.token`, completes with that
  value, and supplies a refresh callback.

Eclipse therefore treats the nonempty `TOKEN_PROVIDER_UNINITIALIZED` response as success with an
empty token. This is the controlled 100 ms native-challenge completion seen in the log; it is not a
renderer, input, class-loading, MessageQueue, or WebView crash.

# Sober Control Experiment

Fresh profile:
`/home/kue/.var/app/org.vinegarhq.Sober/eclipse-sober-fresh.RF4LRq`.

Log:
`/home/kue/.var/app/org.vinegarhq.Sober/eclipse-sober-fresh.RF4LRq/data/sober/sober_logs/2026-07-17_22-08-04.log`.

This resolves the earlier uncertainty about whether Sober only resumed a cookie:

- A fresh password POST received the same 403 native challenge at `03:11:20.242`.
- The native wrapper completed in roughly the same short interval.
- A second Type-10 UI transition occurred at `03:11:31.847`, about 11.605 seconds later.
- WebKit cache metadata identifies an official first-party Roblox hybrid URL with challenge type
  `two-step-verification`.
- The owner completed 2SV and Sober entered an app session at `03:11:53.864`.

An earlier attempt in the same log has the same roughly 11.7-second native-to-hybrid delay.

The fresh Sober profile contains only Roblox base/split APKs, not a Play Store APK or microG. The host
has no relevant D-Bus service. Sober's closed runtime implementation is not inspected. Given identical
APK Java and Luau logic, identical quick native completion, absent Play services, and the later hybrid
transition, Sober must differ at a runtime integration boundary. The exact proprietary behavior is
not proven; do not claim more than the evidence supports.

# Boundary and Decision

The remaining direct-password blocker is a genuine missing external capability: official Play
Integrity depends on Google Play Store infrastructure that is absent from Eclipse's Linux translation
environment. `bindService` action+package resolution is a real general ATL gap, but implementing it
cannot produce an absent external service.

Do **not** solve this by:

- inventing Play Store package/certificate metadata;
- fabricating a binder provider or integrity token;
- patching `km.i`, the AccountProtocol handler, or `DeviceIntegrityAvailable`;
- delaying, dropping, or suppressing the MessageBus authentication response;
- changing auth fast flags to force a fallback;
- importing Sober/browser cookies or another session.

Those actions would manipulate authentication instead of implementing a missing Android contract.

The approved legitimate alternative is now implemented and verified: an official Roblox web login
entry lets the owner enter credentials and 2SV only into a first-party page, and a subsequent normal
app boot reaches native Home without another password POST. Eclipse did not fabricate or bypass the
challenge and did not inspect the owner's code.

# Relevant Paths

- Queue implementation: `src/framework/message_queue.rs`.
- Queue JNI registration: `src/framework.rs`.
- Current release log: `/home/kue/eclipse-queue-only-login-20260722.log`.
- APK disassembly: `/home/kue/eclipse-bytecode-20260722`.
- Extracted APK: `/home/kue/eclipse-apk-work`.
- Sober fresh profile/log: paths above.

# Cleanup and Verification Before Handoff

- Remove `.scratch-rbx-inspect/`; it is temporary analysis code and decoder output.
- Confirm no scratch artifacts remain. The currently verified native-Home process is intentionally
  left open for the owner; close it cleanly only if requested.
- Run `cargo fmt --check`.
- Run `cargo clippy --all-targets --all-features -- -D warnings`.
- Run `cargo test`.
- Run `git diff --check`.
- Rebuild the framework overlay and release binary if implementation changes continue.
- Home and a clean normal relaunch are verified. Commit/push only when explicitly requested.
