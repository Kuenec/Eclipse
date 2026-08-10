#![forbid(unsafe_code)]

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::objects::JObject;
use jni::refs::Global;
use jni::sys::jlong;

static VIEWS: OnceLock<Mutex<Registry>> = OnceLock::new();

static ACTIVE_ROOT: AtomicI64 = AtomicI64::new(0);

static FOCUSED_VIEW: AtomicI64 = AtomicI64::new(0);

pub type ViewHandle = jlong;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewRegistryError {
    OutOfRange,

    StaleHandle,

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

pub const MATCH_PARENT: i32 = -1;

pub const WRAP_CONTENT: i32 = -2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutParams {
    pub width: i32,

    pub height: i32,

    pub gravity: i32,

    pub weight: f32,

    pub margins: [i32; 4],

    pub padding: [i32; 4],
}

impl Default for LayoutParams {
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

#[derive(Debug, Default)]
pub struct ViewState {
    pub class_name: String,

    pub text: Option<String>,

    pub children: Vec<ViewHandle>,

    pub layout: LayoutParams,

    pub clickable: bool,

    pub jobject: Option<Global<JObject<'static>>>,

    pub background_color: Option<i32>,

    pub text_watchers: Vec<Global<JObject<'static>>>,

    pub editor_action_listener: Option<Global<JObject<'static>>>,

    pub frame: Option<[i32; 4]>,
}

struct Slot {
    generation: u32,
    state: Option<ViewState>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> ViewHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: ViewHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, ViewRegistryError> {
    VIEWS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| ViewRegistryError::Poisoned)
}

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
        frame: None,
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

    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

pub fn set_clickable(handle: ViewHandle) -> Result<(), ViewRegistryError> {
    with_view(handle, |v| v.clickable = true)
}

pub fn set_background_color(handle: ViewHandle, argb: i32) -> Result<(), ViewRegistryError> {
    with_view(handle, |v| v.background_color = Some(argb))
}

pub fn set_frame(handle: ViewHandle, frame: [i32; 4]) -> Result<(), ViewRegistryError> {
    with_view(handle, move |v| v.frame = Some(frame))
}

pub fn set_jobject(
    handle: ViewHandle,
    jobject: Global<JObject<'static>>,
) -> Result<(), ViewRegistryError> {
    with_view(handle, move |v| v.jobject = Some(jobject))
}

pub fn with_jobject<R>(
    handle: ViewHandle,
    f: impl FnOnce(&Global<JObject<'static>>) -> R,
) -> Result<Option<R>, ViewRegistryError> {
    with_view(handle, |v| v.jobject.as_ref().map(f))
}

pub fn add_text_watcher(
    handle: ViewHandle,
    watcher: Global<JObject<'static>>,
) -> Result<(), ViewRegistryError> {
    with_view(handle, move |v| v.text_watchers.push(watcher))
}

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

pub fn set_editor_action_listener(
    handle: ViewHandle,
    listener: Option<Global<JObject<'static>>>,
) -> Result<(), ViewRegistryError> {
    with_view(handle, move |v| v.editor_action_listener = listener)
}

pub fn text_watcher_count(handle: ViewHandle) -> Result<usize, ViewRegistryError> {
    with_view(handle, |v| v.text_watchers.len())
}

pub fn editor_action_listener_is_set(handle: ViewHandle) -> Result<bool, ViewRegistryError> {
    with_view(handle, |v| v.editor_action_listener.is_some())
}

pub fn set_active_root(handle: ViewHandle) {
    ACTIVE_ROOT.store(handle, Ordering::Release);
}

pub fn active_root() -> ViewHandle {
    ACTIVE_ROOT.load(Ordering::Acquire)
}

pub fn set_focused_view(handle: ViewHandle) {
    FOCUSED_VIEW.store(handle, Ordering::Release);
}

pub fn focused_view() -> ViewHandle {
    FOCUSED_VIEW.load(Ordering::Acquire)
}

pub fn is_focused(handle: ViewHandle) -> bool {
    handle != 0 && handle == focused_view()
}

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

pub fn absolute_frame(handle: ViewHandle) -> Option<(i32, i32, u32, u32)> {
    const MAX_DEPTH: u32 = 256;
    if handle == 0 {
        return None;
    }
    let root = active_root();
    if root == 0 {
        return None;
    }
    let reg = lock().ok()?;

    let mut stack: Vec<(ViewHandle, i32, i32, u32)> = vec![(root, 0, 0, 0)];
    while let Some((h, ox, oy, depth)) = stack.pop() {
        if depth >= MAX_DEPTH {
            continue;
        }
        let (index, generation) = unpack(h);
        let Some(slot) = reg.slots.get(index as usize) else {
            continue;
        };
        if slot.generation != generation {
            continue;
        }
        let Some(state) = slot.state.as_ref() else {
            continue;
        };
        if h == handle {
            let [l, t, r, b] = state.frame?;
            if r <= l || b <= t {
                return None;
            }
            return Some((ox + l, oy + t, (r - l) as u32, (b - t) as u32));
        }
        let (cx, cy) = match state.frame {
            Some([l, t, _, _]) => (ox + l, oy + t),
            None => (ox, oy),
        };
        for &child in state.children.iter().rev() {
            stack.push((child, cx, cy, depth + 1));
        }
    }
    None
}

pub fn subtree_contains(root: ViewHandle, needle: ViewHandle) -> bool {
    if root == 0 || needle == 0 {
        return false;
    }
    let Ok(reg) = lock() else {
        return false;
    };
    let mut visited: std::collections::HashSet<ViewHandle> = std::collections::HashSet::new();
    let mut stack: Vec<ViewHandle> = vec![root];
    while let Some(h) = stack.pop() {
        if !visited.insert(h) {
            continue;
        }
        let (index, generation) = unpack(h);
        let Some(slot) = reg.slots.get(index as usize) else {
            continue;
        };
        if slot.generation != generation {
            continue;
        }
        let Some(state) = slot.state.as_ref() else {
            continue;
        };

        if h == needle {
            return true;
        }
        for &child in &state.children {
            stack.push(child);
        }
    }
    false
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderNode {
    pub handle: ViewHandle,

    pub class_name: String,

    pub text: Option<String>,

    pub depth: u32,

    pub layout: LayoutParams,

    pub clickable: bool,

    pub background_color: Option<i32>,

    pub children: Vec<usize>,
}

pub fn snapshot_tree() -> Vec<RenderNode> {
    const MAX_DEPTH: u32 = 256;

    let root = active_root();
    if root == 0 {
        return Vec::new();
    }
    let Ok(reg) = lock() else {
        return Vec::new();
    };
    let mut out: Vec<RenderNode> = Vec::new();

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

        for &child in state.children.iter().rev() {
            stack.push((child, depth + 1, my_idx));
        }
    }

    drop(reg);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let old = allocate("old").expect("allocate old");
        with_view(old, |s| s.text = Some("old".to_owned())).expect("write old");
        free(old).expect("free old");

        let new = allocate("new").expect("allocate new");
        assert_eq!(
            with_view(old, |s| s.text.clone()),
            Err(ViewRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );

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

        assert_eq!(free(h), Err(ViewRegistryError::StaleHandle));
    }

    #[test]
    fn frame_records_layout_and_rejects_stale_handles() {
        let v = allocate("android.view.View").expect("allocate");
        assert_eq!(
            with_view(v, |s| s.frame).expect("read fresh"),
            None,
            "a never-laid-out view has no recorded frame"
        );
        set_frame(v, [10, 20, 110, 220]).expect("set_frame");
        assert_eq!(
            with_view(v, |s| s.frame).expect("read back"),
            Some([10, 20, 110, 220]),
            "native_layout's record must read back verbatim [l, t, r, b]"
        );

        set_frame(v, [10, 25, 110, 225]).expect("re-set_frame");
        assert_eq!(
            with_view(v, |s| s.frame).expect("read shifted"),
            Some([10, 25, 110, 225])
        );
        free(v).expect("free");
        assert_eq!(
            set_frame(v, [0, 0, 1, 1]),
            Err(ViewRegistryError::StaleHandle),
            "a freed handle must be rejected, never resurrect the slot"
        );
    }

    #[test]
    fn focus_record_serves_the_last_requester_and_never_the_null_or_reused_handle() {
        let a = allocate("android.widget.EditText").expect("allocate a");
        let b = allocate("android.widget.Button").expect("allocate b");
        assert!(!is_focused(0), "the reserved null handle is never focused");
        set_focused_view(a);
        assert_eq!(focused_view(), a);
        assert!(is_focused(a), "the last requester reads focused");
        assert!(!is_focused(b), "a non-requester reads unfocused");

        set_focused_view(b);
        assert!(is_focused(b));
        assert!(!is_focused(a));

        set_focused_view(a);
        free(a).expect("free a");
        let reused = allocate("android.view.View").expect("reuse slot");
        assert!(
            !is_focused(reused),
            "a reused slot's new handle must not inherit the freed view's focus"
        );
        set_focused_view(0);
        free(b).expect("free b");
        free(reused).expect("free reused");
    }

    #[test]
    fn surface_view_peer_round_trips_and_destructor_tolerates_null() {
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
    fn absolute_frame_sums_ancestor_origins_and_rejects_unreachable_views() {
        let child = allocate("android.webkit.WebView").expect("alloc child");
        set_frame(child, [5, 7, 105, 57]).expect("frame child");
        let root = allocate("android.widget.FrameLayout").expect("alloc root");
        set_frame(root, [10, 20, 800, 600]).expect("frame root");
        with_view(root, |s| s.children.push(child)).expect("wire child");
        let orphan = allocate("android.view.View").expect("alloc orphan");
        set_frame(orphan, [1, 1, 2, 2]).expect("frame orphan");
        let frameless = allocate("android.view.View").expect("alloc frameless");
        with_view(root, |s| s.children.push(frameless)).expect("wire frameless");

        set_active_root(root);

        assert_eq!(absolute_frame(child), Some((15, 27, 100, 50)));

        assert_eq!(absolute_frame(root), Some((10, 20, 790, 580)));

        assert_eq!(absolute_frame(orphan), None);

        assert_eq!(absolute_frame(frameless), None);

        assert_eq!(absolute_frame(0), None);
        set_active_root(0);
        for h in [child, root, orphan, frameless] {
            free(h).expect("free");
        }
    }

    #[test]
    fn subtree_contains_matches_self_direct_and_deep_and_rejects_non_members_and_stale() {
        let deep = allocate("android.webkit.WebView").expect("alloc deep");
        let mid = allocate("android.widget.FrameLayout").expect("alloc mid");
        with_view(mid, |s| s.children.push(deep)).expect("wire deep");
        let root = allocate("android.widget.LinearLayout").expect("alloc root");
        with_view(root, |s| s.children.push(mid)).expect("wire mid");
        let outsider = allocate("android.view.View").expect("alloc outsider");

        assert!(subtree_contains(root, root), "a view contains itself");
        assert!(subtree_contains(root, mid), "direct child");
        assert!(subtree_contains(root, deep), "deep descendant");
        assert!(
            !subtree_contains(root, outsider),
            "non-member is not contained"
        );
        assert!(
            !subtree_contains(root, 0),
            "the reserved null handle is never a member"
        );
        assert!(
            !subtree_contains(0, deep),
            "the reserved null root contains nothing"
        );

        free(deep).expect("free deep");
        assert!(
            !subtree_contains(root, deep),
            "a stale needle handle must not match"
        );

        with_view(mid, |s| s.children.push(root)).expect("introduce cycle");
        assert!(
            subtree_contains(root, mid),
            "a cyclic registry still terminates and finds a live member"
        );
        assert!(
            !subtree_contains(root, outsider),
            "a cyclic registry still terminates for a non-member"
        );

        for h in [mid, root, outsider] {
            free(h).expect("free");
        }
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

        free(surface).expect("free first test peer");
        assert_eq!(
            find_by_class("eclipse.test.FindByClassSurface"),
            None,
            "a freed entry is not returned by find_by_class"
        );
        free(other).expect("free second test peer");
    }

    #[test]
    fn snapshot_tree_walks_preorder_with_depth() {
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

        set_active_root(h);
        assert!(
            snapshot_tree().is_empty(),
            "stale root handle → empty snapshot (no UB)"
        );
        set_active_root(0);
    }

    #[test]
    fn set_clickable_marks_view_and_flows_into_snapshot() {
        let h = allocate("android.widget.ImageButton").expect("alloc");

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

        assert_eq!(with_jobject(h, |_| 1i32), Ok(None));
        free(h).expect("free");

        assert_eq!(
            with_jobject(h, |_| 1i32),
            Err(ViewRegistryError::StaleHandle)
        );
    }

    #[test]
    fn listener_retention_counts_start_empty_and_clear_is_a_noop_on_empty() {
        let h = allocate("android.widget.EditText").expect("alloc");
        assert_eq!(text_watcher_count(h), Ok(0));
        assert_eq!(editor_action_listener_is_set(h), Ok(false));

        assert_eq!(retain_text_watchers(h, |_| true), Ok(0));
        assert_eq!(text_watcher_count(h), Ok(0));

        set_editor_action_listener(h, None).expect("clear editor-action listener");
        assert_eq!(editor_action_listener_is_set(h), Ok(false));
        free(h).expect("free");
    }

    #[test]
    fn listener_retention_helpers_reject_stale_and_fabricated_handles() {
        let h = allocate("android.widget.EditText").expect("alloc");
        free(h).expect("free");

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

        assert_eq!(
            text_watcher_count(pack(u32::MAX, 1)),
            Err(ViewRegistryError::OutOfRange)
        );
    }
}
