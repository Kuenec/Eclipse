# Engineering Instructions

## Scope

**Every instruction in this file is a hard requirement.** It is mandatory policy for every project, language, platform, and domain, and for every agent, script, tool, and implementation plan — before, during, and after code changes. Nothing here is optional, advisory, or best-effort unless a line explicitly says so. Do not reinterpret any requirement as a suggestion, and do not skip one because the task seems small. If an instruction conflicts with speed, convenience, assumptions, or preferred style, this file wins.

Compliance is verified, not assumed: work is not compliant until the final answer demonstrates how the relevant requirements were satisfied (see Completion Standard).

**Follow the spirit, not just the letter.** Restructuring a violation so it technically passes a rule is itself a violation. Splitting, renaming, relocating, fragmenting, or rewording prohibited behavior does not make it permitted — if the outcome is what a rule prohibits, the rule is broken regardless of the form. When unsure what a rule intends, honor its stated purpose or ask.

---

## Persistent Memory

**Hard requirement — reading and updating this file is never optional.** Maintain `Persistent_Memory.md` in the project root as a handoff document. Its only purpose is continuity: if context is compacted, the session ends, or a new agent takes over, it must contain enough to continue exactly where work stopped without re-asking or re-deriving anything. It holds the project's living state only — never these engineering principles.

**Startup:** Read it fully before doing anything else. If it doesn't exist, create it, ask the user for the goal, task, and constraints, then populate it from their answer and what you learn about the project.

**It must always contain:**

- The current goal and the specific task being worked on.
- The current state of the code on the active task — what is in place and working now, not how it got there.
- What is in progress right now, including the exact stopping point.
- Concrete next steps, in order.
- Open questions and hypotheses, each marked confirmed, disproven, or unverified.
- Key decisions that still affect the current approach, with reasoning.
- Relevant file paths, commands, artifacts, logs, and how to reproduce or verify.
- Known blockers, risks, and constraints.

**Maintenance (every session):** Update it before ending a session and after changes, new information or logs, builds or artifacts, and confirmed or disproven hypotheses. It is a current-state snapshot, not a changelog or diary. Prune aggressively every time you touch it: delete completed steps, resolved issues, abandoned approaches, superseded decisions, and anything that no longer changes what happens next. When something is resolved, remove every trace of it — including that the problem ever existed. Keep a past issue only if it still constrains the current approach (e.g. an approach that must be avoided because it reintroduces the bug). Stale, resolved, or irrelevant content is a defect.

---

## Core Principle

**Hard requirement — workarounds are prohibited, not discouraged.** Produce durable, root-cause fixes with the smallest necessary change. Every line must have a clear reason to exist — no bloated, padded, or speculative code. Never use workarounds, temporary mitigations, shortcuts, bypasses, feature disabling, code removal, or error suppression unless explicitly requested. A fix is unacceptable if it merely avoids the failing path, disables or removes the affected component, catches and ignores the error, changes behavior to dodge the crash, or narrows the symptom without correcting the underlying cause. Never present a workaround as a fix or ask the user to accept one. When in doubt, choose durable engineering over speed.

---

## Compatibility and Portability

Changes must work on every platform, environment, and configuration the project targets — not just the current machine. Do not assume an operating system, version, vendor, hardware capability, CPU/architecture, runtime version, locale, install path, permission level, or configuration unless explicitly verified and documented. Code touching the OS, hardware, drivers, system services, filesystems, networking, or low-level APIs must account for the full supported range, including fresh, upgraded, unusual, and restricted setups.

Compatibility-sensitive code must:

1. Detect capabilities instead of assuming them.
2. Handle unsupported or unavailable features with clear, intentional behavior.
3. Avoid hardcoded vendor, version, path, or hardware assumptions unless required and justified.
4. Preserve existing support for other environments while fixing the current issue.
5. Include edge-case or regression coverage for materially different platforms, versions, architectures, permission states, or configurations when relevant.

A task that only works on the developer's machine, one platform, one version, or one common configuration is not complete. Low-level code must work on everyone's machine and handle every edge case.

---

## Build and Environment Portability

The project must build and run consistently for every developer and intended user. Never hardcode local machine paths, usernames, drive letters, absolute build folders, SDK/tool/cache/temp locations, Desktop or Downloads folders, local clone paths, one developer's IDE/compiler/runtime/package-manager paths, or machine-specific identifiers, device IDs, or environment-specific keys (unless explicitly required and detected). Use project-relative paths, documented environment variables, build-system discovery, toolchain files, configuration files, or explicit user-provided settings.

If a dependency, path, SDK, tool, or capability is required:

1. Detect it through the build system or runtime capability checks.
2. Document the requirement clearly.
3. Fail with an actionable error if it is missing.
4. Never silently fall back to a developer-specific path.
5. Verify the solution works from a clean checkout at a different machine path.

A change that only builds or works because of files, folders, environment variables, cached artifacts, or tools that exist only on the current machine is unacceptable.

---

## Mandatory Research and Context7

**Hard requirement — no exception for familiarity or confidence.** Context7 is how you stay current on language features and syntax, standard-library and framework APIs, toolchains, platform behavior, and idiomatic best practices — not only third-party libraries. Before any change whose correctness depends on how something currently works, consult Context7.

For every change:

1. Read the relevant project files before editing.
2. Use Context7 for anything that has a current correct form or may have changed over time: languages, libraries, frameworks, APIs, toolchains, platforms, idioms.
3. Never write or modify code from memory or assumption when correctness depends on current behavior — verify features, signatures, parameters, return values, version differences, and idioms first.
4. Prefer official documentation, source code, project docs, vendor docs, and standards over guesses, blog posts, or outdated examples.
5. Verify the planned approach matches the actual versions, APIs, configuration, and constraints of this project.
6. State explicitly which Context7 documentation and which project files were checked.

If Context7 is unavailable or doesn't cover the specific library or version: never silently fall back to guessing or training-data memory; state it clearly, name the authoritative source used instead, and flag anything unverifiable as a known risk in the final response.

When the user asks for research, it must be deep, thorough, credible, and practical — prioritizing authoritative sources and known-working solutions. No shallow research, unsupported claims, random snippets, or guesses.

---

## Before Changing Anything

1. Read and follow the project's instruction files (such as `CLAUDE.md`) before making changes.
2. Read the relevant existing implementation before proposing edits.
3. Use Context7 when documentation, APIs, libraries, tools, or platform behavior are relevant.
4. State assumptions explicitly. If something is unclear, name what is confusing and ask before proceeding.
5. If multiple interpretations exist, present them instead of silently choosing one.
6. If a simpler approach exists, say so. Push back when the requested approach seems overcomplicated, risky, or inconsistent with the project.
7. Define success criteria before making changes.

Never guess or speculate about a bug's cause when evidence is insufficient. Before proposing a fix, clearly separate: (a) what is known from evidence, (b) what is suspected but unproven, and (c) what diagnostics would prove or disprove it. Do not proceed with a code change as the fix until the failure mechanism is confirmed by evidence, reproduction, logs, tests, traces, diagnostics, or code-path analysis.

---

## Root-Cause Diagnosis

**Hard requirement — no fix without a confirmed failure mechanism.** The task is always a permanent root-cause fix. If the root cause is unknown, the next step is improving observability until the failure mechanism is identified with confidence — not repeatedly changing nearby code until the symptom moves.

Useful diagnostics: targeted debug prints (file, function, branch, state, key values, error codes, timestamps); assertions proving required invariants where they matter; structured logs around suspected boundaries or state transitions; error context (operation attempted, exact failure, environment details); traces showing call order, threads, timing, ownership, lifetimes, or data flow; removable temporary diagnostic output; minimal reproductions or test instrumentation.

Diagnostics must be specific enough to answer: where exactly is the failure; which condition, value, state, or assumption is wrong; why that wrong state is possible; which code path produced it; and the smallest correct place to fix it. Do not brute-force fixes. Diagnostic changes exist only to discover the root cause — they are not the fix, and temporary output is removed or reduced once the cause is confirmed.

For every bug, crash, failure, regression, or implementation issue:

1. Identify the actual root cause and failure mechanism.
2. Fix the broken mechanism itself, not the visible symptom.
3. Preserve the intended architecture, behavior, and feature set unless a design change is explicitly approved.
4. Search the entire codebase for equivalent instances of the same flawed pattern — including adjacent modules, shared helpers, generated or configured entry points, startup paths, tests, and build scripts — and fix every instance, not just the one that failed first.
5. Add or update regression protection so the same class of issue cannot be silently reintroduced.
6. Clearly state: the root cause found, what was fixed, what was audited, what regression guard was added, and why the issue won't recur.

A task is complete only when the failure mechanism is corrected everywhere the pattern exists and protection is in place — not merely when the current error stops appearing.

---

## Error Handling and Failure Behavior

Errors must be visible, accurate, and actionable. Surface real failures early; never hide them to make the symptom disappear.

Required:

1. Fail fast and loudly on programming errors and unmet invariants rather than continuing in an invalid state.
2. Preserve error context — the operation attempted, relevant inputs or state, and the original cause. Chain or wrap errors instead of discarding them.
3. Handle only the specific errors you can meaningfully recover from, at the level where recovery is actually possible.
4. Validate inputs and invariants at boundaries so failures happen close to their cause.

Never: swallow or silently ignore errors (no empty catch blocks, no discarded error returns); use broad catch-alls to hide unhandled failures; convert a real error into a default, empty, or null value that hides it from callers; use exceptions or error returns for normal control flow; log an error and continue as if nothing happened when the operation cannot succeed. An error path is correct only when the failure is properly recovered from or clearly propagated.

---

## State, Resources, and Concurrency

**Resources:**

1. Release every acquired resource on all paths, including error paths: files, sockets, connections, handles, locks, timers, subscriptions.
2. Use the language's scoped-cleanup mechanism (context managers, RAII, `defer`, `try/finally`, `using`) rather than manual cleanup a later edit can skip.
3. No unbounded growth: caches, queues, buffers, and collections must have a bound or eviction policy.

**State:**

1. Avoid global and shared mutable state; pass state explicitly and scope it narrowly.
2. Keep functions free of hidden side effects not obvious from their signature and purpose.
3. Never rely on initialization or import order unless it is explicitly guaranteed.

**Concurrency:**

1. Protect shared mutable state across threads, tasks, or processes; never assume an operation is atomic when it isn't.
2. Never assume callbacks, async tasks, events, or messages arrive in order unless that order is guaranteed.
3. Make any operation that may be retried idempotent.
4. Be deliberate about lock ordering and scope to avoid deadlocks and contention; hold locks for the minimum necessary time.

---

## Performance and Memory Safety

Code must be fast and memory-safe at the same time. Optimize real runtime speed, not just what satisfies the compiler.

1. Choose efficient algorithms and data structures from the start — that is the baseline, not premature optimization.
2. Avoid unnecessary work: redundant computation, repeated lookups, needless allocations, copies, and clones. Reuse buffers and reserve capacity where it clearly helps.
3. Pay closest attention to hot paths, inner loops, and large inputs; avoid accidental quadratic or worse behavior.
4. Prefer streaming or incremental processing over loading everything into memory when data can be large.
5. Optimize the code itself, not just compiler flags.

Memory safety is never traded for speed: stay within safe constructs by default (the fast path and safe path are usually the same with the right data structures). In Rust, use `unsafe` only when there is no safe way to achieve a required result — keep each block as small as possible, encapsulate it behind a safe interface, and document the exact soundness invariants with a dated comment. Never introduce data races, use-after-free, buffer overruns, uninitialized reads, or other undefined behavior. When speed and clarity genuinely conflict, keep the code correct and memory-safe first, then make it as fast as possible within those constraints. Avoid micro-optimizations that obscure the code without a real speed benefit.

---

## Zero Errors and Zero Warnings

A clean build is part of the definition of done for every project, regardless of language.

1. Code must compile and build with zero errors and zero warnings under the project's standard build and toolchain settings.
2. Fix the cause of every warning. Never silence one with suppressions, pragmas, allow attributes, or lowered warning levels unless suppression is genuinely correct and justified in a dated comment.
3. Treat warnings as defects — they often point at real bugs (unused results, implicit conversions, null/uninitialized use, unreachable code, deprecated APIs).
4. Never disable, downgrade, or remove existing warning or lint configuration to make the build look clean.
5. This covers the project's own code. If a warning originates in a third-party dependency and cannot be fixed, say so explicitly and explain why rather than blanket-suppressing it.

---

## Regression Protection

A regression guard is the smallest reliable check that would fail if the same bug or class of bug came back. Do not automatically create new scripts — a new script is acceptable only when it is the simplest maintainable option and nothing existing can cover the issue.

Preference order:

1. Existing unit, integration, or end-to-end tests.
2. Existing project test harnesses or build checks.
3. Focused test cases added to the closest relevant test file.
4. Assertions or validation checks enforcing an invariant at the correct boundary.
5. Existing lint/static-analysis/configuration rules.
6. A small new test file only when no suitable test location exists.
7. A new script only when the project has no better mechanism and it is clearly justified.

Guards must be directly tied to the confirmed root cause — never broad, unrelated, speculative, or created just to satisfy the words "regression guard." A guard is acceptable only if it would have caught the confirmed bug, runs on other developers' machines without local assumptions, fits the project's existing test/build style, adds no unnecessary maintenance burden, hardcodes no user-specific paths or environment details, and is documented in the final response with the exact command or check. If no automated guard is practical, explain why and provide the strongest alternative: a targeted manual verification checklist, runtime assertion, build-time validation, or diagnostic check.

---

## Testing Integrity

Tests exist to prove behavior is correct. A test that passes without proving correctness is worse than no test — it creates false confidence.

Required:

1. Test observable behavior and contracts, not incidental implementation details.
2. Tests must be deterministic and independent: no reliance on timing, ordering, network, wall-clock time, or leftover state from other tests.
3. Assertions must be meaningful and specific enough to fail when behavior is actually wrong.
4. When fixing a bug, add a test that fails before the fix and passes after it.

Prohibited: special-casing the test's inputs, hardcoding expected outputs, or detecting the test environment; weakening, skipping, commenting out, or deleting a failing test to go green (unless genuinely obsolete and stated explicitly); tautological tests that assert the implementation against itself; over-mocking until no real behavior is exercised; loosening an assertion to stop a flaky failure instead of fixing the nondeterminism. If a test fails, fix the cause or the test's correctness — never its honesty.

---

## Simplicity First

Write the minimum code that solves the confirmed problem. If a line, function, abstraction, or file has no clear reason to exist, it should not be there. Do not add speculative features, abstractions, configurability, or flexibility that was not requested.

Avoid: features beyond what was asked; abstractions for single-use code; "future-proofing" without evidence; error handling for impossible scenarios; large rewrites when a small fix suffices; new scripts, tools, or frameworks when an existing project mechanism is enough.

If the solution is longer or more complex than necessary, simplify it before presenting. Ask: "Would a senior engineer say this is overcomplicated?" If yes, rewrite it.

---

## Surgical Changes

Touch only what is required to satisfy the request.

1. Do not improve adjacent code, comments, formatting, or structure unless directly required.
2. Do not refactor unrelated code.
3. Match the existing project style, even if you would normally do it differently.
4. Mention unrelated dead code instead of deleting it; never remove pre-existing dead code unless explicitly asked.
5. Do remove imports, variables, functions, or files that your own changes made unused.

Every changed line must trace directly to the user's request, the confirmed root cause, the same-pattern audit, or the regression guard.

---

## Data Safety and Destructive Operations

For any operation that deletes, overwrites, migrates, or mutates persistent data (databases, files, schemas, production state):

1. Never perform irreversible or destructive operations unless they are clearly part of the request.
2. Confirm the exact scope before acting; never run a broad destructive operation when a narrow one is intended.
3. Prefer reversible, idempotent, re-runnable changes with a defined rollback or recovery path.
4. Ensure a backup, transaction, or recovery path exists before running a destructive change against important data.
5. Wrap multi-step data changes in transactions where supported, so partial failure doesn't leave corrupt state.
6. Default to a dry-run or preview for bulk operations when available.

Never use a destructive shortcut (dropping, truncating, recreating, or wiping data) as a workaround for a problem that has a non-destructive root-cause fix.

---

## Anti-Patterns to Avoid

Avoid unless there is a specific, justified reason:

- Magic numbers and strings — name constants instead of scattering unexplained literals.
- Copy-paste duplication — extract shared logic once a pattern genuinely repeats.
- God functions and god classes — keep units focused; split unrelated responsibilities.
- Deep nesting and long parameter lists — prefer early returns, guard clauses, and grouped parameters.
- Boolean-trap parameters whose meaning is unclear at the call site.
- Mutable default arguments and shared default containers.
- Stringly-typed code — use proper types and enums instead of passing meaning as raw strings.
- Premature micro-optimization that sacrifices safety or readability (efficient algorithms and data structures up front are expected, not premature).
- N+1 queries, repeated work in hot loops, and accidental quadratic behavior on large inputs.
- Speculative abstraction for cases that do not exist yet.
- Commented-out code left in place — delete it; version control is the history.
- Inconsistent or misleading names — a name must match what the code does.
- Reinventing well-tested functionality the project or standard library already provides.
- Floating point for money or exact values, and floating-point equality comparisons.
- Naive date/time handling — store and compute in an unambiguous form (such as UTC) and handle time zones explicitly.
- Assumed text encoding — be explicit (default UTF-8) instead of relying on the environment.
- Debug prints, dead branches, or scaffolding left in final code; remove temporary diagnostics once the root cause is confirmed.

---

## Comments and Documentation

Keep comments minimal. "Minimal" is measured by the **total volume of commentary**, not the length of individual comments. Splitting one long comment into several short stacked or scattered comments is the same violation in a different shape — consecutive or fragmented comment lines count as one comment, and their combined content must obey these rules. Comments exist only to explain what is not obvious from the code — the *why* behind a non-obvious decision or constraint, never the *what* the code already states. Default to no comment when the code is self-explanatory. A file must remain overwhelmingly code, not commentary.

Never: restate what the code plainly does; narrate routine code line by line; write a long block where a short one, a clearer name, or clearer code would do; break one comment into many small ones to appear concise; let comments dominate a file; leave decorative banners, filler, or boilerplate scaffolding.

Date (`YYYY-MM-DD`) any comment describing behavior, assumptions, platform details, external APIs, offsets, compatibility constraints, or implementation reasoning, so future readers know when it was written or verified:

```
// 2026-06-05: This API returns an "insufficient buffer" error on the first call when it is used only to query the required size.
```

Never trust an old comment over the code. If a comment conflicts with the implementation, tests, logs, or current documentation, treat it as stale and verify against the source of truth. When changing code near a stale or misleading comment, update or remove it; never preserve comments known to be wrong or add broad comments that will quickly rot.

---

## Goal-Driven Execution

Turn tasks into verifiable goals:

- "Add validation" means: write or update tests for invalid inputs, then make them pass.
- "Fix the bug" means: reproduce it with a test, diagnostic evidence, logs, traces, or code-path proof, then make the check pass.
- "Refactor X" means: verify behavior before and after.

For multi-step tasks, state a brief plan before implementing, with a specific verification check per step. Success criteria must be concrete — never vague goals like "make it work." Loop until the defined checks pass.

---

## Truthful Reporting and No Fabrication

Every claim about the work must be true and verifiable. False confidence is more dangerous than admitted uncertainty.

1. Only state that something was run, built, tested, or verified if you actually did it. Report the real command and the real result.
2. Never fabricate or guess command output, test results, logs, benchmarks, file contents, or API behavior. If you did not observe it, do not present it as observed.
3. Clearly separate what is verified by evidence from what is assumed, expected, or untested.
4. Never invent APIs, functions, parameters, configuration keys, or library behavior — confirm against the source or Context7 first.
5. If something could not be run or verified (missing tool, environment limit, blocked access), say so plainly and state what remains unverified.
6. When uncertain, say so and what would resolve it, rather than presenting a guess as fact.

Never claim completion, success, or correctness you have not actually confirmed.

---

## Completion Standard

Work is complete only when all of the following are true:

1. The root cause is identified with evidence.
2. The broken mechanism is fixed directly.
3. Equivalent instances of the same flawed pattern have been searched for and addressed.
4. Appropriate regression protection has been added or updated, without unnecessary scripts.
5. Relevant tests, checks, or builds pass, and the reported results are real, not assumed.
6. The project builds and runs with zero compiler errors and zero warnings, and no warning was silenced instead of fixed.
7. Build and runtime assumptions are portable and do not depend on one developer's machine.
8. The code is memory-safe, with `unsafe` (in Rust) avoided or minimized, encapsulated, and justified.
9. The code is reasonably optimized for speed without sacrificing memory safety, correctness, or clarity.
10. Errors and resources are handled correctly — no swallowed errors, leaks, or destructive shortcuts.
11. Comments are minimal and explain only what the code does not make obvious.
12. Context7 and relevant project documentation were checked as required, or any unavailability was disclosed.
13. `Persistent_Memory.md` reflects the current state only, with completed and resolved items pruned out.
14. The final response explains: the confirmed root cause; the fix made; the scope of the audit; the regression guard (or why a different verification method was used); the verification performed, with the exact commands/checks and their actual results; and the documentation, project context, and Context7 references checked or noted as unavailable.

The goal: fewer unnecessary diffs, fewer overcomplicated rewrites, no machine-specific changes, better diagnosis before implementation, and fixes that are correct for the whole project and user base.

---

## Enforcement

Before responding, verify against this file. If any requirement above was skipped, the work is not done — go back and satisfy it. Do not report completion, and do not present results as final, unless every applicable item in the Completion Standard is genuinely met and item 14 is included in the response. Partial compliance is non-compliance, and technical compliance that defeats a rule's purpose is non-compliance.