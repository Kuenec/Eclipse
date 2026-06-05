# Eclipse — Bionic-loader v1 strategy: the modern-relocation wall

> **Status: STRATEGY DECISION — decision only, NOT implementation.** Written
> **2026-06-05**. This doc sits one altitude *above*
> [`bionic-loader-plan.md`](./bionic-loader-plan.md): that note is the build-ready
> *NDK-soname-shim* spec (which `.so` files to build, which symbols to forward/define);
> THIS note characterizes the harder wall those shims would still hit — the apkenv-era
> bionic shim linker cannot **relocate** modern host libraries — and chooses the v1 loader
> path that clears it.
>
> **Why a separate doc:** the soname gap (`libmediandk.so`/`libOpenMAXAL.so`/`liblog.so`
> *missing as files*) and the relocation gap (`unknown reloc type 18` *while linking a
> file that WAS found*) are two distinct failure mechanisms. The plan doc's §4 shim recipe
> resolves the first; it does **nothing** for the second. The faithful run evidence below
> shows the relocation gap is now the binding constraint, so the v1 decision belongs here.
>
> **Safeguard boundary (mandatory):** this is a STRATEGY/DECISION doc. It does **not**
> reproduce dynamic-linker internals or relocation-processing code, and the chosen
> implementation (whichever path) is **main-loop / interactive only** — Anthropic's
> cyber-safeguard false-positives on bionic-linker / loader-internal work and kills
> workflow subagents mid-task (AGENTS.md §6, 2026-06-04/2026-06-05 entries;
> `docs/dev-host-runbook.md`). Grounded only in: the faithful `eclipse run` evidence
> captured in AGENTS.md §5/§6, this project's `src/runtime.rs` lib-provisioning strategy,
> `bionic-loader-plan.md`, and general public ELF knowledge. No linker source was read to
> write this; none must be read to act on the *decision* — only on the eventual
> implementation, in the main loop.

---

## 1. The wall, precisely (from the faithful run evidence)

**Confirmed (faithful `eclipse run <merged-APK>` run, log `/tmp/eclipse-roblox.log`, EXIT=139;
AGENTS.md §5 + §6 2026-06-05 `provision_bionic_sonames` entry):**

Eclipse now gets the *necessary* preconditions right end-to-end:

- `boot()` opens libart `RTLD_NOW|RTLD_GLOBAL`, which promotes its transitive
  `NEEDED libdl_bio.so.0` (the bionic translation linker) and `liblog.so` into the global
  scope (`src/runtime.rs`, `LIBART_DLOPEN_FLAGS`).
- `runtime::whitelist_bionic_library_path(...)` calls libdl_bio's `dl_parse_library_path`
  with `<framework-natives>:<app-lib cache dir>` so `System.loadLibrary` resolves the
  *named* libs.
- `runtime::provision_bionic_sonames(app_lib_dir)` symlinks each run-confirmed **bare host
  soname** to its real-ELF provider — currently `libm.so → /usr/lib/libm.so.6`, found
  portably (`cc -print-file-name=libm.so.6`, with an `is_real_elf` check that rejects the
  host's bare `/usr/lib/libm.so` GNU **ld linker script**).

With those in place the bionic shim linker **finds and opens** the libs (the
`library '…' not found` lines are GONE, grep count 0). It then **fails to relocate them**:

```
linker.c:2128 unknown reloc type 18 @ 0x… → linker.c:2901 failed to link libm.so
```

(The line numbers above are quoted verbatim from the faithful run log already recorded in
AGENTS.md; this doc does not open or analyze that source.)

### 1.1 The three unsupported features (what the apkenv-era shim linker lacks)

Decoded from the faithful run log + benign `readelf -r`/`readelf -d` on the host
`libm.so.6` (recorded in AGENTS.md §6 2026-06-05), interpreted with general public ELF
knowledge:

| Feature | What it is (public ELF knowledge) | Why a modern host lib has it | Evidence on host `libm.so.6` |
|---|---|---|---|
| **`R_X86_64_TPOFF64`** (reloc **type 18**) | A **TLS** (thread-local storage) relocation: writes the offset of a TLS symbol relative to the thread pointer (`%fs`-based) into a GOT slot. | The platform `errno`, and `libm`'s own per-thread math state, are **thread-local**. Any modern glibc/bionic lib that touches TLS emits these. | `1 R_X86_64_TPOFF64` (the lib has `STATIC_TLS`). This is the reloc that aborts the link. |
| **`RELR`** (compressed relative relocations) | A bitmap-packed encoding of the `R_*_RELATIVE` relocations (`DT_RELR`/`SHT_RELR`). Shrinks PIE relocation tables dramatically. | Default in modern toolchains/linkers (glibc, recent bionic, lld) for PIE shared objects. | `RELR` present. |
| **`BIND_NOW`** (eager binding) | `DT_FLAGS = BIND_NOW` / `DF_1_NOW`: the loader must resolve **all** relocations at load time, not lazily on first call. Paired with full RELRO. | Default hardening (`-Wl,-z,now,-z,relro`) in modern toolchains. | `BIND_NOW` set; `32 R_X86_64_GLOB_DAT`, RELR present, no IFUNC. |

The apkenv-era vendored shim linker (`libdl_bio.so.0`, AUR `bionic_translation
r107.026ea254-1`) predates all three being commonplace and does not implement them.

### 1.2 Why this is pervasive, not a one-lib quirk

- **TLS `errno` is universal.** Per-thread `errno` is the C-standard contract; `libm`,
  `libc`, and essentially every native lib that reports errors reads/writes it via TLS,
  emitting `R_X86_64_TPOFF64` (or the GD/LD/IE TLS variants). There is no realistic modern
  native lib that avoids TLS.
- **RELR + BIND_NOW are toolchain defaults**, not opt-ins, for PIE shared objects built in
  the last several years. Anything compiled with a current NDK/clang or current glibc
  carries them.

So provisioning host libs is **necessary-but-insufficient**: making the file findable
(whitelist + bare-soname symlink) is solved; making it *relocatable* is not. **The
limitation is the linker, not the libs.**

### 1.3 The blast radius (why it blocks the whole engine)

The relocation wall is on the **transitive `DT_NEEDED` resolution path**, which every
native load funnels through:

```
System.loadLibrary("zstd-jni-1.5.7-6")   ← opens (whitelist OK), then NEEDED libm.so → reloc type 18 → fail
        … and independently …
System.loadLibrary("roblox")  → libroblox.so (111 MB)
        NEEDED: libm.so, libmediandk.so, libOpenMAXAL.so, libOpenSLES.so,
                libGLESv2.so, libEGL.so, libandroid.so, liblog.so, libdl.so, libc.so
```

`libm.so` is a `DT_NEEDED` of **both** zstd-jni and `libroblox.so` (bionic-loader-plan.md
§4b.1). Even after the plan doc's `libmediandk.so`/`libOpenMAXAL.so`/`liblog.so` soname
shims are built, the *engine still cannot link* until the loader can relocate a
TLS+RELR+BIND_NOW lib — because `libm.so` (and any shim that itself NEEDs a modern
provider) hits the exact same wall. The order is therefore: **clear the relocation wall
first; the soname shims (plan doc §4) are downstream of it.**

---

## 2. Options evaluated

Each option is judged on whether it actually clears **`R_X86_64_TPOFF64` / RELR /
BIND_NOW** (the binding constraint), plus pros/cons/risk/effort and safeguard exposure.

### (a) Extend the C `bionic_translation` shim linker to handle the missing relocations

Teach `libdl_bio`'s linker `R_X86_64_TPOFF64` (allocate a static-TLS block / compute the
thread-pointer offset), `RELR` (iterate the bitmap), and honor `BIND_NOW`.

- **Clears the wall?** In principle yes — it is the most direct attack on the exact error.
- **Pros:** smallest *conceptual* delta IF the apkenv linker already has a TLS module
  table and a relocation dispatch to extend; reuses the proven dlopen/soinfo/namespace
  machinery; no new large component.
- **Cons / risk:** **TLS in a translation linker is genuinely hard** — it must allocate and
  register a static-TLS block compatible with how the *host glibc* `%fs`/TCB is laid out,
  for libs loaded *after* the host runtime already initialized its own TLS. Getting the
  thread-pointer math wrong is silent memory corruption, not a clean error. The apkenv-era
  codebase may have no TLS infrastructure to extend at all (then this is not "small"). It
  is forked C we'd then own/maintain.
- **Safeguard exposure:** **HOT.** This is dynamic-linker relocation-handling code — exactly
  the category the cyber-safeguard kills. Main-loop only; cannot be done in subagents.
- **Effort:** small-to-medium *if* the linker is close; medium-to-large if TLS infra is
  absent.

### (b) From-scratch Rust bionic-loader port (AGENTS.md's #1 do-LAST priority)

Eclipse's own pure-Rust bionic-namespace loader (`elf_loader`/`dlopen-rs`-class base per
component map), modern-relocation-capable from day one.

- **Clears the wall?** Yes, by construction — a modern Rust ELF loader implements TLS
  relocations, RELR, and BIND_NOW as table-stakes.
- **Pros:** the **durable, charter-aligned answer** (§2.1 purely-Rust for every line we
  own; bionic loader is the #1 Rust-port priority). Eclipse-owned, no apkenv-era debt,
  testable behind an ABI conformance suite, fixes the *class* of problem (any modern reloc),
  not one symptom. Removes the long-term dependency on a frozen upstream C shim.
- **Cons / risk:** **largest effort.** Must reproduce the two-namespace bionic ABI
  (bionic-linked vs host-glibc, ABI-incompatible — plan doc §3.2), `soinfo`/namespace
  semantics, constructor ordering, and host-TLS interop — the highest-risk item in the
  project. Charter says do it **last** precisely because of this.
- **Safeguard exposure:** **HOT** (it *is* a dynamic linker). Main-loop only.
- **Effort:** large.

### (c) Bionic-ABI shim libs built WITHOUT the unsupported relocations

Build `*_bio`-style shims for the offending libs (esp. `libm`) compiled so the *shim* emits
no TLS / RELR / BIND_NOW relocs, then have it forward to a provider.

- **Clears the wall?** **Largely no — assessed honestly as infeasible for `libm`/`errno`.**
  You can compile a *thin* shim `-fno-PIE`-ish / `-z norelro` / no-RELR so the **shim
  object itself** is reloc-clean. But the shim must still *provide working math + errno*,
  which means either (i) forwarding to the real glibc `libm.so.6` — whose TLS `errno`
  contract reappears at the boundary the moment any forwarded function touches `errno`, or
  (ii) reimplementing libm, which is absurd. A reloc-clean *wrapper* around a TLS-using
  *implementation* does not make the TLS go away; it relocates the problem to the call
  boundary. RELR/BIND_NOW you can suppress on the shim; **TLS you cannot**, because the
  behavior (per-thread `errno`) is semantic, not cosmetic.
- **Pros:** would be the smallest if it worked.
- **Cons / risk:** does not actually clear `R_X86_64_TPOFF64` for the real workload; risks a
  "links but corrupts errno across threads" outcome — a CLAUDE.md-forbidden symptom-hider.
- **Safeguard exposure:** low (shim build, not linker internals) — but moot since it doesn't
  solve the wall.
- **Effort:** small, but **wrong**.

### (d) Adopt/relink against a NEWER bionic linker (a real AOSP `linker64` port)

Replace the apkenv-era `libdl_bio` with a port/build of a *modern* AOSP `bionic/linker`
(which natively does TLS relocs, RELR, BIND_NOW), used as the translation linker.

- **Clears the wall?** Yes — modern AOSP `linker64` implements all three.
- **Pros:** the relocation correctness is upstream-maintained and battle-tested on billions
  of devices; no need to hand-write TLS reloc math.
- **Cons / risk:** a real AOSP linker expects the **bionic libc TLS/TCB layout and a bionic
  `__libc_init`-style runtime**, not host glibc's `%fs`/TCB. Dropping it into a glibc-hosted
  process (where libart is itself a *host glibc* build — AGENTS.md §6 2026-06-04 VM-boot
  entry) is a deep ABI-interop project: the very glibc-vs-bionic TLS mismatch that makes
  (c) hard, now at linker scope. It also pulls a large non-Rust C component permanently into
  the tree (against §2.1). Effectively as much integration risk as (b) with *less* Eclipse
  ownership.
- **Safeguard exposure:** **HOT** (linker source). Main-loop only.
- **Effort:** large; high integration risk; poor charter fit.

### (e) Hybrid — extend C now (a), Rust port later (b)

Do the **minimal** relocation extension in the C `bionic_translation` linker to clear the
*observed* relocs and unblock the engine bring-up now; keep AGENTS.md's #1 do-LAST Rust port
(b) as the durable replacement behind the ABI conformance suite.

- **Clears the wall?** Yes (via the (a) step), then re-clears it durably (via (b)).
- **Pros:** unblocks the engine-load frontier on the **shortest path** that actually works,
  while preserving the charter's end-state (Eclipse-owned Rust loader). Matches the existing
  charter language verbatim: "v1 may FFI the proven C `bionic_translation`… then port behind
  an ABI conformance suite (do it **last**)." The (a) extension is also a *spec* for (b):
  whatever TLS/RELR/BIND_NOW behavior we add in C is the conformance target the Rust port
  must match.
- **Cons / risk:** the (a) step's risks (TLS math) still apply; we maintain forked C in the
  interim.
- **Safeguard exposure:** HOT for both halves. Main-loop only.
- **Effort:** the (a) effort now + (b) effort later (already the planned #1 priority).

---

## 3. Recommendation (v1 path) + the smallest first validation step

**Recommended v1 path: (e) HYBRID — minimally extend the C `bionic_translation` linker to
support `R_X86_64_TPOFF64` (static-TLS), `RELR`, and `BIND_NOW` to unblock the engine NOW;
keep the from-scratch Rust bionic-loader (b) as the durable do-LAST replacement behind an
ABI conformance suite.**

**Why:**

1. It is the **only short path that actually clears `R_X86_64_TPOFF64`.** (c) is honestly
   infeasible for `libm`/errno TLS; (d) imports a glibc-vs-bionic TLS-interop project at
   linker scope with worse charter fit; (b) alone is the right *end-state* but is the
   project's largest, highest-risk, explicitly-do-LAST item — gating the entire engine-load
   frontier on it stalls progress.
2. It is **exactly what the charter already sanctions** (AGENTS.md §6 2026-06-04: "v1 may
   FFI the proven C `bionic_translation`… then port behind an ABI conformance suite, do it
   last"). The relocation evidence refines that sanction: v1 FFI is not enough *as-is* — the
   apkenv-era C linker must be **extended** for modern relocs before it can carry Roblox.
3. The C extension **doubles as the spec** for the Rust port: the TLS/RELR/BIND_NOW behavior
   we implement in C is the conformance target (b) must reproduce, so the work is not thrown
   away.

This explicitly honors Priority #1 (Stability) over #2 (Purely-Rust): ship a working,
modern-reloc-capable loader via the proven C base now; converge to the pure-Rust owned
loader behind tests later.

### 3.1 Smallest first step — DE-RISK WITH A PROBE before any linker edit

Per Eclipse's established de-risk-with-a-probe technique (the C-probe that proved the bare
ART boot, the empirical TypedArray-stride sweep): **before touching linker logic, write a
throwaway probe that loads a single TLS-using `.so` through the existing
`bionic_dlopen` path and confirms the failure mechanism + the chosen fix mechanism — in
isolation, away from the 111 MB engine.**

Concretely (main-loop / dev-host only):

1. **Reproduce in the small.** A ~10-line C/Rust probe that `dlopen`s the *already-provisioned*
   `libm.so` symlink via the bionic entry (`bionic_dlopen`) and prints the result. Expected:
   it reproduces `unknown reloc type 18 … failed to link libm.so` in isolation (no ART, no
   engine) — confirming the wall is purely the loader, reproducible in seconds.
2. **Prove the fix mechanism on one reloc.** Extend the C linker's relocation dispatch to
   handle `R_X86_64_TPOFF64` (allocate/register a static-TLS block, compute the
   thread-pointer-relative offset), re-run the probe, and confirm `libm.so` **links** and a
   trivial `errno`-touching call (`sqrt(-1.0)` → `errno == EDOM`) returns the right per-thread
   value. That single green result validates the whole path; RELR + BIND_NOW are then
   incremental on the same dispatch.
3. **Only then** scale up: re-run `eclipse run <apk>` and watch the frontier advance from
   "failed to link libm.so" to the next real stop (the `libmediandk.so`/`libOpenMAXAL.so`
   soname shims of `bionic-loader-plan.md` §4, now downstream).

The probe is throwaway diagnostic scaffolding (CLAUDE.md "de-risk with a probe"), not a
shipped artifact; it confirms the failure mechanism and the fix mechanism with evidence
before any durable linker change — and it keeps the risky TLS math contained to one tiny,
inspectable case first.

> **Implementation altitude reminder:** steps 2–3 above are dynamic-linker
> relocation-handling work — **main-loop / interactive only**, never a workflow subagent
> (cyber-safeguard). This doc deliberately stops at the *decision* and the *probe shape*; it
> writes no relocation code.

---

## 4. Parallel framework status (holistic project state, 2026-06-05)

So the project state is captured as a whole — the engine-load relocation wall is **one of
two** live frontiers; the framework side is further along:

- **Roblox reaches its own `Application.onCreate`** and runs startup tasks (`roblox.config`
  → `setBaseUrl www.roblox.com`, `AppStartupTaskManager`, `androidx.startup.
  InitializationProvider`) — far past the demo's `onCreate`. Eclipse supplies its own
  **non-GTK** `Context`/`Log`/`AssetManager`/`Environment`/`XmlBlock`/`View`/`ViewGroup`/
  `TextView`/`Window`/`Paint`/`Theme` natives via `RegisterNatives`, and the
  AssetManager/AXML/ARSC backing resolves manifest + framework + app resources (AGENTS.md
  §5/§6).
- **Framework-side remainders** (independent of the relocation wall):
  1. **Rendering** — the real **ash/Vulkan surface + draw** (Vulkan-first, EGL fallback).
     The View tree + ids + text are already recorded in `view_registry`, ready to render;
     this is the deferred big M2/M3 build. The engine makes its own `VkInstance` later, so no
     surface is needed merely to reach `onCreate`.
  2. **Background-thread `Looper` provisioning** — Roblox's `AppStartupTaskManager` runs on a
     background thread that NPEs on `Looper.mQueue` (background threads have no Looper);
     needs main-thread/background-thread Looper+MessageQueue provisioning. (Faithful run:
     this and the subsequent SIGSEGV are on the **engine-load native track**, not a Rust
     panic/`RuntimeError` — the provisioning/whitelist calls are clean, grep count 0.)

**Net:** the framework track is at "drive the post-`onCreate` lifecycle + stand up
rendering + background-thread Looper"; the engine-load track is now blocked precisely at the
**bionic-linker modern-relocation wall** characterized above, with the v1 path chosen
(hybrid: extend C now, Rust port last) and a probe as the smallest first validation.

---

## 5. References

- **AGENTS.md §5 / §6** (2026-06-05 `dl_parse_library_path`, `provision_bionic_sonames`
  entries) — the faithful `eclipse run` evidence digested here; the `unknown reloc type 18`
  → `failed to link libm.so` wall; `readelf -r`/`-d` on host `libm.so.6`
  (`R_X86_64_TPOFF64`/RELR/BIND_NOW). Faithful run log: `/tmp/eclipse-roblox.log` (EXIT=139).
- [`bionic-loader-plan.md`](./bionic-loader-plan.md) — the build-ready *soname-shim* spec
  (§4b/§4c: `libroblox.so` `DT_NEEDED`, the 3 missing sonames, the bionic-ABI build recipe).
  The relocation wall in *this* doc is **upstream** of that shim work.
- `src/runtime.rs` — Eclipse's current lib-provisioning strategy
  (`whitelist_bionic_library_path` / `provision_bionic_sonames` / `boot()`'s
  `RTLD_NOW|RTLD_GLOBAL`) — the necessary-but-insufficient preconditions referenced in §1.
- [`component-map.md`](./component-map.md) — bionic loader as the #1 Rust-port priority,
  deferred to last (the (b) end-state).
- [`dev-host-runbook.md`](./dev-host-runbook.md) — Section B (engine-load bionic track); the
  probe + extension here is its next concrete step, main-loop only.
- General public ELF knowledge (this author's own memory): the meaning of
  `R_X86_64_TPOFF64` (TLS thread-pointer offset), `RELR` (compressed relative relocations),
  `BIND_NOW`/`DF_1_NOW` (eager binding). **No linker source was read to write this doc.**
