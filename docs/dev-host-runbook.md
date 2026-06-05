# Eclipse — Dev-host runbook (the steps the cargo-test harness can't run)

> **Purpose:** *Execution, not decision.* Both Eclipse frontiers have reached the point
> where the next concrete steps require **dev-host execution on the process main thread** —
> they cannot run in the cargo-test harness (ART aborts off the main thread via
> `scoped_thread_state_change`) or in workflow subagents (the cyber-safeguard false-positives
> on bionic-linker / ART-VM analysis). This runbook consolidates the scattered next-actions
> (`AGENTS.md` §5, `docs/art-and-runtime.md` onCreate recipe + non-GTK design,
> `docs/bionic-loader-plan.md` §4b/§4c/§5) into ONE executable, decision-driven script a
> dev-host session runs and reports back from.
>
> Written **2026-06-05**. Style mirrors [`m0-runbook.md`](./m0-runbook.md): commands, expected
> milestones, and pass/fail decision trees — not prose. Companion authoritative docs:
> [`art-and-runtime.md`](./art-and-runtime.md) (onCreate JNI recipe + non-GTK backing design),
> [`bionic-loader-plan.md`](./bionic-loader-plan.md) (NDK-shim spec), `AGENTS.md` §5/§6.
>
> **Hard constraint (applies to every command below):** run on the **process main thread**
> via `cargo run -- run …` (i.e. from `main()`). Do **NOT** run any of this inside `cargo test`
> — ART aborts on worker threads. The discovery/option-split logic is unit-tested; the *live*
> boot is validated only here.

---

## Section A — Framework frontier: drive a demo APK to `Application.onCreate`

State of the code (confirmed, `AGENTS.md` §6 2026-06-05): `eclipse run <apk>` boots the
vendored ART VM, `framework::drive_application_lifecycle(&vm, apk_path)` registers Eclipse's
**own non-GTK** backing for the two `Context` static-init natives via
`env.register_native_methods`, proves the JNI bridge (bootstrap classes resolve), then drives
recipe **steps 1–3** (`Context.createApplication(0)` → `ContentProvider.createContentProviders()`
→ `Application.onCreate()`). Whether step 3 is cleanly reached is **UNCONFIRMED — pending this
dev-host run.**

### A1. Exact command

```bash
cargo run -- run ~/eclipse-m0/atl_test_apks/demo_app.apk 2>&1 \
  | tee ~/eclipse-m0/framework-demo-boot.log
```

- Run from a real terminal on the dev host (main thread). The demo APK is the pure-Java
  `demo_app.apk` (confirmed present, `~/eclipse-m0/atl_test_apks/demo_app.apk`) — pure Java so
  steps 1–3 take no native Window/Surface (Tier A; `art-and-runtime.md` "Non-GTK api-impl
  backing" §Tier A).
- Optional verbosity: prefix `RUST_LOG=debug` and/or `ANDROID_LOG_TAGS="*:v"` for more ART/JNI
  detail.
- After exit, snapshot the loaded libraries to prove the non-GTK invariant (see A3):
  `grep -c libgtk-4 /proc/<pid>/maps` — captured live, or rely on the in-log assertion. Since
  the process exits, the durable check is: **the boot must never `dlopen`
  `libtranslation_layer_main.so`** (the GTK-linked ATL natives lib); confirm no GTK `NEEDED`
  lib appears in the log.

### A2. Expected boot-log milestones, in order

1. **ART boots** — `JNI_CreateJavaVM` returns `JNI_OK`; libcore boot image loads (the two
   `libjavacore.so`/`libopenjdk.so` "not found" first-pass stderr lines are the known
   non-fatal bionic-linker probes — ignore them).
2. **Non-GTK Context natives registered** — `register_native_methods` binds Eclipse's own
   `native_get_apk_path` (`()Ljava/lang/String;`) + `native_updateConfig`
   (`(Landroid/content/res/Configuration;)V`) on `android/content/Context`, **before**
   `Context.<clinit>` runs (registration wins over ATL's name-based lazy binding — JNI 1.1).
3. **Framework bridge proven** — `LifecycleProgress::BridgeProven`: `find_class` resolves the
   bootstrap classes `android/content/Context` + `android/app/Application` through the typed
   `Env`.
4. **Steps 1–3 driven** — step 1 `Context.createApplication(0) -> Application` → step 2
   `ContentProvider.createContentProviders()` → step 3 instance `Application.onCreate()` on the
   step-1 object. The `jlong` handle is `0`/null (confirmed safe — steps 1–3 only *store* it,
   never deref; `art-and-runtime.md` Tier A). Success = `LifecycleProgress::ApplicationOnCreate`.
5. **Window opens** — `graphics::run_windowed(…)` opens the host winit window (no GTK).
6. **EXIT 0.**

### A3. Decision tree on outcome

Every JNI call goes through `framework.rs::checked()`, which on a thrown Java exception calls
`exception_describe` (→ stderr) + `exception_clear` and surfaces a typed `FrameworkError::Jni`.
So whatever fails, the log names it. Branch on the **first** failure:

**(i) Reaches `Application.onCreate` cleanly** (`ApplicationOnCreate` logged, window opens,
exit 0, **no `libgtk-4` / no `libtranslation_layer_main.so` in the process map**).
→ **Milestone met.** Tier A is proven on the dev host.
→ **Next = recipe steps 4–5** (`art-and-runtime.md` recipe rows 4–5 + "Non-GTK api-impl
  backing" Tier B):
  - Step 4 `Activity.createMainActivity((Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;)`
    then step 5 `Activity.onCreate((Landroid/os/Bundle;)V)`.
  - These need the **owned-handle non-GTK Window natives** bound the same way (Tier B table):
    `android/view/Window` `set_jobject` (store the Java `Window` ref in a Rust-side map),
    `set_widget_as_root` (view→surface, stub first), `set_title` (winit `Window::set_title`),
    `set_layout` (winit set size), `take_input_queue` (winit input bridge), plus
    `android/os/MessageQueue` `nativeInit`/`nativePollOnce`.
  - The `jlong` for steps 4–5 is **non-null** and **dereferenced** (deref begins at step 4):
    pass an **Eclipse-owned `intptr_t`** (a registry handle / `Box::into_raw` of an Eclipse
    `WindowState` holding the winit `Window` + later the `ash` `vk::SurfaceKHR`), **not** a
    `GtkWidget*` and **not** raw-window-handle bytes (`art-and-runtime.md` "Render stack +
    window-handle mapping"). Resolving that handle type is the still-UNCONFIRMED Tier-B design.

**(ii) `UnsatisfiedLinkError` naming native `X`.**
→ `X` is the **next non-GTK native to bind**. The error names the next gap in the
  static-init / `PackageParser` path (dropping ATL's GTK natives dir can surface more than the
  two Tier-A natives — `art-and-runtime.md` UNCONFIRMED list).
→ **Loop (one small subagent-safe increment per native, once `X` is known):** follow the
  established increment-9 pattern in `src/framework.rs`:
  1. Confirm `X`'s **compiled** signature with `javap -s` on the installed `api-impl.jar`
     (the compiled jar can differ from source — verify, don't guess):
     ```bash
     javap -s -p -classpath /usr/.../api-impl.jar <fully.qualified.Class> | grep -A1 <method>
     ```
  2. Add a GTK-free `extern "system"` Rust impl (body inside `EnvUnowned::with_env` →
     `catch_unwind`-wrapped, `.resolve::<LogErrorAndDefault>()` returns a neutral default — see
     `native_get_apk_path`/`native_update_config` in `framework.rs`).
  3. Add a `NativeMethod::from_raw_parts(name, sig, fn as *mut c_void)` entry to the
     `register_native_methods` array, with the name/sig pinned as `jni_str!`/`jni_sig!`
     constants and a host-independent unit test guarding name+descriptor (mirror
     `context_native_names_and_sigs_match_context_java`).
  4. Re-run A1. Repeat until steps 1–3 reach `onCreate` cleanly, then go to branch (i).
→ **LIKELY candidates to expect** (so the operator knows what's coming): `Resources` /
  `AssetManager` natives (asset/`resources.arsc` open + read), `PackageParser` natives
  (manifest/package parse the static init drives), and further `Configuration` natives beyond
  `native_updateConfig`. These are the framework families that surface once GTK's
  `libtranslation_layer_main.so` is off `java.library.path`.

**(iii) Java exception** (the `checked()` helper has described+cleared it to stderr).
→ Read the stack in the log — it names the **Java-side gap** (a missing/unstubbed framework
  class, a `NoSuchMethodError`/`NoSuchFieldError` from a descriptor mismatch, or a manifest/
  resource the demo APK assumes). A `NoSuchMethodError`/`NoSuchFieldError` here means a
  transcription drift between the `RecipeStep` constants and `Context.java` — the
  `call_site_literals_match_recipe_constants` / `context_native_names_and_sigs_match_context_java`
  guards should have caught it, so re-check those first.
→ Fix is then a small, now-known increment (correct the descriptor / stub the named class).

**Discovery-loop note:** once the failing `X` (branch ii) or the Java gap (branch iii) is
**named** by the log, the corresponding fix is a small, well-scoped increment that is
subagent-safe (it's ordinary Rust JNI binding, not ART-VM/bionic-linker internals). Only the
*live run* itself must stay on the dev host.

---

## Section B — Engine-load frontier: `libroblox.so` relocation via bionic NDK shims

State of the code (confirmed, `AGENTS.md` §6 + `bionic-loader-plan.md` §2): Eclipse boots ART,
puts Roblox's Java on the classpath, extracts `lib/x86_64/*.so`, whitelists that dir via
`dl_parse_library_path`, and `System.loadLibrary("roblox")` finds `libroblox.so` (111 MB) and
links it **to the relocation stage**, then fails on missing sonames
(`libmediandk.so`/`libOpenMAXAL.so`/`liblog.so`) + the unresolved symbol `AMediaFormat_delete`.

### B1. Shim spec — build-ready (`bionic-loader-plan.md` §4c)

The shim spec is build-ready (all CONFIRMED by `readelf`/`nm` + the bionic_translation
`meson.build`):

- **`libmediandk.so`** must (a) forward the **23 `AMedia*` functions** — 100% present in ATL's
  `/usr/lib/libandroid.so.0` (thin re-export: `DT_NEEDED [libandroid.so.0]` +
  `-Wl,--no-as-needed` + `b_lundef=false`), AND (b) **DEFINE the 7 missing `AMEDIAFORMAT_KEY_*`
  data globals** (`WIDTH`, `HEIGHT`, `COLOR_FORMAT`, `STRIDE`, `BIT_RATE`, `FRAME_RATE`,
  `I_FRAME_INTERVAL` — only `CHANNEL_COUNT`/`MIME`/`SAMPLE_RATE` are in `libandroid.so.0`) as
  `const char*` string constants (exact strings from the NDK header / `media.c`, not guessed),
  AND (c) supply the **2 `AConfiguration_getScreen{Width,Height}Dp`** functions (0/2 in
  `libandroid.so.0`).
- **`libOpenMAXAL.so`** = an **empty/stub** bionic-ABI `.so` (0 direct imports from
  `libroblox.so`) or a `cfg.d` alias to `libOpenSLES.so.1`.
- **`liblog.so`** resolvable: its `__android_log_*` family lives at `/usr/lib/art/liblog.so`
  (off the bionic ldpath) — resolve via a `cfg.d` absolute mapping **or**
  `dl_parse_library_path("/usr/lib/art", ":")` from the Eclipse runtime.
- **Build mode:** Meson, host `cc`, `-fPIC -D_GNU_SOURCE`, `b_lundef=false`/`b_asneeded=false`,
  `-Wl,--no-as-needed`, `soversion: 0`; linked against the `*_bio.so.0` providers (a plain host
  glibc `.so` crashes — §3.2). Symbol-supply precedent in-tree: `-Wl,--defsym` (scalars) / a C
  definition (string-pointer data).

### B2. The ONE main-loop-only blocker before building

> **Do NOT read the bionic linker source in a workflow subagent** (cyber-safeguard). This step
> is **MAIN-LOOP ONLY** (or a dev-host experiment).

**Confirm the bionic re-export mechanism:** does a thin `DT_NEEDED`-only shim (one that merely
`NEEDS` `libandroid.so.0`) re-export the forwarded symbols to a downstream consumer
(`libroblox.so`) **in the bionic namespace**, or must the shim **physically define forwarding
stubs / aliases** for each symbol? This is the first UNCONFIRMED item in
`bionic-loader-plan.md` §5 and gates the shape of `libmediandk.so`.

Resolve it by **either**:
- **(main loop)** reading `vendor/atl/thirdparty/bionic_translation/linker/linker.c` — the
  symbol-registration / `DT_NEEDED`-resolution walk (referenced, not reproduced, in
  `bionic-loader-plan.md` §3.2); **or**
- **(dev-host experiment)** building a minimal test shim (thin `DT_NEEDED [libandroid.so.0]`,
  exporting nothing itself) under the soname `libmediandk.so`, registering it (B3 mechanism),
  and checking whether the bionic resolver forwards `AMediaFormat_delete` to it for a
  bionic-linked consumer.

Two further §5 loader-behavior items settle from the **same** load probe and need not block the
*build*, only the final wiring: **cfg.d-vs-ldpath precedence/sufficiency**, and the **`liblog`
resolution + `libm` bionic-alias behavior** (does the loader accept an absolute cfg.d provider;
does the extra ldpath shadow `/usr/lib`; is a bionic consumer content with glibc `libm.so.6`).
A fourth (`bionic_android_dlopen_ext` ignoring `dlextinfo`) is unknown-if-relevant.

### B3. Dev-host validation once built

```bash
# 1. Build the shims (Meson, host cc — see B1 / bionic-loader-plan.md §4c.5).
# 2. Register them on the bionic ldpath / cfg.d:
#    - install a cfg.d mapping for libmediandk.so / libOpenMAXAL.so (/ liblog.so), AND/OR
#    - place the built shim .so on the dir Eclipse registers via dl_parse_library_path
#      (the same call that already whitelists the extracted app-lib dir).
# 3. Run the real Roblox APK on the dev host (main thread):
APK=~/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk
cargo run -- run "$APK" 2>&1 | tee ~/eclipse-m0/engine-load-boot.log
```

**Pass criterion:** `libroblox.so` links **past relocation** (no `cannot locate symbol
AMediaFormat_delete`, no `library "libmediandk.so"/"libOpenMAXAL.so"/"liblog.so" not found`).
The next landmark after that is `JNI_OnLoad` running for the engine.

**On failure, branch:** a still-missing **soname** → that shim isn't registered on a path the
bionic namespace searches (revisit B2 precedence); a still-unresolved **symbol** → the
re-export mechanism didn't forward it (B2 thin-`DT_NEEDED` vs physical-define question) → define
the symbol in the shim; a **crash during the load call** (not a clean missing-symbol) → an
ABI mismatch (a host-glibc `.so` was used instead of a bionic-ABI one, or a provider was
double-loaded under two sonames — `bionic-loader-plan.md` §3.2).

---

## Section C — Quick reference

### Quality gate (run before any commit / handoff; `AGENTS.md` §4, must be 0 warnings/errors)

```bash
cargo fmt --all
cargo build --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

### The dev-host-only constraint

ART must be created on the **process main thread**. The cargo-test harness runs on a worker
thread and aborts ART via `scoped_thread_state_change`; workflow subagents are
cyber-safeguard-blocked on ART-VM/bionic analysis. So every *live* boot in Sections A and B
runs via `cargo run -- run …` from `main()` on the dev host — never inside `cargo test`, never
in a subagent. (The discovery/option-split/descriptor logic is unit-tested; only the live boot
is dev-host-only.)

### Authoritative docs

| Doc | What it pins |
|---|---|
| [`art-and-runtime.md`](./art-and-runtime.md) | onCreate JNI recipe (steps 1–5, confirmed signatures); non-GTK api-impl backing design (Tier A/B natives, window-handle mapping). |
| [`bionic-loader-plan.md`](./bionic-loader-plan.md) | NDK-shim spec (§4b/§4c build-ready), bionic loader contract (§3), open loader-behavior questions (§5). |
| `AGENTS.md` §5 / §6 | Living State next-actions + dated Decisions Log (the surrounding M1/M2 state). |
| [`component-map.md`](./component-map.md) | Authoritative component matrix (bionic loader = #1 Rust-port priority, do-it-last; component F = the winit/`ash` framework). |
| [`m0-runbook.md`](./m0-runbook.md) | The foundation-validation runbook this one continues from. |
