# Eclipse

**Open-source, Rust, distro-agnostic runtime for running Roblox on Linux.**

Eclipse is an open alternative to [Sober](https://sober.vinegarhq.org/) (the closed-source
Roblox-on-Linux runtime). Like Sober, it runs the **Android x86-64 build of Roblox**
natively on the Linux kernel via an Android-compatibility layer — *not* Wine, *not* a
full Android emulator. Unlike Sober, every line Eclipse owns is open and written in Rust.

> **Status (2026-06-04):** Research & design **complete and locked**. Code is a **compiling
> skeleton** only. The next step is foundation validation — see [`docs/m0-runbook.md`](docs/m0-runbook.md).

## Priorities (in order)

1. **Stability** — proven over clever; never trade correctness for purity.
2. **Purely Rust** — for every line we own; thinnest possible binding where the host owns
   the component (GPU driver, audio server, the dex VM).
3. **Minimal overhead / no bloat.**

## How it works (one paragraph)

Roblox's native engine (`.so`, already x86-64) does the rendering/physics/networking;
Eclipse gives it the Android environment it expects and **forwards** its Vulkan/GLES and
audio straight to the host (near-zero overhead — the reason this can match/beat the Windows
client). Roblox's Java/Kotlin shell runs on a **vendored AOSP ART** that sits *off the
gameplay hot path*, so it costs no FPS. The hard, security-critical core is the bionic→glibc
loader/ABI shim. See the docs.

## Documentation

| Doc | What |
|---|---|
| [`docs/sober-research.md`](docs/sober-research.md) | How Sober/ATL works (the full technical writeup) |
| [`docs/component-map.md`](docs/component-map.md) | **Authoritative** component matrix under the priorities (this repo mirrors it) |
| [`docs/tech-selection.md`](docs/tech-selection.md) | Library selection rationale |
| [`docs/art-and-runtime.md`](docs/art-and-runtime.md) | The vendored ART/runtime: build, perf, stability |
| [`docs/dependency-plan.md`](docs/dependency-plan.md) | What each module will depend on |
| [`docs/m0-runbook.md`](docs/m0-runbook.md) | **Next step:** validate the foundation |

## Key locked decisions

- **Architecture:** Android-Translation-Layer approach (confirmed state-of-the-art in 2026).
- **dex VM:** **vendor AOSP ART + libcore** — proven unavoidable for Roblox (it uses a
  custom Java Activity tightly coupled to the engine via JNI; the "fake-JVM" path only ever
  ran simple games). Apache-2.0, off the hot path. See `docs/component-map.md` §3.
- **Graphics:** forward via `ash` (Vulkan) / `khronos-egl` (GL fallback), window via `winit`.
- **Irreducible non-Rust:** ART (forever) + the host GPU/audio loaders (physically can't be
  Rust). Everything else we author is Rust.

## Build the skeleton

```bash
cargo build          # compiles with just rustc; no system libs or external crates yet
cargo run -- help    # runnable placeholder CLI
cargo test           # (no tests yet)
```

The skeleton has **no dependencies** on purpose; deps get added per-subsystem as we build,
following [`docs/dependency-plan.md`](docs/dependency-plan.md).

## Roadmap

- **M0 — Validate the foundation** *(next; manual, no Eclipse code)*: build the ATL stack,
  boot the Roblox APK, capture the framework work-list. → [`docs/m0-runbook.md`](docs/m0-runbook.md)
- **M1 — Rust launcher** around the (initially reused-C) runtime: config, APK fetch/verify,
  ART boot to `onCreate`, GPU/Vulkan detection.
- **M2 — Rust framework backends** (`jni`) + services (Discord RPC, GameMode), driven by the
  M0 work-list.
- **M3 — Rust bionic loader/shim** (port off C `bionic_translation` onto `elf_loader`),
  behind an ABI conformance suite. *Highest risk — done with the most tests, not first.*
- **M4 — Rust graphics/input/audio bridges** (`ash`/`winit`/`gilrs`/`libpulse`).
- **Throughout:** vendor a pinned ART; package as a Flatpak on `org.freedesktop.Platform`.

## Repo layout

```
Cargo.toml          buildable skeleton manifest (no deps yet)
src/
  main.rs           launcher entry (placeholder CLI)
  lib.rs            crate root; declares the subsystem modules
  config.rs apk.rs runtime.rs bionic.rs framework.rs
  graphics.rs audio.rs input.rs services.rs diagnostics.rs   (documented stubs)
docs/               research, design, and the M0 runbook
```

## License

To be decided (`TBD`). AOSP ART is Apache-2.0; the reusable ATL/`bionic_translation` code is
GPLv3+, which will influence the choice if/when that code is reused. Tracked as an open item.

---
*Eclipse is unofficial and not affiliated with Roblox Corporation. You must supply your own
Roblox APK; Eclipse does not redistribute Roblox.*
