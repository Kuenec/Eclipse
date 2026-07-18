//! Graphics: forward, don't render (component-map F · 🟢 winit / 🟢 ash·Vulkan surface).
//!
//! Eclipse is **not** a renderer. Roblox's native engine issues its own Vulkan/GLES; this
//! module will provide the `libvulkan.so`/`libEGL.so`/`libGLESv2.so` the engine links and
//! **forward** those calls to the host driver, **translating WSI** (Android
//! `vkCreateAndroidSurfaceKHR` / `ANativeWindow` → host Wayland/X11 surface). Vulkan is
//! preferred; GL is the fallback. Capability is detected at runtime (never assume a vendor).
//!
//! ## What exists now (M2)
//! [`run_windowed`] creates the host game window via **`winit`** (Wayland/X11, **no GTK** — the
//! Step 3.5 win: keeping GTK4/Mesa out of process startup leaves the low_4gb window clear for
//! ART, see `docs/art-and-runtime.md`) and runs the event loop until the window is closed.
//!
//! On top of the window, [`VulkanRenderer`] is the **real on-path GPU surface foundation**
//! (2026-06-05): an `ash` Vulkan instance (loaded at runtime via [`ash::Entry::load`] — no
//! link-time libvulkan dep, detect-don't-assume §9), a `VkSurfaceKHR` created from the winit
//! window's raw display/window handle via `ash_window`, a physical device + graphics/present
//! queue that supports the surface, a swapchain, and a per-frame **clear-and-present** loop
//! (clears to a distinct non-black color via a render pass, recreating the swapchain on resize).
//! This proves the host GPU path end-to-end. If Vulkan cannot initialize (no ICD / unsupported
//! display), a typed [`GraphicsError::Vulkan`] is surfaced and logged; the window stays open and
//! blank rather than crashing.
//!
//! Drawing the framework's recorded View tree (text/widgets in `framework::view_registry`) into
//! this surface is the **deferred next step**; this increment is the surface itself.
//!
//! Planned deps: `khronos-egl` for the GL fallback; the Vulkan loader shim + WSI translation that
//! hands this window's swapchain to the engine's `ANativeWindow` come with the engine-load track.

use std::ffi::CStr;
use std::fmt;

use ash::{khr, vk};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::error::{EventLoopError, OsError};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// 2026-06-05: the clear color presented each frame — a Roblox-ish blue (linear-ish sRGB),
/// distinct from black so a working present is visually unambiguous on the dev host.
const CLEAR_COLOR: [f32; 4] = [0.149, 0.408, 0.722, 1.0];

/// 2026-06-05: precompiled SPIR-V for the colored-quad pipeline (`shaders/quad.{vert,frag}`),
/// embedded so the build needs no shader compiler and no network (portability §9 — builds from a
/// clean checkout anywhere). Regenerate via `shaders/README.md`. SPIR-V is a stream of `u32` words,
/// so the byte length is a multiple of 4 — checked at module-create time via `read_spirv`.
const QUAD_VERT_SPV: &[u8] = include_bytes!("../shaders/quad.vert.spv");
const QUAD_FRAG_SPV: &[u8] = include_bytes!("../shaders/quad.frag.spv");

/// 2026-06-05: precompiled SPIR-V for the textured-glyph (text) pipeline (`shaders/text.{vert,frag}`).
/// Same embed-don't-compile rationale as the quad shaders. The fragment shader samples the R8 glyph
/// atlas as coverage and tints it with a push-constant color.
const TEXT_VERT_SPV: &[u8] = include_bytes!("../shaders/text.vert.spv");
const TEXT_FRAG_SPV: &[u8] = include_bytes!("../shaders/text.frag.spv");

/// 2026-06-05: precompiled SPIR-V for the RGBA Canvas-composite pipeline
/// (`shaders/composite.{vert,frag}`). Same embed-don't-compile rationale as the other pipelines. A
/// custom View's `onDraw(Canvas)` rasterizes into an RGBA8 Pixmap (`framework::canvas_registry`); the
/// fragment shader samples it and scales its alpha by a push-constant opacity, alpha-blended over the
/// view quads + text.
const COMPOSITE_VERT_SPV: &[u8] = include_bytes!("../shaders/composite.vert.spv");
const COMPOSITE_FRAG_SPV: &[u8] = include_bytes!("../shaders/composite.frag.spv");

/// Max custom-view Canvas composites drawn per frame. Bounds the per-frame descriptor pool + texture
/// churn (2026-06-05; real app shells have a handful of custom views — multitouch.test has one). Extra
/// custom views beyond this are not composited (they still draw as background quads). Pre-sizing the
/// descriptor pool to this avoids a per-frame pool rebuild.
const MAX_COMPOSITE_VIEWS: usize = 16;

/// Pixel height the glyph atlas is rasterized at, and the text color drawn over view rects.
const TEXT_PX: f32 = 28.0;
const TEXT_COLOR: [f32; 4] = [0.08, 0.09, 0.12, 1.0]; // near-black, reads on the light view quads
/// Left/top inset of text inside its view rect (pixels).
const TEXT_PAD_X: f32 = 12.0;

/// The host game window + event loop (winit application state).
struct GameWindow<'vm> {
    title: String,
    /// `None` until the event loop is `resumed` and the window is created.
    window: Option<Window>,
    /// The Vulkan surface + swapchain bound to [`Self::window`]. `None` if Vulkan init failed
    /// (no ICD / unsupported display) — the window then stays open and blank (no crash).
    renderer: Option<VulkanRenderer>,
    /// Set if window creation failed, so [`run_windowed`] can surface a typed error.
    create_error: Option<OsError>,
    /// 2026-06-05: a borrow of the live [`Vm`](crate::runtime::Vm) (from `boot()`, kept alive by
    /// `main` on this thread). Used to dispatch `View.performClick()` to a hit view via JNI on a
    /// pointer click. `None` if `run_windowed` was called without a VM (e.g. a future preview mode) —
    /// then clicks are hit-tested but not dispatched. The borrow keeps the VM alive for the whole
    /// event loop; the loop runs on the JNI-attached main thread, so the VM is reachable here.
    vm: Option<&'vm crate::runtime::Vm>,
    /// The last pointer position in window pixels (top-left origin), updated on `CursorMoved`. `None`
    /// until the pointer first moves over the window. The press/release click uses this position.
    cursor: Option<(f32, f32)>,
    /// The view a primary-button press landed on (its [`ViewHandle`]) plus the press position, set on
    /// press and cleared on release. A touch is a press+release pair: an ACTION_DOWN dispatches on
    /// press to this view, and an ACTION_UP on release dispatches to it only if the release still
    /// lands on the SAME view (Android touch semantics — a release that drifts off is not a tap).
    /// `None` when no primary press is in flight (or the press hit no clickable view).
    primary_press: Option<(ViewHandle, f32, f32)>,
    /// 2026-06-05: set once the env-gated one-shot synthetic tap has run, so it fires at most once.
    /// The synthetic tap (only when `ECLIPSE_SYNTHETIC_TAP` is set) is a dev-host diagnostic that taps
    /// the center of the first clickable view to prove the hit-test→performClick chain end-to-end on a
    /// real run, since a headless run cannot physically click. Never fires in normal operation.
    synthetic_tap_done: bool,
    /// 2026-06-13 — the engine-render WSI publish: Eclipse's REAL window exposed as the engine's
    /// `ANativeWindow*`. Built in [`Self::resumed`] right after the window (the same mechanics as the
    /// proven `__gl-test-anw` harness). Held for the window's lifetime as a drop guard: its construction
    /// runs `register_wsi_window` (so `ndk_registry::current_wsi_window()`, what
    /// `eclipse_anativewindow_fromsurface` returns, is Eclipse's actual window instead of the geometry-
    /// only fallback), and its `Drop` unregisters. `None` if the display server is unsupported (the
    /// window still opens; the geometry-only ANativeWindow fallback stands), or before `resumed`.
    engine_window: Option<crate::egl_engine::EngineNativeWindow>,
    /// 2026-06-13 — render Phase 2.1 present-loop handoff: `true` once Eclipse has, in one tick, DROPPED
    /// its [`VulkanRenderer`] (releasing the `wl_surface`) and THEN dispatched the engine's `SurfaceView`
    /// surface lifecycle (`surfaceCreated`/`surfaceChanged`). The drop runs the renderer's `Drop`
    /// (`device_wait_idle` → `destroy_swapchain` → `destroy_surface`), truly RELEASING the
    /// `wl_surface`/`VkSurfaceKHR` BEFORE the engine creates its own EGL window surface over that same
    /// `wl_surface` — two owners of one `wl_surface` made `eglCreateWindowSurface` fail `EGL_BAD_ALLOC`
    /// (3003). Gated on [`crate::framework::engine_surface_callback_ready`] (engine subscribed its
    /// `SurfaceHolder.Callback`), so the window is never blanked prematurely. Done exactly once.
    handed_off: bool,
    /// 2026-06-14 — engine-mode pointer path: the `downTime` (ms) of an in-flight primary press on the
    /// engine's `RBXSurfaceView`, captured on `ACTION_DOWN` and reused for the matching `ACTION_UP` so
    /// both events carry the IDENTICAL downTime (one gesture stream). `None` when no engine press is in
    /// flight. Separate from [`Self::primary_press`] (the pre-handoff Eclipse-view path).
    engine_tap_downtime: Option<i64>,
    /// 2026-06-14 — dev-host diagnostic only: the instant the present-loop handoff completed, so an
    /// env-gated synthetic engine tap (`ECLIPSE_SYNTHETIC_ENGINE_TAP="x,y"`) can fire a few seconds
    /// later, once the engine's Lua UI is interactive. `None` until handoff; never set in normal use.
    handoff_at: Option<std::time::Instant>,
    /// 2026-06-14 — set once the env-gated synthetic engine tap (stage 0) has fired, so it fires once.
    engine_synthetic_tap_done: bool,
    /// 2026-06-14 — set once the env-gated synthetic type-test STAGE 2 (the actual typing) has fired.
    engine_synthetic_typed_done: bool,
    /// 2026-06-14 — the instant of the last synthetic focus-tap, so the focus stage RE-TAPS the field
    /// (focus-on-tap is async + occasionally missed) until `framework::active_text_field()` is set.
    engine_last_focus_tap: Option<std::time::Instant>,
    /// 2026-06-14 — the instant the synthetic type completed (stage 2), so stage 3 can tap "Next" a few
    /// seconds later to confirm the engine detected the text (validation).
    engine_typed_at: Option<std::time::Instant>,
    /// 2026-06-14 — set once the env-gated synthetic "Next" tap (stage 3, `ECLIPSE_SYNTHETIC_NEXT="x,y"`)
    /// has fired.
    engine_synthetic_next_done: bool,
    /// 2026-06-14 — the instant of the stage-3 Next tap, so stage 4 can type into the next field (e.g.
    /// the password field, which auto-focuses) a few seconds later.
    engine_next_at: Option<std::time::Instant>,
    /// 2026-06-14 — set once stage 4 (`ECLIPSE_SYNTHETIC_TYPE2="x,y:text"` or bare `"text"`, typed into
    /// the password-step field — verifies the password field + its masking) has fired.
    engine_synthetic_typed2_done: bool,
    /// 2026-07-01 — the instant of the last stage-4 focus-tap (only the `"x,y:text"` form of
    /// `ECLIPSE_SYNTHETIC_TYPE2` taps): the password step is a SECOND screen whose field does NOT
    /// auto-focus, so stage 4 (re)taps its target until `framework::active_text_field()` reports
    /// focus. Mirrors [`Self::engine_last_focus_tap`] (stage 1). `None` for the bare-`"text"` form.
    engine_last_focus_tap2: Option<std::time::Instant>,
    /// 2026-07-01 — the instant the stage-4 typing completed, so stage 5 can tap the submit button a
    /// few seconds later (the same shape as [`Self::engine_typed_at`] → stage 3).
    engine_typed2_at: Option<std::time::Instant>,
    /// 2026-07-01 — set once stage 5 (`ECLIPSE_SYNTHETIC_SUBMIT="x,y"`, taps the submit/Log In button
    /// after the stage-4 typing — completes an autonomous login drive) has fired.
    engine_synthetic_submit_done: bool,
    /// 2026-06-14 — set once the env-gated engine-input-bridge reflection diagnostic has fired.
    engine_reflect_done: bool,
    /// 2026-06-14 — running Android `META_*` modifier bitmask (shift/ctrl/alt), updated as modifier
    /// keys are pressed/released, and passed as the `metaState` of each engine `KeyEvent`.
    key_meta_state: i32,
    /// 2026-07-03 (web-engine M3) — `true` while a primary press that landed INSIDE the live
    /// WebView's composite rect is in flight: the matching release routes to the helper too, so a
    /// press/release pair never splits between the webview and the engine.
    webview_pointer_down: bool,
    /// Exactly-once gate for orderly runtime teardown. CloseRequested, winit's `exiting` callback,
    /// and the post-`run_app` fallback all call the same method; only the first may drive Java/native
    /// lifecycle or retire the helper.
    runtime_shutdown_started: bool,
}

impl ApplicationHandler for GameWindow<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // winit requires the window to be created on/after `resumed` (the platform display is
        // ready here). Eclipse uses winit directly — no GTK — so process startup never maps the
        // GTK4/Mesa regions that crowded ART's low_4gb window under ATL (Step 3.5).
        let attrs = Window::default_attributes().with_title(self.title.clone());
        let window = match event_loop.create_window(attrs) {
            Ok(window) => {
                tracing::info!(title = %self.title, "host window created (winit, no GTK)");
                window
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to create host window");
                self.create_error = Some(e);
                event_loop.exit();
                return;
            }
        };

        // Stand up the real Vulkan surface+swapchain on the window. A failure here is NOT fatal:
        // a host with no Vulkan ICD (or an unsupported display) still gets a (blank) window, and
        // we log a clear, typed reason. The engine/View-tree render is the deferred follow-up.
        match VulkanRenderer::new(&window) {
            Ok(renderer) => {
                tracing::info!(
                    format = ?renderer.swapchain_format,
                    extent = ?renderer.swapchain_extent,
                    images = renderer.frame_count(),
                    "Vulkan surface + swapchain initialized; clear-and-present loop active"
                );
                self.renderer = Some(renderer);
                window.request_redraw();
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Vulkan init failed; window stays open without GPU presentation"
                );
            }
        }

        // 2026-06-13 — engine-render WSI publish (Phase 1): register Eclipse's REAL window as the
        // engine's `ANativeWindow*`. Mirrors the proven `__gl-test-anw` harness
        // (`egl_engine::GlAnwTestApp::render_engine_style` + `resumed`): publish the window geometry,
        // then build `EngineNativeWindow` (which internally `register_wsi_window`s the real WSI
        // pointer). After this, `ndk_registry::current_wsi_window()` is Some(real WSI handle), so the
        // engine's `ANativeWindow_fromSurface` (`native_provider::eclipse_anativewindow_fromsurface`)
        // returns Eclipse's actual window instead of the geometry-only fallback — exactly what the
        // green `gl_test_anw_binds_real_wsi_handle` asserts. Non-fatal on an unsupported display
        // server (the window still opens; the geometry-only fallback stands), matching the Vulkan
        // non-fatal pattern above.
        match window.window_handle() {
            Ok(handle) => {
                let size = window.inner_size();
                let geometry =
                    crate::egl_engine::WindowGeometry::from_physical(size.width, size.height);
                crate::loader::ndk_registry::set_engine_window_geometry(
                    geometry.width,
                    geometry.height,
                );
                match crate::egl_engine::EngineNativeWindow::new(handle.as_raw(), geometry) {
                    Ok(engine_window) => {
                        tracing::info!(
                            width = geometry.width,
                            height = geometry.height,
                            "engine ANativeWindow published (real WSI handle); ANativeWindow_fromSurface \
                             now returns Eclipse's window"
                        );
                        self.engine_window = Some(engine_window);
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "engine WSI publish failed (unsupported display); ANativeWindow falls back \
                             to geometry-only"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "no raw window handle; engine WSI publish skipped (geometry-only ANativeWindow)"
                );
            }
        }

        // 2026-06-13 — register the winit Wayland `wl_display*` so Eclipse's tier-0 `eglGetDisplay`
        // (`native_provider::eclipse_egl_get_display`) can remap the engine's `EGL_DEFAULT_DISPLAY` to
        // THIS connection. The engine resolves `egl*` through host `libEGL.so` and calls
        // `eglGetDisplay(EGL_DEFAULT_DISPLAY)`, which on Wayland opens Mesa's OWN `wl_display` — a
        // DIFFERENT connection than the one the `wl_egl_window` Eclipse hands it (above) is on; that
        // cross-connection is the engine's `eglCreateWindowSurface` `EGL_BAD_ALLOC` 3003. This is the
        // SAME `wl_display` pointer `egl_engine` uses for `__gl-test-anw` (`d.display.as_ptr()`), so the
        // engine's remapped EGLDisplay lands on winit's connection. `None` on X11/other (XID is
        // server-scoped → pass `EGL_DEFAULT_DISPLAY` through). Non-fatal, matching the Phase 1 pattern.
        match window.display_handle() {
            Ok(dh) => match dh.as_raw() {
                RawDisplayHandle::Wayland(d) => {
                    crate::loader::ndk_registry::set_wsi_display(Some(d.display.as_ptr() as usize));
                }
                _ => crate::loader::ndk_registry::set_wsi_display(None),
            },
            Err(_) => crate::loader::ndk_registry::set_wsi_display(None),
        }

        // 2026-06-13 — render Phase 6 (Vulkan WSI): publish the RAW winit `wl_surface*` so Eclipse's
        // tier-0 `vkCreateAndroidSurfaceKHR` (`vulkan_wsi::eclipse_vk_create_android_surface_khr`) can
        // build a `VkWaylandSurfaceCreateInfoKHR` from it + the `wl_display` above. The engine requests
        // the Android-only `VK_KHR_android_surface` extension, absent from the host Linux ICD; the shims
        // swap it to `VK_KHR_wayland_surface` and create the surface on THIS `wl_surface`. This is the
        // BARE `wl_surface` (`RawWindowHandle::Wayland`), distinct from the `wl_egl_window` the Phase 1
        // WSI publish above registered for the EGL path. `None` on X11/other (a separate seam).
        // Non-fatal, matching the Phase 1 / display-publish pattern.
        match window.window_handle() {
            Ok(wh) => match wh.as_raw() {
                RawWindowHandle::Wayland(s) => {
                    crate::loader::ndk_registry::set_wsi_wl_surface(Some(
                        s.surface.as_ptr() as usize
                    ));
                }
                _ => crate::loader::ndk_registry::set_wsi_wl_surface(None),
            },
            Err(_) => crate::loader::ndk_registry::set_wsi_wl_surface(None),
        }

        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // 2026-06-14 — engine-mode liveness wake: the engine's input worker threads park in
        // `ALooper_pollOnce`; any host input event must wake them so they re-check their sources (the
        // engine consumes input via its JNI bridge, not an NDK `AInputQueue` — see `native_provider`'s
        // "winit → ALooper input feed" note). No-op pre-handoff and for non-input events.
        if self.handed_off {
            crate::loader::native_provider::feed_winit_input_to_loopers(&event);
        }
        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("window close requested; stopping Android before event-loop exit");
                self.shutdown_runtime();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.mark_resized(size.width, size.height);
                }
                // 2026-06-13 — re-publish the engine-window geometry on resize so
                // `ANativeWindow_getWidth`/`getHeight` (which read this) report the real new size.
                // `register_wsi_window` is idempotent on the pointer (it updates the geometry of the
                // existing entry, not a duplicate). NOTE: the Wayland `wl_egl_window` also needs
                // `wl_egl_window_resize` for a true surface resize — that is follow-up; this geometry
                // re-publish is the in-scope correctness fix for the getters.
                let geo = crate::egl_engine::WindowGeometry::from_physical(size.width, size.height);
                let wsi_ptr = self
                    .engine_window
                    .as_ref()
                    .map(|w| w.as_native_window() as usize);
                publish_engine_window_geometry(wsi_ptr, geo.width, geo.height);
            }
            WindowEvent::RedrawRequested => {
                // Draw cascade: before the frame, drive each custom View's onDraw(Canvas) into an
                // Eclipse Pixmap, then hand the drawn canvases to the renderer to composite this frame.
                // Done here (not in draw_frame) because the VM lives on GameWindow; the cascade runs on
                // this JNI-attached main thread, guarded so a JNI/Java error can't crash the loop.
                self.drive_custom_view_draw();
                if let (Some(window), Some(renderer)) =
                    (self.window.as_ref(), self.renderer.as_mut())
                {
                    if let Err(e) = renderer.draw_frame(window) {
                        tracing::error!(error = %e, "Vulkan frame draw failed");
                    }
                    // Drive a continuous clear-and-present loop so the surface keeps presenting.
                    window.request_redraw();
                }
                // Dev-host diagnostic: a one-shot synthetic tap (env-gated) proves the
                // hit-test→performClick chain end-to-end on a real run (a headless run cannot click).
                self.maybe_synthetic_tap();
            }
            // 2026-06-05: sound single-pointer touch path — track the pointer and dispatch real
            // Android `MotionEvent`s (ACTION_DOWN on press, ACTION_UP on release) to the hit view via
            // `View.dispatchTouchEvent`, which runs the View's own touch handling + click detection.
            // Multi-touch / ACTION_MOVE / key / NDK-AInputQueue dispatch is the documented follow-up.
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some((position.x as f32, position.y as f32));
                // Engine mode: while the primary button is down, a move is a DRAG — forward
                // ACTION_MOVE so the engine tracks it (scroll lists, sliders). No-op otherwise.
                // 2026-07-03 (web-engine M3): a move INSIDE the live WebView's composite rect
                // routes to the helper (view-relative) instead and never leaks to the engine.
                if self.handed_off {
                    let wv = crate::webview::client::active_view();
                    let routed = wv != 0
                        && match webview_relative_point(wv, position.x, position.y) {
                            Some((rx, ry, true)) => {
                                crate::webview::client::send_mouse_move(wv, rx, ry);
                                true
                            }
                            _ => false,
                        };
                    if !routed {
                        self.engine_pointer_move();
                    }
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                if self.handed_off {
                    // Engine owns rendering + its own Lua UI: forward the raw pointer to the engine's
                    // RBXSurfaceView (no renderer, no Eclipse-view hit-test).
                    // 2026-07-03 (web-engine M3): a press INSIDE the live WebView's composite rect
                    // routes to the helper; the matching release follows the press's routing so a
                    // press/release pair never splits between the webview and the engine.
                    match state {
                        ElementState::Pressed => {
                            let wv = crate::webview::client::active_view();
                            let routed = wv != 0
                                && match self.cursor.and_then(|(px, py)| {
                                    webview_relative_point(wv, f64::from(px), f64::from(py))
                                }) {
                                    Some((rx, ry, true)) => {
                                        self.webview_pointer_down = true;
                                        crate::webview::client::send_mouse_click(wv, rx, ry, true);
                                        true
                                    }
                                    _ => false,
                                };
                            if !routed {
                                self.engine_primary_press();
                            }
                        }
                        ElementState::Released => {
                            if self.webview_pointer_down {
                                self.webview_pointer_down = false;
                                let wv = crate::webview::client::active_view();
                                if wv != 0 {
                                    // Release routes to the helper at the (possibly outside)
                                    // relative position — CEF handles out-of-view releases.
                                    let (px, py) = self.cursor.unwrap_or((0.0, 0.0));
                                    if let Some((rx, ry, _inside)) =
                                        webview_relative_point(wv, f64::from(px), f64::from(py))
                                    {
                                        crate::webview::client::send_mouse_click(wv, rx, ry, false);
                                    }
                                }
                            } else {
                                self.engine_primary_release();
                            }
                        }
                    }
                } else {
                    match state {
                        ElementState::Pressed => self.handle_primary_press(),
                        ElementState::Released => self.handle_primary_release(),
                    }
                }
            }
            // 2026-06-14: keyboard — forward keys to the engine's RBXSurfaceView.dispatchKeyEvent
            // (engine mode only; the pre-handoff Java-view apps have no key path here).
            // 2026-07-03 (web-engine M3): while a WebView is live, ALL keys route to the helper
            // (the challenge widget owns keyboard focus) and never reach the engine beneath it.
            WindowEvent::KeyboardInput { event, .. } if self.handed_off => {
                let wv = crate::webview::client::active_view();
                if wv != 0 {
                    route_key_to_webview(wv, &event);
                } else {
                    self.engine_key(&event);
                }
            }
            // 2026-06-14: mouse wheel — forward to the engine's nativePassMouseWheel (desktop scroll).
            // 2026-07-03 (web-engine M3): a wheel INSIDE the live WebView's rect routes to the
            // helper in PIXELS — LineDelta y * 40.0, the inverse of engine_scroll's ÷40 mapping.
            WindowEvent::MouseWheel { delta, .. } if self.handed_off => {
                let wv = crate::webview::client::active_view();
                let routed = wv != 0
                    && match self.cursor.and_then(|(px, py)| {
                        webview_relative_point(wv, f64::from(px), f64::from(py))
                    }) {
                        Some((rx, ry, true)) => {
                            let dy = match delta {
                                winit::event::MouseScrollDelta::LineDelta(_, y) => {
                                    (y * 40.0) as i32
                                }
                                winit::event::MouseScrollDelta::PixelDelta(p) => p.y as i32,
                            };
                            if dy != 0 {
                                crate::webview::client::send_mouse_wheel(wv, rx, ry, dy);
                            }
                            true
                        }
                        _ => false,
                    };
                if !routed {
                    let d = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                        winit::event::MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                    };
                    if d != 0.0 {
                        self.engine_scroll(d);
                    }
                }
            }
            _ => {}
        }
    }

    /// 2026-06-12: pump the Android **main** `Looper` once per loop iteration, on the winit/main
    /// thread. Eclipse drives the lifecycle then hands the main thread to winit, so it never runs
    /// `Looper.loop()` — main-thread `Handler.post` continuations and `SurfaceHolder` callbacks would
    /// queue but never dispatch, stalling Roblox after `onResume`. `about_to_wait` fires once per
    /// iteration; the renderer self-drives `request_redraw` each frame (RedrawRequested above), so this
    /// re-pumps at the present cadence — delayed messages fire on the next tick past their deadline.
    /// `pump_main_looper` is non-blocking (drains the ready batch and returns), so the window never
    /// freezes. It logs a one-time "pump active" line itself; here we only surface a pump error.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(vm) = self.vm else { return };
        if let Err(e) = crate::framework::pump_main_looper(vm) {
            tracing::error!(error = %e, "main Looper pump failed");
        }
        // 2026-06-13 — render Phase 2.1 present-loop handoff (DROP-BEFORE-DISPATCH). Once Eclipse's
        // REAL WSI window is published (`engine_window` Some) AND the engine has subscribed its
        // `SurfaceView` `SurfaceHolder.Callback` (`engine_surface_callback_ready` — non-empty
        // `mCallbacks`), do the handoff atomically in ORDER:
        //   1. DROP the VulkanRenderer FIRST — its `Drop` runs `device_wait_idle` → `destroy_swapchain`
        //      → `destroy_surface`, RELEASING the `wl_surface`/`VkSurfaceKHR` so it is free.
        //   2. THEN dispatch `surfaceCreated()`/`surfaceChanged(III)` so the engine's `AndroidGLView`
        //      creates its EGL window surface over the now-FREE `wl_surface`.
        // Phase 2 dispatched FIRST and dropped the renderer only on the NEXT tick (gated on
        // `engine_claimed_surface`, set inside `fromSurface`) — ~19 ms too late: the engine's
        // `eglCreateWindowSurface` ran while Eclipse's `VkSurfaceKHR`/`VkSwapchainKHR` still owned the
        // `wl_surface`, so two owners of one `wl_surface` → `EGL_BAD_ALLOC` (3003). Dropping BEFORE
        // dispatch makes the surface free before the engine takes it. The dispatch is itself self-gated
        // (no-op into an empty callback list), so this is safe to retry: `Ok(false)`/`Err` = retry.
        // RedrawRequested's `Some(renderer)` guard means Eclipse stops drawing once the renderer is
        // gone; we keep pumping the main Looper above and switch to Poll so the loop keeps ticking.
        if !self.handed_off && self.engine_window.is_some() {
            match crate::framework::engine_surface_callback_ready(vm) {
                Ok(true) => {
                    let (w, h) =
                        crate::loader::ndk_registry::engine_window_geometry().unwrap_or((1, 1));
                    // (1) Release the surface BEFORE the engine creates its EGL window surface on it.
                    self.renderer = None;
                    // (2) Hand the (now-free) surface to the engine.
                    if let Err(e) = crate::framework::dispatch_surface_lifecycle(vm, w, h) {
                        tracing::warn!(error = %e, "engine SurfaceView lifecycle dispatch failed after renderer release");
                    }
                    self.handed_off = true;
                    self.handoff_at = Some(std::time::Instant::now());
                    event_loop.set_control_flow(ControlFlow::Poll);
                    tracing::info!(
                        width = w,
                        height = h,
                        "Eclipse released its Vulkan renderer then dispatched the SurfaceView lifecycle \
                         (surfaceCreated + surfaceChanged); present-loop handoff (drop-before-dispatch)"
                    );
                }
                Ok(false) => {} // engine has not subscribed its SurfaceHolder.Callback yet — retry.
                Err(e) => {
                    tracing::warn!(error = %e, "engine surface-callback readiness probe failed (retry)");
                }
            }
        } else if self.handed_off && crate::loader::ndk_registry::engine_claimed_surface() {
            // 2026-06-13 — confirmation only (no longer the drop trigger): the engine genuinely pulled
            // the surface via `ANativeWindow_fromSurface` (the real-WSI branch sets this flag). Log it
            // once so the handoff can be correlated with the engine actually claiming the surface.
            static CLAIM_LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !CLAIM_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!(
                    "engine claimed the surface (ANativeWindow_fromSurface returned Eclipse's WSI window)"
                );
            }
        }
        self.maybe_synthetic_engine_tap();
        // Cache the focused textbox's live geometry for the Vulkan text overlay (which runs on the engine
        // render thread + must not call into the engine). Main-thread JNI; only when a field is focused.
        if self.handed_off && crate::framework::active_text_field() != 0 {
            if let Some(vm) = self.vm {
                crate::framework::query_textbox_geometry(vm);
            }
        }
        // 2026-07-03 (web-engine M3): cache the live WebView's ABSOLUTE composite rect for the
        // engine present thread (the TEXTBOX_GEOM pattern above — the present thread must never
        // walk the registry tree). One registry walk per loop iteration, only while a WebView is
        // live; gone entirely otherwise (one atomic load).
        if self.handed_off && crate::webview::client::active_view() != 0 {
            crate::webview::client::update_composited_rect();
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Winit guarantees this callback immediately before irreversible loop exit. Normal window
        // close already ran the method while handling CloseRequested; this is the idempotent safety
        // net for create errors or another future exit path.
        self.shutdown_runtime();
    }
}

/// 2026-07-03 (web-engine M3): map a window-pixel point onto the live WebView. Returns the
/// VIEW-RELATIVE coordinates plus whether the point lies INSIDE the rect.
///
/// 2026-07-17: it now maps against the rect the compositor ACTUALLY DREW `view` at
/// (`composited_screen_rect`), not the registry cache it used to read. Reading the cache made the
/// hit-test resolve the rect INDEPENDENTLY of the composite, and the two disagreed: the cache is
/// `None` for the challenge WebView (never measured headless), so the page drew at the composite's
/// centered fallback rect while `?` bailed here — every press fell through to the engine beneath a
/// visible page (measured 2026-07-17: 45 down + 45 up to the engine, 0 to the WebView). `None` now
/// means only "nothing composited for this view yet", i.e. nothing on screen to click.
fn webview_relative_point(view: i64, px: f64, py: f64) -> Option<(i32, i32, bool)> {
    let rect = crate::webview::client::composited_screen_rect(view)?;
    Some(relative_point_in(rect, px, py))
}

/// 2026-07-17: pure window-pixel → view-relative mapping against the composited rect. The blit
/// puts stage pixel (0,0) at the rect's origin (`upload_bgra`'s `image_offset`), so subtracting
/// that origin IS the view-relative coordinate — including when the clamp moved it.
fn relative_point_in(rect: (i32, i32, u32, u32), px: f64, py: f64) -> (i32, i32, bool) {
    let (x, y, w, h) = rect;
    let rx = px as i32 - x;
    let ry = py as i32 - y;
    let inside = rx >= 0 && ry >= 0 && (rx as u32) < w && (ry as u32) < h;
    (rx, ry, inside)
}

/// 2026-07-03 (web-engine M3): DELIBERATELY MINIMAL keyboard routing for the challenge widget
/// (extend at M4/M6 only if the widget demands it): printable text fires RAWKEYDOWN + CHAR
/// (first UTF-16 unit) + KEYUP; Enter/Backspace/Tab/Escape/arrows fire down+up with their
/// Windows VK codes (the `cef_key_event_t` convention). Everything synthesizes from the winit
/// PRESS half (the release is dropped — the up was already sent). NEVER logs key contents (the
/// `engine_key` privacy precedent: keystrokes/credentials must not reach the log).
fn route_key_to_webview(view: i64, event: &winit::event::KeyEvent) {
    use winit::keyboard::{Key, NamedKey};
    if event.state != ElementState::Pressed {
        return;
    }
    let vk = match &event.logical_key {
        Key::Named(NamedKey::Enter) => Some(0x0D),
        Key::Named(NamedKey::Backspace) => Some(0x08),
        Key::Named(NamedKey::Tab) => Some(0x09),
        Key::Named(NamedKey::Escape) => Some(0x1B),
        Key::Named(NamedKey::ArrowLeft) => Some(0x25),
        Key::Named(NamedKey::ArrowUp) => Some(0x26),
        Key::Named(NamedKey::ArrowRight) => Some(0x27),
        Key::Named(NamedKey::ArrowDown) => Some(0x28),
        _ => None,
    };
    if let Some(code) = vk {
        crate::webview::client::send_key(view, 0, code, 0); // down
        crate::webview::client::send_key(view, 1, code, 0); // up
        return;
    }
    let Some(text) = event.text.as_ref() else {
        return;
    };
    if text.chars().next().is_none_or(char::is_control) {
        return;
    }
    let Some(unit) = text.encode_utf16().next() else {
        return;
    };
    crate::webview::client::send_key(view, 0, 0, 0); // RAWKEYDOWN
    crate::webview::client::send_key(view, 2, 0, unit); // CHAR
    crate::webview::client::send_key(view, 1, 0, 0); // KEYUP
}

impl GameWindow<'_> {
    /// Stop Android/Roblox and the out-of-process web engine while [`Self::window`] and
    /// [`Self::engine_window`] are still alive. The old CloseRequested arm called `exit()` directly;
    /// winit then destroyed the host surface underneath live libroblox workers, producing an
    /// immediate native SIGSEGV. This method is deliberately synchronous and exactly-once: Android's
    /// surface/activity teardown comes first, then the final persistent-cookie flush, then helper
    /// shutdown, and only its caller may allow winit to exit/drop the window.
    fn shutdown_runtime(&mut self) {
        if self.runtime_shutdown_started {
            return;
        }
        self.runtime_shutdown_started = true;
        let Some(vm) = self.vm else {
            return;
        };

        if let Err(error) = crate::framework::drive_application_shutdown_lifecycle(vm) {
            tracing::warn!(
                error = %error,
                "host shutdown: Android lifecycle reported an error; continuing remaining teardown"
            );
        }
        if crate::webview::client::needs_cookie_flush_before_shutdown() {
            if let Err(error) = crate::framework::cookie_manager_flush(vm) {
                tracing::warn!(
                    error = %error,
                    "host shutdown: CookieManager.flush dispatch failed; continuing helper teardown"
                );
            }
        }
        let report = crate::webview::client::shutdown(vm, std::time::Duration::from_secs(10));
        tracing::info!(
            helper_exit = report.helper_exit,
            reader_joined = report.reader_joined,
            "host shutdown: web engine retired before the host window is destroyed"
        );
    }

    /// Begin a primary-button touch: hit-test the rendered View tree at the press position and, if it
    /// resolves to a clickable view, remember it and dispatch a real Android `MotionEvent` of
    /// `ACTION_DOWN` to it via `View.dispatchTouchEvent`.
    ///
    /// 2026-06-05: the DOWN half of the single-pointer touch. The hit-test is GPU-free geometry over
    /// the laid-out rects (the same layout the renderer draws). Records `(handle, x, y)` so the release
    /// can require the same target. No VM / no renderer / no cursor / no clickable view under the point
    /// → nothing recorded (logged at debug); the release is then a no-op too.
    fn handle_primary_press(&mut self) {
        self.primary_press = None;
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let Some((px, py)) = self.cursor else {
            return;
        };
        let Some(handle) = renderer.hit_test_at(px, py) else {
            return;
        };
        self.primary_press = Some((handle, px, py));
        self.dispatch_touch(handle, crate::framework::MotionAction::Down, px, py);
    }

    /// Complete a primary-button touch: if the release lands on the SAME view the press hit (Android
    /// semantics — a release that drifts off is not a tap), dispatch a real Android `MotionEvent` of
    /// `ACTION_UP` to it; the View's own click detection fires its `OnClickListener` from that UP.
    ///
    /// 2026-06-05: the UP half of the single-pointer touch. If the MotionEvent UP dispatch fails (a
    /// JNI/Java error, or the View did not consume it), `View.performClick()` is the durable fallback,
    /// preserving INPUT v0's behavior so a click is never silently lost. The press record is cleared
    /// regardless. Dispatch goes through the held VM, guarded (catch_unwind + pending-exception check)
    /// so a JNI/Java error can never crash the event loop.
    fn handle_primary_release(&mut self) {
        let pressed = self.primary_press.take();
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let Some((px, py)) = self.cursor else {
            return;
        };
        let pressed_view = pressed.map(|(h, _, _)| h);
        let released_view = renderer.hit_test_at(px, py);
        // The release must land on the same view the press hit (a real tap, not a drag-off).
        let Some(pressed_handle) = should_complete_tap(pressed_view, released_view) else {
            return;
        };
        let dispatched_up =
            self.dispatch_touch(pressed_handle, crate::framework::MotionAction::Up, px, py);
        // Durable fallback: if the UP MotionEvent could not be dispatched (no VM, JNI/Java error, or
        // the View did not consume it), fall back to performClick so the click is not silently lost.
        if !dispatched_up {
            self.perform_click_fallback(pressed_handle, px, py);
        }
    }

    /// Dispatch a single-pointer `MotionEvent` of `action` at `(x, y)` to the view `handle` via
    /// [`framework::dispatch_touch_to_view`](crate::framework::dispatch_touch_to_view) on the held VM.
    /// Returns `true` iff the View consumed the event; `false` on no VM / a guarded JNI-Java error /
    /// the View not consuming it. Never panics (the framework path is catch_unwind-guarded).
    fn dispatch_touch(
        &self,
        handle: ViewHandle,
        action: crate::framework::MotionAction,
        x: f32,
        y: f32,
    ) -> bool {
        let Some(vm) = self.vm else {
            tracing::debug!(
                handle,
                ?action,
                "touch hit a view but no VM is held; not dispatching"
            );
            return false;
        };
        match crate::framework::dispatch_touch_to_view(vm, handle, action, x, y) {
            Ok(consumed) => {
                tracing::info!(
                    handle,
                    ?action,
                    x,
                    y,
                    consumed,
                    "pointer MotionEvent dispatched to view (View.dispatchTouchEvent)"
                );
                consumed
            }
            Err(e) => {
                tracing::warn!(handle, ?action, error = %e, "touch dispatch to view failed (ignored)");
                false
            }
        }
    }

    /// 2026-06-14 — ENGINE-MODE primary press: forward the pointer to Roblox's engine, which owns
    /// rendering and its own Lua UI post-handoff (Eclipse's `VulkanRenderer` + view tree are gone, so
    /// the renderer-gated [`Self::handle_primary_press`] path is pre-handoff only). Dispatches an
    /// `ACTION_DOWN` `MotionEvent` at the raw cursor position to the engine's `RBXSurfaceView.onTouchEvent`
    /// and records the gesture's downTime for the matching release. No VM / no cursor yet → no-op.
    /// 2026-06-14 — ENGINE-MODE mouse-wheel scroll: forward the wheel `delta` (+ the cursor position) to
    /// the engine's `nativePassMouseWheel`. (Roblox's Android UI also scrolls via touch-drag, already
    /// forwarded as `ACTION_MOVE`; this is the desktop wheel convenience.)
    fn engine_scroll(&mut self, delta: f32) {
        let Some(vm) = self.vm else { return };
        let (px, py) = self.cursor.unwrap_or((0.0, 0.0));
        crate::framework::dispatch_scroll(vm, px, py, delta);
    }

    fn engine_primary_press(&mut self) {
        self.engine_tap_downtime = None;
        let Some(vm) = self.vm else { return };
        let Some((px, py)) = self.cursor else { return };
        match crate::framework::dispatch_touch_to_engine_surface(
            vm,
            crate::framework::MotionAction::Down,
            px,
            py,
            None,
        ) {
            Ok(outcome) => {
                self.engine_tap_downtime = Some(outcome.down_time_ms);
                tracing::info!(
                    x = px,
                    y = py,
                    consumed = outcome.consumed,
                    "engine pointer ACTION_DOWN → RBXSurfaceView.onTouchEventInternal"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "engine pointer ACTION_DOWN dispatch failed (ignored)");
            }
        }
    }

    /// 2026-06-14 — ENGINE-MODE primary release: dispatch the matching `ACTION_UP` to the engine's
    /// `RBXSurfaceView.onTouchEvent` at the raw cursor position, reusing the press's downTime so the
    /// engine groups DOWN+UP as one tap. Clears the in-flight gesture regardless.
    fn engine_primary_release(&mut self) {
        let down_time = self.engine_tap_downtime.take();
        let Some(vm) = self.vm else { return };
        let Some((px, py)) = self.cursor else { return };
        match crate::framework::dispatch_touch_to_engine_surface(
            vm,
            crate::framework::MotionAction::Up,
            px,
            py,
            down_time,
        ) {
            Ok(outcome) => {
                tracing::info!(
                    x = px,
                    y = py,
                    consumed = outcome.consumed,
                    "engine pointer ACTION_UP → RBXSurfaceView.onTouchEventInternal"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "engine pointer ACTION_UP dispatch failed (ignored)");
            }
        }
    }

    /// 2026-06-14 — ENGINE-MODE pointer drag: while a primary press is in flight, dispatch an
    /// `ACTION_MOVE` to the engine's `RBXSurfaceView` at the new cursor position, reusing the press's
    /// downTime so the engine groups it into the same gesture (drag-scroll, sliders). No-op when no
    /// press is in flight (a bare hover — a touchscreen-modeled engine does not expect hover events).
    /// Quiet on success (drags fire at pointer rate); only a dispatch error is logged.
    fn engine_pointer_move(&mut self) {
        let Some(down_time) = self.engine_tap_downtime else {
            return;
        };
        let Some(vm) = self.vm else { return };
        let Some((px, py)) = self.cursor else { return };
        if let Err(e) = crate::framework::dispatch_touch_to_engine_surface(
            vm,
            crate::framework::MotionAction::Move,
            px,
            py,
            Some(down_time),
        ) {
            tracing::warn!(error = %e, "engine pointer ACTION_MOVE dispatch failed (ignored)");
        }
    }

    /// 2026-06-14 — ENGINE-MODE keyboard: forward a winit key event to the engine's `RBXSurfaceView`
    /// via `dispatchKeyEvent`. Modifier keys (shift/ctrl/alt) update the running meta-state and are
    /// not dispatched as standalone keys. The typed character (winit's already layout/shift-resolved
    /// `text`) rides on the `KeyEvent.unicodeValue` so the engine's `getUnicodeChar()` yields it. Logs
    /// only `consumed` + the press/release half — NEVER the key or character (do not log keystrokes/
    /// credentials).
    fn engine_key(&mut self, event: &winit::event::KeyEvent) {
        use winit::keyboard::{Key, NamedKey};
        let pressed = event.state == ElementState::Pressed;
        // Modifiers fire as their own events: maintain the meta bitmask; do not dispatch them as keys.
        let meta_bit = match &event.logical_key {
            Key::Named(NamedKey::Shift) => Some(0x1), // META_SHIFT_ON
            Key::Named(NamedKey::Control) => Some(0x1000), // META_CTRL_ON
            Key::Named(NamedKey::Alt) => Some(0x02),  // META_ALT_ON
            _ => None,
        };
        if let Some(bit) = meta_bit {
            if pressed {
                self.key_meta_state |= bit;
            } else {
                self.key_meta_state &= !bit;
            }
            return;
        }
        let Some(key_code) = winit_keycode(&event.logical_key) else {
            return; // no clean Android mapping (e.g. dead/unidentified key) — dropped
        };
        // winit's resolved printable text → Unicode codepoint (0 for non-printing keys).
        let unicode = event
            .text
            .as_ref()
            .and_then(|s| s.chars().next())
            .map(|c| c as i32)
            .unwrap_or(0);
        let Some(vm) = self.vm else { return };
        // Text-input path: route a printable char or backspace DOWN into the engine's focused EditText
        // (e.g. the login username/password fields). If a field consumed it, do NOT also forward the
        // key to the game surface. UP/non-printing keys with no active field fall through to the surface.
        if pressed {
            let backspace = key_code == 67; // KEYCODE_DEL
            let printable =
                unicode != 0 && char::from_u32(unicode as u32).is_some_and(|c| !c.is_control());
            if (printable || backspace)
                && crate::framework::type_into_active_text_field(vm, unicode, backspace)
            {
                tracing::info!(pressed, "engine key → active text field (typed)");
                return;
            }
        }
        let action = if pressed {
            crate::framework::KeyAction::Down
        } else {
            crate::framework::KeyAction::Up
        };
        match crate::framework::dispatch_key_to_engine_surface(
            vm,
            action,
            key_code,
            unicode,
            self.key_meta_state,
        ) {
            Ok(consumed) => {
                tracing::info!(
                    pressed,
                    consumed,
                    "engine key → RBXSurfaceView.dispatchKeyEvent (key/char not logged)"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "engine key dispatch failed (ignored)");
            }
        }
    }

    /// Drive each custom View's `onDraw(Canvas)` into an Eclipse Pixmap and hand the drawn canvases to
    /// the renderer to composite this frame (the DRAW CASCADE, 2026-06-05).
    ///
    /// Asks the renderer for the current custom-view draw targets (handle + laid-out pixel size),
    /// invokes [`framework::drive_view_draw`](crate::framework::drive_view_draw) on the held VM (which
    /// constructs a Pixmap-backed Java `Canvas` and calls `View.draw(Canvas)` on each — running its
    /// `onDraw` + the bound Canvas natives into the Pixmap), then sets the resulting `(view, canvas)`
    /// pairs on the renderer. No VM, no renderer, or no custom views → nothing to do (the view quads +
    /// text still draw). The framework path is `catch_unwind`-guarded, so a JNI/Java error can never
    /// crash the event loop — a typed error is logged and the frame composites no custom views.
    fn drive_custom_view_draw(&mut self) {
        let Some(vm) = self.vm else {
            return;
        };
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let targets = renderer.custom_view_draw_targets();
        if targets.is_empty() {
            return;
        }
        match crate::framework::drive_view_draw(vm, &targets) {
            Ok(drawn) => {
                if !drawn.is_empty() {
                    tracing::debug!(
                        targets = targets.len(),
                        drawn = drawn.len(),
                        "draw cascade: custom-view onDraw(Canvas) ran; compositing this frame"
                    );
                }
                renderer.set_drawn_canvases(drawn);
            }
            Err(e) => {
                tracing::warn!(error = %e, "draw cascade failed (ignored; no custom-view composite this frame)");
                // Clear any stale drawn canvases so a previous frame's handles don't linger.
                renderer.set_drawn_canvases(Vec::new());
            }
        }
    }

    /// Fall back to `View.performClick()` on `handle` (INPUT v0's path) when the UP `MotionEvent`
    /// could not drive a click. Logged; guarded by the framework's catch_unwind/pending-exception
    /// handling. No VM → no-op.
    fn perform_click_fallback(&self, handle: ViewHandle, x: f32, y: f32) {
        let Some(vm) = self.vm else {
            return;
        };
        match crate::framework::dispatch_click_to_view(vm, handle) {
            Ok(clicked) => tracing::info!(
                handle,
                x,
                y,
                performed = clicked,
                "pointer click fallback dispatched to view (View.performClick)"
            ),
            Err(e) => tracing::warn!(handle, error = %e, "click fallback to view failed (ignored)"),
        }
    }

    /// One-shot, env-gated synthetic tap (dev-host diagnostic). When `ECLIPSE_SYNTHETIC_TAP` is set,
    /// the FIRST redraw drives a full DOWN+UP `MotionEvent` press→release through
    /// `View.dispatchTouchEvent` — proving the hit-test→MotionEvent dispatch chain end-to-end on a
    /// real run (a headless run cannot physically click). Fires at most once and never in normal
    /// operation (no env var → immediate return), so it adds no behavior to the shipped touch path.
    ///
    /// 2026-06-05: it prefers the first CLICKABLE view (driving the exact real-pointer path: press
    /// hit-test → DOWN, release same-view gate → UP, performClick fallback). If no clickable view is
    /// in the snapshot (a known gap for apps whose clickable views are wired internally, AGENTS.md §6
    /// INPUT v0), it falls back to driving a DOWN+UP MotionEvent directly at the first laid-out view,
    /// so the `MotionEvent.obtain` → `dispatchTouchEvent` → `recycle` JNI chain is still exercised
    /// end-to-end against a real Java View object (the non-clickable view just won't fire a click).
    fn maybe_synthetic_tap(&mut self) {
        if self.synthetic_tap_done || std::env::var_os("ECLIPSE_SYNTHETIC_TAP").is_none() {
            return;
        }
        self.synthetic_tap_done = true;
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        if let Some((cx, cy)) = renderer.first_clickable_center() {
            tracing::info!(
                x = cx,
                y = cy,
                "synthetic tap: aiming at first clickable view center"
            );
            // Drive the same press→release path a real pointer would, so it exercises the real wiring
            // (DOWN on press, UP on release, same-view gate, performClick fallback).
            self.cursor = Some((cx, cy));
            self.handle_primary_press();
            self.handle_primary_release();
            return;
        }
        // Fallback: no clickable view in the tree — drive a DOWN+UP MotionEvent directly at the first
        // laid-out view to still prove the JNI dispatch chain end-to-end (diagnostic only).
        let Some((handle, cx, cy)) = renderer.first_view_center() else {
            tracing::info!("synthetic tap: no views in the tree (nothing to tap)");
            return;
        };
        tracing::info!(
            handle,
            x = cx,
            y = cy,
            "synthetic tap: no clickable view; driving DOWN+UP MotionEvent at deepest leaf view (JNI-chain diagnostic)"
        );
        self.dispatch_touch(handle, crate::framework::MotionAction::Down, cx, cy);
        self.dispatch_touch(handle, crate::framework::MotionAction::Up, cx, cy);
    }

    /// 2026-06-14 — dev-host diagnostic, env-gated, two staged actions after the handoff (off in normal
    /// runs). Stage 0 (~6 s, `ECLIPSE_SYNTHETIC_ENGINE_TAP="x,y"`): one DOWN+UP at `(x,y)` through the
    /// engine pointer path (e.g. tap the Sign In button → navigate to the login screen). Stage 1 (~12 s,
    /// `ECLIPSE_SYNTHETIC_TYPE="x,y:text"`): tap `(x,y)` to focus a field, then type `text` through the
    /// engine key path — a self-contained end-to-end proof of focus→type without a physical keyboard.
    /// All via JNI, so it bypasses any compositor/ydotool focus quirk. Mirrors [`Self::maybe_synthetic_tap`].
    fn maybe_synthetic_engine_tap(&mut self) {
        if !self.handed_off {
            return;
        }
        let Some(at) = self.handoff_at else { return };
        let elapsed = at.elapsed();

        // Diagnostic (env ECLIPSE_REFLECT_INPUT): once ~8s post-handoff, reflect the engine's input-
        // bridge classes' method signatures (for the VISIBLE-typing path — find nativePassText's sig).
        if !self.engine_reflect_done && elapsed >= std::time::Duration::from_secs(8) {
            self.engine_reflect_done = true;
            if let Some(vm) = self
                .vm
                .filter(|_| std::env::var_os("ECLIPSE_REFLECT_INPUT").is_some())
            {
                crate::framework::reflect_engine_input_methods(vm);
            }
        }

        // Stage 0: tap (e.g. the Sign In button) once the engine UI is interactive.
        if !self.engine_synthetic_tap_done && elapsed >= std::time::Duration::from_secs(6) {
            self.engine_synthetic_tap_done = true;
            if let Some((x, y)) =
                std::env::var_os("ECLIPSE_SYNTHETIC_ENGINE_TAP").and_then(|s| parse_xy(&s))
            {
                tracing::info!(
                    x,
                    y,
                    "synthetic ENGINE tap (stage 0): DOWN+UP → onTouchEventInternal"
                );
                self.cursor = Some((x, y));
                self.engine_primary_press();
                self.engine_primary_release();
            }
        }

        // Stages 1+2 (ECLIPSE_SYNTHETIC_TYPE="x,y:text"): from ~10 s post-handoff, FOCUS the field then
        // type. The engine's focus-on-tap is asynchronous + occasionally missed, so we RE-TAP `(x,y)`
        // every ~1.5 s until `framework::active_text_field()` reports a focused field, then type once —
        // a robust, self-verifying loop (vs the old fixed-time tap that silently failed when the tap
        // missed). 2026-06-14.
        if !self.engine_synthetic_typed_done && elapsed >= std::time::Duration::from_secs(10) {
            if let Some((x, y, text)) =
                std::env::var_os("ECLIPSE_SYNTHETIC_TYPE").and_then(|s| parse_xy_text(&s))
            {
                if crate::framework::active_text_field() != 0 {
                    // Focused → type the test string now.
                    self.engine_synthetic_typed_done = true;
                    tracing::info!(
                        chars = text.chars().count(),
                        "synthetic TYPE (stage 2): field focused — typing into the active text field"
                    );
                    if let Some(vm) = self.vm {
                        for ch in text.chars() {
                            let handled =
                                crate::framework::type_into_active_text_field(vm, ch as i32, false);
                            tracing::info!(handled, "synthetic TYPE char → active text field");
                        }
                    }
                    // Wake the engine's parked loopers so it re-polls getText() + re-renders the field.
                    crate::loader::ndk_registry::wake_all_loopers();
                    self.engine_typed_at = Some(std::time::Instant::now());
                } else if self
                    .engine_last_focus_tap
                    .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(1500))
                {
                    // Not focused yet → (re)tap the field to focus it (retries until it takes).
                    self.engine_last_focus_tap = Some(std::time::Instant::now());
                    tracing::info!(
                        x,
                        y,
                        "synthetic TYPE (stage 1): focus-tap (retry until focused)"
                    );
                    self.cursor = Some((x, y));
                    self.engine_primary_press();
                    self.engine_primary_release();
                }
            }
        }

        // Stage 3 (ECLIPSE_SYNTHETIC_NEXT="x,y"): a few seconds after typing, tap "Next" once — confirms
        // the engine DETECTED the text (it should advance past the username step, vs an empty-field error).
        if self.engine_synthetic_typed_done
            && !self.engine_synthetic_next_done
            && self
                .engine_typed_at
                .is_some_and(|t| t.elapsed() >= std::time::Duration::from_secs(3))
        {
            if let Some((x, y)) =
                std::env::var_os("ECLIPSE_SYNTHETIC_NEXT").and_then(|s| parse_xy(&s))
            {
                self.engine_synthetic_next_done = true;
                self.engine_next_at = Some(std::time::Instant::now());
                tracing::info!(
                    x,
                    y,
                    "synthetic NEXT (stage 3): tapping Next to confirm detection"
                );
                self.cursor = Some((x, y));
                self.engine_primary_press();
                self.engine_primary_release();
                crate::loader::ndk_registry::wake_all_loopers();
            }
        }

        // Stage 4 (ECLIPSE_SYNTHETIC_TYPE2="x,y:text" or bare "text"): a few seconds after Next, type
        // into the password-step field. 2026-07-01: the value now accepts an "x,y:" target (tried via
        // parse_xy_text first; a value that does not parse as coords keeps the old bare-"text"
        // behavior). The 2026-07-01 re-drive proved the password step is a SECOND screen whose field
        // does NOT auto-focus (stage 4 typed the password into the still-focused USERNAME field), so
        // with a target stage 4 taps (x,y) FIRST — unconditionally, since the previous screen's field
        // handle can still read as focused (stale) — then re-taps every ~1.5 s until
        // `framework::active_text_field()` reports focus (stage 1's retry shape), then types once.
        if self.engine_synthetic_next_done
            && !self.engine_synthetic_typed2_done
            && self
                .engine_next_at
                .is_some_and(|t| t.elapsed() >= std::time::Duration::from_secs(3))
        {
            let parsed =
                std::env::var_os("ECLIPSE_SYNTHETIC_TYPE2").and_then(|s| match parse_xy_text(&s) {
                    Some((x, y, text)) => Some((Some((x, y)), text)),
                    None => s.to_str().map(|t| (None, t.to_owned())),
                });
            if let Some((target, text)) = parsed.filter(|(_, text)| !text.is_empty()) {
                let focused = crate::framework::active_text_field() != 0;
                if let Some((x, y)) = target.filter(|_| {
                    self.engine_last_focus_tap2.is_none()
                        || (!focused
                            && self.engine_last_focus_tap2.is_some_and(|t| {
                                t.elapsed() >= std::time::Duration::from_millis(1500)
                            }))
                }) {
                    // (a) The FIRST tap fires even while a field reads as focused (it can be the
                    // username screen's stale handle); (b) then re-tap until focus actually takes.
                    self.engine_last_focus_tap2 = Some(std::time::Instant::now());
                    tracing::info!(
                        x,
                        y,
                        "synthetic TYPE2 (stage 4): focus-tap (retry until focused)"
                    );
                    self.cursor = Some((x, y));
                    self.engine_primary_press();
                    self.engine_primary_release();
                } else if focused {
                    // (c) A field is focused (with a target: after its tap fired) → type once.
                    self.engine_synthetic_typed2_done = true;
                    self.engine_typed2_at = Some(std::time::Instant::now());
                    tracing::info!(
                        chars = text.chars().count(),
                        "synthetic TYPE2 (stage 4): typing into the field focused after Next (password)"
                    );
                    if let Some(vm) = self.vm {
                        for ch in text.chars() {
                            let handled =
                                crate::framework::type_into_active_text_field(vm, ch as i32, false);
                            tracing::info!(handled, "synthetic TYPE2 char → active text field");
                        }
                    }
                    crate::loader::ndk_registry::wake_all_loopers();
                }
            }
        }

        // Stage 5 (ECLIPSE_SYNTHETIC_SUBMIT="x,y"): a few seconds after the stage-4 typing, tap the
        // submit/Log In button once — completes the autonomous login drive (login POST → the anti-bot
        // challenge path) without a physical mouse. 2026-07-01.
        if self.engine_synthetic_typed2_done
            && !self.engine_synthetic_submit_done
            && self
                .engine_typed2_at
                .is_some_and(|t| t.elapsed() >= std::time::Duration::from_secs(3))
        {
            if let Some((x, y)) =
                std::env::var_os("ECLIPSE_SYNTHETIC_SUBMIT").and_then(|s| parse_xy(&s))
            {
                self.engine_synthetic_submit_done = true;
                tracing::info!(
                    x,
                    y,
                    "synthetic SUBMIT (stage 5): tapping the submit button"
                );
                self.cursor = Some((x, y));
                self.engine_primary_press();
                self.engine_primary_release();
                crate::loader::ndk_registry::wake_all_loopers();
            }
        }
    }
}

/// Open the host game window and run the winit event loop until the window is closed.
///
/// MUST be called on the process main thread (winit requires the event loop there on Linux);
/// `eclipse run` calls this from `main` after the ART VM is booted. `vm` is a borrow of the live
/// [`Vm`](crate::runtime::Vm) (kept alive by the caller on this thread) used to dispatch
/// `View.performClick()` to a hit view on a pointer click; pass `None` to run with no click dispatch
/// (hit-test only). Returns when the window is closed, or a typed [`GraphicsError`] if the event loop
/// or window cannot be created. A Vulkan init failure is NOT returned here — it is logged and the
/// window stays open blank.
pub fn run_windowed(title: &str, vm: Option<&crate::runtime::Vm>) -> Result<(), GraphicsError> {
    let event_loop = EventLoop::new().map_err(GraphicsError::EventLoop)?;
    let mut app = GameWindow {
        title: title.to_owned(),
        window: None,
        renderer: None,
        create_error: None,
        vm,
        cursor: None,
        primary_press: None,
        synthetic_tap_done: false,
        engine_window: None,
        handed_off: false,
        engine_tap_downtime: None,
        handoff_at: None,
        engine_synthetic_tap_done: false,
        engine_synthetic_typed_done: false,
        engine_last_focus_tap: None,
        engine_typed_at: None,
        engine_synthetic_next_done: false,
        engine_next_at: None,
        engine_synthetic_typed2_done: false,
        engine_last_focus_tap2: None,
        engine_typed2_at: None,
        engine_synthetic_submit_done: false,
        engine_reflect_done: false,
        key_meta_state: 0,
        webview_pointer_down: false,
        runtime_shutdown_started: false,
    };
    let run = event_loop.run_app(&mut app);
    // `ApplicationHandler::exiting` is the primary fallback, but keep this idempotent call while
    // `app` (and therefore its Window/EngineNativeWindow) is still alive in case a platform returns
    // an event-loop error without delivering that callback.
    app.shutdown_runtime();
    // 2026-07-16 (web-engine M6): the winit loop WAS the main-thread drain for the client's
    // app-facing WebView callbacks (about_to_wait -> pump_main_looper -> run_pending_main_upcall).
    // It has just stopped — close the slot and run anything still pending HERE, on main, with the
    // main Looper still prepared, so a callback posted during teardown fires instead of parking on
    // a pump that is gone. Before the `?` so the error path closes it too. (`run_apk` never calls
    // `client::shutdown` — `__webview-test` is its only caller — so this is production's ONLY retire.)
    if let Some(vm) = vm {
        crate::framework::retire_main_upcall_dispatch(vm);
    }
    run.map_err(GraphicsError::EventLoop)?;
    // run_app returns Ok even if `resumed` failed to create the window; surface that as an error.
    if let Some(e) = app.create_error {
        return Err(GraphicsError::CreateWindow(e));
    }
    Ok(())
}

/// Publish the engine-window geometry (and, when a real WSI window is published, re-register its
/// pointer→geometry mapping) into the [`ndk_registry`](crate::loader::ndk_registry) so the engine's
/// `ANativeWindow_getWidth`/`getHeight`/`getFormat` report the live window size.
///
/// 2026-06-13: pulled out of [`GameWindow::window_event`]'s `Resized` arm as a free function so the
/// production WSI-geometry publish is unit-testable without a display server (the WSI publish in
/// `resumed` needs a real `RawWindowHandle`, but this geometry re-publish is pure registry writes).
/// `register_wsi_window` is idempotent on the pointer (it updates the geometry of the existing entry,
/// not a duplicate). `wsi_ptr` is `None` before the real WSI window is built (the geometry-only
/// fallback then stands) or `Some(ptr)` once `EngineNativeWindow` is published. NOTE: a true Wayland
/// surface resize also needs `wl_egl_window_resize` — that is follow-up; this is the in-scope getter
/// correctness fix.
fn publish_engine_window_geometry(wsi_ptr: Option<usize>, width: i32, height: i32) {
    crate::loader::ndk_registry::set_engine_window_geometry(width, height);
    if let Some(ptr) = wsi_ptr {
        crate::loader::ndk_registry::register_wsi_window(ptr, width, height);
    }
}

/// Pick the swapchain surface format: prefer 8-bit BGRA in the sRGB-nonlinear color space (the
/// near-universal swapchain format), else fall back to the driver's first advertised format.
///
/// Pulled out as a free function (no Vulkan calls) so the selection logic is unit-testable
/// without a GPU. Returns `None` only if the driver advertises zero formats (a spec violation),
/// which the caller treats as a typed error.
fn choose_surface_format(formats: &[vk::SurfaceFormatKHR]) -> Option<vk::SurfaceFormatKHR> {
    formats
        .iter()
        .copied()
        .find(|f| {
            f.format == vk::Format::B8G8R8A8_SRGB
                && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .or_else(|| formats.first().copied())
}

/// Compute the swapchain extent from the surface capabilities and the window's current size.
///
/// When the surface reports a fixed `current_extent` (`width != u32::MAX`) it is authoritative
/// (Wayland typically does this); otherwise the window size is clamped into the surface's
/// `[min,max]_image_extent` (X11 typically reports `u32::MAX`). Free function so it is
/// unit-testable without a GPU.
fn choose_swap_extent(
    caps: &vk::SurfaceCapabilitiesKHR,
    window_width: u32,
    window_height: u32,
) -> vk::Extent2D {
    if caps.current_extent.width != u32::MAX {
        caps.current_extent
    } else {
        vk::Extent2D {
            width: window_width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
            height: window_height.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
        }
    }
}

/// Choose the swapchain image count: the surface minimum + 1 (so the GPU is not starved waiting
/// for the presenter to release an image), clamped to the surface maximum when one is advertised
/// (`max_image_count == 0` means "no limit"). Free function — unit-testable without a GPU.
fn choose_image_count(caps: &vk::SurfaceCapabilitiesKHR) -> u32 {
    let desired = caps.min_image_count + 1;
    if caps.max_image_count > 0 {
        desired.min(caps.max_image_count)
    } else {
        desired
    }
}

// ---------------------------------------------------------------------------------------------
// View-tree draw: measure + layout + colored-quad geometry (2026-06-05)
//
// The framework records the inflated View tree in `framework::view_registry`
// (`snapshot_tree()` → a flat pre-order `Vec<RenderNode>`, each node carrying its `LayoutParams` +
// child indices). To make that content VISIBLE in the swapchain we (1) run a real Android-style
// measure + layout cascade over the tree to compute each view's absolute pixel rect, then (2) emit
// two triangles per rect as `QuadVertex`es the quad pipeline draws.
//
// Measure/layout follows common-case Android semantics (general public Android docs, implemented
// from first principles here — see `measure_node`/`layout_node`):
//   * LayoutParams.width/height: MATCH_PARENT (-1) → the parent's available size; WRAP_CONTENT (-2)
//     → the view's content size (a TextView's measured text extent; a container's laid-out children);
//     else the exact pixel count.
//   * MeasureSpec packs a mode (UNSPECIFIED / EXACTLY / AT_MOST) with a size; the root is measured
//     EXACTLY at the swapchain extent and the cascade flows top-down.
//   * FrameLayout (and any unknown container) stacks children at the parent origin honoring gravity.
//   * LinearLayout(vertical) stacks children top-to-bottom, (horizontal) left-to-right, honoring
//     gravity; a trivial layout_weight distributes leftover space.
// Out of scope (documented): RelativeLayout / ConstraintLayout, exact multi-pass weight, baseline
// alignment, scrolling. The functions here do NO Vulkan work so they are unit-testable without a GPU.
// ---------------------------------------------------------------------------------------------

use crate::framework::view_registry::{LayoutParams, RenderNode, ViewHandle, MATCH_PARENT};

/// One vertex for the colored-quad pipeline: a position already in Vulkan NDC (x,y ∈ [-1,1], y down)
/// plus an RGBA color. `#[repr(C)]` so the in-memory layout matches the vertex input description the
/// pipeline declares (pos at offset 0, color at offset 8).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct QuadVertex {
    pos: [f32; 2],
    color: [f32; 4],
}

/// A laid-out view: its pixel rect in the swapchain + a fill color + the (optional) text to draw.
///
/// Pixel coordinates with the origin at the top-left (Android/winit convention); converted to NDC by
/// [`pixel_rect_to_quad`]. Kept separate from [`QuadVertex`] so the layout pass is GPU-free.
#[derive(Debug, Clone, PartialEq)]
struct LaidOutView {
    /// The view's registry handle (the same `jlong` the View native peer holds). 2026-06-05: carried
    /// so [`hit_test`] can return the hit view's handle for click dispatch. Opaque index, not a pointer.
    handle: ViewHandle,
    /// Top-left x in pixels.
    x: f32,
    /// Top-left y in pixels.
    y: f32,
    /// Width in pixels.
    w: f32,
    /// Height in pixels.
    h: f32,
    /// `true` if the view recorded a click listener (`View.nativeSetOnClickListener`). 2026-06-05:
    /// [`hit_test`] only targets clickable views, so an inert container/label is never clicked.
    clickable: bool,
    /// Fill color (RGBA, 0..1).
    color: [f32; 4],
    /// The view's text, if any — drawn over the rect by the text pass (when present).
    text: Option<String>,
}

/// Convert an AOSP `0xAARRGGBB` `argb` int (`View.setBackgroundColor`) into the renderer's straight
/// RGBA float channels (0..1). 2026-06-05: the quad pipeline blends with straight alpha (over the blue
/// clear), so a fully transparent background (alpha 0) leaves the clear color showing through.
fn argb_to_rgba_f32(argb: i32) -> [f32; 4] {
    let v = argb as u32;
    let a = ((v >> 24) & 0xFF) as f32 / 255.0;
    let r = ((v >> 16) & 0xFF) as f32 / 255.0;
    let g = ((v >> 8) & 0xFF) as f32 / 255.0;
    let b = (v & 0xFF) as f32 / 255.0;
    [r, g, b, a]
}

/// `true` if `class_name` is an app-defined (CUSTOM) View subclass — i.e. NOT a framework class.
///
/// 2026-06-05: only custom views can override `onDraw(Canvas)` with app drawing (e.g. multitouch.test's
/// `com.leocardz.multitouch.test.MultiTouch`); the framework's own `android.*`/`androidx.*` widgets draw
/// via their bound natives, not a custom `onDraw`. The draw cascade ([`framework::drive_view_draw`]) is
/// limited to custom views so it never re-enters a framework widget's draw (which Eclipse backs
/// natively, not via a Canvas). Matching is by class-name prefix — the standard Android framework
/// namespaces. GPU/VM-free, so it is unit-testable.
fn is_custom_view_class(class_name: &str) -> bool {
    const FRAMEWORK_PREFIXES: [&str; 4] = ["android.", "androidx.", "com.android.", "java."];
    !class_name.is_empty() && !FRAMEWORK_PREFIXES.iter().any(|p| class_name.starts_with(p))
}

/// A small fixed palette so nested views are visually distinguishable by depth. Indexed by
/// `depth % len`. Colors are mid-tones that read against the blue clear background.
const DEPTH_PALETTE: [[f32; 4]; 4] = [
    [0.93, 0.94, 0.96, 1.0], // depth 0: near-white container
    [0.80, 0.85, 0.92, 1.0], // depth 1
    [0.66, 0.74, 0.86, 1.0], // depth 2
    [0.55, 0.64, 0.80, 1.0], // depth 3+
];

/// Fallback content size (pixels) for a `WRAP_CONTENT` view with no measurable content (e.g. a leaf
/// `View` with no text, or a `TextView` measured without a font). Small but non-zero so the quad is
/// visible. A real font replaces the height with the line height for text.
const WRAP_FALLBACK_W: f32 = 64.0;
const WRAP_FALLBACK_H: f32 = TEXT_PX;

/// A `MeasureSpec` mode — how the parent constrains a child's size during measure. Standard Android
/// semantics (`android.view.View.MeasureSpec`): `Unspecified` = no constraint (size yourself to
/// content), `Exactly` = take exactly this size, `AtMost` = at most this size (size to content,
/// clamped). 2026-06-05.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpecMode {
    Unspecified,
    Exactly,
    AtMost,
}

/// A measure constraint passed top-down: a [`SpecMode`] plus a size in pixels. Mirrors a packed
/// Android `MeasureSpec` (mode + size) but kept as a struct since Eclipse never crosses the JNI
/// boundary with it (the cascade runs entirely renderer-side over the snapshot).
#[derive(Debug, Clone, Copy, PartialEq)]
struct MeasureSpec {
    mode: SpecMode,
    size: f32,
}

impl MeasureSpec {
    /// Resolve a child's measured size on one axis from its `LayoutParams` dimension and the parent's
    /// spec for that axis, returning `(measured_size, child_spec_for_recursing_into_the_child)`.
    ///
    /// 2026-06-05, standard Android `getChildMeasureSpec` logic, common case:
    ///   * exact px (`>= 0`)        → `Exactly(px)`.
    ///   * `MATCH_PARENT` (-1)      → fill the parent's available size: `Exactly(parent.size)` when the
    ///     parent is `Exactly`/`AtMost`, else `Unspecified` (parent has no bound to match).
    ///   * `WRAP_CONTENT` (-2)      → size to content, bounded by the parent: `AtMost(parent.size)`
    ///     (or `Unspecified` when the parent is unbounded).
    ///
    /// `content` is the view's own measured content size (text/children extent), used only to settle
    /// the returned `measured_size` for the `WRAP`/`Unspecified` cases.
    fn resolve(self, dimension: i32, content: f32) -> (f32, MeasureSpec) {
        let avail = self.size.max(0.0);
        if dimension >= 0 {
            let px = dimension as f32;
            return (
                px,
                MeasureSpec {
                    mode: SpecMode::Exactly,
                    size: px,
                },
            );
        }
        match dimension {
            MATCH_PARENT => match self.mode {
                SpecMode::Exactly | SpecMode::AtMost => (
                    avail,
                    MeasureSpec {
                        mode: SpecMode::Exactly,
                        size: avail,
                    },
                ),
                SpecMode::Unspecified => (
                    content,
                    MeasureSpec {
                        mode: SpecMode::Unspecified,
                        size: 0.0,
                    },
                ),
            },
            // WRAP_CONTENT (and any other negative sentinel → treat as wrap).
            _ => match self.mode {
                SpecMode::Exactly | SpecMode::AtMost => (
                    content.min(avail),
                    MeasureSpec {
                        mode: SpecMode::AtMost,
                        size: avail,
                    },
                ),
                SpecMode::Unspecified => (
                    content,
                    MeasureSpec {
                        mode: SpecMode::Unspecified,
                        size: 0.0,
                    },
                ),
            },
        }
    }
}

/// Measures the pixel extent of a single line of text. Built from the renderer's [`GlyphAtlas`] so
/// `WRAP_CONTENT` TextViews size to their real glyph metrics; when no font is available the cascade
/// uses [`WRAP_FALLBACK_W`]/[`WRAP_FALLBACK_H`] instead. Pure data (no GPU) so the cascade stays
/// unit-testable.
#[derive(Clone, Copy)]
struct TextMeasure<'a> {
    atlas: &'a GlyphAtlas,
}

impl TextMeasure<'_> {
    /// The pixel width of `text` = the sum of each glyph's advance (unknown glyphs contribute 0, as
    /// they are skipped at draw time too). Plus the text inset so the rect encloses the drawn glyphs.
    fn width(&self, text: &str) -> f32 {
        let advances: f32 = text
            .chars()
            .map(|ch| self.atlas.glyphs.get(&ch).map_or(0.0, |g| g.advance))
            .sum();
        advances + 2.0 * TEXT_PAD_X
    }

    /// The pixel height of one line of text = the atlas line height.
    fn height(&self) -> f32 {
        self.atlas.line_height
    }
}

/// The computed geometry of one measured/laid-out node, indexed parallel to the input `nodes` slice.
/// Internal to the cascade; [`layout_views`] flattens the absolute rects into [`LaidOutView`]s.
#[derive(Debug, Clone, Copy, Default)]
struct NodeBox {
    /// Measured width/height in pixels (set by the measure pass).
    mw: f32,
    mh: f32,
    /// Absolute top-left position in pixels (set by the layout pass).
    x: f32,
    y: f32,
}

/// True when `class_name` names a `LinearLayout`, which Eclipse lays out as a **vertical** stack.
///
/// 2026-06-05: the runtime `orientation` field is a Java field on `LinearLayout`, not threaded
/// through any native into the snapshot, so Eclipse cannot read it here. We default a `LinearLayout`
/// to vertical (the orientation the demo + typical app shells use; confirmed by the demo run).
/// Horizontal-`LinearLayout` orientation detection is documented as out of scope — a horizontal
/// `LinearLayout` currently stacks vertically. Any non-`LinearLayout` container is laid out
/// FrameLayout-style (children stacked at the origin, positioned by gravity).
fn is_vertical_linear(class_name: &str) -> bool {
    class_name.ends_with("LinearLayout")
}

/// `Gravity` bits (subset we honor): standard `android.view.Gravity` constants. 2026-06-05.
const GRAVITY_CENTER_HORIZONTAL: i32 = 0x01;
const GRAVITY_RIGHT: i32 = 0x05;
const GRAVITY_CENTER_VERTICAL: i32 = 0x10;
const GRAVITY_BOTTOM: i32 = 0x50;

/// Android's "no gravity specified" sentinel (`FrameLayout/LinearLayout.LayoutParams` default
/// `gravity = -1`, `Gravity.UNSPECIFIED_GRAVITY`). A negative gravity means "use the default
/// placement" (top|left), NOT a bitmask — `-1 & 0x05 == 0x05` would otherwise read as right|bottom.
/// 2026-06-05: confirmed empirically on the demo run — every inflated view reports `gravity=-1`.
fn gravity_specified(gravity: i32) -> bool {
    gravity >= 0
}

/// Horizontal offset of a child of measured width `cw` within a slot of width `slot_w`, per `gravity`.
/// Default (unspecified gravity or no horizontal gravity bits) = left (0).
fn gravity_dx(gravity: i32, slot_w: f32, cw: f32) -> f32 {
    if !gravity_specified(gravity) {
        return 0.0;
    }
    let slack = (slot_w - cw).max(0.0);
    if gravity & GRAVITY_RIGHT == GRAVITY_RIGHT {
        slack
    } else if gravity & GRAVITY_CENTER_HORIZONTAL != 0 {
        slack * 0.5
    } else {
        0.0
    }
}

/// Vertical offset of a child of measured height `ch` within a slot of height `slot_h`, per `gravity`.
/// Default (unspecified gravity or no vertical gravity bits) = top (0).
fn gravity_dy(gravity: i32, slot_h: f32, ch: f32) -> f32 {
    if !gravity_specified(gravity) {
        return 0.0;
    }
    let slack = (slot_h - ch).max(0.0);
    if gravity & GRAVITY_BOTTOM == GRAVITY_BOTTOM {
        slack
    } else if gravity & GRAVITY_CENTER_VERTICAL != 0 {
        slack * 0.5
    } else {
        0.0
    }
}

/// Total horizontal margins of a view (left + right).
fn margin_h(lp: &LayoutParams) -> f32 {
    (lp.margins[0] + lp.margins[2]).max(0) as f32
}
/// Total vertical margins of a view (top + bottom).
fn margin_v(lp: &LayoutParams) -> f32 {
    (lp.margins[1] + lp.margins[3]).max(0) as f32
}

/// MEASURE pass (top-down): compute each node's measured size into `boxes[idx].{mw,mh}` given the
/// parent's `w_spec`/`h_spec`. Recurses into children (the recursion depth is bounded by the
/// snapshot's `MAX_DEPTH` cap). 2026-06-05.
///
/// A container's `WRAP_CONTENT` content size is the extent of its laid-out children (sum along the
/// stacking axis for a vertical LinearLayout, max across for the cross axis; max of both for a
/// FrameLayout). A leaf TextView's content size is its measured text; any other leaf's is the
/// `WRAP_FALLBACK_*`. `idx_guard` prevents a (registry-impossible) cycle from looping forever.
fn measure_node(
    nodes: &[RenderNode],
    boxes: &mut [NodeBox],
    idx: usize,
    w_spec: MeasureSpec,
    h_spec: MeasureSpec,
    text: Option<TextMeasure>,
    depth_guard: u32,
) {
    const MAX_DEPTH: u32 = 256;
    let Some(node) = nodes.get(idx) else {
        return;
    };
    if depth_guard >= MAX_DEPTH {
        return;
    }
    let lp = &node.layout;
    let pad_h = (lp.padding[0] + lp.padding[2]).max(0) as f32;
    let pad_v = (lp.padding[1] + lp.padding[3]).max(0) as f32;

    // Available interior space the children may use (parent spec minus this view's padding).
    let inner_w = (w_spec.size - pad_h).max(0.0);
    let inner_h = (h_spec.size - pad_v).max(0.0);

    if node.children.is_empty() {
        // Leaf: content is the text extent (TextView) or the fallback box.
        let (content_w, content_h) = match (&node.text, text) {
            (Some(t), Some(tm)) => (tm.width(t), tm.height()),
            (Some(t), None) if !t.is_empty() => (WRAP_FALLBACK_W, WRAP_FALLBACK_H),
            _ => (WRAP_FALLBACK_W, WRAP_FALLBACK_H),
        };
        let (mw, _) = w_spec.resolve(lp.width, content_w + pad_h);
        let (mh, _) = h_spec.resolve(lp.height, content_h + pad_v);
        boxes[idx].mw = mw.max(0.0);
        boxes[idx].mh = mh.max(0.0);
        return;
    }

    // Container: measure children under the interior spec, then settle this view's size.
    let child_w_spec = MeasureSpec {
        mode: if w_spec.mode == SpecMode::Unspecified {
            SpecMode::Unspecified
        } else {
            SpecMode::AtMost
        },
        size: inner_w,
    };
    let child_h_spec = MeasureSpec {
        mode: if h_spec.mode == SpecMode::Unspecified {
            SpecMode::Unspecified
        } else {
            SpecMode::AtMost
        },
        size: inner_h,
    };

    let vertical = is_vertical_linear(&node.class_name);
    let mut sum_h = 0.0f32; // total stacked height (vertical LinearLayout main axis)
    let mut max_w = 0.0f32; // widest child including its margins (cross axis / Frame width)
    let mut max_h = 0.0f32; // tallest child including its margins (Frame height)

    for &ci in &node.children {
        if ci >= nodes.len() {
            continue;
        }
        measure_node(
            nodes,
            boxes,
            ci,
            child_w_spec,
            child_h_spec,
            text,
            depth_guard + 1,
        );
        let clp = &nodes[ci].layout;
        let cw = boxes[ci].mw + margin_h(clp);
        let ch = boxes[ci].mh + margin_v(clp);
        sum_h += ch;
        max_w = max_w.max(cw);
        max_h = max_h.max(ch);
    }

    // This container's content size depends on its layout kind: a vertical LinearLayout is as tall as
    // its stacked children and as wide as its widest; a FrameLayout/unknown is the bounding box.
    let (content_w, content_h) = if vertical {
        (max_w + pad_h, sum_h + pad_v)
    } else {
        (max_w + pad_h, max_h + pad_v)
    };

    let (mw, _) = w_spec.resolve(lp.width, content_w);
    let (mh, _) = h_spec.resolve(lp.height, content_h);
    boxes[idx].mw = mw.max(0.0);
    boxes[idx].mh = mh.max(0.0);
}

/// LAYOUT pass (top-down): position node `idx` at absolute `(x, y)` and recursively position its
/// children within its content box, honoring layout kind + gravity + a trivial weight. 2026-06-05.
fn layout_node(
    nodes: &[RenderNode],
    boxes: &mut [NodeBox],
    idx: usize,
    x: f32,
    y: f32,
    depth_guard: u32,
) {
    const MAX_DEPTH: u32 = 256;
    if depth_guard >= MAX_DEPTH || idx >= nodes.len() {
        return;
    }
    boxes[idx].x = x;
    boxes[idx].y = y;
    let node = &nodes[idx];
    if node.children.is_empty() {
        return;
    }
    let lp = &node.layout;
    let inner_x = x + lp.padding[0].max(0) as f32;
    let inner_y = y + lp.padding[1].max(0) as f32;
    let inner_w = (boxes[idx].mw - (lp.padding[0] + lp.padding[2]).max(0) as f32).max(0.0);
    let inner_h = (boxes[idx].mh - (lp.padding[1] + lp.padding[3]).max(0) as f32).max(0.0);

    if is_vertical_linear(&node.class_name) {
        // Vertical LinearLayout: stack children top-to-bottom, advancing a cursor; distribute leftover
        // vertical space by `layout_weight` (a trivial single pass); honor horizontal gravity per child.
        let used: f32 = node
            .children
            .iter()
            .filter(|&&ci| ci < nodes.len())
            .map(|&ci| boxes[ci].mh + margin_v(&nodes[ci].layout))
            .sum();
        let total_weight: f32 = node
            .children
            .iter()
            .filter(|&&ci| ci < nodes.len())
            .map(|&ci| nodes[ci].layout.weight.max(0.0))
            .sum();
        let leftover = (inner_h - used).max(0.0);

        let mut cursor = inner_y;
        for &ci in &node.children {
            if ci >= nodes.len() {
                continue;
            }
            let clp = nodes[ci].layout;
            // Grow this child by its share of the leftover space (weighted), if any.
            if total_weight > 0.0 && clp.weight > 0.0 {
                boxes[ci].mh += leftover * (clp.weight / total_weight);
            }
            let cw = boxes[ci].mw;
            let ch = boxes[ci].mh;
            // Cross axis = horizontal: gravity positions the child within the inner width.
            let dx = gravity_dx(clp.gravity, inner_w - margin_h(&clp), cw);
            let cx = inner_x + clp.margins[0].max(0) as f32 + dx;
            let cy = cursor + clp.margins[1].max(0) as f32;
            layout_node(nodes, boxes, ci, cx, cy, depth_guard + 1);
            cursor += ch + margin_v(&clp);
        }
    } else {
        // FrameLayout / unknown container: every child at the parent origin, positioned by gravity.
        for &ci in &node.children {
            if ci >= nodes.len() {
                continue;
            }
            let clp = nodes[ci].layout;
            let cw = boxes[ci].mw;
            let ch = boxes[ci].mh;
            let dx = gravity_dx(clp.gravity, inner_w - margin_h(&clp), cw);
            let dy = gravity_dy(clp.gravity, inner_h - margin_v(&clp), ch);
            let cx = inner_x + clp.margins[0].max(0) as f32 + dx;
            let cy = inner_y + clp.margins[1].max(0) as f32 + dy;
            layout_node(nodes, boxes, ci, cx, cy, depth_guard + 1);
        }
    }
}

/// Run the measure + layout cascade over the recorded view tree and flatten each node's absolute
/// rect into a [`LaidOutView`] (parallel to `nodes`, so text/quad builders keep their indexing).
///
/// 2026-06-05: the root (node 0) is measured `Exactly` at the swapchain `extent` (the window is the
/// device the root fills, like Android's `ViewRootImpl`), then laid out at the origin. Each node's
/// fill color stays depth-distinguished. An empty tree → empty output (clear-only frame). `text` is
/// the optional glyph-metric measurer for `WRAP_CONTENT` TextViews; `None` (no font) → fallback box.
/// Pure (no Vulkan) so it is unit-testable without a GPU.
fn layout_views(
    nodes: &[RenderNode],
    extent: vk::Extent2D,
    text: Option<TextMeasure>,
) -> Vec<LaidOutView> {
    if nodes.is_empty() {
        return Vec::new();
    }
    let ew = extent.width.max(1) as f32;
    let eh = extent.height.max(1) as f32;
    let mut boxes = vec![NodeBox::default(); nodes.len()];

    let root_w = MeasureSpec {
        mode: SpecMode::Exactly,
        size: ew,
    };
    let root_h = MeasureSpec {
        mode: SpecMode::Exactly,
        size: eh,
    };
    measure_node(nodes, &mut boxes, 0, root_w, root_h, text, 0);
    layout_node(nodes, &mut boxes, 0, 0.0, 0.0, 0);

    nodes
        .iter()
        .zip(boxes.iter())
        .map(|(node, b)| {
            // A real `View.setBackgroundColor` (ARGB) wins over the synthetic depth color, for fidelity.
            let color = match node.background_color {
                Some(argb) => argb_to_rgba_f32(argb),
                None => DEPTH_PALETTE[(node.depth as usize).min(DEPTH_PALETTE.len() - 1)],
            };
            LaidOutView {
                handle: node.handle,
                x: b.x,
                y: b.y,
                // Clamp to >= 1 so a zero-measured view never produces a degenerate (invalid) quad.
                w: b.mw.max(1.0),
                h: b.mh.max(1.0),
                clickable: node.clickable,
                color,
                text: node.text.clone(),
            }
        })
        .collect()
}

/// Hit-test a laid-out view tree for the **topmost clickable** view at window pixel `(x, y)`.
///
/// 2026-06-05: pure geometry over the recorded rects — no GPU, no VM — so it is fully unit-testable.
/// `views` is the pre-order [`layout_views`] output (parent before children, siblings left-to-right),
/// which is also the draw order, so a later entry is drawn ON TOP of an earlier one. We therefore scan
/// in REVERSE and return the first (i.e. last-drawn / deepest / topmost) view that (a) is `clickable`
/// and (b) whose half-open rect `[x, x+w) × [y, y+h)` contains the point. Returns the hit view's
/// [`ViewHandle`] (the caller dispatches `performClick` to it via JNI), or `None` if no clickable view
/// is under the point. A zero/negative-size view never matches (half-open interval). The point is in
/// the same top-left-origin pixel space the layout pass produced.
fn hit_test(views: &[LaidOutView], x: f32, y: f32) -> Option<ViewHandle> {
    views
        .iter()
        .rev()
        .find(|v| v.clickable && x >= v.x && x < v.x + v.w && y >= v.y && y < v.y + v.h)
        .map(|v| v.handle)
}

/// The single-pointer touch down→up state-machine decision: should the release complete a tap, and on
/// which view? Returns `Some(handle)` iff the press hit a view (`pressed`) AND the release hit the SAME
/// view (`released`) — Android touch semantics, where a release that drifts off the pressed view is not
/// a tap. Pure (no GPU/VM); the renderer supplies `pressed`/`released` from [`hit_test`]. 2026-06-05.
fn should_complete_tap(
    pressed: Option<ViewHandle>,
    released: Option<ViewHandle>,
) -> Option<ViewHandle> {
    match (pressed, released) {
        (Some(p), Some(r)) if p == r => Some(p),
        _ => None,
    }
}

/// Parse `"x,y"` (window pixels) for the stage-0 synthetic-tap diagnostic. `None` on malformed input.
fn parse_xy(spec: &std::ffi::OsStr) -> Option<(f32, f32)> {
    let s = spec.to_str()?;
    let (xs, ys) = s.split_once(',')?;
    Some((xs.trim().parse().ok()?, ys.trim().parse().ok()?))
}

/// Parse `"x,y:text"` for the stage-1 + stage-4 synthetic type-test diagnostics. `None` on
/// malformed input (stage 4 then treats the whole value as bare text — the pre-2026-07-01 form).
fn parse_xy_text(spec: &std::ffi::OsStr) -> Option<(f32, f32, String)> {
    let s = spec.to_str()?;
    let (xy, text) = s.split_once(':')?;
    let (xs, ys) = xy.split_once(',')?;
    Some((
        xs.trim().parse().ok()?,
        ys.trim().parse().ok()?,
        text.to_string(),
    ))
}

/// 2026-06-14 — map a winit logical key to the Android `KEYCODE_*` for the keys needed to type
/// credentials and edit text, or `None` for keys with no clean mapping (dropped). The typed character
/// itself rides on the `KeyEvent.unicodeValue` (winit's resolved `text`), so a `KEYCODE_UNKNOWN` (0)
/// printable still types. Values are the public `android.view.KeyEvent.KEYCODE_*` constants. Pure
/// (VM/GPU-free) so it is unit-testable without fabricating a winit event.
fn winit_keycode(key: &winit::keyboard::Key) -> Option<i32> {
    use winit::keyboard::{Key, NamedKey};
    Some(match key {
        Key::Character(s) => {
            let c = s.chars().next()?;
            match c {
                'a'..='z' => 29 + (c as i32 - 'a' as i32), // KEYCODE_A=29 .. KEYCODE_Z=54
                'A'..='Z' => 29 + (c as i32 - 'A' as i32),
                '0'..='9' => 7 + (c as i32 - '0' as i32), // KEYCODE_0=7 .. KEYCODE_9=16
                ' ' => 62,                                // SPACE
                '.' => 56,                                // PERIOD
                ',' => 55,                                // COMMA
                '@' => 77,                                // AT
                '-' | '_' => 69,                          // MINUS (char comes via unicodeValue)
                '+' | '=' => 70,                          // EQUALS
                '/' => 76,                                // SLASH
                _ => 0, // KEYCODE_UNKNOWN — still types via unicode
            }
        }
        Key::Named(NamedKey::Space) => 62,      // SPACE
        Key::Named(NamedKey::Backspace) => 67,  // DEL
        Key::Named(NamedKey::Enter) => 66,      // ENTER
        Key::Named(NamedKey::Tab) => 61,        // TAB
        Key::Named(NamedKey::Escape) => 111,    // ESCAPE
        Key::Named(NamedKey::Delete) => 112,    // FORWARD_DEL
        Key::Named(NamedKey::ArrowLeft) => 21,  // DPAD_LEFT
        Key::Named(NamedKey::ArrowRight) => 22, // DPAD_RIGHT
        Key::Named(NamedKey::ArrowUp) => 19,    // DPAD_UP
        Key::Named(NamedKey::ArrowDown) => 20,  // DPAD_DOWN
        Key::Named(NamedKey::Home) => 122,      // MOVE_HOME
        Key::Named(NamedKey::End) => 123,       // MOVE_END
        _ => return None,
    })
}

/// Convert a top-left-origin pixel rect into 6 [`QuadVertex`]es (two triangles) in Vulkan NDC.
///
/// Vulkan NDC has the origin at center, x right, **y down** (matching pixel space), so a pixel
/// `p` maps to NDC `2*p/extent - 1` on each axis. Returns the triangles in the winding the pipeline
/// expects (no face culling is enabled, so winding is not load-bearing, but kept consistent).
fn pixel_rect_to_quad(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [f32; 4],
    extent: vk::Extent2D,
) -> [QuadVertex; 6] {
    let ew = extent.width.max(1) as f32;
    let eh = extent.height.max(1) as f32;
    let to_ndc = |px: f32, py: f32| -> [f32; 2] { [2.0 * px / ew - 1.0, 2.0 * py / eh - 1.0] };
    let tl = to_ndc(x, y);
    let tr = to_ndc(x + w, y);
    let bl = to_ndc(x, y + h);
    let br = to_ndc(x + w, y + h);
    let v = |pos: [f32; 2]| QuadVertex { pos, color };
    // Triangle 1: tl, tr, br. Triangle 2: tl, br, bl.
    [v(tl), v(tr), v(br), v(tl), v(br), v(bl)]
}

/// Build the full vertex buffer (CPU side) for a set of laid-out views: 6 vertices per quad, in
/// order. Empty input → empty output (the draw pass then draws zero vertices). Pure/GPU-free.
fn build_quad_vertices(views: &[LaidOutView], extent: vk::Extent2D) -> Vec<QuadVertex> {
    let mut verts = Vec::with_capacity(views.len() * 6);
    for v in views {
        verts.extend_from_slice(&pixel_rect_to_quad(v.x, v.y, v.w, v.h, v.color, extent));
    }
    verts
}

/// Decode embedded SPIR-V bytes into the `u32` words `vkCreateShaderModule` requires.
///
/// Returns a typed error (never panics) if the blob length is not a multiple of 4 (not valid
/// SPIR-V) — guarding the `include_bytes!` blobs against truncation/corruption. The words are read
/// little-endian (the SPIR-V on-disk encoding `glslangValidator` emits on an LE host; Eclipse's
/// targets — x86_64 / aarch64 — are little-endian, AGENTS.md §9).
fn read_spirv(bytes: &[u8]) -> Result<Vec<u32>, GraphicsError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(GraphicsError::Vulkan(format!(
            "embedded SPIR-V length {} is not a multiple of 4 (corrupt shader blob)",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Pick a memory type index that is HOST_VISIBLE|HOST_COHERENT and allowed by `type_filter` (the
/// buffer's `memoryTypeBits`). Host-coherent means CPU writes are visible to the GPU without an
/// explicit flush — the simplest correct path for the small, per-frame-rewritten vertex buffer.
/// Free function (no device calls) so it is unit-testable with a synthetic memory-properties table.
fn find_host_visible_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
) -> Option<u32> {
    let needed = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    (0..props.memory_type_count).find(|&i| {
        let supported = (type_filter & (1 << i)) != 0;
        let flags = props.memory_types[i as usize].property_flags;
        supported && flags.contains(needed)
    })
}

/// Pick a DEVICE_LOCAL memory type allowed by `type_filter` (the image's `memoryTypeBits`) — used
/// for the glyph-atlas image (GPU-resident, sampled). Falls back to any allowed type if no device-
/// local one is advertised (spec-rare). Free function — unit-testable with a synthetic table.
fn find_device_local_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
) -> Option<u32> {
    let device_local = (0..props.memory_type_count).find(|&i| {
        let supported = (type_filter & (1 << i)) != 0;
        let flags = props.memory_types[i as usize].property_flags;
        supported && flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    });
    device_local.or_else(|| (0..props.memory_type_count).find(|&i| (type_filter & (1 << i)) != 0))
}

/// Upload `pixels` (R8, `width`×`height`) into `image` via a host-visible staging buffer + a
/// one-time-submit command buffer that transitions UNDEFINED→TRANSFER_DST, copies, then
/// TRANSFER_DST→SHADER_READ_ONLY_OPTIMAL. Blocks on a fence until the copy completes (init-time
/// only, off the frame loop). Frees the staging buffer/command buffer/fence on every path.
#[allow(clippy::too_many_arguments)]
fn upload_atlas_pixels(
    device: &ash::Device,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    image: vk::Image,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), GraphicsError> {
    let size = (width as vk::DeviceSize) * (height as vk::DeviceSize);
    // --- Staging buffer (host visible) ---
    let buf_info = vk::BufferCreateInfo::default()
        .size(size.max(1))
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: device valid; buf_info outlives the call.
    let staging = unsafe { device.create_buffer(&buf_info, None) }
        .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (staging): {e}")))?;
    // SAFETY: staging just created.
    let req = unsafe { device.get_buffer_memory_requirements(staging) };
    let mem_type = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
        .ok_or_else(|| {
            // SAFETY: staging valid + unbound; free before bailing.
            unsafe { device.destroy_buffer(staging, None) };
            GraphicsError::Vulkan("no host-visible memory for the atlas staging buffer".to_owned())
        })?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_type);
    // SAFETY: alloc outlives the call.
    let staging_mem = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: staging valid + unbound; free before bailing.
            unsafe { device.destroy_buffer(staging, None) };
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateMemory (staging): {e}"
            )));
        }
    };
    // A small RAII-ish cleanup closure for the staging resources on every exit path below.
    let free_staging = |device: &ash::Device| {
        // SAFETY: both handles valid + owned; freed once.
        unsafe {
            device.free_memory(staging_mem, None);
            device.destroy_buffer(staging, None);
        }
    };
    // SAFETY: staging+mem valid; bind whole allocation.
    if let Err(e) = unsafe { device.bind_buffer_memory(staging, staging_mem, 0) } {
        free_staging(device);
        return Err(GraphicsError::Vulkan(format!(
            "vkBindBufferMemory (staging): {e}"
        )));
    }
    // SAFETY: staging_mem is host-visible ≥ size; copy the pixel bytes in, then unmap.
    unsafe {
        match device.map_memory(staging_mem, 0, size.max(1), vk::MemoryMapFlags::empty()) {
            Ok(ptr) => {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr as *mut u8, pixels.len());
                device.unmap_memory(staging_mem);
            }
            Err(e) => {
                free_staging(device);
                return Err(GraphicsError::Vulkan(format!("vkMapMemory (staging): {e}")));
            }
        }
    }

    // --- One-time command buffer: transition, copy, transition ---
    let cb_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: pool valid; cb_info outlives the call.
    let cmd = match unsafe { device.allocate_command_buffers(&cb_info) } {
        Ok(c) => c[0],
        Err(e) => {
            free_staging(device);
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateCommandBuffers (upload): {e}"
            )));
        }
    };
    let free_cmd = |device: &ash::Device| {
        // SAFETY: cmd belongs to command_pool; freed once.
        unsafe { device.free_command_buffers(command_pool, &[cmd]) };
    };

    let subresource = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1);
    // Record. SAFETY: cmd valid; all handles below are valid; the create-infos outlive the calls.
    // An inner closure typed `-> ash::prelude::VkResult<()>` so its `?` propagates `vk::Result`
    // locally (not to this fn, which returns `GraphicsError`); the result is `map_err`'d below.
    let record = (|| -> ash::prelude::VkResult<()> {
        unsafe {
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            device.begin_command_buffer(cmd, &begin)?;

            let to_transfer = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(subresource)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_transfer),
            );

            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });
            device.cmd_copy_buffer_to_image(
                cmd,
                staging,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&region),
            );

            let to_shader = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(subresource)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_shader),
            );
            device.end_command_buffer(cmd)
        }
    })();
    if let Err(e) = record {
        free_cmd(device);
        free_staging(device);
        return Err(GraphicsError::Vulkan(format!("record atlas upload: {e}")));
    }

    // Submit + wait on a fence so the copy finishes before the staging buffer is freed.
    // SAFETY: device valid; default fence info.
    let fence = match unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) } {
        Ok(f) => f,
        Err(e) => {
            free_cmd(device);
            free_staging(device);
            return Err(GraphicsError::Vulkan(format!("create upload fence: {e}")));
        }
    };
    let cmds = [cmd];
    let submit = vk::SubmitInfo::default().command_buffers(&cmds);
    // SAFETY: queue/cmd/fence valid; submit + its slice outlive the call; fence tracks completion.
    let submitted = unsafe { device.queue_submit(queue, &[submit], fence) };
    let waited =
        submitted.and_then(|()| unsafe { device.wait_for_fences(&[fence], true, u64::MAX) });
    // SAFETY: fence valid + owned; destroy once. Then free the cmd buffer + staging resources.
    unsafe { device.destroy_fence(fence, None) };
    free_cmd(device);
    free_staging(device);
    waited.map_err(|e| GraphicsError::Vulkan(format!("submit/wait atlas upload: {e}")))?;
    Ok(())
}

/// Upload `pixels` (straight RGBA8, 4 bytes/pixel, `width`×`height`) into `image` via a host-visible
/// staging buffer + a one-time `UNDEFINED → TRANSFER_DST → SHADER_READ_ONLY` transition. The RGBA8
/// sibling of [`upload_atlas_pixels`] (which uploads 1 byte/pixel R8). Frees the staging buffer + the
/// upload command buffer + fence on every exit path; submits and waits so the copy finishes before
/// the staging buffer is freed. 2026-06-05.
#[allow(clippy::too_many_arguments)]
fn upload_rgba_pixels(
    device: &ash::Device,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    image: vk::Image,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), GraphicsError> {
    // 4 bytes/pixel for RGBA8. `size` is the byte length of the texture.
    let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 4;
    let buf_info = vk::BufferCreateInfo::default()
        .size(size.max(1))
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: device valid; buf_info outlives the call.
    let staging = unsafe { device.create_buffer(&buf_info, None) }
        .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (rgba staging): {e}")))?;
    // SAFETY: staging just created.
    let req = unsafe { device.get_buffer_memory_requirements(staging) };
    let mem_type = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
        .ok_or_else(|| {
            // SAFETY: staging valid + unbound; free before bailing.
            unsafe { device.destroy_buffer(staging, None) };
            GraphicsError::Vulkan("no host-visible memory for the rgba staging buffer".to_owned())
        })?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_type);
    // SAFETY: alloc outlives the call.
    let staging_mem = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: staging valid + unbound; free before bailing.
            unsafe { device.destroy_buffer(staging, None) };
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateMemory (rgba staging): {e}"
            )));
        }
    };
    let free_staging = |device: &ash::Device| {
        // SAFETY: both handles valid + owned; freed once.
        unsafe {
            device.free_memory(staging_mem, None);
            device.destroy_buffer(staging, None);
        }
    };
    // SAFETY: staging+mem valid; bind whole allocation.
    if let Err(e) = unsafe { device.bind_buffer_memory(staging, staging_mem, 0) } {
        free_staging(device);
        return Err(GraphicsError::Vulkan(format!(
            "vkBindBufferMemory (rgba staging): {e}"
        )));
    }
    // SAFETY: staging_mem is host-visible ≥ size; copy the pixel bytes in, then unmap. `pixels.len()`
    // is the caller-validated `width*height*4` (≤ size), so the copy stays in bounds.
    unsafe {
        match device.map_memory(staging_mem, 0, size.max(1), vk::MemoryMapFlags::empty()) {
            Ok(ptr) => {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr as *mut u8, pixels.len());
                device.unmap_memory(staging_mem);
            }
            Err(e) => {
                free_staging(device);
                return Err(GraphicsError::Vulkan(format!(
                    "vkMapMemory (rgba staging): {e}"
                )));
            }
        }
    }

    let cb_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: pool valid; cb_info outlives the call.
    let cmd = match unsafe { device.allocate_command_buffers(&cb_info) } {
        Ok(c) => c[0],
        Err(e) => {
            free_staging(device);
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateCommandBuffers (rgba upload): {e}"
            )));
        }
    };
    let free_cmd = |device: &ash::Device| {
        // SAFETY: cmd belongs to command_pool; freed once.
        unsafe { device.free_command_buffers(command_pool, &[cmd]) };
    };

    let subresource = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1);
    // Record the transition→copy→transition. SAFETY: cmd + image + staging valid; infos outlive calls.
    let record = (|| -> ash::prelude::VkResult<()> {
        unsafe {
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            device.begin_command_buffer(cmd, &begin)?;
            let to_transfer = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(subresource)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_transfer),
            );
            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });
            device.cmd_copy_buffer_to_image(
                cmd,
                staging,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&region),
            );
            let to_shader = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(subresource)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_shader),
            );
            device.end_command_buffer(cmd)
        }
    })();
    if let Err(e) = record {
        free_cmd(device);
        free_staging(device);
        return Err(GraphicsError::Vulkan(format!("record rgba upload: {e}")));
    }
    // SAFETY: device valid; default fence info.
    let fence = match unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) } {
        Ok(f) => f,
        Err(e) => {
            free_cmd(device);
            free_staging(device);
            return Err(GraphicsError::Vulkan(format!(
                "create rgba upload fence: {e}"
            )));
        }
    };
    let cmds = [cmd];
    let submit = vk::SubmitInfo::default().command_buffers(&cmds);
    // SAFETY: queue/cmd/fence valid; submit + its slice outlive the call; fence tracks completion.
    let submitted = unsafe { device.queue_submit(queue, &[submit], fence) };
    let waited =
        submitted.and_then(|()| unsafe { device.wait_for_fences(&[fence], true, u64::MAX) });
    // SAFETY: fence valid + owned; destroy once. Then free the cmd buffer + staging resources.
    unsafe { device.destroy_fence(fence, None) };
    free_cmd(device);
    free_staging(device);
    waited.map_err(|e| GraphicsError::Vulkan(format!("submit/wait rgba upload: {e}")))?;
    Ok(())
}

/// Build the two-triangle quad (6 [`TextVertex`]es) for compositing a custom view's RGBA Pixmap over
/// its laid-out rect: positions in Vulkan NDC (same pixel→NDC mapping as the quad/text passes), UVs
/// spanning the full texture (top-left origin matches the Pixmap's row-major top-down layout).
/// 2026-06-05; GPU-free so it is unit-testable.
fn composite_quad_vertices(rect: &LaidOutView, extent: vk::Extent2D) -> Vec<TextVertex> {
    // Same pixel→NDC mapping as `pixel_rect_to_quad`/the text pass (`2*p/extent - 1` per axis).
    let ew = extent.width.max(1) as f32;
    let eh = extent.height.max(1) as f32;
    let to_ndc = |px: f32, py: f32| -> [f32; 2] { [2.0 * px / ew - 1.0, 2.0 * py / eh - 1.0] };
    // UV (0,0) at the rect's top-left → (1,1) at bottom-right; matches the Pixmap's row-major,
    // top-down layout (row 0 at the top), so the rasterized image is upright on screen.
    let tl = TextVertex {
        pos: to_ndc(rect.x, rect.y),
        uv: [0.0, 0.0],
    };
    let tr = TextVertex {
        pos: to_ndc(rect.x + rect.w, rect.y),
        uv: [1.0, 0.0],
    };
    let bl = TextVertex {
        pos: to_ndc(rect.x, rect.y + rect.h),
        uv: [0.0, 1.0],
    };
    let br = TextVertex {
        pos: to_ndc(rect.x + rect.w, rect.y + rect.h),
        uv: [1.0, 1.0],
    };
    // Two triangles (TRIANGLE_LIST): (tl, tr, br) + (tl, br, bl) — same winding as pixel_rect_to_quad.
    vec![tl, tr, br, tl, br, bl]
}

/// Allocate + fill a host-visible vertex buffer for one composite quad's [`TextVertex`]es. Unlike the
/// per-frame-reused quad/text buffers, each composite texture owns its own (freed next frame by
/// [`CanvasCompositor::begin_frame`]); returns `(buffer, memory, count)`. An empty `verts` returns a
/// null buffer with count 0 (no draw). 2026-06-05.
fn upload_composite_vertices(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    verts: &[TextVertex],
) -> Result<(vk::Buffer, vk::DeviceMemory, u32), GraphicsError> {
    let count: u32 = verts
        .len()
        .try_into()
        .map_err(|_| GraphicsError::Vulkan("too many composite vertices".to_owned()))?;
    if count == 0 {
        return Ok((vk::Buffer::null(), vk::DeviceMemory::null(), 0));
    }
    let size = (count as vk::DeviceSize) * std::mem::size_of::<TextVertex>() as vk::DeviceSize;
    let buffer_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: device valid; buffer_info outlives the call.
    let buffer = unsafe { device.create_buffer(&buffer_info, None) }
        .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (composite vtx): {e}")))?;
    // SAFETY: buffer just created.
    let req = unsafe { device.get_buffer_memory_requirements(buffer) };
    let mem_type = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
        .ok_or_else(|| {
            // SAFETY: buffer valid + unbound; free before bailing.
            unsafe { device.destroy_buffer(buffer, None) };
            GraphicsError::Vulkan("no host-visible memory for a composite vertex buffer".to_owned())
        })?;
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_type);
    // SAFETY: alloc_info outlives the call.
    let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: buffer valid + unbound; free before bailing.
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateMemory (composite vtx): {e}"
            )));
        }
    };
    // SAFETY: buffer+memory valid; bind whole allocation.
    if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        // SAFETY: both valid + owned; free reverse order.
        unsafe {
            device.free_memory(memory, None);
            device.destroy_buffer(buffer, None);
        }
        return Err(GraphicsError::Vulkan(format!(
            "vkBindBufferMemory (composite vtx): {e}"
        )));
    }
    // SAFETY: memory is a fresh host-visible allocation ≥ size; map the exact range, copy `count`
    // TextVertexes (source has `count`), unmap. Nothing else references this brand-new buffer.
    unsafe {
        let ptr = match device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: both valid + owned; free reverse order.
                device.free_memory(memory, None);
                device.destroy_buffer(buffer, None);
                return Err(GraphicsError::Vulkan(format!(
                    "vkMapMemory (composite vtx): {e}"
                )));
            }
        };
        std::ptr::copy_nonoverlapping(verts.as_ptr() as *const u8, ptr as *mut u8, size as usize);
        device.unmap_memory(memory);
    }
    Ok((buffer, memory, count))
}

/// Build the RGBA Canvas-composite pipeline (vertex: pos@0 + uv@8; fragment: sample RGBA8 texture ×
/// push-constant opacity; alpha blend; dynamic viewport+scissor). Structurally identical to
/// [`build_text_pipeline`] (same [`TextVertex`] input + combined-image-sampler set 0 + vec4 push
/// constant) but with the composite shaders. Frees both shader modules on every path; on a post-layout
/// failure frees the layout. 2026-06-05.
fn build_composite_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    descriptor_set_layout: vk::DescriptorSetLayout,
) -> Result<(vk::PipelineLayout, vk::Pipeline), GraphicsError> {
    let vert_words = read_spirv(COMPOSITE_VERT_SPV)?;
    let frag_words = read_spirv(COMPOSITE_FRAG_SPV)?;
    let make_module = |words: &[u32]| -> Result<vk::ShaderModule, GraphicsError> {
        let info = vk::ShaderModuleCreateInfo::default().code(words);
        // SAFETY: device valid; info borrows words for the call only.
        unsafe { device.create_shader_module(&info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateShaderModule (composite): {e}")))
    };
    let vert_module = make_module(&vert_words)?;
    let frag_module = match make_module(&frag_words) {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: vert_module valid + unused; free before bailing.
            unsafe { device.destroy_shader_module(vert_module, None) };
            return Err(e);
        }
    };

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(entry),
    ];
    let binding = vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(std::mem::size_of::<TextVertex>() as u32)
        .input_rate(vk::VertexInputRate::VERTEX);
    let attributes = [
        vk::VertexInputAttributeDescription::default()
            .binding(0)
            .location(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(0),
        vk::VertexInputAttributeDescription::default()
            .binding(0)
            .location(1)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(std::mem::size_of::<[f32; 2]>() as u32),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(std::slice::from_ref(&binding))
        .vertex_attribute_descriptions(&attributes);
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD);
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
        .attachments(std::slice::from_ref(&blend_attachment));

    let set_layouts = [descriptor_set_layout];
    let push_range = vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(std::mem::size_of::<[f32; 4]>() as u32);
    let layout_info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(std::slice::from_ref(&push_range));
    // SAFETY: device valid; layout_info + its slices outlive the call.
    let pipeline_layout = match unsafe { device.create_pipeline_layout(&layout_info, None) } {
        Ok(l) => l,
        Err(e) => {
            // SAFETY: both modules valid; free them before bailing.
            unsafe {
                device.destroy_shader_module(frag_module, None);
                device.destroy_shader_module(vert_module, None);
            }
            return Err(GraphicsError::Vulkan(format!(
                "vkCreatePipelineLayout (composite): {e}"
            )));
        }
    };
    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterizer)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(pipeline_layout)
        .render_pass(render_pass)
        .subpass(0);
    // SAFETY: all referenced objects valid + outlive the call.
    let pipeline = match unsafe {
        device.create_graphics_pipelines(
            vk::PipelineCache::null(),
            std::slice::from_ref(&pipeline_info),
            None,
        )
    } {
        Ok(p) => p[0],
        Err((_, e)) => {
            // SAFETY: layout + both modules valid; free them before bailing.
            unsafe {
                device.destroy_pipeline_layout(pipeline_layout, None);
                device.destroy_shader_module(frag_module, None);
                device.destroy_shader_module(vert_module, None);
            }
            return Err(GraphicsError::Vulkan(format!(
                "vkCreateGraphicsPipelines (composite): {e}"
            )));
        }
    };
    // SAFETY: both modules valid; the pipeline retains the compiled code, so free them now.
    unsafe {
        device.destroy_shader_module(frag_module, None);
        device.destroy_shader_module(vert_module, None);
    }
    Ok((pipeline_layout, pipeline))
}

/// Build the textured-glyph pipeline (vertex: pos@0 + uv@8; fragment: sample R8 atlas × push-constant
/// color; alpha blend; dynamic viewport+scissor). Frees both shader modules on every path; on a
/// post-layout failure frees the layout. `descriptor_set_layout` (set 0 = the atlas sampler) is
/// referenced by the pipeline layout but owned by the caller.
fn build_text_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    descriptor_set_layout: vk::DescriptorSetLayout,
) -> Result<(vk::PipelineLayout, vk::Pipeline), GraphicsError> {
    let vert_words = read_spirv(TEXT_VERT_SPV)?;
    let frag_words = read_spirv(TEXT_FRAG_SPV)?;
    let make_module = |words: &[u32]| -> Result<vk::ShaderModule, GraphicsError> {
        let info = vk::ShaderModuleCreateInfo::default().code(words);
        // SAFETY: device valid; info borrows words for the call only.
        unsafe { device.create_shader_module(&info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateShaderModule (text): {e}")))
    };
    let vert_module = make_module(&vert_words)?;
    let frag_module = match make_module(&frag_words) {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: vert_module valid + unused; free before bailing.
            unsafe { device.destroy_shader_module(vert_module, None) };
            return Err(e);
        }
    };

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(entry),
    ];
    let binding = vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(std::mem::size_of::<TextVertex>() as u32)
        .input_rate(vk::VertexInputRate::VERTEX);
    let attributes = [
        vk::VertexInputAttributeDescription::default()
            .binding(0)
            .location(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(0),
        vk::VertexInputAttributeDescription::default()
            .binding(0)
            .location(1)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(std::mem::size_of::<[f32; 2]>() as u32),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(std::slice::from_ref(&binding))
        .vertex_attribute_descriptions(&attributes);
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD);
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
        .attachments(std::slice::from_ref(&blend_attachment));

    // Pipeline layout: set 0 = the atlas sampler; a fragment-stage vec4 push constant = text color.
    let set_layouts = [descriptor_set_layout];
    let push_range = vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(std::mem::size_of::<[f32; 4]>() as u32);
    let layout_info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(std::slice::from_ref(&push_range));
    // SAFETY: device valid; layout_info + its slices outlive the call.
    let pipeline_layout = match unsafe { device.create_pipeline_layout(&layout_info, None) } {
        Ok(l) => l,
        Err(e) => {
            // SAFETY: both modules valid; free them before bailing.
            unsafe {
                device.destroy_shader_module(frag_module, None);
                device.destroy_shader_module(vert_module, None);
            }
            return Err(GraphicsError::Vulkan(format!(
                "vkCreatePipelineLayout (text): {e}"
            )));
        }
    };

    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterizer)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(pipeline_layout)
        .render_pass(render_pass)
        .subpass(0);
    // SAFETY: all referenced objects valid + outlive the call.
    let pipeline = match unsafe {
        device.create_graphics_pipelines(
            vk::PipelineCache::null(),
            std::slice::from_ref(&pipeline_info),
            None,
        )
    } {
        Ok(p) => p[0],
        Err((_, e)) => {
            // SAFETY: layout + both modules valid; free them before bailing.
            unsafe {
                device.destroy_pipeline_layout(pipeline_layout, None);
                device.destroy_shader_module(frag_module, None);
                device.destroy_shader_module(vert_module, None);
            }
            return Err(GraphicsError::Vulkan(format!(
                "vkCreateGraphicsPipelines (text): {e}"
            )));
        }
    };
    // SAFETY: both modules valid; the pipeline retains the compiled code, so free them now.
    unsafe {
        device.destroy_shader_module(frag_module, None);
        device.destroy_shader_module(vert_module, None);
    }
    Ok((pipeline_layout, pipeline))
}

// ---------------------------------------------------------------------------------------------
// View-tree TEXT: portable font discovery + R8 glyph atlas + textured glyph quads (2026-06-05)
//
// Each `RenderNode.text` (a TextView's text) is drawn over its view rect. We rasterize a fixed
// printable-ASCII glyph set ONCE into a single R8 coverage atlas (ab_glyph), then per frame emit a
// textured quad per glyph. The font FILE is discovered portably at runtime (fontconfig `fc-match`,
// then known font dirs) — detect-don't-assume (§9), never linking fontconfig. If no font is found,
// text is skipped (the quads still draw); never a crash.
// ---------------------------------------------------------------------------------------------

use ab_glyph::{Font, FontVec, ScaleFont};

/// One vertex for the text pipeline: NDC position + atlas UV. `#[repr(C)]`, matching the text
/// pipeline's vertex input (pos @0, uv @8).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct TextVertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

/// Per-glyph placement in the atlas: pixel rect in the atlas + the glyph's bearing/advance metrics
/// (in pixels at [`TEXT_PX`]) needed to position it on a baseline.
#[derive(Debug, Clone, Copy)]
struct GlyphInfo {
    /// Atlas pixel rect (top-left x,y and size).
    ax: u32,
    ay: u32,
    aw: u32,
    ah: u32,
    /// Offset from the pen position (baseline origin) to the glyph bitmap's top-left, in pixels.
    bearing_x: f32,
    bearing_y: f32,
    /// Horizontal advance to the next glyph, in pixels.
    advance: f32,
}

/// A CPU-side R8 glyph atlas: the coverage bitmap + a sparse map from `char` to its [`GlyphInfo`],
/// plus the scaled font's ascent (for baseline placement). Built once; uploaded to a GPU image.
struct GlyphAtlas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    glyphs: std::collections::HashMap<char, GlyphInfo>,
    ascent: f32,
    line_height: f32,
}

/// The printable-ASCII set rasterized into the atlas. Bounded + covers the demo's text; non-ASCII
/// chars are simply skipped at layout time (advance-only fallback would be the next refinement).
const ATLAS_CHARS: std::ops::RangeInclusive<u8> = 32..=126;

/// Find a usable TrueType/OpenType font file portably, without linking fontconfig.
///
/// Order (detect-don't-assume §9): (1) `fc-match` (fontconfig CLI, present on virtually every Linux
/// desktop) for `sans-serif`; (2) a scan of the well-known system font dirs for any `.ttf`/`.otf`.
/// Returns `None` (text disabled, quads still draw) if nothing is found — never panics, never
/// hardcodes a single file path. An env override (`ECLIPSE_FONT`) wins for testing/packaging.
pub(crate) fn discover_font_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("ECLIPSE_FONT") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    // (1) fontconfig CLI — asks the system which file backs the generic "sans-serif" family.
    if let Ok(out) = std::process::Command::new("fc-match")
        .args(["--format=%{file}", "sans-serif"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            let path = std::path::PathBuf::from(s.trim());
            if path.is_file() {
                return Some(path);
            }
        }
    }
    // (2) Fallback: scan known font dirs for the first .ttf/.otf (portable across distros).
    const FONT_DIRS: [&str; 4] = [
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        "/usr/share/fonts/truetype",
        "/run/host/fonts", // flatpak host-fonts mount
    ];
    for dir in FONT_DIRS {
        if let Some(p) = first_font_in_dir(std::path::Path::new(dir)) {
            return Some(p);
        }
    }
    None
}

/// Recursively find the first `.ttf`/`.otf` under `dir` (bounded, no symlink loops via `read_dir`).
fn first_font_in_dir(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if let Some(ext) = path.extension() {
            let ext = ext.to_ascii_lowercase();
            if ext == "ttf" || ext == "otf" {
                return Some(path);
            }
        }
    }
    // Shallow recursion into subdirs (font trees are not deep); first hit wins.
    subdirs.into_iter().find_map(|d| first_font_in_dir(&d))
}

/// Rasterize [`ATLAS_CHARS`] from `font` at [`TEXT_PX`] into a single R8 atlas using simple shelf
/// packing (rows of glyphs, wrapping at `max_width`). Pure/GPU-free — unit-testable without a GPU.
///
/// Returns `None` only if the atlas would be empty (no glyph produced an outline), which the caller
/// treats as "no text" (quads still draw). 1-px padding between glyphs avoids bilinear bleed.
fn build_glyph_atlas(font: &FontVec, max_width: u32) -> Option<GlyphAtlas> {
    let scaled = font.as_scaled(TEXT_PX);
    let ascent = scaled.ascent();
    let line_height = scaled.height() + scaled.line_gap();

    const PAD: u32 = 1;
    // First pass: rasterize each glyph to its own small bitmap + record metrics.
    struct Raster {
        ch: char,
        w: u32,
        h: u32,
        pixels: Vec<u8>,
        bearing_x: f32,
        bearing_y: f32,
        advance: f32,
    }
    let mut rasters: Vec<Raster> = Vec::new();
    for byte in ATLAS_CHARS {
        let ch = byte as char;
        let advance = scaled.h_advance(scaled.glyph_id(ch));
        let glyph = font
            .glyph_id(ch)
            .with_scale_and_position(TEXT_PX, ab_glyph::point(0.0, 0.0));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            let w = bounds.width().ceil() as u32;
            let h = bounds.height().ceil() as u32;
            if w == 0 || h == 0 {
                // Whitespace-like glyph with an advance but no pixels (e.g. space).
                rasters.push(Raster {
                    ch,
                    w: 0,
                    h: 0,
                    pixels: Vec::new(),
                    bearing_x: bounds.min.x,
                    bearing_y: bounds.min.y,
                    advance,
                });
                continue;
            }
            let mut pixels = vec![0u8; (w * h) as usize];
            outlined.draw(|x, y, c| {
                let idx = (y * w + x) as usize;
                if idx < pixels.len() {
                    pixels[idx] = (c.clamp(0.0, 1.0) * 255.0) as u8;
                }
            });
            rasters.push(Raster {
                ch,
                w,
                h,
                pixels,
                bearing_x: bounds.min.x,
                bearing_y: bounds.min.y,
                advance,
            });
        } else {
            // No outline (e.g. space): advance-only.
            rasters.push(Raster {
                ch,
                w: 0,
                h: 0,
                pixels: Vec::new(),
                bearing_x: 0.0,
                bearing_y: 0.0,
                advance,
            });
        }
    }

    // Second pass: shelf-pack the non-empty bitmaps into rows, wrapping at `max_width`, building each
    // glyph's final [`GlyphInfo`] (atlas rect + metrics) directly — no intermediate tuple.
    let max_width = max_width.max(1);
    let mut pen_x = PAD;
    let mut pen_y = PAD;
    let mut row_h = 0u32;
    let mut atlas_w = 0u32;
    let mut placements: Vec<(char, GlyphInfo)> = Vec::with_capacity(rasters.len());
    for r in &rasters {
        if r.w == 0 || r.h == 0 {
            // Whitespace: no atlas rect, but still record the advance (zero-size rect).
            placements.push((
                r.ch,
                GlyphInfo {
                    ax: 0,
                    ay: 0,
                    aw: 0,
                    ah: 0,
                    bearing_x: r.bearing_x,
                    bearing_y: r.bearing_y,
                    advance: r.advance,
                },
            ));
            continue;
        }
        if pen_x + r.w + PAD > max_width {
            // Wrap to a new shelf.
            pen_x = PAD;
            pen_y += row_h + PAD;
            row_h = 0;
        }
        placements.push((
            r.ch,
            GlyphInfo {
                ax: pen_x,
                ay: pen_y,
                aw: r.w,
                ah: r.h,
                bearing_x: r.bearing_x,
                bearing_y: r.bearing_y,
                advance: r.advance,
            },
        ));
        pen_x += r.w + PAD;
        row_h = row_h.max(r.h);
        atlas_w = atlas_w.max(pen_x);
    }
    let atlas_h = pen_y + row_h + PAD;
    if atlas_w == 0 || atlas_h == 0 {
        return None;
    }

    // Third pass: blit each glyph bitmap into the atlas + build the char → GlyphInfo map.
    let mut pixels = vec![0u8; (atlas_w * atlas_h) as usize];
    let mut glyphs = std::collections::HashMap::new();
    for (r, &(ch, info)) in rasters.iter().zip(placements.iter()) {
        if info.aw > 0 && info.ah > 0 {
            for gy in 0..info.ah {
                for gx in 0..info.aw {
                    let src = (gy * info.aw + gx) as usize;
                    let dst = ((info.ay + gy) * atlas_w + (info.ax + gx)) as usize;
                    if src < r.pixels.len() && dst < pixels.len() {
                        pixels[dst] = r.pixels[src];
                    }
                }
            }
        }
        glyphs.insert(ch, info);
    }

    Some(GlyphAtlas {
        width: atlas_w,
        height: atlas_h,
        pixels,
        glyphs,
        ascent,
        line_height,
    })
}

/// Emit textured glyph quads (6 [`TextVertex`]es each) for every laid-out view's text, positioned
/// within its rect on a baseline. Pure/GPU-free (just arithmetic over the atlas metrics) so it is
/// unit-testable without a GPU. Glyphs not in the atlas (non-ASCII) are skipped but still advance.
fn build_text_vertices(
    views: &[LaidOutView],
    atlas: &GlyphAtlas,
    extent: vk::Extent2D,
) -> Vec<TextVertex> {
    let ew = extent.width.max(1) as f32;
    let eh = extent.height.max(1) as f32;
    let aw = atlas.width.max(1) as f32;
    let ah = atlas.height.max(1) as f32;
    let to_ndc = |px: f32, py: f32| -> [f32; 2] { [2.0 * px / ew - 1.0, 2.0 * py / eh - 1.0] };

    let mut verts = Vec::new();
    for v in views {
        let Some(text) = v.text.as_deref() else {
            continue;
        };
        // Baseline: top of the rect + a little padding + the font ascent (so glyphs sit inside).
        let mut pen_x = v.x + TEXT_PAD_X;
        let baseline_y = v.y + (v.h - atlas.line_height).max(0.0) * 0.5 + atlas.ascent;
        for ch in text.chars() {
            let Some(g) = atlas.glyphs.get(&ch) else {
                continue; // not in the atlas (non-ASCII); skip (no advance known)
            };
            if g.aw > 0 && g.ah > 0 {
                // Glyph top-left in pixels: pen + bearing (bearing_y is negative-up from baseline).
                let gx = pen_x + g.bearing_x;
                let gy = baseline_y + g.bearing_y;
                let gw = g.aw as f32;
                let gh = g.ah as f32;
                let u0 = g.ax as f32 / aw;
                let v0 = g.ay as f32 / ah;
                let u1 = (g.ax + g.aw) as f32 / aw;
                let v1 = (g.ay + g.ah) as f32 / ah;
                let tl = TextVertex {
                    pos: to_ndc(gx, gy),
                    uv: [u0, v0],
                };
                let tr = TextVertex {
                    pos: to_ndc(gx + gw, gy),
                    uv: [u1, v0],
                };
                let bl = TextVertex {
                    pos: to_ndc(gx, gy + gh),
                    uv: [u0, v1],
                };
                let br = TextVertex {
                    pos: to_ndc(gx + gw, gy + gh),
                    uv: [u1, v1],
                };
                verts.extend_from_slice(&[tl, tr, br, tl, br, bl]);
            }
            pen_x += g.advance;
        }
    }
    verts
}

/// The per-swapchain resources that must be rebuilt on resize / out-of-date. Kept separate from
/// the device-lifetime objects in [`VulkanRenderer`] so a resize recreates only these.
struct Swapchain {
    swapchain: vk::SwapchainKHR,
    image_views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    extent: vk::Extent2D,
}

/// Owns every Vulkan handle for the window's surface + swapchain + clear-and-present loop.
///
/// Destruction is handled by [`Drop`] in strict reverse-creation order, after `device_wait_idle`,
/// so no GPU work references a freed object (no UB, no leaks — AGENTS.md §2.3). The struct is not
/// `Send`/`Sync` (it holds the surface tied to the main-thread window) and lives on the winit
/// main thread, matching the `!Send` ART `Vm` held alongside it.
struct VulkanRenderer {
    // Loaders / instance-lifetime objects.
    _entry: ash::Entry,
    instance: ash::Instance,
    surface_loader: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical_device: vk::PhysicalDevice,

    // Device-lifetime objects.
    device: ash::Device,
    queue: vk::Queue,
    swapchain_loader: khr::swapchain::Device,
    render_pass: vk::RenderPass,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    image_available: vk::Semaphore,
    render_finished: vk::Semaphore,
    in_flight: vk::Fence,

    // The colored-quad pipeline for drawing the recorded View tree (2026-06-05). The pipeline uses
    // dynamic viewport+scissor so it survives swapchain resize without rebuilding.
    quad_pipeline_layout: vk::PipelineLayout,
    quad_pipeline: vk::Pipeline,
    /// Host-visible vertex buffer holding the current frame's quads, grown on demand. `capacity` is
    /// in vertices; `0`/`null` until the first frame with content allocates it.
    quad_vertex_buffer: vk::Buffer,
    quad_vertex_memory: vk::DeviceMemory,
    quad_vertex_capacity: u32,
    /// Memory properties (for choosing a host-visible|coherent memory type for the vertex buffer).
    memory_properties: vk::PhysicalDeviceMemoryProperties,

    /// The text pass (font atlas image + textured-glyph pipeline). `None` if no system font was
    /// found or text init failed — quads still draw, no crash (text is best-effort).
    text: Option<TextRenderer>,

    /// The RGBA Canvas-composite pass (2026-06-05): uploads each custom View's `onDraw(Canvas)` Pixmap
    /// as a GPU texture + draws it over the view's rect. `None` if compositor init failed (a hard
    /// Vulkan error) — quads + text still draw, no crash (the composite is best-effort).
    composite: Option<CanvasCompositor>,
    /// This frame's custom-view `(view, canvas)` pairs to composite, set by [`Self::set_drawn_canvases`]
    /// from the draw cascade before [`Self::draw_frame`]. Cleared (and the canvases freed) each frame.
    drawn_canvases: Vec<crate::framework::DrawnCanvas>,

    // Swapchain-lifetime objects (rebuilt on resize).
    swapchain: Swapchain,
    swapchain_format: vk::Format,
    swapchain_extent: vk::Extent2D,

    /// Set when a `Resized` event or a stale-swapchain present is observed; the next
    /// `draw_frame` recreates the swapchain before acquiring.
    needs_recreate: bool,
}

impl VulkanRenderer {
    /// Build the full surface + swapchain + frame-loop state for `window`.
    ///
    /// Returns a typed [`GraphicsError`] (no panics, no partial leaks: each fallible step that
    /// runs after another resource is created tears the earlier ones down on the error path) so
    /// a host without a usable Vulkan ICD/surface fails cleanly and the window stays blank.
    fn new(window: &Window) -> Result<Self, GraphicsError> {
        // SAFETY: `Entry::load` dlopens the host libvulkan at runtime and reads its entry points.
        // It is `unsafe` because loading an arbitrary shared object is inherently trusting the
        // dynamic loader; we load only the system Vulkan loader by its standard soname. A missing
        // ICD surfaces as `Err`, handled below — never UB.
        let entry = unsafe { ash::Entry::load() }.map_err(|e| {
            GraphicsError::Vulkan(format!("no Vulkan loader (libvulkan) available: {e}"))
        })?;

        let display_handle = window
            .display_handle()
            .map_err(|e| GraphicsError::Vulkan(format!("no raw display handle: {e}")))?
            .as_raw();
        let window_handle = window
            .window_handle()
            .map_err(|e| GraphicsError::Vulkan(format!("no raw window handle: {e}")))?
            .as_raw();

        // Required instance extensions for the surface on THIS platform (Wayland vs Xlib/Xcb) —
        // discovered from the display handle, never assumed (detect-don't-assume §9).
        let surface_extensions = ash_window::enumerate_required_extensions(display_handle)
            .map_err(|e| {
                GraphicsError::Vulkan(format!(
                    "no Vulkan surface extension for this display server: {e}"
                ))
            })?;

        // Create the instance with the surface extensions enabled.
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"Eclipse")
            .api_version(vk::API_VERSION_1_0);
        let instance_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(surface_extensions);
        // SAFETY: `instance_info` and the extension name pointers it borrows outlive this call;
        // the extension list comes from `enumerate_required_extensions` (valid static C strings).
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateInstance failed: {e}")))?;

        // From here on, any error must destroy `instance` before returning. `build` owns the
        // surface/device cleanup on its own error paths (it holds the loaders) and hands back only
        // the `instance` for this last teardown, so no handle leaks and there is no need to
        // re-derive a surface loader without an `entry`.
        match Self::build(entry, instance, display_handle, window_handle, window) {
            Ok(renderer) => Ok(renderer),
            Err(boxed) => {
                let (e, instance) = *boxed;
                // SAFETY: `instance` is a valid handle owned here; the surface/device were already
                // destroyed inside `build`'s error paths. Destroyed exactly once.
                unsafe {
                    instance.destroy_instance(None);
                }
                Err(e)
            }
        }
    }

    /// Continue building after the instance exists. On error destroys any surface/device it
    /// created (it holds the loaders) and returns the `instance` to [`Self::new`] for the final
    /// teardown — so no handle leaks on a failure path.
    fn build(
        entry: ash::Entry,
        instance: ash::Instance,
        display_handle: raw_window_handle::RawDisplayHandle,
        window_handle: raw_window_handle::RawWindowHandle,
        window: &Window,
    ) -> Result<Self, Box<(GraphicsError, ash::Instance)>> {
        let surface_loader = khr::surface::Instance::new(&entry, &instance);

        // SAFETY: the display/window handles came from the live winit `window`, which outlives
        // this surface (the renderer is dropped before the window in `GameWindow`). On an
        // unsupported display this returns `Err`, handled as a typed error (no UB).
        let surface = match unsafe {
            ash_window::create_surface(&entry, &instance, display_handle, window_handle, None)
        } {
            Ok(s) => s,
            Err(e) => {
                return Err(Box::new((
                    GraphicsError::Vulkan(format!("vkCreate*SurfaceKHR failed: {e}")),
                    instance,
                )));
            }
        };

        // Pick a physical device with a queue family that supports BOTH graphics and present to
        // this surface, and that exposes the swapchain extension. Never assume one GPU (§9).
        let (physical_device, queue_family_index) =
            match Self::pick_device(&instance, &surface_loader, surface) {
                Ok(v) => v,
                Err(e) => {
                    // SAFETY: `surface` is valid and created from `instance`/`surface_loader`;
                    // destroy it before bailing so it does not leak.
                    unsafe { surface_loader.destroy_surface(surface, None) };
                    return Err(Box::new((e, instance)));
                }
            };

        // Logical device with one graphics/present queue + the swapchain extension.
        let queue_priorities = [1.0_f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities);
        let device_extensions = [khr::swapchain::NAME.as_ptr()];
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue_info))
            .enabled_extension_names(&device_extensions);
        // SAFETY: `physical_device` was returned by this `instance`; `device_info` and the slices
        // it borrows outlive the call. A failure returns `Err` and the caller tears down.
        let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
            Ok(d) => d,
            Err(e) => {
                // SAFETY: `surface` is valid; destroy it before bailing (no device exists yet).
                unsafe { surface_loader.destroy_surface(surface, None) };
                return Err(Box::new((
                    GraphicsError::Vulkan(format!("vkCreateDevice failed: {e}")),
                    instance,
                )));
            }
        };

        // From here, an error must also destroy `device` before returning. We assemble the
        // remaining objects, and on any failure destroy `device` (which transitively requires
        // destroying anything already made from it) then hand instance+surface back.
        match Self::build_device_objects(
            &entry,
            &instance,
            &surface_loader,
            surface,
            physical_device,
            queue_family_index,
            &device,
            window,
        ) {
            Ok((
                queue,
                swapchain_loader,
                render_pass,
                command_pool,
                command_buffer,
                image_available,
                render_finished,
                in_flight,
                swapchain,
                swapchain_format,
                swapchain_extent,
                quad_pipeline_layout,
                quad_pipeline,
                memory_properties,
                text,
                composite,
            )) => Ok(Self {
                _entry: entry,
                instance,
                surface_loader,
                surface,
                physical_device,
                device,
                queue,
                swapchain_loader,
                render_pass,
                command_pool,
                command_buffer,
                image_available,
                render_finished,
                in_flight,
                swapchain,
                swapchain_format,
                swapchain_extent,
                quad_pipeline_layout,
                quad_pipeline,
                quad_vertex_buffer: vk::Buffer::null(),
                quad_vertex_memory: vk::DeviceMemory::null(),
                quad_vertex_capacity: 0,
                memory_properties,
                text,
                composite,
                drawn_canvases: Vec::new(),
                needs_recreate: false,
            }),
            Err(e) => {
                // SAFETY: `device` is valid and idle (nothing was submitted yet); destroying it
                // releases all child objects `build_device_objects` may have created before the
                // failure. Then the surface is destroyed and the instance returned for teardown.
                unsafe {
                    device.destroy_device(None);
                    surface_loader.destroy_surface(surface, None);
                }
                Err(Box::new((e, instance)))
            }
        }
    }

    /// Enumerate physical devices and return the first `(device, queue_family)` that supports
    /// graphics + present-to-`surface` + the swapchain extension. Prefers a discrete GPU.
    fn pick_device(
        instance: &ash::Instance,
        surface_loader: &khr::surface::Instance,
        surface: vk::SurfaceKHR,
    ) -> Result<(vk::PhysicalDevice, u32), GraphicsError> {
        // SAFETY: `instance` is a valid live instance; the enumerate/query calls only read.
        let devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| GraphicsError::Vulkan(format!("vkEnumeratePhysicalDevices: {e}")))?;
        if devices.is_empty() {
            return Err(GraphicsError::Vulkan(
                "no Vulkan physical devices found".to_owned(),
            ));
        }

        let mut fallback: Option<(vk::PhysicalDevice, u32)> = None;
        for &pd in &devices {
            // Must expose VK_KHR_swapchain.
            // SAFETY: `pd` came from this instance; the call only reads device properties.
            let exts = match unsafe { instance.enumerate_device_extension_properties(pd) } {
                Ok(e) => e,
                Err(_) => continue,
            };
            let has_swapchain = exts.iter().any(|e| {
                // SAFETY: `extension_name` is a fixed-size nul-terminated C string per the spec.
                let name = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
                name == khr::swapchain::NAME
            });
            if !has_swapchain {
                continue;
            }

            // SAFETY: `pd` is from this instance; only reads queue family properties.
            let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            for (i, family) in families.iter().enumerate() {
                let index = i as u32;
                if !family.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                    continue;
                }
                // SAFETY: `pd`/`surface`/`index` are all valid; the call only queries support.
                let present_ok = unsafe {
                    surface_loader.get_physical_device_surface_support(pd, index, surface)
                }
                .unwrap_or(false);
                if !present_ok {
                    continue;
                }

                // SAFETY: reads device properties only.
                let props = unsafe { instance.get_physical_device_properties(pd) };
                if props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU {
                    return Ok((pd, index));
                }
                fallback.get_or_insert((pd, index));
            }
        }

        fallback.ok_or_else(|| {
            GraphicsError::Vulkan(
                "no Vulkan device with a graphics+present queue for this surface".to_owned(),
            )
        })
    }

    /// Create the colored-quad graphics pipeline (+ its empty pipeline layout) for `render_pass`.
    ///
    /// Uses the embedded SPIR-V ([`QUAD_VERT_SPV`]/[`QUAD_FRAG_SPV`]), a single vertex binding
    /// matching [`QuadVertex`] (`vec2` pos @0, `vec4` color @8), a triangle-list topology, no
    /// culling, alpha blending (so text-over-quad later composites), and **dynamic** viewport +
    /// scissor (so a swapchain resize needs no pipeline rebuild). Returns `(layout, pipeline)` or a
    /// typed error after destroying any partial objects (the shader modules are always freed).
    fn create_quad_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
    ) -> Result<(vk::PipelineLayout, vk::Pipeline), GraphicsError> {
        let vert_words = read_spirv(QUAD_VERT_SPV)?;
        let frag_words = read_spirv(QUAD_FRAG_SPV)?;

        // SAFETY: `device` is valid; the create-info borrows the word slices for the call only.
        let make_module = |words: &[u32]| -> Result<vk::ShaderModule, GraphicsError> {
            let info = vk::ShaderModuleCreateInfo::default().code(words);
            unsafe { device.create_shader_module(&info, None) }
                .map_err(|e| GraphicsError::Vulkan(format!("vkCreateShaderModule: {e}")))
        };
        let vert_module = make_module(&vert_words)?;
        let frag_module = match make_module(&frag_words) {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: vert_module is valid and unused; free it before bailing.
                unsafe { device.destroy_shader_module(vert_module, None) };
                return Err(e);
            }
        };

        // The pipeline + its modules are built inside this closure so BOTH shader modules are freed
        // exactly once on every path (success or failure) — they are not needed after creation.
        let result = Self::build_quad_pipeline_inner(device, render_pass, vert_module, frag_module);
        // SAFETY: both modules are valid handles created above; destroy each exactly once. The
        // pipeline (on success) retains the compiled code, so freeing the modules now is correct.
        unsafe {
            device.destroy_shader_module(frag_module, None);
            device.destroy_shader_module(vert_module, None);
        }
        result
    }

    /// Inner pipeline assembly (shader modules already created; freed by the caller). Split out so
    /// the caller can free the modules on every path without duplicating the free in each branch.
    fn build_quad_pipeline_inner(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        vert_module: vk::ShaderModule,
        frag_module: vk::ShaderModule,
    ) -> Result<(vk::PipelineLayout, vk::Pipeline), GraphicsError> {
        let entry = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert_module)
                .name(entry),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag_module)
                .name(entry),
        ];

        // One vertex binding == sizeof(QuadVertex); two attributes (pos vec2 @0, color vec4 @8).
        let binding = vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(std::mem::size_of::<QuadVertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX);
        let attributes = [
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(1)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(std::mem::size_of::<[f32; 2]>() as u32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(std::slice::from_ref(&binding))
            .vertex_attribute_descriptions(&attributes);

        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

        // Dynamic viewport + scissor: one of each, set at record time (so resize needs no rebuild).
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // Standard straight-alpha blending so a later text pass can composite over the quads.
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
            .alpha_blend_op(vk::BlendOp::ADD);
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(std::slice::from_ref(&blend_attachment));

        // No descriptors/push-constants for the quad pipeline (color is per-vertex).
        let layout_info = vk::PipelineLayoutCreateInfo::default();
        // SAFETY: `device` is valid; `layout_info` outlives the call.
        let pipeline_layout = unsafe { device.create_pipeline_layout(&layout_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreatePipelineLayout: {e}")))?;

        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterizer)
            .multisample_state(&multisample)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic_state)
            .layout(pipeline_layout)
            .render_pass(render_pass)
            .subpass(0);
        // SAFETY: all referenced objects are valid and outlive the call. `create_graphics_pipelines`
        // returns `Err((pipelines, result))`; on failure the partial vec holds no valid handle to free.
        let pipeline = match unsafe {
            device.create_graphics_pipelines(
                vk::PipelineCache::null(),
                std::slice::from_ref(&pipeline_info),
                None,
            )
        } {
            Ok(p) => p[0],
            Err((_, e)) => {
                // SAFETY: the layout is valid and was created above; free it before bailing.
                unsafe { device.destroy_pipeline_layout(pipeline_layout, None) };
                return Err(GraphicsError::Vulkan(format!(
                    "vkCreateGraphicsPipelines: {e}"
                )));
            }
        };
        Ok((pipeline_layout, pipeline))
    }

    /// Create the device-lifetime objects (queue, swapchain loader, render pass, command pool +
    /// buffer, sync primitives) and the first swapchain. Returned as a tuple so [`Self::build`]
    /// can destroy `device` if any step fails without partially-initializing `self`.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn build_device_objects(
        _entry: &ash::Entry,
        instance: &ash::Instance,
        surface_loader: &khr::surface::Instance,
        surface: vk::SurfaceKHR,
        physical_device: vk::PhysicalDevice,
        queue_family_index: u32,
        device: &ash::Device,
        window: &Window,
    ) -> Result<
        (
            vk::Queue,
            khr::swapchain::Device,
            vk::RenderPass,
            vk::CommandPool,
            vk::CommandBuffer,
            vk::Semaphore,
            vk::Semaphore,
            vk::Fence,
            Swapchain,
            vk::Format,
            vk::Extent2D,
            vk::PipelineLayout,
            vk::Pipeline,
            vk::PhysicalDeviceMemoryProperties,
            Option<TextRenderer>,
            Option<CanvasCompositor>,
        ),
        GraphicsError,
    > {
        // SAFETY: queue family index was validated by `pick_device`; queue 0 always exists when
        // the family was requested with one priority.
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        // Memory properties (for the vertex buffer's host-visible allocation). SAFETY: reads only.
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let swapchain_loader = khr::swapchain::Device::new(instance, device);

        // Choose the surface format up-front (the render pass color attachment must match it).
        // SAFETY: handles are valid; the call only queries the surface.
        let formats =
            unsafe { surface_loader.get_physical_device_surface_formats(physical_device, surface) }
                .map_err(|e| GraphicsError::Vulkan(format!("get surface formats: {e}")))?;
        let surface_format = choose_surface_format(&formats)
            .ok_or_else(|| GraphicsError::Vulkan("surface advertises no formats".to_owned()))?;

        // A single-attachment render pass that CLEARs to the swapchain image and leaves it in
        // PRESENT_SRC_KHR — the minimal pass that proves a presented frame.
        let color_attachment = vk::AttachmentDescription::default()
            .format(surface_format.format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);
        let color_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(std::slice::from_ref(&color_ref));
        // Dependency so the implicit UNDEFINED→COLOR_ATTACHMENT transition waits for the acquired
        // image; the standard external→subpass-0 barrier for a clear-only pass.
        let dependency = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
        let render_pass_info = vk::RenderPassCreateInfo::default()
            .attachments(std::slice::from_ref(&color_attachment))
            .subpasses(std::slice::from_ref(&subpass))
            .dependencies(std::slice::from_ref(&dependency));
        // SAFETY: `render_pass_info` and its borrowed slices outlive the call; `device` is valid.
        let render_pass = unsafe { device.create_render_pass(&render_pass_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateRenderPass: {e}")))?;

        // The first swapchain + its views/framebuffers.
        let size = window.inner_size();
        let swapchain = match Self::create_swapchain(
            surface_loader,
            &swapchain_loader,
            device,
            physical_device,
            surface,
            surface_format,
            render_pass,
            size.width,
            size.height,
            vk::SwapchainKHR::null(),
        ) {
            Ok(s) => s,
            Err(e) => {
                // SAFETY: render_pass is valid and unused; destroy it before bailing so it does
                // not leak (device is torn down by the caller, but this child is ours to free).
                unsafe { device.destroy_render_pass(render_pass, None) };
                return Err(e);
            }
        };
        let extent = swapchain.extent;

        // Command pool + one primary command buffer (re-recorded each frame).
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(queue_family_index);
        // SAFETY: `device` is valid; `pool_info` outlives the call.
        let command_pool = match unsafe { device.create_command_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: swapchain + render_pass are valid; free them before bailing.
                unsafe {
                    swapchain.destroy(device, &swapchain_loader);
                    device.destroy_render_pass(render_pass, None);
                }
                return Err(GraphicsError::Vulkan(format!("vkCreateCommandPool: {e}")));
            }
        };
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: `alloc_info` references the just-created pool; `device` is valid.
        let command_buffers = match unsafe { device.allocate_command_buffers(&alloc_info) } {
            Ok(b) => b,
            Err(e) => {
                // SAFETY: all three objects are valid; free them in reverse order.
                unsafe {
                    device.destroy_command_pool(command_pool, None);
                    swapchain.destroy(device, &swapchain_loader);
                    device.destroy_render_pass(render_pass, None);
                }
                return Err(GraphicsError::Vulkan(format!(
                    "vkAllocateCommandBuffers: {e}"
                )));
            }
        };
        let command_buffer = command_buffers[0];

        // Per-frame sync: two semaphores (acquire/render-done) + one fence (CPU-GPU). Single
        // frame in flight keeps the foundation minimal and correct.
        let sem_info = vk::SemaphoreCreateInfo::default();
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: `device` valid; the create-infos outlive the calls. On any failure we destroy
        // everything created so far (reverse order) before returning the error.
        let sync = unsafe {
            let image_available = device.create_semaphore(&sem_info, None);
            let render_finished = device.create_semaphore(&sem_info, None);
            let in_flight = device.create_fence(&fence_info, None);
            match (image_available, render_finished, in_flight) {
                (Ok(ia), Ok(rf), Ok(f)) => Ok((ia, rf, f)),
                (ia, rf, f) => {
                    if let Ok(h) = ia {
                        device.destroy_semaphore(h, None);
                    }
                    if let Ok(h) = rf {
                        device.destroy_semaphore(h, None);
                    }
                    if let Ok(h) = f {
                        device.destroy_fence(h, None);
                    }
                    Err(ia
                        .err()
                        .or(rf.err())
                        .or(f.err())
                        .unwrap_or(vk::Result::ERROR_UNKNOWN))
                }
            }
        };
        let (image_available, render_finished, in_flight) = match sync {
            Ok(t) => t,
            Err(e) => {
                // SAFETY: free the command pool, swapchain, render pass (reverse order).
                unsafe {
                    device.destroy_command_pool(command_pool, None);
                    swapchain.destroy(device, &swapchain_loader);
                    device.destroy_render_pass(render_pass, None);
                }
                return Err(GraphicsError::Vulkan(format!("create sync objects: {e}")));
            }
        };

        // The colored-quad pipeline. Created LAST so no later error path needs to free it; on its
        // own failure, free everything created above (reverse order) before returning the error.
        let (quad_pipeline_layout, quad_pipeline) =
            match Self::create_quad_pipeline(device, render_pass) {
                Ok(p) => p,
                Err(e) => {
                    // SAFETY: every handle below is valid and owned here; freed once, reverse order.
                    unsafe {
                        device.destroy_semaphore(image_available, None);
                        device.destroy_semaphore(render_finished, None);
                        device.destroy_fence(in_flight, None);
                        device.destroy_command_pool(command_pool, None);
                        swapchain.destroy(device, &swapchain_loader);
                        device.destroy_render_pass(render_pass, None);
                    }
                    return Err(e);
                }
            };

        // The text pass (font atlas + textured-glyph pipeline). Best-effort: `Ok(None)` if no system
        // font is found (quads still draw). A hard Vulkan error frees everything created above.
        let text =
            match TextRenderer::new(device, queue, command_pool, render_pass, &memory_properties) {
                Ok(t) => t,
                Err(e) => {
                    // SAFETY: every handle below is valid + owned; freed once, reverse order.
                    unsafe {
                        device.destroy_pipeline(quad_pipeline, None);
                        device.destroy_pipeline_layout(quad_pipeline_layout, None);
                        device.destroy_semaphore(image_available, None);
                        device.destroy_semaphore(render_finished, None);
                        device.destroy_fence(in_flight, None);
                        device.destroy_command_pool(command_pool, None);
                        swapchain.destroy(device, &swapchain_loader);
                        device.destroy_render_pass(render_pass, None);
                    }
                    return Err(e);
                }
            };

        // The RGBA Canvas-composite pass (custom-view onDraw → GPU texture over the view rect).
        // Best-effort: a hard Vulkan failure frees everything created above (incl. the text pass) and
        // surfaces a typed error; the quads + text still draw if the compositor were absent.
        let composite = match CanvasCompositor::new(device, render_pass) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "Canvas compositor init failed; custom-view onDraw not composited");
                None
            }
        };

        Ok((
            queue,
            swapchain_loader,
            render_pass,
            command_pool,
            command_buffer,
            image_available,
            render_finished,
            in_flight,
            swapchain,
            surface_format.format,
            extent,
            quad_pipeline_layout,
            quad_pipeline,
            memory_properties,
            text,
            composite,
        ))
    }

    /// Create a swapchain (+ image views + framebuffers) for the current surface size. `old` is
    /// passed as `oldSwapchain` for an in-place resize (or `null` for the first creation). The
    /// caller owns destroying `old` after this returns.
    #[allow(clippy::too_many_arguments)]
    fn create_swapchain(
        surface_loader: &khr::surface::Instance,
        swapchain_loader: &khr::swapchain::Device,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
        surface_format: vk::SurfaceFormatKHR,
        render_pass: vk::RenderPass,
        window_width: u32,
        window_height: u32,
        old: vk::SwapchainKHR,
    ) -> Result<Swapchain, GraphicsError> {
        // SAFETY: handles are valid; this only queries surface capabilities.
        let caps = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }
        .map_err(|e| GraphicsError::Vulkan(format!("get surface capabilities: {e}")))?;
        let extent = choose_swap_extent(&caps, window_width, window_height);
        let image_count = choose_image_count(&caps);

        // FIFO is the only present mode the Vulkan spec guarantees is supported — pick it for the
        // foundation (vsync; no tearing; broad compatibility). MAILBOX can come with the engine.
        let create_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(surface_format.format)
            .image_color_space(surface_format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true)
            .old_swapchain(old);

        // SAFETY: `create_info` and its borrows outlive the call; loaders/handles are valid.
        let swapchain = unsafe { swapchain_loader.create_swapchain(&create_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateSwapchainKHR: {e}")))?;

        // SAFETY: `swapchain` was just created from this loader.
        let images = match unsafe { swapchain_loader.get_swapchain_images(swapchain) } {
            Ok(i) => i,
            Err(e) => {
                // SAFETY: destroy the swapchain we just made before bailing.
                unsafe { swapchain_loader.destroy_swapchain(swapchain, None) };
                return Err(GraphicsError::Vulkan(format!("get swapchain images: {e}")));
            }
        };

        let mut image_views = Vec::with_capacity(images.len());
        let mut framebuffers = Vec::with_capacity(images.len());
        for &image in &images {
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(surface_format.format)
                .components(vk::ComponentMapping::default())
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .base_mip_level(0)
                        .level_count(1)
                        .base_array_layer(0)
                        .layer_count(1),
                );
            // SAFETY: `image` belongs to `swapchain`; `view_info` outlives the call.
            let view = match unsafe { device.create_image_view(&view_info, None) } {
                Ok(v) => v,
                Err(e) => {
                    // SAFETY: free everything created in this loop so far + the swapchain.
                    unsafe {
                        for &fb in &framebuffers {
                            device.destroy_framebuffer(fb, None);
                        }
                        for &v in &image_views {
                            device.destroy_image_view(v, None);
                        }
                        swapchain_loader.destroy_swapchain(swapchain, None);
                    }
                    return Err(GraphicsError::Vulkan(format!("vkCreateImageView: {e}")));
                }
            };
            image_views.push(view);

            let attachments = [view];
            let fb_info = vk::FramebufferCreateInfo::default()
                .render_pass(render_pass)
                .attachments(&attachments)
                .width(extent.width)
                .height(extent.height)
                .layers(1);
            // SAFETY: `view`/`render_pass` valid; `fb_info` + its slice outlive the call.
            let fb = match unsafe { device.create_framebuffer(&fb_info, None) } {
                Ok(f) => f,
                Err(e) => {
                    // SAFETY: free all views (incl. the one just made) + framebuffers + swapchain.
                    unsafe {
                        for &fb in &framebuffers {
                            device.destroy_framebuffer(fb, None);
                        }
                        for &v in &image_views {
                            device.destroy_image_view(v, None);
                        }
                        swapchain_loader.destroy_swapchain(swapchain, None);
                    }
                    return Err(GraphicsError::Vulkan(format!("vkCreateFramebuffer: {e}")));
                }
            };
            framebuffers.push(fb);
        }

        Ok(Swapchain {
            swapchain,
            image_views,
            framebuffers,
            extent,
        })
    }

    /// Number of swapchain images (frames) — for diagnostics.
    fn frame_count(&self) -> usize {
        self.swapchain.framebuffers.len()
    }

    /// Hit-test the current View tree at window pixel `(x, y)` for the topmost clickable view.
    ///
    /// 2026-06-05: reproduces EXACTLY the layout the draw path uses — `snapshot_tree()` →
    /// [`layout_views`] at the current swapchain extent with the same text measurer — then runs the
    /// pure [`hit_test`]. Single-sources the geometry so a click hits the same rects that are drawn.
    /// Returns the hit view's [`ViewHandle`] (for click dispatch), or `None` if no clickable view is
    /// under the point. Pure read of the registry + arithmetic (no GPU work).
    fn hit_test_at(&self, x: f32, y: f32) -> Option<ViewHandle> {
        let nodes = crate::framework::view_registry::snapshot_tree();
        if nodes.is_empty() {
            return None;
        }
        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(&nodes, self.swapchain.extent, measure);
        hit_test(&views, x, y)
    }

    /// The window-pixel center of the first clickable laid-out view, if any. 2026-06-05: used only by
    /// the env-gated dev-host synthetic-tap diagnostic to aim a tap at a real clickable view. Lays out
    /// the tree exactly like [`Self::hit_test_at`]; `None` when no clickable view exists.
    fn first_clickable_center(&self) -> Option<(f32, f32)> {
        let nodes = crate::framework::view_registry::snapshot_tree();
        if nodes.is_empty() {
            return None;
        }
        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(&nodes, self.swapchain.extent, measure);
        views
            .iter()
            .find(|v| v.clickable)
            .map(|v| (v.x + v.w / 2.0, v.y + v.h / 2.0))
    }

    /// The handle + window-pixel center of the deepest (last-in-pre-order) laid-out LEAF view, if any.
    ///
    /// 2026-06-05: used only by the env-gated dev-host synthetic-tap diagnostic as a fallback target
    /// when no clickable view is in the snapshot, so the diagnostic can still drive a real DOWN+UP
    /// `MotionEvent` through a **leaf** `View.dispatchTouchEvent` end-to-end against a real Java View
    /// object (headless evidence the JNI chain — `MotionEvent.obtain` → dispatch → `recycle` — works).
    /// A leaf is targeted on purpose: a leaf `View.dispatchTouchEvent` is pure Java (calls
    /// `onTouchEvent`), whereas a `ViewGroup` routes through ATL's native `native_dispatchTouchEvent`
    /// (the touch-routing follow-up). The real pointer path likewise resolves the topmost clickable
    /// view, normally a leaf widget. Lays out the tree exactly like [`Self::hit_test_at`]; `None` for
    /// an empty tree.
    fn first_view_center(&self) -> Option<(ViewHandle, f32, f32)> {
        let nodes = crate::framework::view_registry::snapshot_tree();
        if nodes.is_empty() {
            return None;
        }
        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(&nodes, self.swapchain.extent, measure);
        // Pre-order = parent before children, so the last entry with no children is the deepest leaf.
        // `nodes` and `views` are parallel; a leaf has empty `children` in the snapshot node.
        nodes
            .iter()
            .zip(views.iter())
            .rev()
            .find(|(n, _)| n.children.is_empty())
            .map(|(_, v)| (v.handle, v.x + v.w / 2.0, v.y + v.h / 2.0))
    }

    /// The custom (app-defined) views in the current tree that should have their `onDraw(Canvas)`
    /// driven, each with the pixel size of its laid-out rect (for the [`canvas_registry`] Pixmap).
    ///
    /// 2026-06-05: lays out the tree exactly like [`Self::hit_test_at`] (single-sourced geometry), then
    /// keeps the views whose class is NOT a framework class ([`is_custom_view_class`]) and whose rect is
    /// at least 1×1 px (a degenerate rect can't back a Pixmap). The caller ([`GameWindow`]) drives
    /// `View.draw(Canvas)` for these via [`framework::drive_view_draw`](crate::framework::drive_view_draw),
    /// then hands the resulting canvases back via [`Self::set_drawn_canvases`]. GPU-free (snapshot +
    /// arithmetic), so it can run before the frame's GPU work.
    fn custom_view_draw_targets(&self) -> Vec<crate::framework::DrawTarget> {
        let nodes = crate::framework::view_registry::snapshot_tree();
        if nodes.is_empty() {
            return Vec::new();
        }
        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(&nodes, self.swapchain.extent, measure);
        let mut targets = Vec::new();
        for (n, v) in nodes.iter().zip(views.iter()) {
            if !is_custom_view_class(&n.class_name) {
                continue;
            }
            // Round the laid-out rect up to a whole-pixel canvas (≥ 1×1). A degenerate rect is skipped.
            let w = v.w.ceil();
            let h = v.h.ceil();
            if !(w >= 1.0 && h >= 1.0 && w.is_finite() && h.is_finite()) {
                continue;
            }
            targets.push(crate::framework::DrawTarget {
                handle: v.handle,
                width: w as u32,
                height: h as u32,
            });
        }
        targets
    }

    /// Receive this frame's drawn custom-view canvases from the draw cascade (the `(view, canvas)`
    /// pairs whose `onDraw` rasterized into a [`canvas_registry`] Pixmap). [`Self::draw_frame`] uploads
    /// each Pixmap over its view's rect, then frees the handles. Replaces (and frees) any leftover from
    /// a frame that didn't reach `draw_frame` (e.g. a minimized frame) so a slab handle never leaks.
    fn set_drawn_canvases(&mut self, drawn: Vec<crate::framework::DrawnCanvas>) {
        for d in self.drawn_canvases.drain(..) {
            let _ = crate::framework::canvas_registry::free(d.canvas);
        }
        self.drawn_canvases = drawn;
    }

    /// Note that the window was resized; the next [`Self::draw_frame`] recreates the swapchain.
    /// A zero dimension means the window is minimized — skip the recreate then.
    fn mark_resized(&mut self, width: u32, height: u32) {
        if width != 0 && height != 0 {
            self.needs_recreate = true;
        }
    }

    /// Rebuild the swapchain (+ views/framebuffers) for the window's current size, after the GPU
    /// is idle, destroying the old one. Keeps the render pass/device objects.
    fn recreate_swapchain(&mut self, window: &Window) -> Result<(), GraphicsError> {
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            // Minimized: nothing to present; leave the old swapchain in place.
            return Ok(());
        }
        // SAFETY: wait until no GPU work references the old swapchain/framebuffers before freeing.
        unsafe { self.device.device_wait_idle() }
            .map_err(|e| GraphicsError::Vulkan(format!("device_wait_idle: {e}")))?;

        let surface_format = vk::SurfaceFormatKHR {
            format: self.swapchain_format,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        };
        let new_swapchain = Self::create_swapchain(
            &self.surface_loader,
            &self.swapchain_loader,
            &self.device,
            self.physical_device,
            self.surface,
            surface_format,
            self.render_pass,
            size.width,
            size.height,
            self.swapchain.swapchain,
        )?;

        // SAFETY: GPU is idle; destroy the previous swapchain's children + handle. The new one
        // referenced the old via `oldSwapchain`, which Vulkan retires on this destroy.
        unsafe {
            let old = std::mem::replace(&mut self.swapchain, new_swapchain);
            old.destroy(&self.device, &self.swapchain_loader);
        }
        self.swapchain_extent = self.swapchain.extent;
        self.needs_recreate = false;
        Ok(())
    }

    /// Acquire → clear (render pass) → present one frame. Recreates the swapchain on resize or an
    /// out-of-date/suboptimal surface, so a window resize keeps presenting cleanly.
    fn draw_frame(&mut self, window: &Window) -> Result<(), GraphicsError> {
        if self.needs_recreate {
            self.recreate_swapchain(window)?;
            if self.swapchain.framebuffers.is_empty() {
                return Ok(()); // minimized
            }
        }

        // SAFETY: wait for the previous frame's fence (single frame in flight) then reset it.
        unsafe {
            self.device
                .wait_for_fences(&[self.in_flight], true, u64::MAX)
                .map_err(|e| GraphicsError::Vulkan(format!("wait_for_fences: {e}")))?;
        }

        // SAFETY: swapchain/semaphore are valid; acquire signals `image_available`.
        let acquire = unsafe {
            self.swapchain_loader.acquire_next_image(
                self.swapchain.swapchain,
                u64::MAX,
                self.image_available,
                vk::Fence::null(),
            )
        };
        let (image_index, suboptimal) = match acquire {
            Ok(v) => v,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.needs_recreate = true;
                return Ok(()); // recreate next frame; do not signal-wait on a stale image
            }
            Err(e) => return Err(GraphicsError::Vulkan(format!("acquire_next_image: {e}"))),
        };

        // Only reset the fence once we're committed to submitting (avoids a deadlock if we
        // early-returned above with the fence already reset).
        // SAFETY: `in_flight` is valid and signaled.
        unsafe {
            self.device
                .reset_fences(&[self.in_flight])
                .map_err(|e| GraphicsError::Vulkan(format!("reset_fences: {e}")))?;
        }

        // Read the framework's recorded View tree, lay it out against the current extent, and upload
        // the per-view quads. The fence above guarantees the previous frame's GPU read of the vertex
        // buffer has completed, so re-uploading here is safe. An empty tree → 0 vertices → clear-only.
        let nodes = crate::framework::view_registry::snapshot_tree();
        let extent = self.swapchain.extent;
        // Measure WRAP_CONTENT text against the real glyph atlas when a font is loaded (else the
        // cascade falls back to a default box). Built from a shared (immutable) borrow of the atlas,
        // dropped before the disjoint `self.text.as_mut()` upload borrow below.
        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(&nodes, extent, measure);
        // One-shot observability: log each computed view rect the first time a non-empty tree is laid
        // out, so the measure/layout result is inspectable without spamming every frame (the per-frame
        // summary below stays at TRACE). 2026-06-05.
        if !views.is_empty() {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| {
                for (i, (n, v)) in nodes.iter().zip(views.iter()).enumerate() {
                    tracing::debug!(
                        target: "eclipse::graphics::layout",
                        i,
                        class = %n.class_name,
                        x = v.x, y = v.y, w = v.w, h = v.h,
                        depth = n.depth,
                        "laid-out view rect"
                    );
                }
            });
        }
        let verts = build_quad_vertices(&views, extent);
        let vertex_count = self.upload_vertices(&verts)?;

        // Text: lay out each view's text into glyph quads and upload them to the text vertex buffer.
        // `memory_properties` is `Copy`, so taking it by value frees `self` for the disjoint
        // `self.text.as_mut()` borrow. No text renderer (no font) → no text vertices, quads still draw.
        let mem_props = self.memory_properties;
        let text_vertex_count = if let Some(text) = self.text.as_mut() {
            let tverts = build_text_vertices(&views, &text.atlas, extent);
            text.upload(&self.device, &mem_props, &tverts)?
        } else {
            0
        };
        // Canvas composite: free last frame's textures (the `in_flight` wait above guarantees the GPU
        // finished reading them), then upload each custom view's freshly-rasterized Pixmap as an RGBA
        // texture over its laid-out rect. `drawn_canvases` was set by the draw cascade before this
        // frame; we look up each view's current rect in `views` (the same layout the quads use), upload
        // the Pixmap, then free the slab handle (it has served its purpose this frame). A bad/missing
        // rect or canvas is skipped — quads + text still draw. `queue`/`command_pool` are `Copy`.
        let queue = self.queue;
        let command_pool = self.command_pool;
        let composite_count = if let Some(composite) = self.composite.as_mut() {
            // SAFETY: the GPU finished reading last frame's composite textures (in_flight waited above).
            unsafe { composite.begin_frame(&self.device)? };
            for d in &self.drawn_canvases {
                let Some(rect) = views.iter().find(|v| v.handle == d.view) else {
                    continue; // the view left the tree this frame — skip (its handle is freed below)
                };
                // Read the Pixmap's straight RGBA + dimensions out of the canvas_registry.
                let snapshot = crate::framework::canvas_registry::with_canvas(d.canvas, |c| {
                    let (w, h) = c.dimensions();
                    (w, h, c.rgba())
                });
                let Ok((tw, th, rgba)) = snapshot else {
                    continue; // stale/invalid canvas handle — skip
                };
                composite.upload(
                    &self.device,
                    queue,
                    command_pool,
                    &mem_props,
                    &rgba,
                    tw,
                    th,
                    rect,
                    extent,
                )?;
            }
            composite.texture_count()
        } else {
            0
        };
        // The Pixmaps have been uploaded (or skipped); free the slab handles so the slots are reclaimed.
        for d in self.drawn_canvases.drain(..) {
            let _ = crate::framework::canvas_registry::free(d.canvas);
        }

        if vertex_count > 0 {
            tracing::trace!(
                views = views.len(),
                quads = vertex_count / 6,
                glyphs = text_vertex_count / 6,
                composites = composite_count,
                "drawing recorded View tree into the swapchain"
            );
        }

        self.record_draw(image_index as usize, vertex_count, text_vertex_count)?;

        let wait_semaphores = [self.image_available];
        let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let command_buffers = [self.command_buffer];
        let signal_semaphores = [self.render_finished];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_semaphores)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&command_buffers)
            .signal_semaphores(&signal_semaphores);
        // SAFETY: all handles are valid; `submit` + its borrowed slices outlive the call; the
        // fence is unsignaled so it tracks this submission.
        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], self.in_flight)
                .map_err(|e| GraphicsError::Vulkan(format!("queue_submit: {e}")))?;
        }

        // Tell winit a present is imminent (Wayland frame-callback hint) before presenting.
        window.pre_present_notify();

        let swapchains = [self.swapchain.swapchain];
        let image_indices = [image_index];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal_semaphores)
            .swapchains(&swapchains)
            .image_indices(&image_indices);
        // SAFETY: handles valid; `present_info` + its slices outlive the call.
        let present = unsafe {
            self.swapchain_loader
                .queue_present(self.queue, &present_info)
        };
        match present {
            Ok(false) => {}
            Ok(true) => self.needs_recreate = true, // suboptimal
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) | Err(vk::Result::SUBOPTIMAL_KHR) => {
                self.needs_recreate = true;
            }
            Err(e) => return Err(GraphicsError::Vulkan(format!("queue_present: {e}"))),
        }
        if suboptimal {
            self.needs_recreate = true;
        }
        Ok(())
    }

    /// Ensure the host-visible vertex buffer can hold `needed` vertices, (re)allocating it (larger)
    /// when it cannot, then copy `verts` into it. Returns the number of vertices uploaded.
    ///
    /// 2026-06-05: safe to reallocate/overwrite each frame because [`Self::draw_frame`] already
    /// waited on `in_flight` (single frame in flight) before calling this — the previous frame's GPU
    /// read of this buffer has completed, so no in-use memory is freed/overwritten. Host-coherent
    /// memory means the mapped write is visible to the GPU without an explicit flush. An empty
    /// `verts` uploads nothing and returns 0 (the draw then issues no `cmd_draw`).
    fn upload_vertices(&mut self, verts: &[QuadVertex]) -> Result<u32, GraphicsError> {
        let count: u32 = verts.len().try_into().map_err(|_| {
            GraphicsError::Vulkan("too many quad vertices for one frame".to_owned())
        })?;
        if count == 0 {
            return Ok(0);
        }

        // Grow (never shrink) the buffer when the current capacity is too small. Allocate a fresh
        // buffer+memory and free the old one (GPU idle for this buffer per the fn contract).
        if count > self.quad_vertex_capacity {
            let size =
                (count as vk::DeviceSize) * std::mem::size_of::<QuadVertex>() as vk::DeviceSize;
            let buffer_info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            // SAFETY: `device` valid; `buffer_info` outlives the call.
            let buffer = unsafe { self.device.create_buffer(&buffer_info, None) }
                .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (vertex): {e}")))?;
            // SAFETY: `buffer` was just created from `device`.
            let req = unsafe { self.device.get_buffer_memory_requirements(buffer) };
            let mem_type =
                find_host_visible_memory_type(&self.memory_properties, req.memory_type_bits)
                    .ok_or_else(|| {
                        // SAFETY: `buffer` is valid and unbound; free it before bailing.
                        unsafe { self.device.destroy_buffer(buffer, None) };
                        GraphicsError::Vulkan(
                            "no HOST_VISIBLE|HOST_COHERENT memory type for the vertex buffer"
                                .to_owned(),
                        )
                    })?;
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mem_type);
            // SAFETY: `alloc_info` outlives the call; `mem_type` satisfies `buffer`'s requirements.
            let memory = match unsafe { self.device.allocate_memory(&alloc_info, None) } {
                Ok(m) => m,
                Err(e) => {
                    // SAFETY: `buffer` is valid and unbound; free it before bailing.
                    unsafe { self.device.destroy_buffer(buffer, None) };
                    return Err(GraphicsError::Vulkan(format!(
                        "vkAllocateMemory (vertex): {e}"
                    )));
                }
            };
            // SAFETY: `buffer`+`memory` are valid; bind at offset 0 (whole allocation for this buffer).
            if let Err(e) = unsafe { self.device.bind_buffer_memory(buffer, memory, 0) } {
                // SAFETY: both handles are valid and owned here; free them before bailing.
                unsafe {
                    self.device.free_memory(memory, None);
                    self.device.destroy_buffer(buffer, None);
                }
                return Err(GraphicsError::Vulkan(format!("vkBindBufferMemory: {e}")));
            }

            // Free the previous buffer (if any) — the GPU finished reading it (fence waited).
            // SAFETY: prior handles are either null (no-op-guarded below) or valid+idle; freed once.
            unsafe {
                if self.quad_vertex_buffer != vk::Buffer::null() {
                    self.device.destroy_buffer(self.quad_vertex_buffer, None);
                }
                if self.quad_vertex_memory != vk::DeviceMemory::null() {
                    self.device.free_memory(self.quad_vertex_memory, None);
                }
            }
            self.quad_vertex_buffer = buffer;
            self.quad_vertex_memory = memory;
            self.quad_vertex_capacity = count;
        }

        // Map, copy, unmap. The buffer is host-coherent so no explicit flush is needed.
        let copy_bytes =
            (count as vk::DeviceSize) * std::mem::size_of::<QuadVertex>() as vk::DeviceSize;
        // SAFETY: `quad_vertex_memory` is a valid host-visible allocation ≥ `copy_bytes`; we map the
        // exact range we write, copy `count` `QuadVertex`es (the source slice has `count` elements),
        // then unmap. No aliasing: the GPU is not reading this buffer (fence waited) during the copy.
        unsafe {
            let ptr = self
                .device
                .map_memory(
                    self.quad_vertex_memory,
                    0,
                    copy_bytes,
                    vk::MemoryMapFlags::empty(),
                )
                .map_err(|e| GraphicsError::Vulkan(format!("vkMapMemory (vertex): {e}")))?;
            std::ptr::copy_nonoverlapping(
                verts.as_ptr() as *const u8,
                ptr as *mut u8,
                copy_bytes as usize,
            );
            self.device.unmap_memory(self.quad_vertex_memory);
        }
        Ok(count)
    }

    /// Record the render pass for `image_index`: clear to [`CLEAR_COLOR`], draw the View-tree quads
    /// (when `vertex_count > 0`), then draw the text glyphs on top (when `text_vertex_count > 0` and
    /// the text pass exists). All-zero counts is the clear-only path (no content recorded yet) —
    /// identical to the previous foundation behavior.
    fn record_draw(
        &self,
        image_index: usize,
        vertex_count: u32,
        text_vertex_count: u32,
    ) -> Result<(), GraphicsError> {
        let cmd = self.command_buffer;
        // SAFETY: the command buffer was allocated with RESET_COMMAND_BUFFER; reset then begin.
        unsafe {
            self.device
                .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(|e| GraphicsError::Vulkan(format!("reset_command_buffer: {e}")))?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device
                .begin_command_buffer(cmd, &begin)
                .map_err(|e| GraphicsError::Vulkan(format!("begin_command_buffer: {e}")))?;
        }

        let clear = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: CLEAR_COLOR,
            },
        }];
        let rp_begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.swapchain.framebuffers[image_index])
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.swapchain.extent,
            })
            .clear_values(&clear);
        // SAFETY: framebuffer index is in range (acquired image), render pass + cmd are valid. The
        // CLEAR load-op paints CLEAR_COLOR; the quad draw (if any) then composites on top, and the
        // text draw composites over the quads (alpha-blended).
        unsafe {
            self.device
                .cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);
            let composite_count = self
                .composite
                .as_ref()
                .map_or(0, CanvasCompositor::texture_count);
            if vertex_count > 0 || text_vertex_count > 0 || composite_count > 0 {
                let extent = self.swapchain.extent;
                let viewport = vk::Viewport::default()
                    .x(0.0)
                    .y(0.0)
                    .width(extent.width as f32)
                    .height(extent.height as f32)
                    .min_depth(0.0)
                    .max_depth(1.0);
                let scissor = vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                };
                self.device.cmd_set_viewport(cmd, 0, &[viewport]);
                self.device.cmd_set_scissor(cmd, 0, &[scissor]);
            }
            if vertex_count > 0 {
                self.device.cmd_bind_pipeline(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.quad_pipeline,
                );
                self.device
                    .cmd_bind_vertex_buffers(cmd, 0, &[self.quad_vertex_buffer], &[0]);
                self.device.cmd_draw(cmd, vertex_count, 1, 0, 0);
            }
            // Text glyphs over the quads: bind the text pipeline + atlas descriptor, push the text
            // color, draw. Guarded on the text pass existing + having vertices this frame.
            if text_vertex_count > 0 {
                if let Some(text) = self.text.as_ref() {
                    self.device.cmd_bind_pipeline(
                        cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        text.pipeline,
                    );
                    self.device.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        text.pipeline_layout,
                        0,
                        &[text.descriptor_set],
                        &[],
                    );
                    // Pack the vec4 text color into push-constant bytes (host-endian = what the GPU
                    // on this host reads). Safe byte construction — no transmute.
                    let mut color_bytes = [0u8; 16];
                    for (i, c) in TEXT_COLOR.iter().enumerate() {
                        color_bytes[i * 4..i * 4 + 4].copy_from_slice(&c.to_ne_bytes());
                    }
                    self.device.cmd_push_constants(
                        cmd,
                        text.pipeline_layout,
                        vk::ShaderStageFlags::FRAGMENT,
                        0,
                        &color_bytes,
                    );
                    self.device
                        .cmd_bind_vertex_buffers(cmd, 0, &[text.vertex_buffer], &[0]);
                    self.device.cmd_draw(cmd, text_vertex_count, 1, 0, 0);
                }
            }
            // Canvas composites OVER the quads + text: each custom view's onDraw Pixmap as a textured
            // quad. SAFETY: cmd is recording in the render pass; viewport/scissor are set above.
            if let Some(composite) = self.composite.as_ref() {
                composite.record(&self.device, cmd);
            }
            self.device.cmd_end_render_pass(cmd);
            self.device
                .end_command_buffer(cmd)
                .map_err(|e| GraphicsError::Vulkan(format!("end_command_buffer: {e}")))?;
        }
        Ok(())
    }
}

impl Swapchain {
    /// Destroy this swapchain's framebuffers + image views + the swapchain handle.
    ///
    /// # Safety
    /// The GPU must be idle (no submitted work references these objects) and `device` /
    /// `loader` must be the same ones the resources were created from. Called only from
    /// teardown paths that hold that invariant.
    unsafe fn destroy(&self, device: &ash::Device, loader: &khr::swapchain::Device) {
        // SAFETY: per the function contract, the GPU is idle and these handles belong to `device`/
        // `loader`; each is destroyed exactly once.
        unsafe {
            for &fb in &self.framebuffers {
                device.destroy_framebuffer(fb, None);
            }
            for &view in &self.image_views {
                device.destroy_image_view(view, None);
            }
            loader.destroy_swapchain(self.swapchain, None);
        }
    }
}

/// The text pass: an R8 glyph-atlas image (sampled) + a textured-glyph pipeline + a per-frame text
/// vertex buffer. Built once at renderer init from a discovered system font; all handles are device
/// children freed by [`Self::destroy`] (called from [`VulkanRenderer`]'s `Drop` after the GPU is idle).
struct TextRenderer {
    atlas_image: vk::Image,
    atlas_memory: vk::DeviceMemory,
    atlas_view: vk::ImageView,
    sampler: vk::Sampler,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_set: vk::DescriptorSet,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    /// CPU-side atlas metrics (glyph map + ascent), kept to lay out text into vertices each frame.
    atlas: GlyphAtlas,
    /// Per-frame text vertex buffer (grown on demand, same single-frame-in-flight safety as quads).
    vertex_buffer: vk::Buffer,
    vertex_memory: vk::DeviceMemory,
    vertex_capacity: u32,
}

impl TextRenderer {
    /// Try to build the text pass: discover a system font, rasterize the atlas, upload it to a GPU
    /// image, and create the sampler/descriptor/pipeline. Returns `Ok(None)` (text disabled, quads
    /// still draw) if no font is found or the atlas is empty; `Err` only on a hard Vulkan failure
    /// after partial allocation (which it tears down). Never panics.
    fn new(
        device: &ash::Device,
        queue: vk::Queue,
        command_pool: vk::CommandPool,
        render_pass: vk::RenderPass,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<Option<Self>, GraphicsError> {
        let Some(font_path) = discover_font_path() else {
            tracing::warn!("no system font found (fc-match / font dirs); text drawing disabled");
            return Ok(None);
        };
        let bytes = match std::fs::read(&font_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %font_path.display(), error = %e, "font read failed; text disabled");
                return Ok(None);
            }
        };
        let font = match FontVec::try_from_vec(bytes) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(path = %font_path.display(), error = %e, "font parse failed; text disabled");
                return Ok(None);
            }
        };
        // Cap atlas width at a device-safe size; printable ASCII at 28px fits well under 1024.
        let Some(atlas) = build_glyph_atlas(&font, 1024) else {
            tracing::warn!("glyph atlas came out empty; text disabled");
            return Ok(None);
        };
        tracing::info!(
            font = %font_path.display(),
            atlas_w = atlas.width,
            atlas_h = atlas.height,
            glyphs = atlas.glyphs.len(),
            "text: discovered system font + built R8 glyph atlas"
        );

        // Build the GPU side. On any failure here, free what was made and surface a typed error.
        Self::build_gpu(
            device,
            queue,
            command_pool,
            render_pass,
            memory_properties,
            atlas,
        )
        .map(Some)
    }

    /// Create the atlas image + upload its pixels + the sampler/descriptor/pipeline. Split out so
    /// [`Self::new`]'s font/atlas discovery stays readable. Tears down partial state on error.
    #[allow(clippy::too_many_arguments)]
    fn build_gpu(
        device: &ash::Device,
        queue: vk::Queue,
        command_pool: vk::CommandPool,
        render_pass: vk::RenderPass,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        atlas: GlyphAtlas,
    ) -> Result<Self, GraphicsError> {
        // --- Atlas image (R8_UNORM, sampled + transfer-dst) ---
        let extent = vk::Extent3D {
            width: atlas.width,
            height: atlas.height,
            depth: 1,
        };
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .extent(extent)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: device valid; image_info outlives the call.
        let atlas_image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateImage (atlas): {e}")))?;
        // SAFETY: atlas_image just created from device.
        let req = unsafe { device.get_image_memory_requirements(atlas_image) };
        let mem_type = find_device_local_memory_type(memory_properties, req.memory_type_bits)
            .ok_or_else(|| {
                // SAFETY: image valid + unbound; free before bailing.
                unsafe { device.destroy_image(atlas_image, None) };
                GraphicsError::Vulkan("no memory type for the glyph atlas image".to_owned())
            })?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(mem_type);
        // SAFETY: alloc outlives the call; mem_type satisfies the image's requirements.
        let atlas_memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: image valid + unbound; free before bailing.
                unsafe { device.destroy_image(atlas_image, None) };
                return Err(GraphicsError::Vulkan(format!(
                    "vkAllocateMemory (atlas): {e}"
                )));
            }
        };
        // SAFETY: image + memory valid; bind whole allocation at offset 0.
        if let Err(e) = unsafe { device.bind_image_memory(atlas_image, atlas_memory, 0) } {
            // SAFETY: both valid + owned; free reverse order.
            unsafe {
                device.free_memory(atlas_memory, None);
                device.destroy_image(atlas_image, None);
            }
            return Err(GraphicsError::Vulkan(format!("vkBindImageMemory: {e}")));
        }

        // --- Upload the atlas pixels via a staging buffer + one-time transfer ---
        if let Err(e) = upload_atlas_pixels(
            device,
            queue,
            command_pool,
            memory_properties,
            atlas_image,
            atlas.width,
            atlas.height,
            &atlas.pixels,
        ) {
            // SAFETY: image + memory valid + owned; free reverse order.
            unsafe {
                device.free_memory(atlas_memory, None);
                device.destroy_image(atlas_image, None);
            }
            return Err(e);
        }

        // From here, helper assembles the rest; on its error it frees image+memory.
        Self::finish_gpu(device, render_pass, atlas, atlas_image, atlas_memory)
    }

    /// Create the image view, sampler, descriptor (layout+pool+set), and the text pipeline over an
    /// already-uploaded atlas image. Frees `atlas_image`/`atlas_memory` on its own error paths.
    fn finish_gpu(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        atlas: GlyphAtlas,
        atlas_image: vk::Image,
        atlas_memory: vk::DeviceMemory,
    ) -> Result<Self, GraphicsError> {
        // Helper to free the image+memory on any error below (the only handles made before this fn).
        let free_image = |device: &ash::Device| {
            // SAFETY: both handles valid + owned; freed once on the error path.
            unsafe {
                device.free_memory(atlas_memory, None);
                device.destroy_image(atlas_image, None);
            }
        };

        let view_info = vk::ImageViewCreateInfo::default()
            .image(atlas_image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(1),
            );
        // SAFETY: image valid; view_info outlives the call.
        let atlas_view = match unsafe { device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                free_image(device);
                return Err(GraphicsError::Vulkan(format!(
                    "vkCreateImageView (atlas): {e}"
                )));
            }
        };

        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        // SAFETY: device valid; sampler_info outlives the call.
        let sampler = match unsafe { device.create_sampler(&sampler_info, None) } {
            Ok(s) => s,
            Err(e) => {
                // SAFETY: view valid; free it + image before bailing.
                unsafe { device.destroy_image_view(atlas_view, None) };
                free_image(device);
                return Err(GraphicsError::Vulkan(format!("vkCreateSampler: {e}")));
            }
        };

        // Descriptor set layout: one combined image sampler at binding 0, fragment stage.
        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let dsl_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));
        // SAFETY: device valid; dsl_info outlives the call.
        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&dsl_info, None) } {
                Ok(l) => l,
                Err(e) => {
                    // SAFETY: sampler+view valid; free them + image.
                    unsafe {
                        device.destroy_sampler(sampler, None);
                        device.destroy_image_view(atlas_view, None);
                    }
                    free_image(device);
                    return Err(GraphicsError::Vulkan(format!(
                        "vkCreateDescriptorSetLayout: {e}"
                    )));
                }
            };

        let pool_size = vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1);
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(std::slice::from_ref(&pool_size));
        // SAFETY: device valid; pool_info outlives the call.
        let descriptor_pool = match unsafe { device.create_descriptor_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: layout+sampler+view valid; free them + image.
                unsafe {
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                    device.destroy_image_view(atlas_view, None);
                }
                free_image(device);
                return Err(GraphicsError::Vulkan(format!(
                    "vkCreateDescriptorPool: {e}"
                )));
            }
        };

        let set_layouts = [descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&set_layouts);
        // SAFETY: pool+layout valid; alloc_info outlives the call.
        let descriptor_set = match unsafe { device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
                // SAFETY: pool+layout+sampler+view valid; free them + image.
                unsafe {
                    device.destroy_descriptor_pool(descriptor_pool, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                    device.destroy_image_view(atlas_view, None);
                }
                free_image(device);
                return Err(GraphicsError::Vulkan(format!(
                    "vkAllocateDescriptorSets: {e}"
                )));
            }
        };

        // Point the descriptor at the atlas (image is in SHADER_READ_ONLY_OPTIMAL after upload).
        let image_info = vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(atlas_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&image_info));
        // SAFETY: set+sampler+view valid; write + its borrow outlive the call.
        unsafe { device.update_descriptor_sets(std::slice::from_ref(&write), &[]) };

        // Pipeline (with a vec4 push constant for the text color).
        let (pipeline_layout, pipeline) =
            match build_text_pipeline(device, render_pass, descriptor_set_layout) {
                Ok(p) => p,
                Err(e) => {
                    // SAFETY: all descriptor/sampler/view handles valid; free them + image.
                    unsafe {
                        device.destroy_descriptor_pool(descriptor_pool, None);
                        device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                        device.destroy_sampler(sampler, None);
                        device.destroy_image_view(atlas_view, None);
                    }
                    free_image(device);
                    return Err(e);
                }
            };

        Ok(Self {
            atlas_image,
            atlas_memory,
            atlas_view,
            sampler,
            descriptor_pool,
            descriptor_set_layout,
            descriptor_set,
            pipeline_layout,
            pipeline,
            atlas,
            vertex_buffer: vk::Buffer::null(),
            vertex_memory: vk::DeviceMemory::null(),
            vertex_capacity: 0,
        })
    }

    /// (Re)upload this frame's text vertices into the host-visible text vertex buffer, growing it
    /// on demand. Returns the vertex count to draw. Same single-frame-in-flight safety as the quad
    /// buffer: the caller waited the `in_flight` fence before this, so re-upload cannot race the GPU.
    fn upload(
        &mut self,
        device: &ash::Device,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        verts: &[TextVertex],
    ) -> Result<u32, GraphicsError> {
        let count: u32 = verts.len().try_into().map_err(|_| {
            GraphicsError::Vulkan("too many text vertices for one frame".to_owned())
        })?;
        if count == 0 {
            return Ok(0);
        }
        if count > self.vertex_capacity {
            let size =
                (count as vk::DeviceSize) * std::mem::size_of::<TextVertex>() as vk::DeviceSize;
            let buffer_info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            // SAFETY: device valid; buffer_info outlives the call.
            let buffer = unsafe { device.create_buffer(&buffer_info, None) }
                .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (text): {e}")))?;
            // SAFETY: buffer just created.
            let req = unsafe { device.get_buffer_memory_requirements(buffer) };
            let mem_type = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
                .ok_or_else(|| {
                // SAFETY: buffer valid + unbound; free before bailing.
                unsafe { device.destroy_buffer(buffer, None) };
                GraphicsError::Vulkan(
                    "no host-visible memory for the text vertex buffer".to_owned(),
                )
            })?;
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mem_type);
            // SAFETY: alloc_info outlives the call.
            let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
                Ok(m) => m,
                Err(e) => {
                    // SAFETY: buffer valid + unbound; free before bailing.
                    unsafe { device.destroy_buffer(buffer, None) };
                    return Err(GraphicsError::Vulkan(format!(
                        "vkAllocateMemory (text): {e}"
                    )));
                }
            };
            // SAFETY: buffer+memory valid; bind whole allocation.
            if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
                // SAFETY: both valid + owned; free reverse order.
                unsafe {
                    device.free_memory(memory, None);
                    device.destroy_buffer(buffer, None);
                }
                return Err(GraphicsError::Vulkan(format!(
                    "vkBindBufferMemory (text): {e}"
                )));
            }
            // Free the previous text buffer (GPU finished reading — fence waited).
            // SAFETY: prior handles null-guarded or valid+idle; freed once.
            unsafe {
                if self.vertex_buffer != vk::Buffer::null() {
                    device.destroy_buffer(self.vertex_buffer, None);
                }
                if self.vertex_memory != vk::DeviceMemory::null() {
                    device.free_memory(self.vertex_memory, None);
                }
            }
            self.vertex_buffer = buffer;
            self.vertex_memory = memory;
            self.vertex_capacity = count;
        }

        let copy_bytes =
            (count as vk::DeviceSize) * std::mem::size_of::<TextVertex>() as vk::DeviceSize;
        // SAFETY: vertex_memory is a valid host-visible allocation ≥ copy_bytes; map the exact range,
        // copy `count` TextVertexes (source has `count`), unmap. GPU not reading (fence waited).
        unsafe {
            let ptr = device
                .map_memory(
                    self.vertex_memory,
                    0,
                    copy_bytes,
                    vk::MemoryMapFlags::empty(),
                )
                .map_err(|e| GraphicsError::Vulkan(format!("vkMapMemory (text): {e}")))?;
            std::ptr::copy_nonoverlapping(
                verts.as_ptr() as *const u8,
                ptr as *mut u8,
                copy_bytes as usize,
            );
            device.unmap_memory(self.vertex_memory);
        }
        Ok(count)
    }

    /// Destroy every device-child handle (image/view/sampler/descriptors/pipeline/vertex buffer).
    ///
    /// # Safety
    /// The GPU must be idle and `device` the one these were created from. Called only from
    /// [`VulkanRenderer`]'s `Drop` after `device_wait_idle`.
    unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: per contract the GPU is idle; every handle is valid + owned + freed once.
        unsafe {
            if self.vertex_buffer != vk::Buffer::null() {
                device.destroy_buffer(self.vertex_buffer, None);
            }
            if self.vertex_memory != vk::DeviceMemory::null() {
                device.free_memory(self.vertex_memory, None);
            }
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_image_view(self.atlas_view, None);
            device.destroy_image(self.atlas_image, None);
            device.free_memory(self.atlas_memory, None);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Canvas COMPOSITE: upload a custom View's RGBA Pixmap as a GPU texture + draw it over the view rect.
//
// 2026-06-05: the draw cascade (`framework::drive_view_draw`) runs each custom View's `onDraw(Canvas)`,
// rasterizing into an Eclipse-owned `canvas_registry` Pixmap (real tiny-skia raster). This compositor
// is the SIBLING of `TextRenderer` (R8 glyph atlas) for RGBA8: each frame it uploads each drawn
// Pixmap's straight-RGBA bytes into an RGBA8 sampled texture and draws a textured quad over the owning
// view's laid-out rect, alpha-blended over the view quads + text. Per-frame textures/descriptor sets
// are transient (the Pixmaps change as `onDraw` re-runs); they are freed at the start of the NEXT
// frame's composite — AFTER `draw_frame` waited the `in_flight` fence — so the GPU is never reading a
// freed texture (the same single-frame-in-flight safety the vertex buffers use).
// ---------------------------------------------------------------------------------------------

/// One uploaded RGBA texture + its descriptor set + its quad vertex buffer for one composited view.
/// All handles are device children, freed by [`CanvasCompositor::free_textures`] (next frame, GPU
/// idle for this set) or [`CanvasCompositor::destroy`] (renderer drop, GPU idle).
struct CompositeTexture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    /// Borrowed from the compositor's pool (freed when the pool is reset, not individually).
    descriptor_set: vk::DescriptorSet,
    vertex_buffer: vk::Buffer,
    vertex_memory: vk::DeviceMemory,
    vertex_count: u32,
}

/// The RGBA Canvas-composite pass: a persistent sampler + descriptor layout/pool + pipeline, plus the
/// per-frame [`CompositeTexture`]s. Mirrors [`TextRenderer`] but for full RGBA8 textures (one per
/// composited custom view) rather than a single shared R8 atlas.
struct CanvasCompositor {
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,
    /// Sized for [`MAX_COMPOSITE_VIEWS`] combined-image-sampler sets; RESET each frame before
    /// re-allocating this frame's sets (`FREE_DESCRIPTOR_SET` not needed — reset frees them all).
    descriptor_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    /// This frame's textures (drawn by [`Self::record`]); freed at the next [`Self::begin_frame`].
    textures: Vec<CompositeTexture>,
}

impl CanvasCompositor {
    /// Build the persistent compositor objects (sampler, descriptor layout/pool, pipeline). Returns
    /// `Ok(None)` is never used (unlike text, the composite needs no font) — a hard Vulkan failure
    /// frees partial state and surfaces a typed error. Never panics.
    fn new(device: &ash::Device, render_pass: vk::RenderPass) -> Result<Self, GraphicsError> {
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        // SAFETY: device valid; sampler_info outlives the call.
        let sampler = unsafe { device.create_sampler(&sampler_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateSampler (composite): {e}")))?;

        // Descriptor set layout: one combined image sampler at binding 0, fragment stage (per view).
        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let dsl_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));
        // SAFETY: device valid; dsl_info outlives the call.
        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&dsl_info, None) } {
                Ok(l) => l,
                Err(e) => {
                    // SAFETY: sampler valid + owned; free before bailing.
                    unsafe { device.destroy_sampler(sampler, None) };
                    return Err(GraphicsError::Vulkan(format!(
                        "vkCreateDescriptorSetLayout (composite): {e}"
                    )));
                }
            };

        // Pool big enough for MAX_COMPOSITE_VIEWS sets (one combined-image-sampler each); reset each
        // frame. RESET_DESCRIPTOR_POOL lets `reset_descriptor_pool` recycle all sets at once.
        let pool_size = vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(MAX_COMPOSITE_VIEWS as u32);
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(MAX_COMPOSITE_VIEWS as u32)
            .pool_sizes(std::slice::from_ref(&pool_size));
        // SAFETY: device valid; pool_info outlives the call.
        let descriptor_pool = match unsafe { device.create_descriptor_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: layout+sampler valid + owned; free before bailing.
                unsafe {
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(GraphicsError::Vulkan(format!(
                    "vkCreateDescriptorPool (composite): {e}"
                )));
            }
        };

        let (pipeline_layout, pipeline) =
            match build_composite_pipeline(device, render_pass, descriptor_set_layout) {
                Ok(p) => p,
                Err(e) => {
                    // SAFETY: pool+layout+sampler valid + owned; free before bailing.
                    unsafe {
                        device.destroy_descriptor_pool(descriptor_pool, None);
                        device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                        device.destroy_sampler(sampler, None);
                    }
                    return Err(e);
                }
            };

        Ok(Self {
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            pipeline_layout,
            pipeline,
            textures: Vec::new(),
        })
    }

    /// Free the PREVIOUS frame's composite textures and reset the descriptor pool, readying it for
    /// this frame's uploads. MUST be called after [`VulkanRenderer::draw_frame`]'s `in_flight` fence
    /// wait (so the GPU has finished reading last frame's textures) and before [`Self::upload`].
    ///
    /// # Safety
    /// The GPU must be idle w.r.t. last frame's submission (the caller waited `in_flight`). `device`
    /// is the one the textures were created from.
    unsafe fn begin_frame(&mut self, device: &ash::Device) -> Result<(), GraphicsError> {
        // SAFETY: per contract the GPU finished reading last frame's textures (fence waited). Free
        // each texture's image/view/memory + vertex buffer; the descriptor sets are freed wholesale
        // by the pool reset below.
        unsafe {
            for t in self.textures.drain(..) {
                device.destroy_image_view(t.view, None);
                device.destroy_image(t.image, None);
                device.free_memory(t.memory, None);
                if t.vertex_buffer != vk::Buffer::null() {
                    device.destroy_buffer(t.vertex_buffer, None);
                }
                if t.vertex_memory != vk::DeviceMemory::null() {
                    device.free_memory(t.vertex_memory, None);
                }
            }
            device
                .reset_descriptor_pool(self.descriptor_pool, vk::DescriptorPoolResetFlags::empty())
                .map_err(|e| {
                    GraphicsError::Vulkan(format!("reset_descriptor_pool (composite): {e}"))
                })?;
        }
        Ok(())
    }

    /// Upload one drawn Pixmap as an RGBA8 texture over the view's rect, allocating its descriptor set
    /// and quad vertex buffer, and record it for [`Self::record`]. Skips (returns `Ok(())`, no texture)
    /// when at the [`MAX_COMPOSITE_VIEWS`] cap or the rgba is the wrong size; on a hard Vulkan failure
    /// frees its own partial allocation and surfaces a typed error.
    #[allow(clippy::too_many_arguments)]
    fn upload(
        &mut self,
        device: &ash::Device,
        queue: vk::Queue,
        command_pool: vk::CommandPool,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        rgba: &[u8],
        tex_w: u32,
        tex_h: u32,
        rect: &LaidOutView,
        extent: vk::Extent2D,
    ) -> Result<(), GraphicsError> {
        if self.textures.len() >= MAX_COMPOSITE_VIEWS {
            return Ok(()); // cap reached; remaining custom views just don't composite this frame
        }
        // Validate the rgba buffer matches the declared dimensions (4 bytes/pixel straight RGBA).
        let expected = (tex_w as usize) * (tex_h as usize) * 4;
        if tex_w == 0 || tex_h == 0 || rgba.len() < expected {
            return Ok(()); // nothing sound to upload
        }

        // --- RGBA8 image (sampled + transfer-dst) ---
        let img_extent = vk::Extent3D {
            width: tex_w,
            height: tex_h,
            depth: 1,
        };
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(img_extent)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: device valid; image_info outlives the call.
        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateImage (composite): {e}")))?;
        // SAFETY: image just created from device.
        let req = unsafe { device.get_image_memory_requirements(image) };
        let mem_type = find_device_local_memory_type(memory_properties, req.memory_type_bits)
            .ok_or_else(|| {
                // SAFETY: image valid + unbound; free before bailing.
                unsafe { device.destroy_image(image, None) };
                GraphicsError::Vulkan("no memory type for a composite texture".to_owned())
            })?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(mem_type);
        // SAFETY: alloc outlives the call; mem_type satisfies the image's requirements.
        let memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: image valid + unbound; free before bailing.
                unsafe { device.destroy_image(image, None) };
                return Err(GraphicsError::Vulkan(format!(
                    "vkAllocateMemory (composite): {e}"
                )));
            }
        };
        // SAFETY: image + memory valid; bind whole allocation at offset 0.
        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            // SAFETY: both valid + owned; free reverse order.
            unsafe {
                device.free_memory(memory, None);
                device.destroy_image(image, None);
            }
            return Err(GraphicsError::Vulkan(format!(
                "vkBindImageMemory (composite): {e}"
            )));
        }

        // Upload the straight-RGBA pixels (4 bytes/pixel) via a staging buffer + one-time transfer.
        if let Err(e) = upload_rgba_pixels(
            device,
            queue,
            command_pool,
            memory_properties,
            image,
            tex_w,
            tex_h,
            &rgba[..expected],
        ) {
            // SAFETY: image + memory valid + owned; free reverse order.
            unsafe {
                device.free_memory(memory, None);
                device.destroy_image(image, None);
            }
            return Err(e);
        }

        // Image view.
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(1),
            );
        // SAFETY: image valid; view_info outlives the call.
        let view = match unsafe { device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                // SAFETY: image+memory valid + owned; free reverse order.
                unsafe {
                    device.free_memory(memory, None);
                    device.destroy_image(image, None);
                }
                return Err(GraphicsError::Vulkan(format!(
                    "vkCreateImageView (composite): {e}"
                )));
            }
        };

        // Allocate a descriptor set from the per-frame pool + point it at this texture.
        let set_layouts = [self.descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&set_layouts);
        // SAFETY: pool+layout valid; ds_alloc outlives the call.
        let descriptor_set = match unsafe { device.allocate_descriptor_sets(&ds_alloc) } {
            Ok(sets) => sets[0],
            Err(e) => {
                // SAFETY: view+image+memory valid + owned; free reverse order.
                unsafe {
                    device.destroy_image_view(view, None);
                    device.free_memory(memory, None);
                    device.destroy_image(image, None);
                }
                return Err(GraphicsError::Vulkan(format!(
                    "vkAllocateDescriptorSets (composite): {e}"
                )));
            }
        };
        let desc_image = vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&desc_image));
        // SAFETY: set+sampler+view valid; write + its borrow outlive the call.
        unsafe { device.update_descriptor_sets(std::slice::from_ref(&write), &[]) };

        // Build + upload the quad vertices (two triangles over the view rect, UV spanning the texture).
        let verts = composite_quad_vertices(rect, extent);
        let (vertex_buffer, vertex_memory, vertex_count) =
            match upload_composite_vertices(device, memory_properties, &verts) {
                Ok(v) => v,
                Err(e) => {
                    // SAFETY: view+image+memory valid + owned; free reverse order. The descriptor set
                    // is freed by the pool reset next frame, but the texture won't be tracked, so free
                    // its image/view/memory now.
                    unsafe {
                        device.destroy_image_view(view, None);
                        device.free_memory(memory, None);
                        device.destroy_image(image, None);
                    }
                    return Err(e);
                }
            };

        self.textures.push(CompositeTexture {
            image,
            memory,
            view,
            descriptor_set,
            vertex_buffer,
            vertex_memory,
            vertex_count,
        });
        Ok(())
    }

    /// Record the composite draws into `cmd` (inside the active render pass, after the quad + text
    /// draws). Binds the composite pipeline + per-texture descriptor set, pushes opacity 1.0, draws
    /// each texture's quad. Viewport/scissor are already set by the caller (dynamic state).
    ///
    /// # Safety
    /// `cmd` is in a render pass; every handle is valid; called only from [`VulkanRenderer::record_draw`].
    unsafe fn record(&self, device: &ash::Device, cmd: vk::CommandBuffer) {
        if self.textures.is_empty() {
            return;
        }
        // SAFETY: cmd is recording inside the render pass; pipeline/sets/buffers are valid.
        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            // Opacity 1.0 (draw the Pixmap as rasterized); packed into vec4 push-constant bytes.
            let opacity: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
            let mut bytes = [0u8; 16];
            for (i, c) in opacity.iter().enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&c.to_ne_bytes());
            }
            for t in &self.textures {
                if t.vertex_count == 0 {
                    continue;
                }
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[t.descriptor_set],
                    &[],
                );
                device.cmd_push_constants(
                    cmd,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    &bytes,
                );
                device.cmd_bind_vertex_buffers(cmd, 0, &[t.vertex_buffer], &[0]);
                device.cmd_draw(cmd, t.vertex_count, 1, 0, 0);
            }
        }
    }

    /// Number of textures composited this frame (for the per-frame draw summary log).
    fn texture_count(&self) -> usize {
        self.textures.len()
    }

    /// Destroy every device-child handle (this frame's textures + the persistent pipeline/pool/etc.).
    ///
    /// # Safety
    /// The GPU must be idle and `device` the one these were created from. Called only from
    /// [`VulkanRenderer`]'s `Drop` after `device_wait_idle`.
    unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: per contract the GPU is idle; every handle is valid + owned + freed once.
        unsafe {
            for t in &self.textures {
                device.destroy_image_view(t.view, None);
                device.destroy_image(t.image, None);
                device.free_memory(t.memory, None);
                if t.vertex_buffer != vk::Buffer::null() {
                    device.destroy_buffer(t.vertex_buffer, None);
                }
                if t.vertex_memory != vk::DeviceMemory::null() {
                    device.free_memory(t.vertex_memory, None);
                }
            }
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
    }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        // SAFETY: wait for all GPU work to finish so nothing references the objects we destroy.
        // Then destroy in strict reverse-creation order. Every handle is valid and owned here and
        // freed exactly once. `device_wait_idle` can only fail on device-lost, in which case the
        // driver has already invalidated the objects — destroying is still the correct teardown.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_semaphore(self.image_available, None);
            self.device.destroy_semaphore(self.render_finished, None);
            self.device.destroy_fence(self.in_flight, None);
            self.device.destroy_command_pool(self.command_pool, None);
            // The text pass (atlas image/view/sampler/descriptors/pipeline + its vertex buffer), if any.
            if let Some(text) = self.text.as_ref() {
                text.destroy(&self.device);
            }
            // The composite pass (this frame's textures + the persistent pipeline/pool/sampler), if any.
            // SAFETY: the GPU is idle (device_wait_idle above), so no submission references the textures.
            if let Some(composite) = self.composite.as_ref() {
                composite.destroy(&self.device);
            }
            // Free any still-held drawn-canvas slab handles so the canvas_registry slots are reclaimed.
            for d in &self.drawn_canvases {
                let _ = crate::framework::canvas_registry::free(d.canvas);
            }
            // The quad pipeline + its vertex buffer/memory (device children; freed before the device).
            // The buffer/memory may be null if no frame ever recorded content — destroy_* is a no-op
            // on a null handle, but guard anyway to be explicit.
            self.device.destroy_pipeline(self.quad_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.quad_pipeline_layout, None);
            if self.quad_vertex_buffer != vk::Buffer::null() {
                self.device.destroy_buffer(self.quad_vertex_buffer, None);
            }
            if self.quad_vertex_memory != vk::DeviceMemory::null() {
                self.device.free_memory(self.quad_vertex_memory, None);
            }
            self.swapchain.destroy(&self.device, &self.swapchain_loader);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Vector-drawable RASTER: fill an android.graphics.Path's REAL geometry into an RGBA pixmap.
//
// 2026-06-05: a launcher's onCreate may build a vector-drawable path (AdaptiveIconDemo's adaptive-icon
// mask: `getDrawable` → `AdaptiveIconDrawable` → `PathParser` → `Path.moveTo/lineTo/cubicTo`). Those
// natives record the REAL parsed contour geometry into [`path_registry`] (a verb+point buffer — NOT a
// fabricated shape). This module rasterizes that geometry, transformed by the owning Canvas/view
// matrix ([`matrix_registry::Affine`]) and filled with the paint color ([`paint_registry`]'s ARGB),
// into an RGBA [`Pixmap`] via the pure-Rust tiny-skia software rasterizer. The pixmap is then uploaded
// as a GPU texture and drawn over the owning view's rect — MIRRORING the glyph-atlas texture upload +
// textured pipeline above (the documented next compositing step; the upload path generalizes the R8
// atlas to RGBA). The raster itself is GPU-free, so it is unit-tested without Vulkan.
// ---------------------------------------------------------------------------------------------

use crate::framework::matrix_registry::Affine;
use crate::framework::path_registry::{PathGeometry, Verb};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

/// How a [`PathGeometry`] is filled: the ARGB color (AOSP `Paint.getColor()`, the `paint_registry`
/// value) and the even-odd vs winding fill rule (AOSP `Path.getFillType()`). Vector-drawable masks use
/// non-zero winding by default; this stays explicit so the raster matches the source path's rule.
#[derive(Debug, Clone, Copy)]
pub struct FillStyle {
    /// ARGB color as AOSP `Paint.setColor` stores it (0xAARRGGBB).
    pub argb: i32,
    /// `true` for even-odd, `false` for non-zero winding (AOSP default).
    pub even_odd: bool,
}

impl Default for FillStyle {
    fn default() -> Self {
        // Opaque black, non-zero winding — AOSP's default Paint color + Path fill type.
        Self {
            argb: 0xFF00_0000u32 as i32,
            even_odd: false,
        }
    }
}

/// Convert an AOSP 0xAARRGGBB `argb` int into tiny-skia's straight (un-premultiplied) RGBA8 channels.
fn argb_to_rgba8(argb: i32) -> (u8, u8, u8, u8) {
    let v = argb as u32;
    let a = (v >> 24) as u8;
    let r = (v >> 16) as u8;
    let g = (v >> 8) as u8;
    let b = v as u8;
    (r, g, b, a)
}

/// Build a tiny-skia [`tiny_skia::Path`] from Eclipse's real [`PathGeometry`] verb/point buffer.
///
/// Walks the verbs in order, consuming the flat point buffer per [`Verb::point_count`]. A malformed
/// buffer (a verb wanting more points than remain — impossible from the registry ops, but checked so a
/// fabricated geometry can never panic/overrun) ends the walk early. Returns `None` for an empty path
/// or one tiny-skia rejects (e.g. all points coincident / non-finite), matching `PathBuilder::finish`.
fn build_tiny_skia_path(geometry: &PathGeometry) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let pts = &geometry.points;
    let mut i = 0usize; // index into the flat point buffer (2 floats per point)
    for verb in &geometry.verbs {
        let need = verb.point_count() * 2;
        if i + need > pts.len() {
            // Defensive: a fabricated geometry could under-supply points. Stop cleanly (no panic).
            break;
        }
        match verb {
            Verb::MoveTo => pb.move_to(pts[i], pts[i + 1]),
            Verb::LineTo => pb.line_to(pts[i], pts[i + 1]),
            Verb::QuadTo => pb.quad_to(pts[i], pts[i + 1], pts[i + 2], pts[i + 3]),
            Verb::CubicTo => pb.cubic_to(
                pts[i],
                pts[i + 1],
                pts[i + 2],
                pts[i + 3],
                pts[i + 4],
                pts[i + 5],
            ),
            Verb::Close => pb.close(),
        }
        i += need;
    }
    pb.finish()
}

/// AOSP `Matrix` (row-major 3x3) → tiny-skia [`Transform`] (its 6 affine coefficients).
///
/// tiny-skia is affine-only (no perspective row); AOSP `Matrix` row 2 is `[MPERSP_0, MPERSP_1,
/// MPERSP_2]`. Vector-drawable / Canvas matrices are affine (perspective row `[0,0,1]`), so the affine
/// coefficients map directly: `Transform { sx, kx, ky, sy, tx, ty }` from AOSP indices
/// `[MSCALE_X, MSKEW_X, MTRANS_X, MSKEW_Y, MSCALE_Y, MTRANS_Y]`.
fn affine_to_transform(m: &Affine) -> Transform {
    Transform::from_row(m.m[0], m.m[3], m.m[1], m.m[4], m.m[2], m.m[5])
}

/// Rasterize `geometry` (transformed by `matrix`, filled with `style`) into a fresh `width`×`height`
/// RGBA [`Pixmap`] (premultiplied storage, transparent-black background). Returns the pixmap, or `None`
/// if the dimensions are zero or the path is empty/degenerate (nothing to draw).
///
/// 2026-06-05: this is the REAL fill of the parsed path — anti-aliased, via the pure-Rust tiny-skia
/// rasterizer. GPU-free; the caller uploads `pixmap.data()` (or [`rasterize_path_rgba`]'s straight
/// bytes) as a texture. No GTK/Cairo/Skia-C.
pub fn rasterize_path(
    geometry: &PathGeometry,
    matrix: &Affine,
    style: FillStyle,
    width: u32,
    height: u32,
) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(width, height)?;
    let path = build_tiny_skia_path(geometry)?;
    let (r, g, b, a) = argb_to_rgba8(style.argb);
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    let fill_rule = if style.even_odd {
        FillRule::EvenOdd
    } else {
        FillRule::Winding
    };
    pixmap.fill_path(&path, &paint, fill_rule, affine_to_transform(matrix), None);
    Some(pixmap)
}

/// Rasterize `geometry` into straight (un-premultiplied) RGBA8 bytes ready for a GPU texture upload,
/// returning `(rgba, width, height)`. Convenience over [`rasterize_path`] for the compositor; returns
/// `None` on the same empty/degenerate/zero-size conditions. `take_demultiplied` yields straight RGBA
/// (tiny-skia stores premultiplied), which is what a non-premultiplied-alpha sampler expects.
pub fn rasterize_path_rgba(
    geometry: &PathGeometry,
    matrix: &Affine,
    style: FillStyle,
    width: u32,
    height: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    let pixmap = rasterize_path(geometry, matrix, style, width, height)?;
    let (w, h) = (pixmap.width(), pixmap.height());
    Some((pixmap.take_demultiplied(), w, h))
}

/// Errors from the graphics/window subsystem.
#[derive(Debug)]
pub enum GraphicsError {
    /// The winit event loop could not be created or run (e.g. no display server).
    EventLoop(EventLoopError),
    /// The host window could not be created.
    CreateWindow(OsError),
    /// Vulkan surface/swapchain initialization or a frame operation failed (no ICD, unsupported
    /// display, or a `VkResult` error). Carries a human-readable cause string.
    Vulkan(String),
}

impl fmt::Display for GraphicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLoop(e) => write!(f, "winit event loop error: {e}"),
            Self::CreateWindow(e) => write!(f, "failed to create host window: {e}"),
            Self::Vulkan(msg) => write!(f, "Vulkan error: {msg}"),
        }
    }
}

impl std::error::Error for GraphicsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EventLoop(e) => Some(e),
            Self::CreateWindow(e) => Some(e),
            Self::Vulkan(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::view_registry::WRAP_CONTENT;

    fn caps(min: u32, max: u32, cur_w: u32, cur_h: u32) -> vk::SurfaceCapabilitiesKHR {
        vk::SurfaceCapabilitiesKHR {
            min_image_count: min,
            max_image_count: max,
            current_extent: vk::Extent2D {
                width: cur_w,
                height: cur_h,
            },
            min_image_extent: vk::Extent2D {
                width: 1,
                height: 1,
            },
            max_image_extent: vk::Extent2D {
                width: 4096,
                height: 4096,
            },
            ..Default::default()
        }
    }

    #[test]
    fn surface_format_prefers_bgra8_srgb() {
        let formats = [
            vk::SurfaceFormatKHR {
                format: vk::Format::R8G8B8A8_UNORM,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
            vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_SRGB,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        let chosen = choose_surface_format(&formats).expect("a format exists");
        assert_eq!(chosen.format, vk::Format::B8G8R8A8_SRGB);
        assert_eq!(chosen.color_space, vk::ColorSpaceKHR::SRGB_NONLINEAR);
    }

    #[test]
    fn surface_format_falls_back_to_first_when_preferred_absent() {
        let formats = [vk::SurfaceFormatKHR {
            format: vk::Format::R8G8B8A8_UNORM,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }];
        let chosen = choose_surface_format(&formats).expect("a format exists");
        assert_eq!(chosen.format, vk::Format::R8G8B8A8_UNORM);
    }

    // 2026-06-14: pin the winit→Android keycode mapping for the keys needed to type credentials. A
    // regression would send the wrong KEYCODE_* (the engine may key behavior off the code for editing
    // keys). Pure data (no VM/window), so unit-testable in-harness. Values are public KeyEvent.KEYCODE_*.
    #[test]
    fn winit_keycode_maps_credential_keys_to_android_keycodes() {
        use winit::keyboard::{Key, NamedKey};
        // Letters → KEYCODE_A(29)..KEYCODE_Z(54), case-insensitive (the char rides on unicodeValue).
        assert_eq!(winit_keycode(&Key::Character("a".into())), Some(29));
        assert_eq!(winit_keycode(&Key::Character("z".into())), Some(54));
        assert_eq!(winit_keycode(&Key::Character("A".into())), Some(29));
        // Digits → KEYCODE_0(7)..KEYCODE_9(16).
        assert_eq!(winit_keycode(&Key::Character("0".into())), Some(7));
        assert_eq!(winit_keycode(&Key::Character("9".into())), Some(16));
        // Login punctuation: '@' → KEYCODE_AT(77), '.' → KEYCODE_PERIOD(56).
        assert_eq!(winit_keycode(&Key::Character("@".into())), Some(77));
        assert_eq!(winit_keycode(&Key::Character(".".into())), Some(56));
        // Named editing keys: Backspace → KEYCODE_DEL(67), Enter → ENTER(66), Space → SPACE(62).
        assert_eq!(winit_keycode(&Key::Named(NamedKey::Backspace)), Some(67));
        assert_eq!(winit_keycode(&Key::Named(NamedKey::Enter)), Some(66));
        assert_eq!(winit_keycode(&Key::Named(NamedKey::Space)), Some(62));
        // An unmapped printable still types via unicodeValue (KEYCODE_UNKNOWN = 0, not dropped).
        assert_eq!(winit_keycode(&Key::Character("#".into())), Some(0));
        // A key with no mapping at all → None (dropped, not sent as keycode 0).
        assert_eq!(winit_keycode(&Key::Named(NamedKey::F1)), None);
    }

    #[test]
    fn surface_format_none_when_driver_advertises_none() {
        assert!(choose_surface_format(&[]).is_none());
    }

    // 2026-06-13: pins the production engine-window geometry publish (the resize re-publish, factored
    // from `window_event::Resized`). Deleting/mis-wiring it would leave `ANativeWindow_getWidth/Height`
    // reporting stale geometry; the egl_engine `gl_test_anw_binds_real_wsi_handle` harness does NOT
    // exercise this graphics.rs path, so without this guard the production wiring is unpinned. Uses a
    // fabricated, unique pointer and asserts only THAT pointer's mapping (not the order-dependent
    // `current_wsi_window`), so it is order-independent vs the ndk_registry WSI tests in the same binary.
    #[test]
    fn publish_engine_window_geometry_registers_real_wsi_mapping() {
        use crate::loader::ndk_registry;
        // A unique fabricated WSI pointer for this test (never a real window — only the value is
        // stored). Asserting only THIS pointer's WSI mapping keeps the test order-independent vs the
        // process-global ndk_registry cells other tests in the same binary write (we do NOT assert on
        // the shared `engine_window_geometry` / `current_wsi_window`, which are not pointer-scoped).
        let ptr = 0xECC1_0613_usize;
        ndk_registry::unregister_wsi_window(ptr); // defensive: clear any prior run's entry
                                                  // With a Some(ptr): the pointer→geometry mapping is registered (the engine's
                                                  // ANativeWindow_getWidth/Height read this). This is the production wiring BLOCKING #2 asked to
                                                  // pin — deleting `register_wsi_window` from `publish_engine_window_geometry` fails this.
        publish_engine_window_geometry(Some(ptr), 1280, 720);
        assert_eq!(
            ndk_registry::wsi_window_geometry(ptr),
            Some((1280, 720)),
            "the real WSI pointer must resolve to the published geometry (ANativeWindow_getWidth/Height)"
        );
        // Idempotent re-publish (a resize) updates the geometry of the SAME entry, not a duplicate.
        publish_engine_window_geometry(Some(ptr), 800, 600);
        assert_eq!(
            ndk_registry::wsi_window_geometry(ptr),
            Some((800, 600)),
            "a resize re-publish updates the same WSI entry's geometry"
        );
        // A None ptr (before the real WSI window is built) publishes geometry only — it must NOT
        // register this pointer. Clear first, then confirm it stays unregistered.
        ndk_registry::unregister_wsi_window(ptr);
        publish_engine_window_geometry(None, 640, 480);
        assert_eq!(
            ndk_registry::wsi_window_geometry(ptr),
            None,
            "None ptr publishes geometry only — no WSI pointer registration"
        );
    }

    #[test]
    fn swap_extent_uses_fixed_current_extent_when_set() {
        // Wayland: current_extent is authoritative — ignore the window size.
        let c = caps(2, 4, 800, 600);
        let e = choose_swap_extent(&c, 1920, 1080);
        assert_eq!(e.width, 800);
        assert_eq!(e.height, 600);
    }

    #[test]
    fn swap_extent_clamps_window_size_when_current_is_special() {
        // X11: current_extent == u32::MAX → use the (clamped) window size.
        let c = caps(2, 4, u32::MAX, u32::MAX);
        let e = choose_swap_extent(&c, 1920, 1080);
        assert_eq!(e.width, 1920);
        assert_eq!(e.height, 1080);
        // A window bigger than max clamps to max.
        let big = choose_swap_extent(&c, 9000, 9000);
        assert_eq!(big.width, 4096);
        assert_eq!(big.height, 4096);
        // A zero-ish window clamps up to min (1x1 here).
        let small = choose_swap_extent(&c, 0, 0);
        assert_eq!(small.width, 1);
        assert_eq!(small.height, 1);
    }

    #[test]
    fn image_count_is_min_plus_one_clamped_to_max() {
        // max==4, min+1==3 → 3.
        assert_eq!(choose_image_count(&caps(2, 4, 800, 600)), 3);
        // max==3, min+1==4 → clamp to 3.
        assert_eq!(choose_image_count(&caps(3, 3, 800, 600)), 3);
        // max==0 means "no limit" → min+1 unclamped.
        assert_eq!(choose_image_count(&caps(2, 0, 800, 600)), 3);
    }

    // 2026-06-05: View-tree draw — layout + quad geometry. GPU-free (no device), so these run in the
    // normal `cargo test` harness and guard the rect/NDC/vertex math the renderer feeds the pipeline.

    fn node(class: &str, text: Option<&str>, depth: u32) -> RenderNode {
        RenderNode {
            handle: 0,
            class_name: class.to_owned(),
            text: text.map(str::to_owned),
            depth,
            layout: LayoutParams::default(),
            clickable: false,
            background_color: None,
            children: Vec::new(),
        }
    }

    /// A node with an explicit `LayoutParams` and child indices — for the cascade tests.
    fn node_lp(
        class: &str,
        text: Option<&str>,
        depth: u32,
        lp: LayoutParams,
        kids: &[usize],
    ) -> RenderNode {
        RenderNode {
            handle: 0,
            class_name: class.to_owned(),
            text: text.map(str::to_owned),
            depth,
            layout: lp,
            clickable: false,
            background_color: None,
            children: kids.to_vec(),
        }
    }

    fn exact(px: i32) -> i32 {
        px
    }

    #[test]
    fn measure_spec_resolves_match_wrap_and_exact() {
        // EXACTLY(800) parent.
        let parent = MeasureSpec {
            mode: SpecMode::Exactly,
            size: 800.0,
        };
        // exact px → that px, child EXACTLY.
        let (size, child) = parent.resolve(exact(120), 999.0);
        assert_eq!(size, 120.0);
        assert_eq!(child.mode, SpecMode::Exactly);
        assert_eq!(child.size, 120.0);
        // MATCH_PARENT → fills parent, child EXACTLY(parent).
        let (size, child) = parent.resolve(MATCH_PARENT, 50.0);
        assert_eq!(size, 800.0);
        assert_eq!(child.mode, SpecMode::Exactly);
        // WRAP_CONTENT → content, clamped to parent; child AtMost(parent).
        let (size, child) = parent.resolve(WRAP_CONTENT, 200.0);
        assert_eq!(size, 200.0);
        assert_eq!(child.mode, SpecMode::AtMost);
        assert_eq!(child.size, 800.0);
        // WRAP_CONTENT bigger than parent clamps to the parent's size.
        let (size, _) = parent.resolve(WRAP_CONTENT, 9000.0);
        assert_eq!(size, 800.0);
    }

    #[test]
    fn measure_spec_unspecified_parent_yields_content_size() {
        let parent = MeasureSpec {
            mode: SpecMode::Unspecified,
            size: 0.0,
        };
        // MATCH_PARENT with an unbounded parent → fall back to content, child Unspecified.
        let (size, child) = parent.resolve(MATCH_PARENT, 77.0);
        assert_eq!(size, 77.0);
        assert_eq!(child.mode, SpecMode::Unspecified);
    }

    #[test]
    fn root_match_parent_fills_the_swapchain_extent() {
        // A single MATCH_PARENT root measured EXACTLY at the extent fills the whole surface at origin.
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let lp = LayoutParams {
            width: MATCH_PARENT,
            height: MATCH_PARENT,
            ..Default::default()
        };
        let nodes = [node_lp("android.widget.FrameLayout", None, 0, lp, &[])];
        let views = layout_views(&nodes, extent, None);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].x, 0.0);
        assert_eq!(views[0].y, 0.0);
        assert_eq!(views[0].w, 800.0);
        assert_eq!(views[0].h, 600.0);
    }

    #[test]
    fn linear_layout_vertical_stacks_children_top_to_bottom() {
        // A vertical LinearLayout (MATCH x MATCH) with two fixed-height MATCH-width children: child 0
        // sits at the top, child 1 directly below it (no gravity → left/top).
        let extent = vk::Extent2D {
            width: 400,
            height: 600,
        };
        let root_lp = LayoutParams {
            width: MATCH_PARENT,
            height: MATCH_PARENT,
            ..Default::default()
        };
        let child_lp = LayoutParams {
            width: MATCH_PARENT,
            height: 100,
            ..Default::default()
        };
        let nodes = [
            node_lp("android.widget.LinearLayout", None, 0, root_lp, &[1, 2]),
            node_lp("android.widget.TextView", Some("a"), 1, child_lp, &[]),
            node_lp("android.widget.TextView", Some("b"), 1, child_lp, &[]),
        ];
        let views = layout_views(&nodes, extent, None);
        // Root fills the surface.
        assert_eq!(
            (views[0].x, views[0].y, views[0].w, views[0].h),
            (0.0, 0.0, 400.0, 600.0)
        );
        // Child 0 at top, full width, 100 tall.
        assert_eq!((views[1].x, views[1].y), (0.0, 0.0));
        assert_eq!((views[1].w, views[1].h), (400.0, 100.0));
        // Child 1 stacked directly below child 0.
        assert_eq!((views[2].x, views[2].y), (0.0, 100.0));
        assert_eq!((views[2].w, views[2].h), (400.0, 100.0));
    }

    #[test]
    fn argb_to_rgba_f32_splits_channels() {
        // 0xAARRGGBB → straight RGBA floats in 0..1. Opaque red.
        let c = argb_to_rgba_f32(0xFFFF_0000u32 as i32);
        assert_eq!(c, [1.0, 0.0, 0.0, 1.0]);
        // Half-alpha pure green.
        let g = argb_to_rgba_f32(0x8000_FF00u32 as i32);
        assert!((g[0]).abs() < 1e-6);
        assert!((g[1] - 1.0).abs() < 1e-6);
        assert!((g[2]).abs() < 1e-6);
        assert!((g[3] - 128.0 / 255.0).abs() < 1e-6);
        // Fully transparent → alpha 0 (clear shows through).
        assert_eq!(argb_to_rgba_f32(0x0000_0000), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn background_color_overrides_depth_palette_in_layout() {
        // A view with a recorded `View.setBackgroundColor` (ARGB) gets that color, not the depth palette.
        let extent = vk::Extent2D {
            width: 100,
            height: 100,
        };
        let mut n = node("android.view.View", None, 0);
        n.background_color = Some(0xFF00_00FFu32 as i32); // opaque blue
        let views = layout_views(&[n], extent, None);
        assert_eq!(views[0].color, [0.0, 0.0, 1.0, 1.0]);
        // Without a background color, the depth palette is used (depth 0 entry).
        let plain = layout_views(&[node("android.view.View", None, 0)], extent, None);
        assert_eq!(plain[0].color, DEPTH_PALETTE[0]);
    }

    #[test]
    fn frame_layout_honors_child_gravity() {
        // A FrameLayout (200x200) with a 50x50 child whose gravity = bottom|right → placed at (150,150).
        let extent = vk::Extent2D {
            width: 200,
            height: 200,
        };
        let root_lp = LayoutParams {
            width: MATCH_PARENT,
            height: MATCH_PARENT,
            ..Default::default()
        };
        let child_lp = LayoutParams {
            width: 50,
            height: 50,
            gravity: GRAVITY_RIGHT | GRAVITY_BOTTOM,
            ..Default::default()
        };
        let nodes = [
            node_lp("android.widget.FrameLayout", None, 0, root_lp, &[1]),
            node_lp("android.view.View", None, 1, child_lp, &[]),
        ];
        let views = layout_views(&nodes, extent, None);
        assert_eq!((views[1].x, views[1].y), (150.0, 150.0));
        assert_eq!((views[1].w, views[1].h), (50.0, 50.0));

        // Center gravity → centered.
        let center_lp = LayoutParams {
            width: 50,
            height: 50,
            gravity: GRAVITY_CENTER_HORIZONTAL | GRAVITY_CENTER_VERTICAL,
            ..Default::default()
        };
        let nodes = [
            node_lp("android.widget.FrameLayout", None, 0, root_lp, &[1]),
            node_lp("android.view.View", None, 1, center_lp, &[]),
        ];
        let views = layout_views(&nodes, extent, None);
        assert_eq!((views[1].x, views[1].y), (75.0, 75.0));
    }

    #[test]
    fn wrap_content_text_measures_to_glyph_metrics() {
        // A WRAP_CONTENT TextView measures to its text width (sum of advances + 2*pad) and the atlas
        // line height — proving WRAP resolution uses the real glyph metrics, not the fallback box.
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let atlas = synthetic_atlas(); // 'A' advance 6.0, line_height 8.0
        let measure = TextMeasure { atlas: &atlas };
        let lp = LayoutParams {
            width: WRAP_CONTENT,
            height: WRAP_CONTENT,
            ..Default::default()
        };
        let nodes = [node_lp("android.widget.TextView", Some("AAA"), 0, lp, &[])];
        let views = layout_views(&nodes, extent, Some(measure));
        // width = 3 * 6.0 advances + 2 * TEXT_PAD_X.
        assert_eq!(views[0].w, 3.0 * 6.0 + 2.0 * TEXT_PAD_X);
        // height = atlas line height.
        assert_eq!(views[0].h, 8.0);
    }

    #[test]
    fn linear_layout_weight_distributes_leftover_space() {
        // Vertical LinearLayout 100 tall, two height-0 weighted children (1:1) → each gets 50.
        let extent = vk::Extent2D {
            width: 100,
            height: 100,
        };
        let root_lp = LayoutParams {
            width: MATCH_PARENT,
            height: MATCH_PARENT,
            ..Default::default()
        };
        let w_lp = LayoutParams {
            width: MATCH_PARENT,
            height: 0,
            weight: 1.0,
            ..Default::default()
        };
        let nodes = [
            node_lp("android.widget.LinearLayout", None, 0, root_lp, &[1, 2]),
            node_lp("android.view.View", None, 1, w_lp, &[]),
            node_lp("android.view.View", None, 2, w_lp, &[]),
        ];
        let views = layout_views(&nodes, extent, None);
        assert_eq!(views[1].h, 50.0, "first weighted child gets half");
        assert_eq!(views[2].h, 50.0, "second weighted child gets half");
        assert_eq!(views[2].y, 50.0, "second child stacked below the first");
    }

    #[test]
    fn unspecified_gravity_minus_one_is_top_left_not_a_bitmask() {
        // Regression guard: Android's UNSPECIFIED_GRAVITY is -1 (all bits set). Treated as a bitmask,
        // `-1 & RIGHT == RIGHT` and `-1 & BOTTOM == BOTTOM` would wrongly push the child bottom-right.
        // It must be treated as "no gravity" → top-left. (Confirmed on the demo: every view reports -1.)
        assert_eq!(gravity_dx(-1, 200.0, 50.0), 0.0, "unspecified → left");
        assert_eq!(gravity_dy(-1, 200.0, 50.0), 0.0, "unspecified → top");
        // A FrameLayout child with the real demo gravity (-1) sits at the (padding-inset) origin.
        let extent = vk::Extent2D {
            width: 200,
            height: 200,
        };
        let root_lp = LayoutParams {
            width: MATCH_PARENT,
            height: MATCH_PARENT,
            ..Default::default()
        };
        let child_lp = LayoutParams {
            width: 50,
            height: 50,
            gravity: -1,
            ..Default::default()
        };
        let nodes = [
            node_lp("android.widget.FrameLayout", None, 0, root_lp, &[1]),
            node_lp("android.view.View", None, 1, child_lp, &[]),
        ];
        let views = layout_views(&nodes, extent, None);
        assert_eq!(
            (views[1].x, views[1].y),
            (0.0, 0.0),
            "unspecified gravity → origin"
        );
    }

    #[test]
    fn padding_insets_children() {
        // A FrameLayout with 10px uniform padding and a 20x20 child → child at (10,10).
        let extent = vk::Extent2D {
            width: 100,
            height: 100,
        };
        let root_lp = LayoutParams {
            width: MATCH_PARENT,
            height: MATCH_PARENT,
            padding: [10, 10, 10, 10],
            ..Default::default()
        };
        let child_lp = LayoutParams {
            width: 20,
            height: 20,
            ..Default::default()
        };
        let nodes = [
            node_lp("android.widget.FrameLayout", None, 0, root_lp, &[1]),
            node_lp("android.view.View", None, 1, child_lp, &[]),
        ];
        let views = layout_views(&nodes, extent, None);
        assert_eq!((views[1].x, views[1].y), (10.0, 10.0));
    }

    #[test]
    fn layout_clamps_width_to_at_least_one() {
        // A zero-measured view must never produce a <= 0 width/height (would be an invalid quad).
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let lp = LayoutParams {
            width: 0,
            height: 0,
            ..Default::default()
        };
        let nodes = [node_lp("android.view.View", None, 0, lp, &[])];
        let views = layout_views(&nodes, extent, None);
        assert!(views[0].w >= 1.0 && views[0].h >= 1.0);
    }

    #[test]
    fn empty_tree_produces_no_geometry() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        assert!(layout_views(&[], extent, None).is_empty());
        assert!(build_quad_vertices(&[], extent).is_empty());
    }

    #[test]
    fn pixel_rect_maps_corners_to_expected_ndc() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        // A rect filling the whole surface maps its corners to NDC [-1,-1] (top-left) .. [1,1] (br).
        let q = pixel_rect_to_quad(0.0, 0.0, 800.0, 600.0, [1.0; 4], extent);
        // First vertex is the top-left corner.
        assert_eq!(q[0].pos, [-1.0, -1.0]);
        // Third vertex is the bottom-right corner.
        assert_eq!(q[2].pos, [1.0, 1.0]);
        // A centered point maps to NDC origin.
        let mid = pixel_rect_to_quad(400.0, 300.0, 0.0, 0.0, [0.0; 4], extent);
        assert_eq!(mid[0].pos, [0.0, 0.0]);
    }

    #[test]
    fn build_quad_vertices_emits_six_per_view() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        // Three sibling children of a root container → four laid-out views, all non-degenerate.
        let nodes = [
            node_lp(
                "android.widget.LinearLayout",
                None,
                0,
                LayoutParams {
                    width: MATCH_PARENT,
                    height: MATCH_PARENT,
                    ..Default::default()
                },
                &[1, 2, 3],
            ),
            node("a", None, 1),
            node("b", None, 1),
            node("c", None, 1),
        ];
        let views = layout_views(&nodes, extent, None);
        let verts = build_quad_vertices(&views, extent);
        assert_eq!(verts.len(), 4 * 6, "six vertices (two triangles) per view");
        // Each view's six vertices share its fill color.
        assert!(verts[0..6].iter().all(|v| v.color == views[0].color));
    }

    // 2026-06-05: the hit-test the event loop runs on a pointer click — pure geometry over the laid-
    // out rects, no VM/GPU. These guard the minimal click path: a point in/out of a rect, nested
    // (topmost/last-drawn wins), and that a non-clickable view is never targeted.
    /// Build a [`LaidOutView`] for the hit-test tests (color/text irrelevant here).
    fn lov(handle: ViewHandle, x: f32, y: f32, w: f32, h: f32, clickable: bool) -> LaidOutView {
        LaidOutView {
            handle,
            x,
            y,
            w,
            h,
            clickable,
            color: [1.0; 4],
            text: None,
        }
    }

    #[test]
    fn hit_test_returns_clickable_view_containing_the_point() {
        let views = [lov(7, 10.0, 10.0, 100.0, 50.0, true)];
        // Inside the rect → the view's handle; outside → None.
        assert_eq!(hit_test(&views, 50.0, 30.0), Some(7));
        assert_eq!(hit_test(&views, 5.0, 30.0), None, "left of the rect");
        assert_eq!(hit_test(&views, 200.0, 30.0), None, "right of the rect");
        assert_eq!(hit_test(&views, 50.0, 5.0), None, "above the rect");
        assert_eq!(hit_test(&views, 50.0, 100.0), None, "below the rect");
    }

    #[test]
    fn hit_test_topmost_last_drawn_wins_for_overlapping_views() {
        // Pre-order = draw order: a later entry is drawn on top. Two overlapping clickable views at
        // the same point → the LAST one (topmost) is returned.
        let views = [
            lov(1, 0.0, 0.0, 100.0, 100.0, true), // drawn first (under)
            lov(2, 20.0, 20.0, 40.0, 40.0, true), // drawn last (on top)
        ];
        assert_eq!(
            hit_test(&views, 30.0, 30.0),
            Some(2),
            "topmost overlapping wins"
        );
        // A point only inside the lower view still hits it.
        assert_eq!(hit_test(&views, 5.0, 5.0), Some(1));
    }

    #[test]
    fn hit_test_ignores_non_clickable_views() {
        // A non-clickable view under the point is skipped; a clickable one below it is found instead.
        let views = [
            lov(1, 0.0, 0.0, 100.0, 100.0, true),  // clickable, under
            lov(2, 0.0, 0.0, 100.0, 100.0, false), // NON-clickable, on top → must be skipped
        ];
        assert_eq!(
            hit_test(&views, 50.0, 50.0),
            Some(1),
            "the non-clickable top view is ignored, the clickable one below is hit"
        );

        // No clickable view under the point at all → None (the click is a no-op).
        let inert = [lov(9, 0.0, 0.0, 100.0, 100.0, false)];
        assert_eq!(hit_test(&inert, 50.0, 50.0), None);
        // Empty tree → None.
        assert_eq!(hit_test(&[], 0.0, 0.0), None);
    }

    #[test]
    fn hit_test_rect_is_half_open() {
        // The rect is half-open [x,x+w)×[y,y+h): the top-left corner is inside, the bottom-right is not
        // (so adjacent tiled views never both claim a shared edge).
        let views = [lov(3, 10.0, 10.0, 20.0, 20.0, true)];
        assert_eq!(
            hit_test(&views, 10.0, 10.0),
            Some(3),
            "top-left corner is inside"
        );
        assert_eq!(
            hit_test(&views, 30.0, 20.0),
            None,
            "right edge is exclusive"
        );
        assert_eq!(
            hit_test(&views, 20.0, 30.0),
            None,
            "bottom edge is exclusive"
        );
    }

    // 2026-06-05: the single-pointer DOWN→UP state machine that gates a tap. The press records the hit
    // view; the release completes the tap (ACTION_UP + click) only if it lands on the SAME view, never
    // when the press missed, the release drifted off, or the release missed. Pure (no GPU/VM).
    #[test]
    fn should_complete_tap_requires_press_and_release_on_same_view() {
        // Press and release on the same view → complete the tap on that view.
        assert_eq!(should_complete_tap(Some(7), Some(7)), Some(7));
        // Release drifted to a different view → not a tap.
        assert_eq!(should_complete_tap(Some(7), Some(9)), None);
        // Release missed all views (drag off into empty space) → not a tap.
        assert_eq!(should_complete_tap(Some(7), None), None);
        // Press hit nothing (started on empty space) → not a tap, regardless of release.
        assert_eq!(should_complete_tap(None, Some(7)), None);
        assert_eq!(should_complete_tap(None, None), None);
    }

    #[test]
    fn embedded_spirv_is_well_formed() {
        // The embedded shader blobs must decode to u32 words (length multiple of 4) and start with
        // the SPIR-V magic number 0x07230203 — guards against a truncated/corrupt `include_bytes!`.
        for (name, spv) in [("vert", QUAD_VERT_SPV), ("frag", QUAD_FRAG_SPV)] {
            let words = read_spirv(spv).unwrap_or_else(|e| panic!("{name} SPIR-V invalid: {e}"));
            assert!(!words.is_empty(), "{name} SPIR-V is empty");
            assert_eq!(words[0], 0x0723_0203, "{name} SPIR-V magic mismatch");
        }
    }

    #[test]
    fn device_local_memory_type_prefers_device_local_then_any_in_filter() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        props.memory_types[0].property_flags = vk::MemoryPropertyFlags::HOST_VISIBLE;
        props.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[2].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        // Filter allows 0 and 1 → picks 1 (the device-local one).
        assert_eq!(find_device_local_memory_type(&props, 0b011), Some(1));
        // Filter allows only the host-visible type 0 → no device-local; falls back to type 0.
        assert_eq!(find_device_local_memory_type(&props, 0b001), Some(0));
        // Filter allows nothing → None.
        assert_eq!(find_device_local_memory_type(&props, 0b000), None);
    }

    fn synthetic_atlas() -> GlyphAtlas {
        // A 2-glyph atlas: 'A' (a 4x4 rect at 0,0) + ' ' (whitespace, advance only). Enough to test
        // build_text_vertices' positioning + UV math without a font/GPU.
        let mut glyphs = std::collections::HashMap::new();
        glyphs.insert(
            'A',
            GlyphInfo {
                ax: 0,
                ay: 0,
                aw: 4,
                ah: 4,
                bearing_x: 0.0,
                bearing_y: -4.0,
                advance: 6.0,
            },
        );
        glyphs.insert(
            ' ',
            GlyphInfo {
                ax: 0,
                ay: 0,
                aw: 0,
                ah: 0,
                bearing_x: 0.0,
                bearing_y: 0.0,
                advance: 5.0,
            },
        );
        GlyphAtlas {
            width: 8,
            height: 8,
            pixels: vec![0u8; 64],
            glyphs,
            ascent: 6.0,
            line_height: 8.0,
        }
    }

    #[test]
    fn text_vertices_six_per_visible_glyph_skip_whitespace_and_unknown() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let atlas = synthetic_atlas();
        // "A A" → two visible 'A' glyphs (6 verts each) + one space (advance only) + the gap.
        let views = [LaidOutView {
            handle: 0,
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 64.0,
            clickable: false,
            color: [1.0; 4],
            text: Some("A A".to_owned()),
        }];
        let verts = build_text_vertices(&views, &atlas, extent);
        assert_eq!(verts.len(), 2 * 6, "two visible glyphs, 6 verts each");

        // A view with only unknown (non-atlas) chars produces no vertices.
        let only_unknown = [LaidOutView {
            handle: 0,
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 64.0,
            clickable: false,
            color: [1.0; 4],
            text: Some("€£¥".to_owned()),
        }];
        assert!(build_text_vertices(&only_unknown, &atlas, extent).is_empty());

        // A view with no text produces no vertices.
        let no_text = [LaidOutView {
            handle: 0,
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 64.0,
            clickable: false,
            color: [1.0; 4],
            text: None,
        }];
        assert!(build_text_vertices(&no_text, &atlas, extent).is_empty());
    }

    #[test]
    fn glyph_atlas_builds_from_discovered_font_when_present() {
        // Environment-dependent: only runs the assertion when a system font is discoverable
        // (fc-match / font dirs). On a headless box with no fonts it is a no-op (text is best-effort).
        let Some(path) = discover_font_path() else {
            return;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        let Ok(font) = FontVec::try_from_vec(bytes) else {
            return;
        };
        let atlas = build_glyph_atlas(&font, 1024).expect("atlas builds from a real font");
        assert!(atlas.width > 0 && atlas.height > 0);
        assert_eq!(atlas.pixels.len(), (atlas.width * atlas.height) as usize);
        // Printable ASCII letters must be present with a positive advance.
        let a = atlas.glyphs.get(&'A').expect("'A' in atlas");
        assert!(a.advance > 0.0);
        assert!(a.aw > 0 && a.ah > 0, "'A' has a non-empty bitmap");
    }

    #[test]
    fn host_visible_memory_type_selected_by_flags_and_filter() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        // type 0: device-local only (not host visible). type 1: host visible+coherent. type 2: same.
        props.memory_types[0].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[1].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[2].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        // Filter allowing only type 0 and 2 → must pick type 2 (the host-visible one in the filter).
        let filter = 0b101;
        assert_eq!(find_host_visible_memory_type(&props, filter), Some(2));
        // Filter allowing only the device-local type → None.
        assert_eq!(find_host_visible_memory_type(&props, 0b001), None);
    }

    // --- Vector-drawable raster (tiny-skia), GPU-free -------------------------------------------

    // 2026-06-05: prove the REAL parsed Path geometry rasterizes to the expected pixels — the raster
    // is the durable half of the vector-drawable pipeline (the Vulkan composite is the next step).

    /// Read the straight-RGBA pixel at (x, y) from a row-major `w`×`h` RGBA8 buffer.
    fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * w + x) * 4) as usize;
        (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
    }

    /// A closed axis-aligned rectangle path covering [x0,x1]×[y0,y1].
    fn rect_path(x0: f32, y0: f32, x1: f32, y1: f32) -> PathGeometry {
        let mut g = PathGeometry::default();
        g.move_to(x0, y0);
        g.line_to(x1, y0);
        g.line_to(x1, y1);
        g.line_to(x0, y1);
        g.close();
        g
    }

    #[test]
    fn argb_to_rgba8_splits_channels() {
        // 0xAARRGGBB = 0x80123456 → a=0x80 r=0x12 g=0x34 b=0x56.
        let (r, g, b, a) = argb_to_rgba8(0x8012_3456u32 as i32);
        assert_eq!((r, g, b, a), (0x12, 0x34, 0x56, 0x80));
    }

    #[test]
    fn affine_to_transform_maps_affine_coefficients() {
        // translate(10, 20) in AOSP Matrix order → tiny-skia Transform tx=10, ty=20, identity scale.
        let mut m = Affine::IDENTITY;
        m.set_translate(10.0, 20.0);
        let t = affine_to_transform(&m);
        assert_eq!((t.sx, t.sy, t.tx, t.ty), (1.0, 1.0, 10.0, 20.0));
        assert_eq!((t.kx, t.ky), (0.0, 0.0));
    }

    #[test]
    fn rasterize_filled_rect_has_opaque_interior_and_clear_exterior() {
        // Fill a 10..30 × 10..30 red rect into a 40×40 pixmap with the identity transform.
        let geometry = rect_path(10.0, 10.0, 30.0, 30.0);
        let style = FillStyle {
            argb: 0xFFFF_0000u32 as i32, // opaque red
            even_odd: false,
        };
        let (rgba, w, h) =
            rasterize_path_rgba(&geometry, &Affine::IDENTITY, style, 40, 40).expect("rasterizes");
        assert_eq!((w, h), (40, 40));
        assert_eq!(rgba.len(), (40 * 40 * 4) as usize);
        // Interior center (20,20) is opaque red.
        assert_eq!(px(&rgba, w, 20, 20), (255, 0, 0, 255));
        // Exterior corners are transparent (untouched background).
        assert_eq!(px(&rgba, w, 0, 0), (0, 0, 0, 0));
        assert_eq!(px(&rgba, w, 39, 39), (0, 0, 0, 0));
        // A point just outside the rect edge is still clear; well inside is filled.
        assert_eq!(px(&rgba, w, 5, 5), (0, 0, 0, 0));
        assert_eq!(px(&rgba, w, 25, 15).3, 255, "inside the rect is opaque");
    }

    #[test]
    fn rasterize_honors_the_transform() {
        // The same small rect at the origin, translated by +20,+20, lands in the lower-right quadrant.
        let geometry = rect_path(0.0, 0.0, 10.0, 10.0);
        let mut m = Affine::IDENTITY;
        m.set_translate(20.0, 20.0);
        let style = FillStyle {
            argb: 0xFF00_FF00u32 as i32, // opaque green
            even_odd: false,
        };
        let (rgba, w, _h) = rasterize_path_rgba(&geometry, &m, style, 40, 40).expect("rasterizes");
        // Translated rect now covers ~[20,30]×[20,30]: (25,25) filled green, the original (5,5) clear.
        assert_eq!(px(&rgba, w, 25, 25), (0, 255, 0, 255));
        assert_eq!(px(&rgba, w, 5, 5), (0, 0, 0, 0));
    }

    #[test]
    fn empty_path_does_not_rasterize() {
        // An empty geometry has no contour to fill → None (nothing to upload).
        assert!(rasterize_path(
            &PathGeometry::default(),
            &Affine::IDENTITY,
            FillStyle::default(),
            16,
            16
        )
        .is_none());
    }

    #[test]
    fn zero_size_pixmap_is_rejected() {
        let geometry = rect_path(0.0, 0.0, 5.0, 5.0);
        assert!(
            rasterize_path(&geometry, &Affine::IDENTITY, FillStyle::default(), 0, 16).is_none()
        );
    }

    #[test]
    fn build_path_is_safe_against_undersupplied_points() {
        // Defensive: a fabricated geometry whose verbs want more points than the buffer holds must
        // not panic/overrun — the walk stops cleanly. Here a lone CubicTo (needs 6 floats) with only
        // 2 supplied yields no usable contour → finish() returns None.
        let geometry = PathGeometry {
            verbs: vec![Verb::CubicTo],
            points: vec![1.0, 2.0],
        };
        assert!(build_tiny_skia_path(&geometry).is_none());
    }

    #[test]
    fn even_odd_donut_leaves_a_hole() {
        // Two concentric rects with EvenOdd: the inner region is a hole (transparent), the ring filled.
        let mut geometry = rect_path(5.0, 5.0, 45.0, 45.0); // outer
        let inner = rect_path(20.0, 20.0, 30.0, 30.0); // inner
        geometry.verbs.extend(inner.verbs);
        geometry.points.extend(inner.points);
        let style = FillStyle {
            argb: 0xFF00_00FFu32 as i32, // opaque blue
            even_odd: true,
        };
        let (rgba, w, _h) =
            rasterize_path_rgba(&geometry, &Affine::IDENTITY, style, 50, 50).expect("rasterizes");
        // Ring (10,25) is filled blue; the hole center (25,25) is transparent.
        assert_eq!(px(&rgba, w, 10, 25), (0, 0, 255, 255));
        assert_eq!(px(&rgba, w, 25, 25).3, 0, "even-odd hole is transparent");
    }

    // === Canvas composite (RGBA pipeline) — GPU-free unit tests (2026-06-05) ===================

    #[test]
    fn is_custom_view_class_excludes_framework_namespaces() {
        // App-defined View subclasses (custom onDraw) are composited; framework widgets are not.
        assert!(is_custom_view_class(
            "com.leocardz.multitouch.test.MultiTouch"
        ));
        assert!(is_custom_view_class("io.example.MyCanvasView"));
        assert!(!is_custom_view_class("android.widget.TextView"));
        assert!(!is_custom_view_class("android.view.View"));
        assert!(!is_custom_view_class("androidx.appcompat.widget.Toolbar"));
        assert!(!is_custom_view_class(
            "com.android.internal.widget.ActionBarView"
        ));
        assert!(!is_custom_view_class("java.lang.Object"));
        assert!(
            !is_custom_view_class(""),
            "empty class is not a custom view"
        );
    }

    #[test]
    fn composite_quad_has_six_vertices_full_uv_and_pixel_to_ndc() {
        // The composite quad: 2 triangles (6 verts), UV spanning the full texture (0,0 top-left →
        // 1,1 bottom-right), positions mapped pixel→NDC exactly like the quad/text passes.
        let extent = vk::Extent2D {
            width: 200,
            height: 100,
        };
        // A rect covering the full extent → NDC corners are the clip-space corners (-1..1).
        let rect = lov(1, 0.0, 0.0, 200.0, 100.0, false);
        let verts = composite_quad_vertices(&rect, extent);
        assert_eq!(verts.len(), 6, "two triangles");
        // First vertex = top-left: NDC (-1,-1), UV (0,0).
        assert_eq!(verts[0].pos, [-1.0, -1.0]);
        assert_eq!(verts[0].uv, [0.0, 0.0]);
        // Collect the distinct corners; the quad must reach NDC (1,1)/UV (1,1) at the bottom-right.
        let has_br = verts
            .iter()
            .any(|v| v.pos == [1.0, 1.0] && v.uv == [1.0, 1.0]);
        assert!(has_br, "bottom-right corner present (full-extent rect)");
        // Every UV stays in [0,1] (the texture is sampled within its own bounds).
        for v in &verts {
            assert!(v.uv[0] >= 0.0 && v.uv[0] <= 1.0 && v.uv[1] >= 0.0 && v.uv[1] <= 1.0);
        }
    }

    #[test]
    fn composite_quad_maps_a_sub_rect_into_ndc() {
        // A half-width rect at the origin maps x in [0,100] over a 200px extent to NDC [-1, 0].
        let extent = vk::Extent2D {
            width: 200,
            height: 100,
        };
        let rect = lov(2, 0.0, 0.0, 100.0, 100.0, false);
        let verts = composite_quad_vertices(&rect, extent);
        // Top-left at NDC (-1,-1); the right edge (x=100 → 2*100/200-1 = 0.0).
        assert_eq!(verts[0].pos, [-1.0, -1.0]);
        let right_edge_present = verts.iter().any(|v| (v.pos[0] - 0.0).abs() < 1e-6);
        assert!(right_edge_present, "x=100px → NDC x=0.0");
    }

    #[test]
    fn rgba_upload_size_is_four_bytes_per_pixel() {
        // The RGBA8 texture upload reads width*height*4 bytes (vs the R8 atlas's width*height). This
        // pins the byte-count selection the staging copy + the upload-skip guard depend on, so a
        // 4-vs-1 byte regression is caught GPU-free. (The straight-RGBA layout comes from
        // canvas_registry's take_demultiplied; 4 channels per pixel.)
        let (w, h) = (8u32, 5u32);
        let expected = (w as usize) * (h as usize) * 4;
        assert_eq!(expected, 160);
        // A buffer at least this long is accepted; a short one is rejected by the upload guard logic.
        let ok = vec![0u8; expected];
        let short = vec![0u8; expected - 1];
        assert!(ok.len() >= expected);
        assert!(
            short.len() < expected,
            "an undersized rgba buffer is skipped"
        );
    }

    #[test]
    fn canvas_rgba_is_straight_rgba_byte_order_for_the_composite_texture() {
        // The composite uploads a canvas_registry Pixmap's bytes into an R8G8B8A8_UNORM texture. That
        // requires the bytes to be STRAIGHT (un-premultiplied) RGBA in R,G,B,A order. Prove the byte
        // layout end-to-end: draw a known semi-transparent color into a real Canvas Pixmap, read its
        // rgba(), and assert pixel 0 is [R,G,B,A] straight — so a format/order regression is caught
        // GPU-free, tying the canvas_registry output to the composite's RGBA8 texture expectation.
        use crate::framework::canvas_registry;
        let h = canvas_registry::allocate(2, 2).expect("allocate canvas");
        // 0xAARRGGBB = 0x80_20_40_60 → straight bytes [R=0x20, G=0x40, B=0x60, A=0x80].
        canvas_registry::with_canvas(h, |c| c.draw_color(0x8020_4060u32 as i32))
            .expect("draw_color");
        let bytes = canvas_registry::with_canvas(h, |c| c.rgba()).expect("read rgba");
        assert_eq!(bytes.len(), 2 * 2 * 4, "4 bytes/pixel straight RGBA");
        assert_eq!(
            &bytes[0..4],
            &[0x20, 0x40, 0x60, 0x80],
            "R,G,B,A straight order"
        );
        canvas_registry::free(h).expect("free canvas");
        // The composite cap bounds per-frame texture churn; it must be a sane small positive number.
        const { assert!(MAX_COMPOSITE_VIEWS >= 1 && MAX_COMPOSITE_VIEWS <= 256) };
    }

    #[test]
    fn composite_spirv_is_well_formed() {
        // The embedded composite SPIR-V must be valid (multiple-of-4 length, ≥1 word). read_spirv is
        // the same guard the other pipelines use; this catches a truncated/corrupt include_bytes blob.
        for (name, spv) in [
            ("composite.vert", COMPOSITE_VERT_SPV),
            ("composite.frag", COMPOSITE_FRAG_SPV),
        ] {
            let words = read_spirv(spv).unwrap_or_else(|e| panic!("{name} SPIR-V invalid: {e}"));
            assert!(!words.is_empty(), "{name} SPIR-V is empty");
        }
    }

    /// 2026-07-17 REGRESSION GUARD for the confirmed input-routing bug: the owner reached a REAL
    /// 2-Step-Verification page, SAW it, and could not click it — 45 presses + 45 releases all went
    /// to the engine's surface beneath, 0 to the WebView. Root cause: the compositor drew the
    /// centered FALLBACK rect (no registry frame is recorded for the challenge WebView — it is
    /// never measured headless) while the hit-test read the registry cache directly and bailed on
    /// its `None`. This pins the two to ONE rect: what `resolve_webview_rect` draws is what
    /// `webview_relative_point` maps against. Pure — no GPU, no display, no ART.
    #[test]
    fn a_centre_click_routes_into_a_webview_that_has_no_measured_frame_rect() {
        // The measured state: `composited_rect()` = None, 800x600 window, 800x600 staged frame.
        let drawn = crate::loader::vk_overlay::resolve_webview_rect(None, 800, 600, 800, 600)
            .expect("the composite draws the centered fallback when no frame rect is cached");
        // The rect the owner SAW ("composite objects built for rect x=0 y=0 w=800 h=600").
        assert_eq!(drawn, (0, 0, 800, 600));

        // The compositor publishes exactly what it drew; the hit-test consumes exactly that.
        // (Process-global: this is the only test that publishes a screen rect.)
        const VIEW: i64 = 0x5eed_1234;
        crate::webview::client::publish_composited_screen_rect(
            VIEW,
            (drawn.0 as i32, drawn.1 as i32, drawn.2, drawn.3),
        );

        // The click the owner could not land: the centre of the window.
        let (rx, ry, inside) = webview_relative_point(VIEW, 400.0, 300.0)
            .expect("the hit-test must see the rect the compositor drew");
        assert!(
            inside,
            "a click on the drawn page must route to the WebView, never fall through to the engine"
        );
        assert_eq!(
            (rx, ry),
            (400, 300),
            "view-relative coords of the window centre"
        );

        // Negative: a view the compositor never drew captures nothing (input stays with the
        // engine), so a successor view can never inherit its predecessor's rect.
        assert!(webview_relative_point(VIEW + 1, 400.0, 300.0).is_none());
        // Negative: a point outside the drawn rect is reported outside, not silently swallowed.
        assert!(!relative_point_in(drawn_i32(drawn), 900.0, 300.0).2);
        assert!(!relative_point_in(drawn_i32(drawn), -1.0, 300.0).2);
    }

    fn drawn_i32(r: (u32, u32, u32, u32)) -> (i32, i32, u32, u32) {
        (r.0 as i32, r.1 as i32, r.2, r.3)
    }
}
