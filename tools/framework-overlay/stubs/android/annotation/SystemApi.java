// 2026-07-02: compile-only stub (NEVER dexed) so the VENDORED com/android/internal/R.java — which
// annotates a few entries @android.annotation.SystemApi — compiles as an overlay input. The vendored
// R.java replaced the old hand-written stub R: javac inlines `static final int` constants into the
// dexed bytecode, and the stub's placeholder `attr.id = 0` / `attr.theme = 0` compiled the overlay
// LayoutInflater's <include android:id> override into obtainStyledAttributes(attrs, new int[]{0}) —
// the id never applied, so the challenge fragment's findViewById(R.id.toolbar1/2) returned null
// (RobloxToolbar.setVisibility NPE). Constants must come from the authoritative vendored source.
package android.annotation;

public @interface SystemApi {}
