//! Process-global generational-slab registry for Eclipse-owned `android.view.View` handles.
//!
//! 2026-06-05: AOSP's `View`/`ViewGroup` hierarchy is fully native-handle-backed — each Java `View`
//! constructs a native peer and stores its `long` handle (`View.java` native constructor /
//! `mNativePtr`), and `ViewGroup`/`setContentView`/`Window.setContentView` wire those handles into a
//! tree. ATL backs these in C against GTK widgets (`GtkWidget*`). Eclipse must NOT pull in GTK (it
//! re-crowds the low_4gb window — AGENTS.md §5 Step 3.5), so a View's `long` handle is an
//! **Eclipse-owned generational-slab index into this slab — NOT `Box::into_raw`, NOT a raw pointer**,
//! exactly the soundness pattern of [`window_registry`](super::window_registry) and
//! [`xml_registry`](super::xml_registry). A stale/fabricated `jlong` from Java is a
//! bounds+generation-checked `Err`, never a wild dereference / use-after-free / UB.
//!
//! ## Handle layout
//! Identical to [`window_registry`](super::window_registry): a [`jlong`] packing a `u32` slot index
//! (low 32 bits) + a `u32` generation (high 32 bits). Generations start at 1, so a valid handle is
//! never `0` — `0` stays the reserved "no view" / null sentinel.
//!
//! ## Scope (this increment)
//! [`ViewState`] records the **non-GTK view-tree metadata** step 4/5 (`createMainActivity` →
//! `Activity.onCreate` → `setContentView` → the `View`/`ViewGroup` native cascade) sets: the view's
//! Java class name, an optional text/title, and its child view handles (the tree relationships). It
//! deliberately holds **no** GTK widget and performs **no** layout/measure/draw — the real
//! ash/Vulkan surface + rendering is the deferred big build (AGENTS.md §5). Recording the tree shape
//! soundly is what lets the lifecycle proceed past the View constructors without GTK.
//!
//! ## Thread-safety
//! The slab lives behind a [`Mutex`] inside a [`OnceLock`] (process-global, std-only, no new dep),
//! same as [`window_registry`](super::window_registry); a poisoned lock surfaces as the typed
//! [`ViewRegistryError::Poisoned`] rather than a panic on the JNI path (AGENTS.md §2.8).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::objects::JObject;
use jni::refs::Global;
use jni::sys::jlong;

/// Process-global slab of [`ViewState`], guarded by a [`Mutex`]. Initialized on first use.
static VIEWS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// 2026-06-05: the handle of the window's content-root view, published by
/// `Window.set_widget_as_root`. The renderer reads it each frame (via [`snapshot_tree`]) to know
/// which view subtree to draw — a single source of truth for "what is on screen". `0` = none set yet.
///
/// An `AtomicI64` (not behind the slab `Mutex`) so a frame read is lock-free and cannot deadlock
/// against a concurrent view mutation; it stores only the packed `jlong` handle (an opaque index,
/// not a pointer), validated against the slab on use, so a stale value is a checked `Err`, not UB.
static ACTIVE_ROOT: AtomicI64 = AtomicI64::new(0);

/// A view-registry handle as it travels across JNI: a `jlong` (`i64`) packing the slot index (low 32
/// bits) and the slot's generation (high 32 bits). `0` is the reserved "no view" / null sentinel.
pub type ViewHandle = jlong;

/// Errors from the view registry. Every fallible path returns one of these instead of panicking, so
/// a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and
    /// possibly reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
    Poisoned,
}

impl fmt::Display for ViewRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("view handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("view handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("view registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for ViewRegistryError {}

/// `LayoutParams.width`/`height` sentinel: the view fills the parent's available size.
/// 2026-06-05: standard Android encoding (`ViewGroup.LayoutParams.MATCH_PARENT`).
pub const MATCH_PARENT: i32 = -1;
/// `LayoutParams.width`/`height` sentinel: the view sizes itself to its content.
/// 2026-06-05: standard Android encoding (`ViewGroup.LayoutParams.WRAP_CONTENT`).
pub const WRAP_CONTENT: i32 = -2;

/// The requested layout parameters of a view, as Android encodes them.
///
/// 2026-06-05: recorded verbatim from `View.native_setLayoutParams`/`native_setPadding` (the values
/// `LayoutInflater` parsed from `layout_width`/`layout_height`/`layout_gravity`/`layout_weight`/
/// margins/padding). `width`/`height` use Android's sentinels: [`MATCH_PARENT`] (-1), [`WRAP_CONTENT`]
/// (-2), else an exact pixel count. `gravity` is the packed `Gravity` bitmask the parent honors when
/// positioning the child (default `0` → top|left). Eclipse's measure/layout pass (in `graphics.rs`)
/// consumes these to compute each view's absolute rect — there is no GTK and no per-view native
/// measure/layout driver, so the cascade runs once over the recorded tree at render time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutParams {
    /// Requested width: [`MATCH_PARENT`], [`WRAP_CONTENT`], or an exact pixel count (`>= 0`).
    pub width: i32,
    /// Requested height: [`MATCH_PARENT`], [`WRAP_CONTENT`], or an exact pixel count (`>= 0`).
    pub height: i32,
    /// The child's `layout_gravity` bitmask (how the parent positions it within the slot). `0` = default.
    pub gravity: i32,
    /// The child's `layout_weight` (LinearLayout leftover-space distribution). `0.0` = no weight.
    pub weight: f32,
    /// Left/top/right/bottom margins in pixels (the gap the parent leaves around the child).
    pub margins: [i32; 4],
    /// Left/top/right/bottom padding in pixels (the gap inside the view, around its content).
    pub padding: [i32; 4],
}

impl Default for LayoutParams {
    /// Android's default for an un-parameterized view: `WRAP_CONTENT` on both axes, no gravity/weight,
    /// no margins/padding. A view never sized by `setLayoutParams` still lays out sanely.
    fn default() -> Self {
        Self {
            width: WRAP_CONTENT,
            height: WRAP_CONTENT,
            gravity: 0,
            weight: 0.0,
            margins: [0; 4],
            padding: [0; 4],
        }
    }
}

/// Per-view state held in a registry slot: the non-GTK view-tree metadata the View natives set.
///
/// 2026-06-05: records what is needed to track the view hierarchy's shape **and lay it out** without
/// GTK. The view's `class_name` (which `View`/`ViewGroup`/`FrameLayout`/`TextView` subclass
/// constructed it), an optional `text` (`TextView.setText`/`Window.setTitle`), its `children` (the
/// ordered child view handles a `ViewGroup.addView` wires in), and its requested [`LayoutParams`]
/// (`native_setLayoutParams`/`native_setPadding`). The real winit `Window`/Vulkan surface
/// association is a later, deferred step; the absolute rect is computed by the renderer's
/// measure/layout pass from these params, not stored here.
#[derive(Debug, Default)]
pub struct ViewState {
    /// The Java class name that constructed this view (e.g. `android.widget.FrameLayout`), recorded
    /// by the view's native constructor for diagnostics + tree shape. Empty until a constructor sets it.
    pub class_name: String,
    /// Optional text/title set on this view (`TextView` text, or a container title). `None` until set.
    pub text: Option<String>,
    /// Ordered child view handles wired into this view by `ViewGroup.addView` (the tree edges).
    /// Stored as handles (not references) so the slab stays a flat `Vec` with no internal aliasing.
    pub children: Vec<ViewHandle>,
    /// The view's requested [`LayoutParams`], recorded by `native_setLayoutParams`/`native_setPadding`.
    /// Defaults to `WRAP_CONTENT` on both axes (Android's default) until those natives set it.
    pub layout: LayoutParams,
    /// 2026-06-05: `true` once `View.nativeSetOnClickListener` recorded a click listener on this view
    /// (Android marks the native peer clickable). The renderer's hit-test only dispatches a click to a
    /// clickable view, so an inert container/text view is never targeted. Defaults `false`.
    pub clickable: bool,
    /// 2026-06-05: a JNI **global** reference to this view's Java `View` object, recorded by the view's
    /// native constructor from its `this`. Held so a pointer click resolved to this view (by handle)
    /// can dispatch `View.performClick()` on the **real** Java object from the event loop, firing the
    /// registered `OnClickListener`. `None` if the constructor could not create the global ref (the
    /// view is then non-dispatchable — logged, never UB). A `Global` is `Send`, so it lives soundly in
    /// this process-global slab; it is released when the slot is [`free`]d (its `Drop` deletes the ref).
    pub jobject: Option<Global<JObject<'static>>>,
    /// 2026-06-05: the view's solid background color (`View.setBackgroundColor`, ARGB as
    /// `Color.argb`/`0xAARRGGBB`), recorded by `View.native_setBackgroundColor`. `None` until set; the
    /// renderer fills the view's rect with this color (real fidelity) and otherwise uses a synthetic
    /// depth-distinguished color. A drawable background (`native_setBackgroundDrawable`) is separate.
    pub background_color: Option<i32>,
    /// 2026-06-13: JNI **global** references to the `android.text.TextWatcher`s registered on this view
    /// by `EditText.native_addTextChangedListener` (removed by `native_removeTextChangedListener`).
    /// Eclipse's vendored `EditText.addTextChangedListener` (EditText.java:52) passes the watcher
    /// straight to the native WITHOUT keeping a Java field, so the native side must RETAIN it (a plain
    /// local arg would be collected the moment the call returns) — held here so a future input-dispatch
    /// path can invoke `TextWatcher.onTextChanged(...)` on the real object. No input occurs during boot,
    /// so recording is the complete correct behavior now. Each `Global` is `Send` and releases its ref
    /// on `Drop` (slot [`free`]d or this `Vec` cleared). Empty until a watcher is added.
    pub text_watchers: Vec<Global<JObject<'static>>>,
    /// 2026-06-13: a JNI **global** reference to the `TextView$OnEditorActionListener` registered on
    /// this view by `EditText.native_setOnEditorActionListener` (EditText.java:57 passes it straight to
    /// the native with no Java field, so the native must retain it). A view has at most one editor-action
    /// listener; a new registration replaces the prior (its `Drop` releases the old ref). Held so a
    /// future IME/input-dispatch path can invoke `onEditorAction(...)`. `None` until set.
    pub editor_action_listener: Option<Global<JObject<'static>>>,
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    state: Option<ViewState>,
}

/// The slab + free list (same shape as [`window_registry`](super::window_registry)).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> ViewHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: ViewHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`ViewRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, ViewRegistryError> {
    VIEWS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| ViewRegistryError::Poisoned)
}

/// Allocate a fresh view slot with the given `class_name` and return its packed [`ViewHandle`]
/// (`jlong`, generation ≥ 1, never the reserved `0`).
///
/// Reuses a freed slot when one is available (its generation was already bumped on [`free`]),
/// otherwise grows the slab. Returns [`ViewRegistryError::Poisoned`] only on a poisoned mutex —
/// never panics.
pub fn allocate(class_name: &str) -> Result<ViewHandle, ViewRegistryError> {
    let state = ViewState {
        class_name: class_name.to_owned(),
        text: None,
        children: Vec::new(),
        layout: LayoutParams::default(),
        clickable: false,
        jobject: None,
        background_color: None,
        text_watchers: Vec::new(),
        editor_action_listener: None,
    };
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.state = Some(state);
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| ViewRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        state: Some(state),
    });
    Ok(pack(index, 1))
}

/// Look up the [`ViewState`] for a `handle` and run `f` against it (mutable, so a constructor/setter
/// can record metadata) under the registry lock.
///
/// Bounds-checks the slot index **and** verifies the handle's generation, so a stale/out-of-range/
/// fabricated handle returns `Err` and never dereferences out of bounds or aliases a different view.
/// The reserved `0` handle fails the check (live generations are ≥ 1).
pub fn with_view<R>(
    handle: ViewHandle,
    f: impl FnOnce(&mut ViewState) -> R,
) -> Result<R, ViewRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(ViewRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(ViewRegistryError::StaleHandle);
    }
    let state = slot.state.as_mut().ok_or(ViewRegistryError::StaleHandle)?;
    Ok(f(state))
}

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`ViewRegistryError::StaleHandle`]. Validates the handle the same way
/// [`with_view`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
pub fn free(handle: ViewHandle) -> Result<(), ViewRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(ViewRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.state.is_none() {
        return Err(ViewRegistryError::StaleHandle);
    }
    slot.state = None;
    // Bump (saturating) so the freed handle and any copy become stale and can never alias a reuse.
    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

/// Mark the view a `handle` refers to as clickable (a click listener was recorded by
/// `View.nativeSetOnClickListener`). 2026-06-05: the renderer's hit-test only dispatches a click to a
/// clickable view. Validates the handle exactly like [`with_view`], so a stale/fabricated handle is a
/// typed `Err`, never UB.
pub fn set_clickable(handle: ViewHandle) -> Result<(), ViewRegistryError> {
    with_view(handle, |v| v.clickable = true)
}

/// Record a solid ARGB background color on the view a `handle` refers to (`View.setBackgroundColor`).
/// 2026-06-05: the renderer fills the view's rect with this color for real fidelity. Validates the
/// handle exactly like [`with_view`], so a stale/fabricated handle is a typed `Err`, never UB.
pub fn set_background_color(handle: ViewHandle, argb: i32) -> Result<(), ViewRegistryError> {
    with_view(handle, |v| v.background_color = Some(argb))
}

/// Record the JNI **global** reference to a view's Java `View` object onto its registry slot, so a
/// click resolved to this view (by handle) can later call `View.performClick()` on the real object.
///
/// 2026-06-05: called by the view's native constructor from a global ref it created over `this`. A
/// `Global` is `Send` and owns the underlying JNI global reference (released on `Drop` when the slot
/// is [`free`]d or this `Option` is replaced). Validates the handle like [`with_view`].
pub fn set_jobject(
    handle: ViewHandle,
    jobject: Global<JObject<'static>>,
) -> Result<(), ViewRegistryError> {
    with_view(handle, move |v| v.jobject = Some(jobject))
}

/// Run `f` with a **borrow** of the view's stored Java-object global reference, if one is recorded.
///
/// 2026-06-05: the click-dispatch path. Returns `Ok(None)` when the handle is valid but no global ref
/// was recorded (a non-dispatchable view — e.g. its constructor failed to create the ref); `Ok(Some(r))`
/// with `f`'s result otherwise; `Err` for a stale/fabricated handle or a poisoned lock. `f` receives
/// `&Global<JObject<'static>>` (not ownership), so the slot keeps the ref alive across the dispatch.
/// The registry lock is held for the duration of `f`, so `f` must not re-enter the registry (the
/// dispatch only makes a JNI call, which does not).
pub fn with_jobject<R>(
    handle: ViewHandle,
    f: impl FnOnce(&Global<JObject<'static>>) -> R,
) -> Result<Option<R>, ViewRegistryError> {
    with_view(handle, |v| v.jobject.as_ref().map(f))
}

/// 2026-06-13: RETAIN a `TextWatcher` global ref on the view a `handle` refers to
/// (`EditText.native_addTextChangedListener`). Eclipse's `EditText.addTextChangedListener` keeps no
/// Java field, so the native must hold the watcher or it is collected immediately — held here so a
/// future input-dispatch path can invoke it. Validates the handle exactly like [`with_view`], so a
/// stale/fabricated handle is a typed `Err`, never UB.
pub fn add_text_watcher(
    handle: ViewHandle,
    watcher: Global<JObject<'static>>,
) -> Result<(), ViewRegistryError> {
    with_view(handle, move |v| v.text_watchers.push(watcher))
}

/// 2026-06-13: keep only the `TextWatcher`s for which `keep` returns `true`, dropping (and thereby
/// releasing the JNI global ref of) the rest — the backing for `EditText.native_removeTextChangedListener`.
/// The caller's `keep` runs under the registry lock and compares each retained watcher against the one
/// being removed (a JNI identity check, which does not re-enter the registry — same contract as
/// [`with_jobject`]). Returns the number of watchers dropped. Validates the handle like [`with_view`].
pub fn retain_text_watchers(
    handle: ViewHandle,
    mut keep: impl FnMut(&Global<JObject<'static>>) -> bool,
) -> Result<usize, ViewRegistryError> {
    with_view(handle, move |v| {
        let before = v.text_watchers.len();
        v.text_watchers.retain(|w| keep(w));
        before - v.text_watchers.len()
    })
}

/// 2026-06-13: RETAIN (replacing any prior) the editor-action listener global ref on the view a
/// `handle` refers to (`EditText.native_setOnEditorActionListener`); passing `None` clears it. Replacing
/// drops the old `Global` (releasing its ref). Validates the handle exactly like [`with_view`].
pub fn set_editor_action_listener(
    handle: ViewHandle,
    listener: Option<Global<JObject<'static>>>,
) -> Result<(), ViewRegistryError> {
    with_view(handle, move |v| v.editor_action_listener = listener)
}

/// 2026-06-13: the number of `TextWatcher`s currently retained on the view a `handle` refers to (the
/// observable side effect of [`add_text_watcher`]/[`retain_text_watchers`]). Validates the handle like
/// [`with_view`].
pub fn text_watcher_count(handle: ViewHandle) -> Result<usize, ViewRegistryError> {
    with_view(handle, |v| v.text_watchers.len())
}

/// 2026-06-13: whether an editor-action listener is currently retained on the view a `handle` refers to
/// (the observable side effect of [`set_editor_action_listener`]). Validates the handle like [`with_view`].
pub fn editor_action_listener_is_set(handle: ViewHandle) -> Result<bool, ViewRegistryError> {
    with_view(handle, |v| v.editor_action_listener.is_some())
}

/// Publish `handle` as the window's content-root view (called by `Window.set_widget_as_root`), so
/// the renderer's per-frame [`snapshot_tree`] knows which subtree to draw. Passing `0` clears it.
///
/// Lock-free (a single atomic store); does not validate the handle here — [`snapshot_tree`]
/// validates it against the slab when it reads, so a stale value yields an empty snapshot, not UB.
pub fn set_active_root(handle: ViewHandle) {
    ACTIVE_ROOT.store(handle, Ordering::Release);
}

/// The currently published content-root view handle, or `0` if none has been set yet.
pub fn active_root() -> ViewHandle {
    ACTIVE_ROOT.load(Ordering::Acquire)
}

/// 2026-06-13: find the live view whose recorded [`ViewState::class_name`] equals `name`, returning
/// its [`ViewHandle`], or `None` if no live slot matches (or the registry mutex is poisoned).
///
/// The render Phase 2 surface-lifecycle dispatch uses this to locate the engine's `SurfaceView` peer
/// by its concrete class `com.roblox.client.RBXSurfaceView` (captured into the slab by
/// `view_native_constructor` via [`set_jobject`] + [`ViewState::class_name`]) so it can JNI-dispatch
/// `surfaceCreated`/`surfaceChanged` to it. Scans the slab under the registry lock and returns the
/// first live (occupied) match in slot order; on the live boot there is exactly one such peer.
pub fn find_by_class(name: &str) -> Option<ViewHandle> {
    let reg = lock().ok()?;
    for (index, slot) in reg.slots.iter().enumerate() {
        if let Some(state) = slot.state.as_ref() {
            if state.class_name == name {
                return Some(pack(index as u32, slot.generation));
            }
        }
    }
    None
}

/// A flattened, owned snapshot of one view in the recorded tree — what the renderer reads per frame.
///
/// 2026-06-05: a depth-first, owned copy (no registry handles / locks held by the renderer) so the
/// GPU code never touches the live slab while drawing. `depth` is the nesting level (root = 0);
/// `class_name`/`text`/`layout` mirror [`ViewState`]. `children` are **indices into the same
/// snapshot `Vec`** (not registry handles) so the renderer can run a measure/layout cascade over the
/// owned tree without re-locking the slab. The renderer computes each node's absolute rect from
/// `layout` + `class_name`; `depth` is kept for depth-distinguished fill colors.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderNode {
    /// The view's registry [`ViewHandle`] (the same `jlong` the View native peer holds). 2026-06-05:
    /// carried into the snapshot so the renderer's hit-test can map a hit rect back to the live view
    /// (to dispatch a click via [`with_jobject`]) without re-walking the tree. An opaque index, never
    /// a pointer.
    pub handle: ViewHandle,
    /// The Java class that constructed the view (e.g. `android.widget.TextView`).
    pub class_name: String,
    /// The view's text, if any (`TextView.setText`). `None` for non-text containers.
    pub text: Option<String>,
    /// Nesting depth in the tree: the root view is `0`, its children `1`, and so on.
    pub depth: u32,
    /// The view's requested layout parameters (sizing/gravity/weight/margins/padding).
    pub layout: LayoutParams,
    /// 2026-06-05: mirrors [`ViewState::clickable`] — `true` if a click listener was recorded. The
    /// hit-test only targets clickable views.
    pub clickable: bool,
    /// 2026-06-05: mirrors [`ViewState::background_color`] — the solid ARGB background color
    /// (`View.setBackgroundColor`), or `None` to use the renderer's synthetic depth color.
    pub background_color: Option<i32>,
    /// Indices (into the flat snapshot `Vec`) of this node's children, in their recorded order.
    pub children: Vec<usize>,
}

/// Walk the recorded view tree from the published [`active_root`] into a flat, depth-first
/// [`Vec`] of [`RenderNode`]s for the renderer. Returns an empty `Vec` when no root is set, the
/// root handle is stale/invalid, or the registry mutex is poisoned (the renderer then draws no
/// view quads — never a panic across the frame loop).
///
/// 2026-06-05: depth-first pre-order (parent before children), the order `setContentView`'s
/// inflater wired the tree, so the renderer paints containers before their content. A depth cap
/// (matching `axml`'s element-nesting guard) bounds the walk so a (registry-impossible but
/// defensive) cycle cannot loop forever.
pub fn snapshot_tree() -> Vec<RenderNode> {
    /// Mirrors the `axml` element-nesting cap: real Android layouts nest far shallower; this only
    /// guards against a malformed/cyclic registry, which the generational slab already prevents.
    const MAX_DEPTH: u32 = 256;

    let root = active_root();
    if root == 0 {
        return Vec::new();
    }
    let Ok(reg) = lock() else {
        return Vec::new();
    };
    let mut out: Vec<RenderNode> = Vec::new();
    // Explicit stack of (registry handle, depth, parent's snapshot index | usize::MAX for root). A
    // popped node is appended to `out` (taking the next snapshot index), linked onto its parent's
    // `children`, then its children are pushed in REVERSE so they pop in recorded (left-to-right)
    // order — yielding the documented pre-order (parent before its children). The parent is always
    // appended before any of its children pop, so the recorded parent index is valid when a child
    // links back. Iterative (no recursion / stack-depth risk); generation/bounds-checked so a
    // stale/cyclic handle is skipped, never UB.
    let mut stack: Vec<(ViewHandle, u32, usize)> = vec![(root, 0, usize::MAX)];
    while let Some((handle, depth, parent_idx)) = stack.pop() {
        if depth >= MAX_DEPTH {
            continue;
        }
        let (index, generation) = unpack(handle);
        let Some(slot) = reg.slots.get(index as usize) else {
            continue;
        };
        if slot.generation != generation {
            continue;
        }
        let Some(state) = slot.state.as_ref() else {
            continue;
        };
        let my_idx = out.len();
        out.push(RenderNode {
            handle,
            class_name: state.class_name.clone(),
            text: state.text.clone(),
            depth,
            layout: state.layout,
            clickable: state.clickable,
            background_color: state.background_color,
            children: Vec::new(),
        });
        if let Some(parent) = out.get_mut(parent_idx) {
            parent.children.push(my_idx);
        }
        // Push children reversed so the first child pops next (pre-order, left-to-right).
        for &child in state.children.iter().rev() {
            stack.push((child, depth + 1, my_idx));
        }
    }
    // `reg` is dropped here, releasing the lock before the owned `Vec` is returned to the renderer.
    drop(reg);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-05: these tests run fully in-harness — no VM, no display. They prove the same soundness
    // contract window_registry/xml_registry do, for the View slab: distinct handles, correct-slot
    // mutation, and (the key property) that a freed handle becomes Stale after the slot is reused with
    // a bumped generation, so a stale/fabricated `jlong` from Java is an `Err`, never UB / cross-talk.
    // The registry is process-global; tests are order-independent (own handles, no absolute indices).

    #[test]
    fn allocate_returns_distinct_nonzero_handles() {
        let a = allocate("android.widget.FrameLayout").expect("allocate a");
        let b = allocate("android.widget.TextView").expect("allocate b");
        assert_ne!(a, b, "distinct allocations must yield distinct handles");
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        assert_ne!(b, 0, "a valid handle is never the reserved null 0");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn with_view_mutates_the_right_slot() {
        let a = allocate("A").expect("allocate a");
        let b = allocate("B").expect("allocate b");
        with_view(a, |s| s.text = Some("text-a".to_owned())).expect("with_view a");
        with_view(b, |s| s.children.push(a)).expect("with_view b");
        let ta = with_view(a, |s| s.text.clone()).expect("read a");
        let cb = with_view(b, |s| s.children.clone()).expect("read b");
        assert_eq!(
            ta.as_deref(),
            Some("text-a"),
            "handle a addresses its own slot"
        );
        assert_eq!(cb, vec![a], "handle b addresses its own slot");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        // The key soundness property. Allocate, mutate, free, re-allocate; the freed handle must NOT
        // see the new occupant — it must be rejected as StaleHandle.
        let old = allocate("old").expect("allocate old");
        with_view(old, |s| s.text = Some("old".to_owned())).expect("write old");
        free(old).expect("free old");

        let new = allocate("new").expect("allocate new");
        assert_eq!(
            with_view(old, |s| s.text.clone()),
            Err(ViewRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        // The live handle still works and is unaffected by the stale lookup.
        assert_eq!(
            with_view(new, |s| s.class_name.clone()),
            Ok("new".to_owned()),
            "the live handle must still address the reused slot"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_view(fabricated, |_| ()),
            Err(ViewRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
        let null_lookup = with_view(0, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(ViewRegistryError::StaleHandle) | Err(ViewRegistryError::OutOfRange)
            ),
            "the reserved null handle 0 must be rejected, got {null_lookup:?}"
        );
        assert_eq!(free(fabricated), Err(ViewRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let h = allocate("x").expect("allocate");
        free(h).expect("first free");
        // The same handle is now stale (generation bumped); a second free returns StaleHandle, not a
        // panic or free-list corruption.
        assert_eq!(free(h), Err(ViewRegistryError::StaleHandle));
    }

    #[test]
    fn surface_view_peer_round_trips_and_destructor_tolerates_null() {
        // 2026-06-13: the two exact properties View.native_destructor relies on (framework.rs
        // view_native_destructor). (1) A real RBXSurfaceView peer (what SurfaceView.native_constructor
        // allocates via the class-agnostic view_native_constructor) frees cleanly. (2) The
        // failed-construct finalizer path — where native_constructor threw so View.widget stayed the
        // `long` default 0 and View.finalize calls native_destructor(0) — must return Err (the reserved
        // null 0 is index 0/generation 0; live generations are ≥ 1), NEVER panic, because the
        // destructor runs on the ART FinalizerDaemon thread and must not throw.
        let peer =
            allocate("com.roblox.client.RBXSurfaceView").expect("allocate RBXSurfaceView peer");
        assert_ne!(peer, 0, "a real view peer is never the reserved null 0");
        free(peer).expect("a live RBXSurfaceView peer frees cleanly");
        assert!(
            matches!(
                free(0),
                Err(ViewRegistryError::StaleHandle) | Err(ViewRegistryError::OutOfRange)
            ),
            "native_destructor(0) (failed-construct finalizer path) must be Err, never panic"
        );
    }

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }

    #[test]
    fn find_by_class_locates_the_right_handle_and_is_none_for_absent_class() {
        // 2026-06-13: the render Phase 2 lookup the surface-lifecycle dispatch uses to locate the
        // RBXSurfaceView peer by class. Two distinct live entries: find_by_class must return the
        // handle whose recorded class_name matches, and None for a class no live slot carries.
        //
        // The class strings here are UNIQUE to this test (the `eclipse.test.FindByClass*` namespace
        // is allocated by no other test or production code) so that `find_by_class` — which returns
        // the FIRST live match in slot order — cannot be aliased to a sibling test's live entry under
        // parallel execution. (Using real production class names like RBXSurfaceView/FrameLayout
        // collided with concurrent tests sharing the process-global slab.) The guard's intent is
        // unchanged: prove class discrimination, liveness filtering, and None-for-absent.
        let surface =
            allocate("eclipse.test.FindByClassSurface").expect("allocate first test peer");
        let other = allocate("eclipse.test.FindByClassOther").expect("allocate second test peer");
        assert_eq!(
            find_by_class("eclipse.test.FindByClassSurface"),
            Some(surface),
            "find_by_class returns the handle of the matching live entry"
        );
        assert_eq!(
            find_by_class("eclipse.test.FindByClassOther"),
            Some(other),
            "find_by_class distinguishes the second class"
        );
        assert_eq!(
            find_by_class("eclipse.test.FindByClassNone"),
            None,
            "an absent class yields None, never a wrong/aliased handle"
        );
        // After freeing the match, the class is no longer found (only live slots are scanned).
        free(surface).expect("free first test peer");
        assert_eq!(
            find_by_class("eclipse.test.FindByClassSurface"),
            None,
            "a freed entry is not returned by find_by_class"
        );
        free(other).expect("free second test peer");
    }

    // 2026-06-05: the snapshot walk the renderer reads each frame. These tests exercise it against
    // the live process-global slab (no VM/display), proving pre-order + depth + that a stale/empty
    // root yields an empty snapshot (never UB / never the wrong subtree). `set_active_root(0)` is
    // restored after each so the global cell does not leak into other tests (order-independent).
    #[test]
    fn snapshot_tree_walks_preorder_with_depth() {
        // root[FrameLayout] → child0[TextView "hello"], child1[LinearLayout] → grandchild[TextView "x"]
        let grandchild = allocate("android.widget.TextView").expect("alloc grandchild");
        with_view(grandchild, |s| s.text = Some("x".to_owned())).expect("text grandchild");
        let child0 = allocate("android.widget.TextView").expect("alloc child0");
        with_view(child0, |s| s.text = Some("hello".to_owned())).expect("text child0");
        let child1 = allocate("android.widget.LinearLayout").expect("alloc child1");
        with_view(child1, |s| s.children.push(grandchild)).expect("wire grandchild");
        let root = allocate("android.widget.FrameLayout").expect("alloc root");
        with_view(root, |s| {
            s.children.push(child0);
            s.children.push(child1);
        })
        .expect("wire children");

        set_active_root(root);
        let snap = snapshot_tree();
        set_active_root(0);

        // Pre-order, left-to-right: root, child0, child1, grandchild.
        let names: Vec<_> = snap.iter().map(|n| n.class_name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "android.widget.FrameLayout",
                "android.widget.TextView",
                "android.widget.LinearLayout",
                "android.widget.TextView",
            ]
        );
        let depths: Vec<_> = snap.iter().map(|n| n.depth).collect();
        assert_eq!(depths, vec![0, 1, 1, 2]);
        assert_eq!(snap[1].text.as_deref(), Some("hello"));
        assert_eq!(snap[3].text.as_deref(), Some("x"));

        // The snapshot-local child indices: root → [1, 2], child1 (idx 2) → [3]; leaves have none.
        // These are what the renderer's measure/layout cascade walks (not registry handles).
        assert_eq!(
            snap[0].children,
            vec![1, 2],
            "root links its two children by snapshot index"
        );
        assert_eq!(
            snap[2].children,
            vec![3],
            "the LinearLayout links its grandchild"
        );
        assert!(snap[1].children.is_empty() && snap[3].children.is_empty());

        for h in [grandchild, child0, child1, root] {
            free(h).expect("free");
        }
    }

    #[test]
    fn snapshot_carries_recorded_layout_params() {
        // The recorded LayoutParams (set by native_setLayoutParams/native_setPadding) must flow
        // through the snapshot so the renderer can measure/lay out the view.
        let h = allocate("android.widget.TextView").expect("alloc");
        with_view(h, |s| {
            s.layout.width = MATCH_PARENT;
            s.layout.height = 120;
            s.layout.gravity = 0x11;
            s.layout.padding = [1, 2, 3, 4];
        })
        .expect("set layout");
        set_active_root(h);
        let snap = snapshot_tree();
        set_active_root(0);
        free(h).expect("free");
        assert_eq!(snap[0].layout.width, MATCH_PARENT);
        assert_eq!(snap[0].layout.height, 120);
        assert_eq!(snap[0].layout.gravity, 0x11);
        assert_eq!(snap[0].layout.padding, [1, 2, 3, 4]);
    }

    #[test]
    fn snapshot_tree_empty_when_no_or_stale_root() {
        set_active_root(0);
        assert!(snapshot_tree().is_empty(), "no root → empty snapshot");

        let h = allocate("android.widget.FrameLayout").expect("alloc");
        free(h).expect("free");
        // h is now stale; publishing it must yield an empty snapshot, never a wrong/aliased subtree.
        set_active_root(h);
        assert!(
            snapshot_tree().is_empty(),
            "stale root handle → empty snapshot (no UB)"
        );
        set_active_root(0);
    }

    // 2026-06-05: the click-path additions. `set_clickable` flips the flag (carried into the
    // snapshot the hit-test reads); `with_jobject` returns `None` for a view with no recorded
    // global ref and `Err` for a stale/fabricated handle — both are "nothing to click" outcomes the
    // event loop treats as no-ops, never UB. (Recording/calling a real jobject needs a VM and is
    // validated on the dev-host run, not in-harness.)
    #[test]
    fn set_clickable_marks_view_and_flows_into_snapshot() {
        let h = allocate("android.widget.ImageButton").expect("alloc");
        // Default is not clickable.
        assert_eq!(with_view(h, |v| v.clickable), Ok(false));
        set_clickable(h).expect("set clickable");
        assert_eq!(with_view(h, |v| v.clickable), Ok(true));

        set_active_root(h);
        let snap = snapshot_tree();
        set_active_root(0);
        free(h).expect("free");
        assert!(
            snap[0].clickable,
            "clickable must flow into the snapshot node"
        );
        assert_eq!(snap[0].handle, h, "the snapshot carries the view's handle");
    }

    #[test]
    fn set_clickable_on_stale_or_fabricated_handle_is_err() {
        let h = allocate("x").expect("alloc");
        free(h).expect("free");
        assert_eq!(set_clickable(h), Err(ViewRegistryError::StaleHandle));
        assert_eq!(
            set_clickable(pack(u32::MAX, 1)),
            Err(ViewRegistryError::OutOfRange)
        );
    }

    #[test]
    fn with_jobject_is_none_without_a_recorded_object_and_err_when_stale() {
        let h = allocate("android.widget.ImageButton").expect("alloc");
        // No jobject recorded (no VM in-harness) → Ok(None): a valid but non-dispatchable view.
        assert_eq!(with_jobject(h, |_| 1i32), Ok(None));
        free(h).expect("free");
        // Stale handle → Err, never UB.
        assert_eq!(
            with_jobject(h, |_| 1i32),
            Err(ViewRegistryError::StaleHandle)
        );
    }

    // 2026-06-13: the EditText listener-retention helpers. Constructing a real `Global` needs a VM
    // (validated on the dev-host run), so these pin the handle validation + count/clear bookkeeping that
    // the listener natives depend on: a fresh view holds no watchers and no editor-action listener;
    // `retain`/`set(None)` on an empty view are well-defined no-ops; and every helper rejects a
    // stale/fabricated handle with a typed Err (never UB).
    #[test]
    fn listener_retention_counts_start_empty_and_clear_is_a_noop_on_empty() {
        let h = allocate("android.widget.EditText").expect("alloc");
        assert_eq!(text_watcher_count(h), Ok(0));
        assert_eq!(editor_action_listener_is_set(h), Ok(false));
        // retain over an empty watcher list drops nothing and keeps the count at 0.
        assert_eq!(retain_text_watchers(h, |_| true), Ok(0));
        assert_eq!(text_watcher_count(h), Ok(0));
        // Clearing an unset editor-action listener is a well-defined no-op.
        set_editor_action_listener(h, None).expect("clear editor-action listener");
        assert_eq!(editor_action_listener_is_set(h), Ok(false));
        free(h).expect("free");
    }

    #[test]
    fn listener_retention_helpers_reject_stale_and_fabricated_handles() {
        let h = allocate("android.widget.EditText").expect("alloc");
        free(h).expect("free");
        // Stale handle (freed) → Err on every helper, never UB.
        assert_eq!(text_watcher_count(h), Err(ViewRegistryError::StaleHandle));
        assert_eq!(
            editor_action_listener_is_set(h),
            Err(ViewRegistryError::StaleHandle)
        );
        assert_eq!(
            retain_text_watchers(h, |_| true),
            Err(ViewRegistryError::StaleHandle)
        );
        assert_eq!(
            set_editor_action_listener(h, None),
            Err(ViewRegistryError::StaleHandle)
        );
        // Fabricated out-of-range index → OutOfRange.
        assert_eq!(
            text_watcher_count(pack(u32::MAX, 1)),
            Err(ViewRegistryError::OutOfRange)
        );
    }
}
