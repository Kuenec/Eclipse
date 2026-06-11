# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> The **Engineering Instructions** below (starting at that heading) are the authoritative,
> mandatory project policy — `AGENTS.md` §0 names this file as "ALWAYS follow it." This top
> section is codebase orientation layered on top of that policy, not a replacement for it.

## Read first, every session

- **`AGENTS.md`** is the durable charter + **living working state**. Read it at session start —
  especially **§5 Living State** (what is implemented vs stubbed right now) and **§6 Decisions
  Log** (append-only, dated). **Update §5 and append to §6 after any meaningful change.** It
  survives context compaction; the code's status comments lag behind it.
- **`docs/component-map.md`** is the authoritative component matrix — the `src/` module layout
  mirrors it. The other `docs/` files (see the index in `AGENTS.md` §7) hold the locked design.

## What Eclipse is

An open-source, **pure-Rust, distro-agnostic** runtime that runs the **Android x86-64 build of
Roblox** natively on Linux (an open alternative to the closed-source Sober). It uses the
Android-Translation-Layer approach: run Roblox's own native engine `.so` directly on the Linux
kernel, give it the Android environment it expects, **forward** its Vulkan/GLES/audio to the host
(near-zero overhead), and run its Java/Kotlin shell on a **vendored AOSP ART** VM that sits off
the gameplay hot path. **Target is Linux** (all distros, Wayland **and** X11, Mesa **and** NVIDIA,
Vulkan **and** GL, Pulse **and** PipeWire) — the "Compatibility Requirements" in the policy below
are written for Windows; apply their *intent* (detect, don't assume) to Linux instead.

## Common commands

**Quality gate** — run all of these clean (0 warnings/errors) before declaring work done or committing:

```bash
cargo fmt --all
cargo build --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release        # also verify the shipped artifact
```

- **Build requires a host C/C++ compiler.** `build.rs` compiles three clean-room shims that Rust
  stable cannot define (C-variadic functions): `src/loader/liblog_shim.c`,
  `src/loader/bionic_syscall_shim.c`, `src/loader/native_load_shim.cpp`. It also builds the
  `crates/libm-shim` sub-crate (a `no_std` cdylib) and a missing compiler fails the build with an
  actionable error.
- **Run a single test:** `cargo test <substring>` (e.g. `cargo test manifest_parses_utf16`), or a
  whole file with `cargo test --test engine_milestones`.
- **The integration tests in `tests/engine_milestones.rs` self-skip** (print `SKIP: …`, pass) when
  their preconditions are absent — no Roblox APK (`ECLIPSE_ROBLOX_APK=<path>` or the default
  `$HOME/eclipse-m0/apk/.../*-merged.apk`) or no display server (`WAYLAND_DISPLAY`/`DISPLAY`). They
  never weaken assertions; with the precondition present they require the exact success marker.
- **Run the app:** `cargo run -- run <APK>` (boots ART + drives lifecycle + opens the window),
  `cargo run -- config`, `cargo run -- help`. The live ART/loader boot **must run on the process
  main thread via `cargo run` — NOT under `cargo test`** (ART aborts on worker threads). The
  unit/discovery logic is what `cargo test` covers; the live boot is validated per
  `docs/dev-host-runbook.md`.
- **Hidden dev-host diagnostic subcommands** (not in `help`; each prints a deterministic SUCCESS
  marker that `tests/engine_milestones.rs` guards): `__run-libroblox-init` (map+relocate+resolve
  libroblox.so and run all DT_INIT_ARRAY constructors), `__gl-test` / `__gl-test-anw` (engine
  GLES2/EGL render path on Eclipse's window), `__input-test` (ALooper input path), `__audio-test`
  (OpenSL ES → cpal host audio).

## Architecture (the big picture)

One Cargo crate (`eclipse`) plus one sub-crate (`crates/libm-shim`). `src/lib.rs` declares the
subsystem modules; each maps to a row in `docs/component-map.md`. The end-to-end boot flow lives in
**`src/main.rs::run_apk`** and is the best map of how the pieces connect:

1. **`apk`** (`src/apk/`) opens the APK zip and reads the binary `AndroidManifest.xml` with
   Eclipse's **own pure-Rust, total** AXML/ARSC readers (`axml.rs`, `arsc.rs` — they return typed
   `ApkError`, never panic, which matters under the release `panic = "abort"`).
2. **`runtime`** (`src/runtime.rs`) builds a `BootPlan` from the manifest+config and **boots the
   vendored ART VM** by `dlopen`-ing `libart.so` and calling `JNI_CreateJavaVM` (no link-time ART
   dep), with Roblox's Java on the classpath.
3. **`loader`** (`src/loader/`) is Eclipse's **own from-scratch pure-Rust bionic dynamic loader** —
   the highest-risk, most-tested core. It exists because the legacy apkenv/`bionic_translation`
   shim linker aborts on modern relocations (`R_X86_64_TPOFF64`, `DT_RELR`, `BIND_NOW`) that
   `libroblox.so` uses. Pipeline: `elf.rs` (decode ET_DYN) → `reloc.rs` (apply relocations) →
   `map.rs` (mmap PT_LOAD segments, the one `unsafe` module) → `resolve.rs` (symbol scope /
   providers) → `tls.rs` (static-TLS layout + TPOFF64) → `link.rs` (DT_NEEDED graph orchestrator).
   On top: `engine.rs` (`load_app_native_lib` — the app-JNI-lib preload entry), `init_run.rs`
   (run constructors), `native_provider.rs`/`ndk_registry.rs`/`bionic_*` (Eclipse-owned bionic+NDK
   natives), `looper.rs` (real fd-backed ALooper), `opensl.rs` (OpenSL ES → host `cpal`).
4. **`framework`** (`src/framework.rs` + `framework/*_registry.rs`) is the **native (JNI) side** of
   the `android.*` framework, written in Rust via the `jni` crate. `drive_application_lifecycle`
   drives the confirmed JNI recipe (steps 1–7: `createApplication` → `onCreate` → `Activity.*` →
   `onResume`) against Eclipse-owned, generational-slab registry handles — **no GTK** (GTK would
   re-crowd the `low_4gb` region ART needs). The `*_registry.rs` files record the view/canvas/paint
   tree shape that the graphics pass renders.
5. **`graphics`** (`ash`/Vulkan + `egl_engine.rs` host EGL/GLES2) / **`input`** / **`audio`** are
   the host-forwarding bridges; `graphics::run_windowed` owns the `winit` window + event loop.

`crates/libm-shim` is a separate `#![no_std]`, `panic="abort"` cdylib re-exporting the pure-Rust
`libm` crate as a clean-relocation `libm.so` the apkenv linker *can* load (the host glibc
`libm.so.6` carries the modern relocs that abort it). `build.rs` builds it and the build *verifies*
via `readelf` that it has no `R_X86_64_TPOFF64`/RELR.

## Repo-specific conventions

- **Pure-Rust for every line Eclipse owns** (AGENTS.md §2.1). The only vendored non-Rust black box
  is ART; thin host bindings are allowed only where the host owns the component (GPU/audio loader).
  Every new dependency is justified against stability > pure-Rust > no-bloat and recorded in
  `docs/dependency-plan.md` + `AGENTS.md` §6 — read `Cargo.toml`'s per-dep comments before adding one.
- **`unsafe` is confined** (FFI/JNI, the loader's `map.rs`, raw Vulkan); every block carries a
  `// SAFETY:` comment, and `unsafe_op_in_unsafe_fn` is denied. Modules that need none declare
  `#![forbid(unsafe_code)]`.
- **Date non-obvious comments** `// YYYY-MM-DD: …` (behavior, ABI/offset, platform, reasoning) — and
  treat an old comment as stale if it conflicts with the code.
- **Dev-host vs harness boundary:** live ART/bionic-loader boots run only on the dev host's main
  thread (`cargo run`), never in `cargo test` (ART aborts off the main thread) and never in workflow
  subagents (the cyber-safeguard false-positives on bionic-linker / ART-VM analysis). See
  `docs/dev-host-runbook.md`.

---

# Engineering Instructions

## Mandatory Scope

This entire file is mandatory project policy. Nothing in this file is optional, advisory, or best-effort unless a line explicitly says otherwise.

Every agent, assistant, script, tool, and implementation plan must follow these requirements before, during, and after code changes. If any instruction conflicts with speed, convenience, assumptions, or a preferred implementation style, this file wins.

Do not reinterpret these requirements as suggestions. Do not skip them because the task seems small. Do not treat compliance as complete unless the final answer clearly shows how the relevant requirements were satisfied.

---

## Core Principle

Produce durable, root-cause fixes with the smallest necessary change. Do not use workarounds, temporary mitigations, bypasses, feature disabling, code removal, error suppression, or “make the symptom disappear” changes unless explicitly requested. A fix is not acceptable if it only avoids the failing path, disables the affected system, removes the problematic component, catches and ignores the error, changes behavior to avoid the crash, or narrows the issue without correcting the underlying cause. When in doubt, choose durable engineering over speed. Do not ask me to accept a workaround. Do not present a workaround as a fix.

---

## Compatibility Requirements

All changes must preserve broad Windows compatibility. When implementing, fixing, or refactoring anything that interacts with the operating system, hardware, firmware, drivers, startup paths, system services, registry, process management, filesystems, networking, or low-level APIs, consider compatibility across:

- Windows versions and builds, including all Windows 10 and Windows 11 releases.
- Different PC vendors, motherboard vendors, BIOS/UEFI implementations, and firmware configurations.
- Intel, AMD, and other supported CPU vendors, families, and generations.
- Different hardware capabilities, missing optional features, permission levels, localization settings, install paths, user profiles, and system policies.
- Both fresh installs and upgraded systems, including machines with unusual configurations or restricted environments.

Do not assume a specific motherboard, OEM, CPU generation, Windows build, driver version, registry layout, service state, path location, or hardware capability unless that requirement is explicitly verified and documented.

Compatibility-sensitive code must:

1. Detect capabilities instead of assuming them.
2. Handle unsupported or unavailable platform features with clear, intentional behavior.
3. Avoid hardcoded vendor-, build-, path-, firmware-, or hardware-specific assumptions unless required and justified.
4. Preserve existing support for other systems while fixing the current issue.
5. Include edge-case coverage or regression tests for materially different Windows versions, hardware paths, CPU vendors, permission states, or configuration variants when relevant.

A task is not complete if it only works on the developer’s machine, one motherboard family, one Windows build, one CPU vendor, or one common configuration. The implementation must be robust for real-world Windows PCs and must support the full intended user base.

---

## Build and Environment Portability

The project must build and function consistently for every developer and intended user, not only for the current machine.

Do not hardcode local machine paths, usernames, drive letters, absolute build folders, SDK locations, tool paths, cache paths, temporary paths, IDE paths, or device-specific locations such as:

- `C:\Users\SomeName\...`
- `/home/someone/...`
- Desktop or Downloads folders.
- Local clone paths.
- One developer’s Visual Studio, LLVM, Windows SDK, Python, Rust, Cargo, CMake, or vcpkg install path.
- Machine-specific registry keys, service names, device IDs, firmware paths, or hardware identifiers unless explicitly required and detected.

Use portable project-relative paths, documented environment variables, build-system discovery, toolchain files, configuration files, or clear user-provided settings instead.

If a dependency, path, SDK, tool, or capability is required:

1. Detect it through the build system or runtime capability checks.
2. Document the requirement clearly.
3. Fail with an actionable error if it is missing.
4. Do not silently fall back to a developer-specific path.
5. Verify the solution works from a clean checkout on a different machine path.

A change is not acceptable if it only builds or works because of files, folders, environment variables, cached artifacts, registry state, or tools that exist only on the current developer’s device.

---

## Required Research and Context7 Usage

Before every code change, use the available project context and Context7. This is required, not optional.

For every change:

1. Read the relevant project files before editing.
2. Use Context7 for current library, framework, API, toolchain, and platform documentation when the change touches external APIs, dependencies, build systems, platform behavior, or anything that may have changed over time.
3. Prefer official documentation, source code, project docs, vendor docs, standards, and well-established references over guesses, blog posts, or outdated examples.
4. Verify that the planned approach matches the actual versions, APIs, configuration, and constraints used by this project.
5. State what documentation or project context was checked before changing code.

When the user asks for research, the research must be deep, thorough, credible, and practical. It must prioritize sources that are known to be authoritative and solutions that are known to work. Do not give shallow research, unsupported claims, random snippets, or guesses.

Research is required before code changes so the implementation is based on verified information, not assumptions.

---

## Before Changing Anything

Before implementing, stop and make the task clear.

1. Read and follow the project’s instruction files, such as `AGENTS.md`, before making changes.
2. Read the relevant existing implementation before proposing edits.
3. Use Context7 when documentation, APIs, libraries, tools, or platform behavior are relevant.
4. State assumptions explicitly.
5. If something is unclear, name what is confusing and ask before proceeding.
6. If multiple interpretations exist, present them instead of silently choosing one.
7. If a simpler approach exists, say so.
8. Push back when the requested approach seems overcomplicated, risky, or inconsistent with the project.
9. Define success criteria before making changes.

Do not guess, assume, or speculate about the cause of a bug when the available evidence is insufficient.

Before proposing or making a fix, clearly distinguish between:

1. What is known from evidence.
2. What is suspected but not yet proven.
3. What additional diagnostics are needed to prove or disprove the suspicion.

Do not proceed with a code change as the fix unless the failure mechanism has been confirmed by evidence, reproduction, logs, tests, traces, diagnostics, or code-path analysis.

---

## Root-Cause Diagnosis

Your task is always to produce a permanent root-cause fix.

If the root cause is not yet known, your next step must be to improve observability and diagnostics until the failure mechanism can be identified with confidence.

Diagnostic work means adding targeted information that reveals the exact failing component, state, branch, input, timing, assumption, or boundary. It does not mean repeatedly changing nearby code until the symptom changes.

Useful diagnostics may include:

- Targeted debug prints with file, function, branch, state, key variable values, error codes, and timestamps.
- Assertions that prove required invariants at the point where they matter.
- Structured logs around the suspected boundary or state transition.
- Error context that includes the operation attempted, the exact failure, and relevant environment details.
- Traces that show call order, thread, timing, ownership, lifetimes, or data flow.
- Temporary diagnostic output that can be removed or reduced after the root cause is confirmed.
- Minimal reproduction checks or test instrumentation that proves the failure mechanism.

Diagnostics must be specific enough to answer:

1. Where exactly is the failure happening?
2. Which condition, value, state, or assumption is wrong?
3. Why is that wrong state possible?
4. Which code path produced it?
5. What is the smallest correct place to fix it?

Do not brute force fixes. Do not keep iterating in the general area without proof. If the current evidence does not identify the exact cause, add better diagnostics before changing logic.

Diagnostic changes are allowed only to discover the root cause. They are not a substitute for the final fix.

For every bug, crash, failure, regression, or implementation issue:

1. Identify the actual root cause and failure mechanism.
2. Fix the broken mechanism itself, not just the visible symptom.
3. Preserve the intended architecture, behavior, and feature set unless a design change is explicitly approved.
4. Search the entire codebase for equivalent instances of the same flawed pattern.
5. Fix all equivalent instances, not just the one that failed first.
6. Add or update appropriate regression protection so the same class of issue cannot be silently reintroduced.
7. Do not mark the task complete until the fix, same-pattern audit, and regression guard all pass.
8. Clearly state:

   - What root cause was found.
   - What was fixed.
   - What was audited.
   - What regression guard was added.
   - Why the issue should not recur.

Search broadly enough to include adjacent modules, shared helpers, generated or configured entry points, startup paths, tests, build scripts, and any code that can exercise the same failure mechanism.

Do not stop at the first discovered instance. Fix the entire class of problem everywhere it exists, and add enforcement so it cannot be reintroduced silently.

A task is not complete just because the current error no longer appears. It is complete only when the underlying failure mechanism has been corrected everywhere the same pattern exists, and the project has appropriate protection against reintroducing that class of bug.

---

## Regression Protection

Regression protection means the smallest reliable check that would fail if the same bug or same class of bug came back.

Do not automatically create new scripts for regression protection. A new script is only acceptable when it is the simplest maintainable option and when no existing test, build check, lint rule, assertion, validation path, CI step, or project harness can cover the issue.

Prefer regression protection in this order:

1. Existing unit, integration, or end-to-end tests.
2. Existing project test harnesses or build checks.
3. Focused test cases added to the closest relevant test file.
4. Assertions or validation checks that enforce an invariant at the correct boundary.
5. Existing lint/static-analysis/configuration rules.
6. A small new test file only when no suitable test location exists.
7. A new script only when the project has no better existing mechanism and the script is clearly justified.

Regression guards must be directly tied to the confirmed root cause. They must not be broad, unrelated, speculative, or created just to satisfy the words “regression guard.”

A regression guard is acceptable only if:

1. It would have caught the confirmed bug.
2. It can be run by other developers without local machine assumptions.
3. It fits the project’s existing test/build style.
4. It does not add unnecessary maintenance burden.
5. It does not hardcode user-specific paths or environment details.
6. It is documented in the final response with the exact command/check used.

If no automated regression guard is practical, explain why and provide the strongest available alternative, such as a targeted manual verification checklist, runtime assertion, build-time validation, or diagnostic check. Do not invent unnecessary scripts.

---

## Simplicity First

Write the minimum code that solves the confirmed problem.

Do not add speculative features, abstractions, configurability, or flexibility that was not requested.

Avoid:

- Features beyond what was asked.
- Abstractions for single-use code.
- “Future-proofing” without evidence.
- Error handling for impossible scenarios.
- Large rewrites when a small fix is sufficient.
- New scripts, tools, or frameworks when an existing project mechanism is enough.

If the solution is longer or more complex than necessary, simplify it before presenting it.

Ask: “Would a senior engineer say this is overcomplicated?”

If yes, rewrite it.

---

## Surgical Changes

Touch only what is required to satisfy the request.

When editing existing code:

1. Do not improve adjacent code, comments, formatting, or structure unless directly required.
2. Do not refactor unrelated code.
3. Match the existing project style, even if you would normally do it differently.
4. If you notice unrelated dead code, mention it instead of deleting it.
5. Remove imports, variables, functions, or files that your own changes made unused.
6. Do not remove pre-existing dead code unless explicitly asked.

Every changed line should trace directly to the user’s request, the confirmed root cause, the same-pattern audit, or the regression guard.

---

## Comments and Documentation

Comments must help future readers understand code that is not obvious from the implementation itself. Comments must not replace reading the code.

Write comments that are short, concise, and specific. Include detail only when it prevents misunderstanding or documents a non-obvious constraint.

When adding or updating comments that describe behavior, assumptions, platform details, external APIs, offsets, compatibility constraints, or implementation reasoning, include the date in `YYYY-MM-DD` format so future readers know when the statement was written or verified.

Example:

```cpp
// 2026-05-30: Windows returns ERROR_INSUFFICIENT_BUFFER here when the first call is used only to query size.
```

Do not trust old comments over code. If a comment conflicts with the implementation, tests, logs, or current documentation, treat the comment as stale and verify the behavior from the source of truth before acting on it.

When changing code near an outdated or misleading comment:

1. Update or remove the stale comment if it is directly related to the change.
2. Do not preserve comments that are known to be wrong.
3. Do not add broad comments that will quickly become outdated.
4. Prefer comments that explain why a non-obvious decision exists, not comments that repeat what the code already says.

---

## Goal-Driven Execution

Turn tasks into verifiable goals.

Examples:

- “Add validation” means: write or update tests for invalid inputs, then make them pass.
- “Fix the bug” means: reproduce the bug with a test, diagnostic evidence, logs, traces, or code-path proof, then make the check pass.
- “Refactor X” means: verify behavior before and after the refactor.

For multi-step tasks, state a brief plan before implementation:

1. Step → verify with specific check.
2. Step → verify with specific check.
3. Step → verify with specific check.

Success criteria must be concrete. Avoid vague goals like “make it work.”

Loop until the defined checks pass.

---

## Completion Standard

Do not mark work complete until all of the following are true:

1. The root cause is identified with evidence.
2. The broken mechanism is fixed directly.
3. Equivalent instances of the same flawed pattern have been searched for and addressed.
4. Appropriate regression protection has been added or updated without unnecessary scripts.
5. Relevant tests, checks, or builds pass.
6. Build and runtime assumptions are portable and do not depend on one developer’s machine.
7. Context7 and relevant project documentation were checked when applicable.
8. The final response explains:

   - The confirmed root cause.
   - The fix made.
   - The scope of the audit.
   - The regression guard or why a different verification method was used.
   - The verification performed, including exact commands/checks and laptop log path when applicable.
   - The documentation, project context, and Context7 references checked when applicable.

The goal is fewer unnecessary diffs, fewer overcomplicated rewrites, fewer machine-specific changes, better diagnosis before implementation, and fixes that are correct for the whole project and user base.
