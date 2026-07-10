# `tools/webview-dist` — the shippable Eclipse + CEF payload (web-engine plan M5, 2026-07-10)

`package-webview.sh` assembles the ONE directory a user runs Eclipse from: the `eclipse` binary,
the `eclipse-webview` CEF helper, and the stripped/pruned CEF runtime payload — with the pinned
archive dual-digest-verified, the licenses shipped, and a no-display packaged-layout smoke.
In-repo tool per the `tools/framework-overlay/patch-framework.sh` precedent (its generator must
survive; AGENTS.md §2.1 pure-Rust binds product lines, not tools).

Provenance (2026-07-10): every shipped third-party byte is extracted from the
dual-digest-verified tarball itself — never from the long-lived extracted `CEF_DIST` working
dir, whose bytes nothing re-attests after export. The dist remains only the helper's *build*
input (`CEF_PATH`), and its `libcef.so` is sha256-checked against the verified tarball before
the build. The script's `rm -rf` of `OUT` is guarded: it refuses a pre-existing non-empty
directory that is not a previous run's payload (the `.eclipse-webview-payload` stamp, or the
`eclipse-webview` + `libcef.so` pair of a pre-stamp run).

## Why bundling

No distro packages CEF — bundling the identical engine everywhere is the portable answer
(`docs/web-engine-plan.md` §4). The helper is spawned lazily on the first challenge load-drive
and killed after completion: dead weight on disk otherwise, zero cost in the main binary, the
test gate, and the gameplay hot path.

## The pin pair (release bumps)

The shipped engine is ONE artifact pair spread over three files that must move TOGETHER:

| What | Where |
|---|---|
| `cef = "=149.3.0"` (crate pin) | `crates/eclipse-webview/Cargo.toml` |
| `PIN_ARCHIVE` + `PIN_SHA1` + `PIN_SHA256` | `tools/webview-dist/package-webview.sh` |
| The vendored-table libcef row | `docs/dependency-plan.md` |

A release bump edits all three, then re-runs: the helper gate, the root gate,
`package-webview.sh` (all self-checks incl. the smoke), and the packaged-layout
`__webview-test` live leg. `PIN_SHA1` is the upstream index digest (what `download-cef`
verifies); `PIN_SHA256` is the stronger digest computed from the verified on-disk tarball —
both are re-checked at package time, independent of trusting the live CDN index.

## Usage

```bash
tools/webview-dist/package-webview.sh          # defaults: repo dev dist → $repo/dist/eclipse-linux-x86_64
CEF_DIST=/path/to/export OUT=/tmp/payload tools/webview-dist/package-webview.sh
tools/webview-dist/package-webview.sh --help   # all parameters
```

From a clean checkout with no dev dist, install the fetch tool once (`cargo install
export-cef-dir`) — the script fetches and verifies the pinned archive itself. The dist is NOT
part of the checkout; `CEF_DIST` is the documented env contract (same as the helper's
`CEF_PATH` build env).

## The layout contract (tier-3 sibling resolution)

`eclipse`, `eclipse-webview`, and the whole CEF payload live in ONE directory:

- `eclipse` resolves the helper as its **sibling** (tier 3 of the spawn contract in
  `src/webview/mod.rs`) — zero configuration.
- `eclipse-webview` resolves `libcef.so` beside itself via its baked **`RUNPATH=$ORIGIN`**
  (`crates/eclipse-webview/.cargo/config.toml`) — zero `LD_LIBRARY_PATH`. The script re-verifies
  the RUNPATH with `readelf` (a user `RUSTFLAGS` env would silently override the crate's
  rustflags table).
- The pre-spawn host-lib probe (`src/webview/hostprobe.rs`) reads `libcef.so`'s `DT_NEEDED`
  beside the helper and names any missing host libraries (apt/dnf/pacman hints) — advisory,
  never gating.

Ship list: `libcef.so` (stripped ~256 MB), `chrome-sandbox`, `icudtl.dat`,
`v8_context_snapshot.bin`, `resources.pak`, `chrome_100_percent.pak`, `chrome_200_percent.pak`,
ANGLE (`libEGL.so`, `libGLESv2.so`), SwiftShader (`libvk_swiftshader.so`,
`vk_swiftshader_icd.json`), `libvulkan.so.1`, `locales/en-US*.pak`, `CREDITS.html` (the Chromium
third-party license aggregate) and `LICENSE.txt` (CEF's own) — ALL extracted from the pinned,
digest-verified tarball (2026-07-10). Never shipped: `include/`, `libcef_dll/`, `cmake/`,
`CMakeLists.txt`, `archive.json`, the other 200+ locale paks — enforced by construction (the
script extracts an explicit member list, it never `cp -r`s anything).

The locale prune to en-US is the recorded owner decision (`docs/dependency-plan.md`); the
footprint sign-off is plan §7 open question #5 — the script's printed `PACKAGED PAYLOAD` size is
the evidence.

## chrome-sandbox (the SUID tier)

The payload ships `chrome-sandbox` mode 0755 exactly as CEF distributes it. The helper's
sandbox selection (plan M5, `crates/eclipse-webview/src/engine.rs::select_sandbox_mode`) prefers
**unprivileged user namespaces**, verified USABLE by a live probe — `unshare(CLONE_NEWUSER)`
plus a capability-gated syscall inside the new namespace, because on stock Ubuntu 24.04+ the
default AppArmor restriction permits the bare `unshare` and then denies every capability inside
the namespace (creation alone would false-positive there). On a host without usable
unprivileged userns, an admin can enable the SUID tier:

```bash
sudo chown root:root <payload>/chrome-sandbox
sudo chmod 4755 <payload>/chrome-sandbox
```

The helper detects the setuid-root binary beside `libcef.so` and exports
`CHROME_DEVEL_SANDBOX` (Chromium's documented override). The mode must be exactly the 4755
shape — root-owned, setuid, **world-executable**: Chromium itself rejects a group-restricted
4750/4700 file, so the helper's probe applies the same predicate and treats such a file as
tier-unavailable (2026-07-10). With NEITHER tier available the helper
**refuses with an actionable error** naming both fixes — unless the user explicitly set
`webview_allow_unsandboxed = true` in `config.json`, which selects a loud, logged `--no-sandbox`
degradation (never a default; the engine renders hostile web content).

## Verify

```bash
tools/webview-dist/package-webview.sh                       # all self-checks + the smoke + the size
(cd dist/eclipse-linux-x86_64 && timeout 180 ./eclipse __webview-test)   # the live strip/prune completeness proof
```

The `__webview-test` leg needs a display session and the dev-host APK (see
`docs/dev-host-runbook.md`); the script's own smoke needs neither (fd-3 handshake stage only —
no engine init, no display, no network).

## Flatpak (`org.freedesktop.Platform`) — the M5 sketch (plan §7 #7; full validation trails)

- **Runtime/SDK:** `org.freedesktop.Platform // 24.08` + `org.freedesktop.Sdk // 24.08` — NOT
  the GNOME runtime (Eclipse uses winit, no GTK; the recorded no-bloat win). App id shape:
  `io.github.kuenec.Eclipse`.
- **Layout:** install this script's whole `$OUT` payload to `/app/lib/eclipse/` (both binaries +
  CEF payload + licenses in one dir — the tier-3 sibling resolution holds with zero env), with
  `/app/bin/eclipse` a symlink into it. `std::env::current_exe()` reads `/proc/self/exe` — the
  RESOLVED real path — so the sibling probe lands in `/app/lib/eclipse/` through the symlink.
- **Finish-args sketch:** `--socket=wayland --socket=fallback-x11 --device=dri
  --socket=pulseaudio --share=network --share=ipc`.
- **The sandbox caveat (honest):** inside Flatpak the SUID tier is impossible (no setuid) and
  unprivileged userns creation is blocked by Flatpak's seccomp filter — the M5 probes will
  honestly detect NEITHER, so as-sketched the helper refuses with the actionable error (the
  DESIGNED outcome — never a crash). Shipping on Flatpak therefore needs a follow-up
  integration: zypak-style `flatpak-spawn --sandbox` routing of Chromium's sandboxed subprocess
  launches (the mechanism the upstream Chromium/Electron flatpaks use), validated in its own
  later pass. `webview_allow_unsandboxed=true` is NOT an acceptable Flatpak default.
- No manifest file lands at M5 (nothing to validate it against yet) — this sketch + the layout
  the script already produces are the normative starting point.
