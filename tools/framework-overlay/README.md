# Patched ATL framework overlay (`framework-patched`)

Rebuilds the **patched `api-impl.jar` overlay** Eclipse boots Roblox against. This tooling
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

## Mechanism

Multidex **first-dex-wins**: the output `api-impl.jar` is
`[classes.dex = the patched classes only | classes2.dex = ATL's original whole api-impl dex]`.
ART's `DexPathList` resolves each class from the first dex that defines it, so the patched
`Build*`, `NetworkRequest*`, and `ActivityManager*` classes shadow the originals and every
other class resolves unchanged from `classes2.dex`.

- `android/os/Build.java` is **generated** from the vendored ATL source
  (`vendor/atl/src/api-impl`) by inserting the two fields after the unique
  `SUPPORTED_ABIS` anchor — zero drift against the vendored file; the script fails
  loudly if the anchor is missing or duplicated.
- `src/android/net/NetworkRequest.java` and `src/android/app/ActivityManager.java` are
  committed **patched copies** of ATL's (Apache-2.0) sources, with `ECLIPSE PATCH` markers.
- `stubs/` are **compile-only** shells so `javac` can compile the patched sources without
  ATL's full source tree (`api-impl.jar` ships dex, not classfiles, so it cannot be a
  javac classpath). Stubs are **excluded from the dex** — only `Build*`, `NetworkRequest*`
  and `ActivityManager*` classes are dexed.

## Usage

```sh
tools/framework-overlay/patch-framework.sh
export ECLIPSE_ANDROID_FRAMEWORK_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/eclipse/framework-patched"
cargo run -- run <APK>
```

Everything is env-overridable, nothing user-specific is hardcoded:

| Var | Default | Meaning |
|---|---|---|
| `ATL_SRC` | `<repo>/vendor/atl/src/api-impl` | ATL api-impl Java sources (for `Build.java`) |
| `ORIG_FW` | `/usr/lib/java/dex/android_translation_layer` | installed stock framework dir |
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

> Durability status (2026-06-11): the overlay output is still a cache artifact and
> `eclipse run` still needs `ECLIPSE_ANDROID_FRAMEWORK_DIR` pointed at it; auto-provisioning
> from inside Eclipse remains an open improvement tracked in `AGENTS.md` §5.
