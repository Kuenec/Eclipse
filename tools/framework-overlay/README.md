# Patched ATL framework overlay (`framework-patched`)

Rebuilds the patched **`api-impl.jar` plus libcore boot-jar overlay** Eclipse boots Roblox against. This tooling
lives in-repo because the 2026-06-11 cache wipe destroyed the previous out-of-tree build
script (`~/.cache/eclipse/patch-framework.sh`) together with its output — the overlay is a
cache artifact, but its **generator must survive** (CLAUDE.md "Build and Environment
Portability": no machine-only state a fix depends on).

## Why this exists

ATL's `api-impl.jar` `android.*` framework has **Java-level** gaps that Eclipse's
`RegisterNatives` mechanism (which binds native *methods*) cannot fix — missing static
fields and wrong/missing pure-Java method behavior:

| Class | Gap | Who trips it |
|---|---|---|
| `android.os.Build` | `SUPPORTED_{32,64}_BIT_ABIS` fields missing | `RobloxApplication.onCreate` (`NoSuchFieldError`) |
| `android.net.NetworkRequest$Builder` | not AOSP-shaped: inner (non-static) class, no no-arg ctor, no `addCapability(int)`/`addTransportType(int)` | jobqueue lib in `ActivitySplash.onCreate` (`NoSuchMethodError`) |
| `android.app.ActivityManager$RunningAppProcessInfo` | `importance` always 0 (never `IMPORTANCE_FOREGROUND`=100) and **no `pkgList` field** | Roblox's foreground-process check (dex `yj.s.b`): scans `getRunningAppProcesses()` for an entry with `importance == 100` whose `pkgList` contains the package; finds none → logs **"Background process detected"** |
| `android.location.LocationManager` | API-level-1 `isProviderEnabled(String)` missing; the overlay returns false for every non-null name because ATL advertises an empty provider set, and preserves AOSP's `IllegalArgumentException` for null | Current-client Backtrace watchdog plus two adjacent SDK paths (`NoSuchMethodError` reached Roblox's process-fatal uncaught handler → `System.exit(10)`) |
| `android.view.PixelCopy` | API-24 class entirely absent; Eclipse posts the honest `ERROR_SOURCE_NO_DATA` result because its framework SurfaceView has no Android pixel-copy backend | Current-client transition screenshot during `surfaceDestroyed` (`NoClassDefFoundError` blocked one shutdown callback) |
| `WolfSSLImplementSSLSession` (libcore boot jar) | installed hostdex returns an empty peer-certificate array although its own vendored source throws `SSLPeerUnverifiedException`; the overlay restores the source/`SSLSession` contract | Current-client background OkHttp hostname verification indexed `[0]`, then its fatal handler called `System.exit(10)` during close |

## Mechanism

Multidex **first-dex-wins**: the output `api-impl.jar` is
`[classes.dex = javac-patched classes | classes2.dex = smali-patched installed classes | classes3.dex = ATL's original whole api-impl dex]`.
ART's `DexPathList` resolves each class from the first dex that defines it, so the patched
`Build*`, `NetworkRequest*`, and `ActivityManager*` classes shadow the originals and every
other class resolves unchanged from `classes3.dex`.

The output also contains `art/`: all ten boot jars in the pinned art_standalone order, with only
`wolfssljni-hostdex.jar` changed. The generator writes `.eclipse-art-overlay-v1` last; Eclipse
selects the ART overlay only when that marker and every jar are present. Parent ART and child
`dex2oat` processes use the same overlay paths as both byte sources and logical identities, keeping
boot-image checksums coherent without modifying the distro's `/usr` files.

- `android/os/Build.java` is **generated** from the vendored ATL source
  (`vendor/atl/src/api-impl`) by inserting the two fields after the unique
  `SUPPORTED_ABIS` anchor — zero drift against the vendored file; the script fails
  loudly if the anchor is missing or duplicated.
- `src/android/net/NetworkRequest.java` and `src/android/app/ActivityManager.java` are
  committed **patched copies** of ATL's (Apache-2.0) sources, with `ECLIPSE PATCH` markers.
- `stubs/` are **compile-only** shells so `javac` can compile the patched sources without
  ATL's full source tree (`api-impl.jar` ships dex, not classfiles, so it cannot be a
  javac classpath). Stubs are **excluded from the dex** — only the whitelisted patched
  classes are dexed. ⚠️ **Never stub a `static final` constant with a placeholder value:**
  javac inlines it into the dexed bytecode (2026-07-02: a stub `internal.R.attr.id = 0`
  silently dropped LayoutInflater's `<include android:id>` override → the challenge
  fragment's RobloxToolbar NPE). `com.android.internal.R` is therefore compiled from the
  **vendored ATL source** (`$ATL_SRC/com/android/internal/R.java`, guarded in the script),
  not a stub; the built `classes.dex` is baksmali-verified to carry the real inlined ids.

## Usage

```sh
tools/framework-overlay/patch-framework.sh
cargo run -- run <APK>      # auto-detects the overlay at $XDG_CACHE_HOME/eclipse/framework-patched
```

`eclipse run` auto-detects the overlay at its default `OUT` location (2026-06-14), so the export
is no longer required after running the script. Set `ECLIPSE_ANDROID_FRAMEWORK_DIR` only to point
at an overlay built elsewhere (it still takes precedence over auto-detection).

Everything is env-overridable, nothing user-specific is hardcoded:

| Var | Default | Meaning |
|---|---|---|
| `ATL_SRC` | `<repo>/vendor/atl/src/api-impl` | ATL api-impl Java sources (for `Build.java`) |
| `ORIG_FW` | `/usr/lib/java/dex/android_translation_layer` | installed stock framework dir |
| `ART_DIR` | `/usr/lib/java/dex/art` | installed pinned art_standalone boot jars copied/patched into the overlay |
| `OUT` | `${XDG_CACHE_HOME:-$HOME/.cache}/eclipse/framework-patched` | output overlay dir |
| `JAVAC`/`JAR`/`JAVA` | repo `vendor/toolchain/jdk-*/bin/*`, else `PATH` | Java compiler / jar / runtime |
| `DX` | `dx` on `PATH` | dexer (class file ≤ v52, hence `--release 8`) |
| `BAKSMALI_JAR`/`SMALI_JAR` | `<repo>/vendor/toolchain/smali/{baksmali,smali}-2.5.2.jar` | dex disassembler/assembler for the View patch (2026-06-13) |

Missing tools/dirs fail with an actionable error (no silent fallback). `vendor/` is git-ignored
(local toolchain, like the JDK): place the smali 2.5.2 jars at `vendor/toolchain/smali/` (upstream
JesusFreke/google `smali`, or the distro `smali` package — see `vendor/toolchain/smali/SOURCE.txt`),
or point `BAKSMALI_JAR`/`SMALI_JAR` elsewhere.

## `android.view.View` pointer-capture (2026-06-13)

ATL's installed `View` omits AOSP's pointer-capture API — `View.OnCapturedPointerListener` +
`View.setOnCapturedPointerListener` — which Roblox calls in `ActivityNativeMain.d1`. Adding a *method*
needs the whole `View` class, and the repo's vendored `View.java` has **drifted** from the installed jar
(e.g. `setBackgroundColor(int)` is `native` in vendored but plain-Java installed), so recompiling vendored
re-breaks it. Instead the script (step 4b) **baksmali-disassembles the authoritative installed `View`**,
adds only the field + setter + the nested interface (anchored inserts with exact-count guards, like the
`Build.java` anchor), and reassembles. Output layout becomes 3-dex: `classes.dex` (javac-patched) +
`classes2.dex` (smali `View` + `View$OnCapturedPointerListener`) + `classes3.dex` (stock), resolved
first-dex-wins. The nested interface lives at `smali/android/view/View$OnCapturedPointerListener.smali`.

> Durability status (2026-06-14): the overlay output is still a cache artifact, but `eclipse run`
> now **auto-detects** it at the default `OUT` location — no `ECLIPSE_ANDROID_FRAMEWORK_DIR`
> export needed (`runtime::framework_dir`). When no overlay is found it warns and points back at
> this script instead of silently booting the stock framework into a `NoSuchFieldError`/SIGSEGV.
> Remaining gap: the script itself is still run by hand (the overlay is not yet built on demand
> from inside Eclipse).
