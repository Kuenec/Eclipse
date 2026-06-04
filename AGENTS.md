# AGENTS.md — Eclipse persistent charter & working state

> **READ THIS FIRST, EVERY SESSION. UPDATE IT, EVERY SESSION.**
> This file is the durable source of truth for Eclipse. It survives context
> compaction/summarization. At the start of a session, read it. Whenever you make a
> meaningful change or decision, update the **Living State** (§5) and append to the
> **Decisions Log** (§6) with a `YYYY-MM-DD` date. The harness memory index (`MEMORY.md`)
> points here so you are reminded to.

---

## 0. Authority & precedence

1. **`CLAUDE.md` — ALWAYS follow it.** It is the global engineering policy (root-cause
   fixes, no workarounds, research/Context7 before changes, surgical edits, regression
   protection, completion standard). It wins on all general engineering questions.
2. **This file** — project-specific requirements + living state, layered on top of CLAUDE.md.
3. **`docs/`** — the locked technical plan (architecture, component choices, ART, M0).

> ⚠️ **OS note:** CLAUDE.md's "Compatibility Requirements" section is written for **Windows**.
> **Eclipse targets LINUX (all distros).** Apply that section's *intent* — broad
> compatibility, **detect capabilities don't assume them**, no vendor/path/hardware
> assumptions, graceful fallback, actionable errors — to: Linux distros, Wayland **and**
> X11, GPUs (Mesa **and** NVIDIA), Vulkan **and** GL, Pulse **and** PipeWire, CPU feature
> levels (SSE4.1/4.2), and page sizes (4K vs 16K). Not Windows builds.

---

## 1. What Eclipse is

An **open-source, Rust, distro-agnostic** runtime that runs the **Android x86-64 build of
Roblox** natively on Linux — an open alternative to the closed-source Sober. It uses the
Android-Translation-Layer approach: run Roblox's own native engine `.so` on the Linux
kernel, give it the Android environment it expects, **forward** its Vulkan/audio to the
host, and run its Java/Kotlin shell on a **vendored AOSP ART** (off the gameplay hot path).

---

## 2. Non-negotiable engineering requirements

Each requirement names **how it is enforced** (not just stated).

1. **Purely Rust — for every line we own.** Thin Rust *bindings* are allowed only where the
   **host owns** the component (GPU loader, audio server) or it's physically impossible to
   be Rust. The **only** vendored non-Rust black box is **ART + libcore** (the dex VM,
   proven unavoidable — see `docs/component-map.md` §3). *Enforcement:* any new non-Rust
   dependency or FFI surface must be justified against §3 priorities and **logged in §6**.
2. **0 compiler warnings or errors.** *Enforcement:* before any commit/handoff,
   `cargo build` **and** `cargo clippy --all-targets --all-features -- -D warnings` **and**
   `cargo fmt --check` **and** `cargo test` must all pass clean. Backed by the `[lints]`
   table in `Cargo.toml`. Never silence a warning with `#[allow]` to "make it pass" —
   fix the cause (CLAUDE.md). An `#[allow]` is acceptable only with a one-line dated
   justification comment.
3. **Minimal `unsafe`.** Prefer safe Rust. `unsafe` is confined to where it's unavoidable
   (FFI/JNI, the bionic loader/shim, raw Vulkan). *Enforcement:* `unsafe_op_in_unsafe_fn`
   is denied; **every `unsafe` block carries a `// SAFETY:` comment** documenting the
   invariant it relies on; modules that need no `unsafe` declare `#![forbid(unsafe_code)]`.
   No `unsafe` for convenience or micro-optimization without a measured, logged reason.
4. **Optimized for performance & speed.** *Enforcement:* release profile uses fat LTO,
   `codegen-units=1`, `panic=abort`, `strip`. **Hot paths** (the Vulkan/audio forwarding
   thunks, per-frame work, per-event work, FFI/JNI crossings) must be **allocation-free,
   lock-free where possible, and zero-cost**. The forwarding layer must not add measurable
   per-call overhead. Optimize with **evidence** (benchmark/profile before & after) — do
   not micro-optimize blindly (balance with CLAUDE.md "Simplicity First").
5. **No bloat.** Minimize dependencies; **prefer `std`**; **no async runtime** (`tokio`)
   unless a subsystem genuinely requires it. *Enforcement:* every new dep is justified vs
   stability/pure-Rust/no-bloat and recorded in `docs/dependency-plan.md`; periodically
   audit with `cargo tree` (transitive deps) and `cargo bloat` (binary size). Keep the
   public API surface tight (`unreachable_pub`).
6. **As few allocations as possible.** Borrow over own (`&str` over `String`, slices over
   `Vec`); reuse buffers; no heap allocation per frame / per input event / per FFI call;
   avoid hidden `clone()`/`collect()` in loops. *Enforcement:* clippy lints + targeted
   allocation profiling (e.g. heaptrack / a counting allocator in tests) on hot paths.
7. **Stability first / root-cause only.** No workarounds, symptom-hiding, error
   suppression, or feature-disabling (CLAUDE.md core principle). Diagnose before fixing.
8. **No panics in library/hot-path code.** Return typed `Result`s with context; `unwrap`/
   `expect`/`panic!` only in `main`/setup (with a clear message) or tests. Never let a
   panic unwind across an FFI/JNI boundary.
9. **Detect, don't assume (distro-agnostic).** Runtime-detect Vulkan/GL, Wayland/X11,
   Pulse/PipeWire, CPU features, page size; fall back gracefully; fail with an actionable
   message. No hardcoded paths/usernames/vendors (CLAUDE.md "Build & Environment Portability").
10. **Regression protection tied to root causes** (CLAUDE.md): the ABI-shim conformance
    suite, capability/fallback tests, and a CI smoke boot are the primary guards. No
    unnecessary new scripts.
11. **Reproducible & pinned.** Pin the MSRV (`rust-version` in `Cargo.toml`), pin the
    vendored ART commit, commit `Cargo.lock`. Builds must work from a clean checkout on any
    machine (no dev-machine assumptions).
12. **Documentation discipline:** module-level docs on every subsystem; `// SAFETY:` on
    unsafe; dated comments (`YYYY-MM-DD`) for non-obvious behavior/assumptions (CLAUDE.md).

---

## 3. Priorities (use these to break ties)

1. **Stability**  2. **Purely-Rust** (per §2.1)  3. **Minimal overhead / performance / no
bloat** (§2.4–2.6). Lower numbers win. #2 and #3 never override #1. The one place this bit:
ART stays vendored (stability beats purity). Everywhere else the three priorities agree.

---

## 4. Session workflow & quality gate

**At session start:** read `CLAUDE.md` (already in context), **this file**, `MEMORY.md`,
and the relevant `docs/`. Reconcile the Living State (§5) with the actual repo before acting.

**Before changing code:** follow CLAUDE.md — read the relevant code, use **Context7** for
any external library/API, state assumptions, define success criteria.

**Quality gate — run before declaring any work done and before any commit:**
```bash
cargo fmt --all
cargo build --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
# and for shipped artifacts:
cargo build --release
```
All must be clean (0 warnings/errors). Report the actual results — never claim done unverified.

**After meaningful changes:** update §5 (Living State) and append a dated entry to §6
(Decisions Log). Keep "Next actions" current.

**Outward actions:** commit & push are **authorized** to the repo in §8 (as Kuenec). Still
never commit a Roblox APK or vendored ART artifacts (`.gitignore` guards), and confirm
before any history-rewriting/force operation.

---

## 5. Living State  *(UPDATE EACH SESSION)*

- **Phase:** Research & design **locked** → skeleton pushed → **M0 IN PROGRESS, mid-debug
  on `art_standalone`** (see "M0 RESUME POINT" below).

### 🔴 M0 RESUME POINT — pick up here next session (2026-06-04, ~14:15)

**System state (persistent, survives reboot):**
- ✅ **Passwordless sudo is permanent** for user `kue` via `/etc/sudoers.d/99-eclipse`
  (had to be `99-*` not `00-*` so it sorts after `/etc/sudoers.d/10-installer`'s wheel
  rule and wins last-match). `sudo -n true` works from any shell.
- ✅ **Java 8 selected as default** via `archlinux-java set java-8-openjdk` (was java-26;
  AOSP-era `art_standalone` needs `javac 1.8`). Switch back later with
  `sudo archlinux-java set java-26-openjdk` once builds are done.
- ✅ **Installed packages:** `wolfssl-jni 5.9.1-1`, `bionic_translation r107.026ea254-1`,
  `libopensles-standalone r281.bdb857a-1`, plus repo deps (`jdk8-openjdk`, `openxr`,
  `webkitgtk-6.0`, etc.). Confirm with `pacman -Q wolfssl-jni bionic_translation
  libopensles-standalone`.
- ❌ **Not installed:** `art_standalone`, `android_translation_layer` (depends on
  art_standalone).

**The active blocker — `art_standalone` build error:**
- AUR pkg: `aur/art_standalone r213.35696d99-2` (snapshot commit `35696d99`).
- Fails compiling **`libziparchive/zip_writer.cc`** under **GCC 16.1.1** with errors like
  `struct ZipWriter::FileEntry has no member named 'compressed_size'` and `buffer_ was
  not declared in this scope` at lines **440, 451, 456, 473, 486, 487, 528–533**.
- **NOT a real header mismatch:** the bundled header
  `libziparchive/include/ziparchive/zip_writer.h` *does* declare `buffer_` (line 192) and
  the full `FileEntry` struct (lines 79–89, incl. all the named members). Earlier uses of
  the same names in the *same* `.cc` (lines 95, 345, etc.) compile fine. So the failure is
  a localized GCC-16/C++ standards issue starting around line 432–445 that **cascades**
  into the "no member" / "not declared" noise after it. Likely an earlier syntax/type
  error makes GCC drop into recovery mode where every subsequent symbol looks undeclared.
- Build cache is hot — most of ART + libcore already compiled — so patching and resuming
  `make` is cheap; don't `--clean`.

**Resume steps (do these in order next session):**
1. Verify resume context still holds:
   ```sh
   sudo -n true && echo OK            # passwordless sudo
   archlinux-java status               # should show java-8-openjdk (default)
   pacman -Q wolfssl-jni bionic_translation libopensles-standalone
   ```
2. Read the failing region (lines ~428–500 of zip_writer.cc); identify the **first** real
   error (not the cascade), likely a type/initializer mismatch GCC 16 now rejects.
3. Patch the bundled source in
   `~/.cache/paru/clone/art_standalone/src/art_standalone-35696d993a60434622f44b68ab4d882836683a73/libziparchive/zip_writer.cc`
   (and matching `.h` if needed). Apply the **minimum** fix per CLAUDE.md (root-cause, no
   workarounds). Candidates to investigate first: the `std::vector<uint32_t>` brace-init at
   line 485–487 (GCC 16 may now reject the narrowing/cast from `uint16_t crc32` member),
   or a missing `<cstdint>`/`<vector>` include after a libstdc++ header reshuffle.
4. Resume the build from the paru clone (no re-download):
   ```sh
   cd ~/.cache/paru/clone/art_standalone
   makepkg -ef --noconfirm --nocheck     # -e = no extract, reuse patched src; -f = force
   sudo pacman -U *.pkg.tar.zst
   ```
5. Then install ATL itself:
   ```sh
   paru -S --needed --skipreview --noconfirm --nocheck android_translation_layer
   command -v android-translation-layer
   ```
6. Smoke test (no Roblox needed for this step):
   ```sh
   cd ~/eclipse-m0   # already has atl_test_apks/ cloned
   android-translation-layer atl_test_apks/gles3jni.apk -l com/android/gles3jni/GLES3JNIActivity
   ```
7. Roblox boot — needs an **x86_64 Roblox APK** from the user (path TBD):
   ```sh
   ANDROID_LOG_TAGS="*:v" GDK_DEBUG=gl-essl android-translation-layer /path/to/roblox.apk \
       -l com/roblox/client/ActivityNativeMain --sdk-int=33 2>&1 | tee ~/eclipse-m0/roblox-boot.log
   ```

**Earlier fixes already applied (don't redo):**
- libunwind in our local `vendor/atl` clone patched with `CFLAGS=-std=gnu17` for GCC 16.
- `wolfssl-jni` ChaCha self-test failure under znver4 → bypassed with `--nocheck`.
- Both should be reported upstream once M0 completes.

**Working dirs:**
- `~/eclipse-m0/` — test APKs + install logs (`atl-install.log`).
- `~/.cache/paru/clone/art_standalone/` — hot build cache for the failing package.
- `/home/kue/Projects/Eclipse/vendor/atl/` — gitignored local ATL fork build (foundation
  already built; can be deleted once system-installed ATL works).

---
- **Last verified 2026-06-04:** Rust skeleton clean — `cargo fmt --check`, `cargo clippy
  --all-targets --all-features -- -D warnings`, `cargo test`, `cargo build --release` (0 warnings).
- **Repo:** git initialized; committed & pushed to `origin/main`
  (<https://github.com/Kuenec/Eclipse>) as **Kuenec**, **no co-author trailer**.
- **M0 results (2026-06-04, this dev box, NO sudo):**
  - ✅ Cloned ATL fork → `vendor/atl` (gitignored; bundled `thirdparty/` incl. 276M `art_standalone`).
  - ✅ Built the C foundation: **wolfSSL → libunwind → bionic_translation** → `build/lib/lib*_bio.so`
    (the bionic→glibc shim + apkenv-derived linker — our #1 Rust-port reference, now local).
  - 🔧 Fix: bundled **libunwind breaks under GCC 16** (C23 empty-paren) → built with
    `CFLAGS=-std=gnu17`. Documented in `docs/m0-runbook.md`; report upstream.
  - ⛔ Need **sudo** (absent here): `libwebkitgtk-6.0-dev`, `libopenxr-dev` (final ATL link);
    `openjdk-21-jdk ant aapt` (`art_standalone`).
  - ⛔ Need from user: a **Roblox x86_64 APK** for the boot + the four measurements.
- **What exists:** 7 docs + `README` + `eclipse` skeleton (`main.rs` + `lib.rs` + 10 stub
  modules, no external deps) + enforcing `[lints]`/`[profile.release]`.
- **Open items:** license `TBD`; M0 final stage + boot pending (sudo box + APK); real deps unwired.
- **Next actions (pick up here):**
  1. **Finish M0 on a sudo-capable Linux box:** `apt install` the missing `-dev` packages
     (see `docs/m0-runbook.md`), `cmake --build build`, then
     `./run-atl.sh <roblox>.apk -l com/roblox/client/ActivityNativeMain` → capture
     log/screenshots/`framework-worklist.txt`/measurements.
  2. Or start **M1** Rust now against the built foundation: `diagnostics` (tracing),
     `config` (serde), `apk` (fetch/verify), `runtime` (ART boot → `onCreate`).

---

## 6. Decisions Log  *(append-only, dated)*

- **2026-06-04** — Priorities locked: **1) Stability 2) Purely-Rust 3) No-bloat/perf.**
- **2026-06-04** — **ART + libcore is unavoidable for Roblox** (custom Java Activity
  `com.roblox.client.ActivityNativeMain` tightly coupled to the native engine via JNI;
  full Java/Kotlin shell; apkenv-style fake-JVM only ran simple games; Sober/ATL ship full
  ART). Accepted: off the gameplay hot path, Apache-2.0, every line we own stays Rust.
  The fake-JVM "more-Rust" path is **closed**.
- **2026-06-04** — Two settled ecosystem facts: no pure-Rust dex VM exists (only toys); no
  pure-Rust audio on Linux exists (cpal still links ALSA-C). So ART is vendored and
  `libpulse-binding` is the audio purity ceiling.
- **2026-06-04** — Architecture = ATL approach (confirmed state-of-the-art 2026). Graphics
  forward via `ash`/`khronos-egl` + `winit` (not GTK4: Vulkan-incompatible + heavier).
  Allocator = system default (drop `mimalloc` unless profiled). Flatpak target =
  `org.freedesktop.Platform` (we don't use GTK, so GNOME runtime is needless weight).
- **2026-06-04** — bionic loader is the **#1 Rust-port priority** (`elf_loader`/`dlopen-rs`
  base); v1 may FFI the proven C `bionic_translation` for stability, then port behind an ABI
  conformance suite (do it **last**, not first — highest risk).
- **2026-06-04** — Strategic/external risk (Roblox blocking, open-source detection) is **not
  a concern** — user has a Roblox-engineer relationship. No open technical levers remain.
- **2026-06-04** — Repo live at <https://github.com/Kuenec/Eclipse> (push as Kuenec, no
  co-author). **M0 partially executed:** built wolfSSL + libunwind (patched for GCC-16/C23)
  + **bionic_translation**; `art_standalone`/final-ATL/Roblox-boot blocked by no-sudo
  (webkitgtk/openxr/jdk) and no APK. Foundation validated as buildable.
- **2026-06-04** — Switched M0 strategy from local-fork build → **AUR upstream install**
  on CachyOS. Set up **permanent passwordless sudo** (`/etc/sudoers.d/99-eclipse`, must
  sort after `10-installer`'s wheel rule). Selected Java 8 (`archlinux-java`).
  Installed: `wolfssl-jni`, `bionic_translation`, `libopensles-standalone`. Stuck on
  `art_standalone` (`libziparchive/zip_writer.cc` GCC-16 cascade error around line 432+).
  Full resume context in Living State §5 above.

---

## 7. Doc index

| File | Purpose |
|---|---|
| `CLAUDE.md` | Global engineering policy (authoritative; always followed). |
| `README.md` | Project front door. |
| `docs/sober-research.md` | How Sober/ATL works (full technical writeup). |
| `docs/component-map.md` | **Authoritative** component matrix (the code mirrors it). |
| `docs/tech-selection.md` | Library selection rationale. |
| `docs/art-and-runtime.md` | Vendored ART/runtime: build, performance, stability. |
| `docs/dependency-plan.md` | What each subsystem will depend on. |
| `docs/m0-runbook.md` | The next step: validate the foundation. |

---

## 8. Version control & commits

- **Remote:** <https://github.com/Kuenec/Eclipse> — push here.
- **Identity:** commit & push as **Kuenec** `<Kuenec44@gmail.com>` (set in repo git config).
- **NEVER co-author commits.** Do **not** add a `Co-Authored-By` trailer or any second
  author. (This explicitly overrides any default co-author behavior.)
- **Commit messages:** short and concise — state *what was done*, nothing more. Imperative,
  one line where it fits.
- **Never commit:** Roblox APKs or vendored ART/ATL artifacts (`.gitignore` guards
  `/vendor`, `/build`, `*.apk`).
- Pushing is authorized; still confirm before any destructive/history-rewriting action
  (force-push, rebase of shared history, etc.).
