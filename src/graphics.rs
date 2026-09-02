use std::ffi::CStr;
use std::fmt;

use ash::{khr, vk};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::error::{EventLoopError, OsError};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const CLEAR_COLOR: [f32; 4] = [0.149, 0.408, 0.722, 1.0];

const QUAD_VERT_SPV: &[u8] = include_bytes!("../shaders/quad.vert.spv");
const QUAD_FRAG_SPV: &[u8] = include_bytes!("../shaders/quad.frag.spv");

const TEXT_VERT_SPV: &[u8] = include_bytes!("../shaders/text.vert.spv");
const TEXT_FRAG_SPV: &[u8] = include_bytes!("../shaders/text.frag.spv");

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!("../shaders/composite.vert.spv");
const COMPOSITE_FRAG_SPV: &[u8] = include_bytes!("../shaders/composite.frag.spv");

const MAX_COMPOSITE_VIEWS: usize = 16;

const ENGINE_MAIN_LOOP_TICK: std::time::Duration = std::time::Duration::from_millis(4);

const DISPLAY_REFRESH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

const TEXT_PX: f32 = 28.0;
const TEXT_COLOR: [f32; 4] = [0.08, 0.09, 0.12, 1.0];

const TEXT_PAD_X: f32 = 12.0;

struct GameWindow<'vm> {
    title: String,

    window: Option<Window>,

    renderer: Option<VulkanRenderer>,

    create_error: Option<OsError>,

    vm: Option<&'vm crate::runtime::Vm>,

    touch_mode: crate::config::TouchMode,

    cursor: Option<(f32, f32)>,

    primary_press: Option<(ViewHandle, f32, f32)>,

    synthetic_tap_done: bool,

    engine_window: Option<crate::egl_engine::EngineNativeWindow>,

    handed_off: bool,

    engine_tap_downtime: Option<i64>,

    handoff_at: Option<std::time::Instant>,

    engine_synthetic_tap_done: bool,

    engine_synthetic_typed_done: bool,

    engine_last_focus_tap: Option<std::time::Instant>,

    engine_typed_at: Option<std::time::Instant>,

    engine_synthetic_next_done: bool,

    engine_next_at: Option<std::time::Instant>,

    engine_synthetic_typed2_done: bool,

    engine_last_focus_tap2: Option<std::time::Instant>,

    engine_typed2_at: Option<std::time::Instant>,

    engine_synthetic_submit_done: bool,

    engine_reflect_done: bool,

    webview_pointer_down: bool,

    runtime_shutdown_started: bool,

    modifiers: winit::keyboard::ModifiersState,

    published_display_refresh_profile: Option<DisplayRefreshProfile>,

    next_display_refresh_poll: std::time::Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplayRefreshProfile {
    current_millihertz: Option<u32>,
    supported_millihertz: Vec<u32>,
}

impl DisplayRefreshProfile {
    fn current_hz(&self) -> Option<f32> {
        self.current_millihertz.map(|rate| rate as f32 / 1000.0)
    }

    fn supported_hz(&self) -> Vec<f32> {
        self.supported_millihertz
            .iter()
            .map(|rate| *rate as f32 / 1000.0)
            .collect()
    }
}

fn normalize_display_refresh_profile(
    current_millihertz: Option<u32>,
    current_size: (u32, u32),
    modes: impl IntoIterator<Item = ((u32, u32), u32)>,
) -> Option<DisplayRefreshProfile> {
    let current_millihertz = current_millihertz.filter(|rate| *rate > 0);
    let mut supported_millihertz: Vec<u32> = modes
        .into_iter()
        .filter_map(|(size, rate)| (size == current_size && rate > 0).then_some(rate))
        .collect();
    if let Some(current) = current_millihertz {
        supported_millihertz.push(current);
    }
    supported_millihertz.sort_unstable();
    supported_millihertz.dedup();
    (!supported_millihertz.is_empty()).then_some(DisplayRefreshProfile {
        current_millihertz,
        supported_millihertz,
    })
}

fn display_refresh_profile(window: &Window) -> Option<DisplayRefreshProfile> {
    let monitor = window.current_monitor()?;
    let size = monitor.size();
    normalize_display_refresh_profile(
        monitor.refresh_rate_millihertz(),
        (size.width, size.height),
        monitor.video_modes().map(|mode| {
            let mode_size = mode.size();
            (
                (mode_size.width, mode_size.height),
                mode.refresh_rate_millihertz(),
            )
        }),
    )
}

impl ApplicationHandler for GameWindow<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
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

        match window.display_handle() {
            Ok(dh) => match dh.as_raw() {
                RawDisplayHandle::Wayland(d) => {
                    crate::loader::ndk_registry::set_wsi_display(Some(d.display.as_ptr() as usize));
                }
                _ => crate::loader::ndk_registry::set_wsi_display(None),
            },
            Err(_) => crate::loader::ndk_registry::set_wsi_display(None),
        }

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
        self.publish_engine_display_refresh_rates();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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

                let geo = crate::egl_engine::WindowGeometry::from_physical(size.width, size.height);
                let wsi_ptr = self
                    .engine_window
                    .as_ref()
                    .map(|w| w.as_native_window() as usize);
                publish_engine_window_geometry(wsi_ptr, geo.width, geo.height);

                self.publish_engine_display_refresh_rates();
            }

            WindowEvent::Moved(_) => self.publish_engine_display_refresh_rates(),
            WindowEvent::RedrawRequested => {
                self.drive_custom_view_draw();
                if let (Some(window), Some(renderer)) =
                    (self.window.as_ref(), self.renderer.as_mut())
                {
                    if let Err(e) = renderer.draw_frame(window) {
                        tracing::error!(error = %e, "Vulkan frame draw failed");
                    }

                    window.request_redraw();
                }

                self.maybe_synthetic_tap();
            }

            WindowEvent::CursorMoved { position, .. } => {
                let previous = self.cursor;
                self.cursor = Some((position.x as f32, position.y as f32));
                let (dx, dy) = previous.map_or((0.0, 0.0), |(old_x, old_y)| {
                    (position.x as f32 - old_x, position.y as f32 - old_y)
                });

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
                        self.engine_pointer_move(dx, dy);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } if self.handed_off => {
                if button == MouseButton::Left {
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
                    self.engine_aux_mouse_button(button, state == ElementState::Pressed);
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed => self.handle_primary_press(),
                ElementState::Released => self.handle_primary_release(),
            },

            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),

            WindowEvent::KeyboardInput { event, .. } if self.handed_off => {
                let wv = crate::webview::client::active_view();
                if wv != 0 {
                    match active_webview_key_route(&event.logical_key) {
                        ActiveWebViewKeyRoute::ActivityBack => {
                            if event.state == ElementState::Pressed {
                                self.activity_back();
                            }
                        }
                        ActiveWebViewKeyRoute::Chromium => route_key_to_webview(wv, &event),
                    }
                } else {
                    self.engine_key(&event);
                }
            }

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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(vm) = self.vm else { return };
        if let Err(e) = crate::framework::pump_main_looper(vm) {
            tracing::error!(error = %e, "main Looper pump failed");
        }

        if !self.handed_off && self.engine_window.is_some() {
            match crate::framework::engine_surface_callback_ready(vm) {
                Ok(true) => {
                    let (w, h) =
                        crate::loader::ndk_registry::engine_window_geometry().unwrap_or((1, 1));

                    let runtime_overrides = crate::loader::engine::reapply_host_bool_overrides();
                    tracing::info!(
                        runtime_overrides,
                        "re-applied host runtime overrides before SurfaceView lifecycle"
                    );

                    self.renderer = None;

                    if let Err(e) = crate::framework::dispatch_surface_lifecycle(vm, w, h) {
                        tracing::warn!(error = %e, "engine SurfaceView lifecycle dispatch failed after renderer release");
                    }
                    self.handed_off = true;
                    self.handoff_at = Some(std::time::Instant::now());
                    tracing::info!(
                        width = w,
                        height = h,
                        "Eclipse released its Vulkan renderer then dispatched the SurfaceView lifecycle \
                         (surfaceCreated + surfaceChanged); present-loop handoff (drop-before-dispatch)"
                    );
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "engine surface-callback readiness probe failed (retry)");
                }
            }
        } else if self.handed_off && crate::loader::ndk_registry::engine_claimed_surface() {
            static CLAIM_LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !CLAIM_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!(
                    "engine claimed the surface (ANativeWindow_fromSurface returned Eclipse's WSI window)"
                );
            }
        }
        self.maybe_synthetic_engine_tap();

        if self.handed_off && crate::framework::active_text_field() != 0 {
            if let Some(vm) = self.vm {
                crate::framework::query_textbox_geometry(vm);
            }
        }

        if self.handed_off && crate::webview::client::active_view() != 0 {
            crate::webview::client::update_composited_rect();
        }
        let now = std::time::Instant::now();
        if now >= self.next_display_refresh_poll {
            self.publish_engine_display_refresh_rates();
            self.next_display_refresh_poll = now + DISPLAY_REFRESH_POLL_INTERVAL;
        }
        if self.handed_off {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + ENGINE_MAIN_LOOP_TICK,
            ));
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.shutdown_runtime();
    }
}

fn webview_relative_point(view: i64, px: f64, py: f64) -> Option<(i32, i32, bool)> {
    let rect = crate::webview::client::composited_screen_rect(view)?;
    Some(relative_point_in(rect, px, py))
}

fn relative_point_in(rect: (i32, i32, u32, u32), px: f64, py: f64) -> (i32, i32, bool) {
    let (x, y, w, h) = rect;
    let rx = px as i32 - x;
    let ry = py as i32 - y;
    let inside = rx >= 0 && ry >= 0 && (rx as u32) < w && (ry as u32) < h;
    (rx, ry, inside)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveWebViewKeyRoute {
    ActivityBack,
    Chromium,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EngineKeyRoute {
    TextEdit,
    Consume,
    Engine,
}

fn engine_key_route(
    active_text_field: bool,
    pressed: bool,
    editable: bool,
    menu_toggle: bool,
) -> EngineKeyRoute {
    if active_text_field && !menu_toggle {
        if pressed && editable {
            EngineKeyRoute::TextEdit
        } else {
            EngineKeyRoute::Consume
        }
    } else {
        EngineKeyRoute::Engine
    }
}

fn active_webview_key_route(key: &winit::keyboard::Key) -> ActiveWebViewKeyRoute {
    use winit::keyboard::{Key, NamedKey};
    match key {
        Key::Named(NamedKey::Escape) => ActiveWebViewKeyRoute::ActivityBack,
        _ => ActiveWebViewKeyRoute::Chromium,
    }
}

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
        crate::webview::client::send_key(view, 0, code, 0);
        crate::webview::client::send_key(view, 1, code, 0);
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
    crate::webview::client::send_key(view, 0, 0, 0);
    crate::webview::client::send_key(view, 2, 0, unit);
    crate::webview::client::send_key(view, 1, 0, 0);
}

impl GameWindow<'_> {
    fn activity_back(&self) {
        let Some(vm) = self.vm else {
            tracing::warn!("active WebView Back input has no JavaVM");
            return;
        };
        match crate::framework::dispatch_back_to_active_activity(vm) {
            Ok(true) => {}
            Ok(false) => tracing::warn!("active WebView Back input has no live Android Activity"),
            Err(error) => tracing::warn!(%error, "active WebView Back dispatch failed"),
        }
    }

    fn publish_engine_display_refresh_rates(&mut self) {
        let Some(profile) = self.window.as_ref().and_then(display_refresh_profile) else {
            tracing::debug!("host monitor refresh rates unavailable; keeping Android fallback");
            return;
        };
        if self.published_display_refresh_profile.as_ref() == Some(&profile) {
            return;
        }
        let Some(vm) = self.vm else { return };
        let supported_hz = profile.supported_hz();
        match crate::framework::publish_engine_display_refresh_rates(
            vm,
            profile.current_hz(),
            &supported_hz,
        ) {
            Ok(()) => {
                tracing::info!(
                    current_hz = ?profile.current_hz(),
                    supported_hz = ?supported_hz,
                    "published host display refresh rates to Roblox"
                );
                self.published_display_refresh_profile = Some(profile);
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "could not publish host display refresh rates to Roblox"
                );
            }
        }
    }

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

        let Some(pressed_handle) = should_complete_tap(pressed_view, released_view) else {
            return;
        };
        let dispatched_up =
            self.dispatch_touch(pressed_handle, crate::framework::MotionAction::Up, px, py);

        if !dispatched_up {
            self.perform_click_fallback(pressed_handle, px, py);
        }
    }

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

    fn engine_scroll(&mut self, delta: f32) {
        let Some(vm) = self.vm else { return };
        let (px, py) = self.cursor.unwrap_or((0.0, 0.0));
        crate::framework::dispatch_scroll(vm, px, py, delta);
    }

    fn engine_primary_press(&mut self) {
        self.engine_tap_downtime = None;
        let Some(vm) = self.vm else { return };
        let Some((px, py)) = self.cursor else { return };

        if crate::framework::prepare_text_field_pointer_press((px, py)) {
            tracing::debug!("engine surface press queued active text field revalidation");
        }
        if self.touch_mode == crate::config::TouchMode::Off {
            if let Err(e) = crate::framework::dispatch_mouse_button(vm, px, py, true, 0) {
                tracing::warn!(error = %e, "engine desktop mouse-button down dispatch failed (ignored)");
            }
            return;
        }
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

    fn engine_primary_release(&mut self) {
        let down_time = self.engine_tap_downtime.take();
        let Some(vm) = self.vm else { return };
        let Some((px, py)) = self.cursor else { return };
        if self.touch_mode == crate::config::TouchMode::Off {
            if let Err(e) = crate::framework::dispatch_mouse_button(vm, px, py, false, 0) {
                tracing::warn!(error = %e, "engine desktop mouse-button up dispatch failed (ignored)");
            }
            return;
        }
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

    fn engine_pointer_move(&mut self, dx: f32, dy: f32) {
        if self.touch_mode == crate::config::TouchMode::Off {
            let Some(vm) = self.vm else { return };
            let Some((px, py)) = self.cursor else { return };
            if let Err(e) = crate::framework::dispatch_mouse_move(vm, px, py, dx, dy) {
                tracing::warn!(error = %e, "engine desktop mouse-move dispatch failed (ignored)");
            }
            return;
        }
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

    fn engine_aux_mouse_button(&mut self, button: MouseButton, pressed: bool) {
        if self.touch_mode != crate::config::TouchMode::Off {
            return;
        }
        let Some(button) = desktop_mouse_button(button) else {
            return;
        };
        let Some(vm) = self.vm else { return };
        let Some((px, py)) = self.cursor else { return };
        if let Err(e) = crate::framework::dispatch_mouse_button(vm, px, py, pressed, button) {
            tracing::warn!(error = %e, "engine auxiliary mouse-button dispatch failed (ignored)");
        }
    }

    fn engine_key(&mut self, event: &winit::event::KeyEvent) {
        use winit::platform::scancode::PhysicalKeyExtScancode;

        let pressed = event.state == ElementState::Pressed;
        let Some(scan_code) = event
            .physical_key
            .to_scancode()
            .and_then(|code| i32::try_from(code).ok())
        else {
            return;
        };

        let key_code = winit_keycode(&event.logical_key).unwrap_or(0);

        let unicode = event
            .text
            .as_ref()
            .and_then(|s| s.chars().next())
            .map(|c| c as i32)
            .unwrap_or(0);
        let Some(vm) = self.vm else { return };

        let active_text_field = crate::framework::active_text_field() != 0;
        let select_all = active_text_field
            && self.modifiers.control_key()
            && matches!(
                &event.logical_key,
                winit::keyboard::Key::Character(character)
                    if character.eq_ignore_ascii_case("a")
            );
        if pressed && select_all {
            crate::framework::select_all_active_text_field();
        }

        let command_modifier =
            self.modifiers.control_key() || self.modifiers.alt_key() || self.modifiers.super_key();
        let backspace = key_code == 67 && !command_modifier;
        let line_break = !command_modifier
            && matches!(
                event.logical_key,
                winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter)
            )
            && crate::framework::active_text_field_accepts_line_breaks();
        let tab = !command_modifier
            && matches!(
                event.logical_key,
                winit::keyboard::Key::Named(winit::keyboard::NamedKey::Tab)
            );
        let printable = !command_modifier
            && unicode != 0
            && char::from_u32(unicode as u32).is_some_and(|c| !c.is_control());
        let edit_unicode = if line_break {
            '\n' as i32
        } else if tab {
            '\t' as i32
        } else {
            unicode
        };
        let menu_toggle = matches!(
            event.logical_key,
            winit::keyboard::Key::Named(winit::keyboard::NamedKey::Insert)
        );
        match engine_key_route(
            active_text_field,
            pressed,
            printable || backspace || line_break || tab,
            menu_toggle,
        ) {
            EngineKeyRoute::TextEdit
                if crate::framework::type_into_active_text_field(vm, edit_unicode, backspace) =>
            {
                tracing::debug!(pressed, "engine key → active text field (typed)");
                return;
            }
            EngineKeyRoute::Consume => return,
            EngineKeyRoute::TextEdit | EngineKeyRoute::Engine => {}
        }
        let action = if pressed {
            crate::framework::KeyAction::Down
        } else {
            crate::framework::KeyAction::Up
        };
        match crate::framework::pass_hardware_key_to_engine(
            vm,
            action,
            scan_code,
            key_code,
            event.repeat,
        ) {
            Ok(()) => {
                static HARDWARE_KEY_PATH_LOGGED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !HARDWARE_KEY_PATH_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!("engine hardware-key input path active (keys not logged)");
                } else {
                    tracing::trace!(pressed, "engine hardware key dispatched (key not logged)");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "engine key dispatch failed (ignored)");
            }
        }
    }

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

                renderer.set_drawn_canvases(Vec::new());
            }
        }
    }

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

            self.cursor = Some((cx, cy));
            self.handle_primary_press();
            self.handle_primary_release();
            return;
        }

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

    fn maybe_synthetic_engine_tap(&mut self) {
        if !self.handed_off {
            return;
        }
        let Some(at) = self.handoff_at else { return };
        let elapsed = at.elapsed();

        if !self.engine_reflect_done && elapsed >= std::time::Duration::from_secs(8) {
            self.engine_reflect_done = true;
            if let Some(vm) = self
                .vm
                .filter(|_| std::env::var_os("ECLIPSE_REFLECT_INPUT").is_some())
            {
                crate::framework::reflect_engine_input_methods(vm);
            }
        }

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

        if !self.engine_synthetic_typed_done && elapsed >= std::time::Duration::from_secs(10) {
            if let Some((x, y, text)) =
                std::env::var_os("ECLIPSE_SYNTHETIC_TYPE").and_then(|s| parse_xy_text(&s))
            {
                if crate::framework::active_text_field() != 0 {
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

                    crate::loader::ndk_registry::wake_all_loopers();
                    self.engine_typed_at = Some(std::time::Instant::now());
                } else if self
                    .engine_last_focus_tap
                    .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(1500))
                {
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

pub fn run_windowed(
    title: &str,
    vm: Option<&crate::runtime::Vm>,
    touch_mode: crate::config::TouchMode,
) -> Result<(), GraphicsError> {
    let event_loop = EventLoop::new().map_err(GraphicsError::EventLoop)?;
    let mut app = GameWindow {
        title: title.to_owned(),
        window: None,
        renderer: None,
        create_error: None,
        vm,
        touch_mode,
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
        webview_pointer_down: false,
        runtime_shutdown_started: false,
        modifiers: winit::keyboard::ModifiersState::default(),
        published_display_refresh_profile: None,
        next_display_refresh_poll: std::time::Instant::now(),
    };
    let run = event_loop.run_app(&mut app);

    app.shutdown_runtime();

    if let Some(vm) = vm {
        crate::framework::retire_main_upcall_dispatch(vm);
    }
    run.map_err(GraphicsError::EventLoop)?;

    if let Some(e) = app.create_error {
        return Err(GraphicsError::CreateWindow(e));
    }
    Ok(())
}

fn publish_engine_window_geometry(wsi_ptr: Option<usize>, width: i32, height: i32) {
    crate::loader::ndk_registry::set_engine_window_geometry(width, height);
    if let Some(ptr) = wsi_ptr {
        crate::loader::ndk_registry::register_wsi_window(ptr, width, height);
    }
}

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

fn choose_image_count(caps: &vk::SurfaceCapabilitiesKHR) -> u32 {
    let desired = caps.min_image_count + 1;
    if caps.max_image_count > 0 {
        desired.min(caps.max_image_count)
    } else {
        desired
    }
}

use crate::framework::view_registry::{LayoutParams, RenderNode, ViewHandle, MATCH_PARENT};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct QuadVertex {
    pos: [f32; 2],
    color: [f32; 4],
}

#[derive(Debug, Clone, PartialEq)]
struct LaidOutView {
    handle: ViewHandle,

    x: f32,

    y: f32,

    w: f32,

    h: f32,

    clickable: bool,

    color: [f32; 4],

    text: Option<String>,
}

fn argb_to_rgba_f32(argb: i32) -> [f32; 4] {
    let v = argb as u32;
    let a = ((v >> 24) & 0xFF) as f32 / 255.0;
    let r = ((v >> 16) & 0xFF) as f32 / 255.0;
    let g = ((v >> 8) & 0xFF) as f32 / 255.0;
    let b = (v & 0xFF) as f32 / 255.0;
    [r, g, b, a]
}

fn is_custom_view_class(class_name: &str) -> bool {
    const FRAMEWORK_PREFIXES: [&str; 4] = ["android.", "androidx.", "com.android.", "java."];
    !class_name.is_empty() && !FRAMEWORK_PREFIXES.iter().any(|p| class_name.starts_with(p))
}

const DEPTH_PALETTE: [[f32; 4]; 4] = [
    [0.93, 0.94, 0.96, 1.0],
    [0.80, 0.85, 0.92, 1.0],
    [0.66, 0.74, 0.86, 1.0],
    [0.55, 0.64, 0.80, 1.0],
];

const WRAP_FALLBACK_W: f32 = 64.0;
const WRAP_FALLBACK_H: f32 = TEXT_PX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpecMode {
    Unspecified,
    Exactly,
    AtMost,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MeasureSpec {
    mode: SpecMode,
    size: f32,
}

impl MeasureSpec {
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

#[derive(Clone, Copy)]
struct TextMeasure<'a> {
    atlas: &'a GlyphAtlas,
}

impl TextMeasure<'_> {
    fn width(&self, text: &str) -> f32 {
        let advances: f32 = text
            .chars()
            .map(|ch| self.atlas.glyphs.get(&ch).map_or(0.0, |g| g.advance))
            .sum();
        advances + 2.0 * TEXT_PAD_X
    }

    fn height(&self) -> f32 {
        self.atlas.line_height
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct NodeBox {
    mw: f32,
    mh: f32,

    x: f32,
    y: f32,
}

fn is_vertical_linear(class_name: &str) -> bool {
    class_name.ends_with("LinearLayout")
}

const GRAVITY_CENTER_HORIZONTAL: i32 = 0x01;
const GRAVITY_RIGHT: i32 = 0x05;
const GRAVITY_CENTER_VERTICAL: i32 = 0x10;
const GRAVITY_BOTTOM: i32 = 0x50;

fn gravity_specified(gravity: i32) -> bool {
    gravity >= 0
}

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

fn margin_h(lp: &LayoutParams) -> f32 {
    (lp.margins[0] + lp.margins[2]).max(0) as f32
}

fn margin_v(lp: &LayoutParams) -> f32 {
    (lp.margins[1] + lp.margins[3]).max(0) as f32
}

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

    let inner_w = (w_spec.size - pad_h).max(0.0);
    let inner_h = (h_spec.size - pad_v).max(0.0);

    if node.children.is_empty() {
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
    let mut sum_h = 0.0f32;
    let mut max_w = 0.0f32;
    let mut max_h = 0.0f32;

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

            if total_weight > 0.0 && clp.weight > 0.0 {
                boxes[ci].mh += leftover * (clp.weight / total_weight);
            }
            let cw = boxes[ci].mw;
            let ch = boxes[ci].mh;

            let dx = gravity_dx(clp.gravity, inner_w - margin_h(&clp), cw);
            let cx = inner_x + clp.margins[0].max(0) as f32 + dx;
            let cy = cursor + clp.margins[1].max(0) as f32;
            layout_node(nodes, boxes, ci, cx, cy, depth_guard + 1);
            cursor += ch + margin_v(&clp);
        }
    } else {
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
            let color = match node.background_color {
                Some(argb) => argb_to_rgba_f32(argb),
                None => DEPTH_PALETTE[(node.depth as usize).min(DEPTH_PALETTE.len() - 1)],
            };
            LaidOutView {
                handle: node.handle,
                x: b.x,
                y: b.y,

                w: b.mw.max(1.0),
                h: b.mh.max(1.0),
                clickable: node.clickable,
                color,
                text: node.text.clone(),
            }
        })
        .collect()
}

fn hit_test(views: &[LaidOutView], x: f32, y: f32) -> Option<ViewHandle> {
    views
        .iter()
        .rev()
        .find(|v| v.clickable && x >= v.x && x < v.x + v.w && y >= v.y && y < v.y + v.h)
        .map(|v| v.handle)
}

fn should_complete_tap(
    pressed: Option<ViewHandle>,
    released: Option<ViewHandle>,
) -> Option<ViewHandle> {
    match (pressed, released) {
        (Some(p), Some(r)) if p == r => Some(p),
        _ => None,
    }
}

fn parse_xy(spec: &std::ffi::OsStr) -> Option<(f32, f32)> {
    let s = spec.to_str()?;
    let (xs, ys) = s.split_once(',')?;
    Some((xs.trim().parse().ok()?, ys.trim().parse().ok()?))
}

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

fn desktop_mouse_button(button: MouseButton) -> Option<i32> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Right => Some(1),
        MouseButton::Middle => Some(3),
        MouseButton::Back => Some(7),
        MouseButton::Forward => Some(15),
        MouseButton::Other(_) => None,
    }
}

fn winit_keycode(key: &winit::keyboard::Key) -> Option<i32> {
    use winit::keyboard::{Key, NamedKey};
    Some(match key {
        Key::Character(s) => {
            let c = s.chars().next()?;
            match c {
                'a'..='z' => 29 + (c as i32 - 'a' as i32),
                'A'..='Z' => 29 + (c as i32 - 'A' as i32),
                '0'..='9' => 7 + (c as i32 - '0' as i32),
                ' ' => 62,
                '.' => 56,
                ',' => 55,
                '@' => 77,
                '-' | '_' => 69,
                '+' | '=' => 70,
                '/' => 76,
                _ => 0,
            }
        }
        Key::Named(NamedKey::Space) => 62,
        Key::Named(NamedKey::Backspace) => 67,
        Key::Named(NamedKey::Enter) => 66,
        Key::Named(NamedKey::Tab) => 61,
        Key::Named(NamedKey::Escape) => 4,
        Key::Named(NamedKey::Insert) => 124,
        Key::Named(NamedKey::Delete) => 112,
        Key::Named(NamedKey::ArrowLeft) => 21,
        Key::Named(NamedKey::ArrowRight) => 22,
        Key::Named(NamedKey::ArrowUp) => 19,
        Key::Named(NamedKey::ArrowDown) => 20,
        Key::Named(NamedKey::Home) => 122,
        Key::Named(NamedKey::End) => 123,
        _ => return None,
    })
}

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

    [v(tl), v(tr), v(br), v(tl), v(br), v(bl)]
}

fn build_quad_vertices(views: &[LaidOutView], extent: vk::Extent2D) -> Vec<QuadVertex> {
    let mut verts = Vec::with_capacity(views.len() * 6);
    for v in views {
        verts.extend_from_slice(&pixel_rect_to_quad(v.x, v.y, v.w, v.h, v.color, extent));
    }
    verts
}

fn read_spirv(bytes: &[u8]) -> Result<Vec<u32>, GraphicsError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(GraphicsError::Vulkan(format!(
            "embedded SPIR-V length {} is not a multiple of 4 (corrupt shader blob)",
            bytes.len()
        )));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

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

    let buf_info = vk::BufferCreateInfo::default()
        .size(size.max(1))
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let staging = unsafe { device.create_buffer(&buf_info, None) }
        .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (staging): {e}")))?;

    let req = unsafe { device.get_buffer_memory_requirements(staging) };
    let mem_type = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
        .ok_or_else(|| {
            unsafe { device.destroy_buffer(staging, None) };
            GraphicsError::Vulkan("no host-visible memory for the atlas staging buffer".to_owned())
        })?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_type);

    let staging_mem = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_buffer(staging, None) };
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateMemory (staging): {e}"
            )));
        }
    };

    let free_staging = |device: &ash::Device| unsafe {
        device.free_memory(staging_mem, None);
        device.destroy_buffer(staging, None);
    };

    if let Err(e) = unsafe { device.bind_buffer_memory(staging, staging_mem, 0) } {
        free_staging(device);
        return Err(GraphicsError::Vulkan(format!(
            "vkBindBufferMemory (staging): {e}"
        )));
    }

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

    let cb_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);

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
        unsafe { device.free_command_buffers(command_pool, &[cmd]) };
    };

    let subresource = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1);

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

    let submitted = unsafe { device.queue_submit(queue, &[submit], fence) };
    let waited =
        submitted.and_then(|()| unsafe { device.wait_for_fences(&[fence], true, u64::MAX) });

    unsafe { device.destroy_fence(fence, None) };
    free_cmd(device);
    free_staging(device);
    waited.map_err(|e| GraphicsError::Vulkan(format!("submit/wait atlas upload: {e}")))?;
    Ok(())
}

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
    let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 4;
    let buf_info = vk::BufferCreateInfo::default()
        .size(size.max(1))
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let staging = unsafe { device.create_buffer(&buf_info, None) }
        .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (rgba staging): {e}")))?;

    let req = unsafe { device.get_buffer_memory_requirements(staging) };
    let mem_type = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
        .ok_or_else(|| {
            unsafe { device.destroy_buffer(staging, None) };
            GraphicsError::Vulkan("no host-visible memory for the rgba staging buffer".to_owned())
        })?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_type);

    let staging_mem = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_buffer(staging, None) };
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateMemory (rgba staging): {e}"
            )));
        }
    };
    let free_staging = |device: &ash::Device| unsafe {
        device.free_memory(staging_mem, None);
        device.destroy_buffer(staging, None);
    };

    if let Err(e) = unsafe { device.bind_buffer_memory(staging, staging_mem, 0) } {
        free_staging(device);
        return Err(GraphicsError::Vulkan(format!(
            "vkBindBufferMemory (rgba staging): {e}"
        )));
    }

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
        unsafe { device.free_command_buffers(command_pool, &[cmd]) };
    };

    let subresource = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1);

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

    let submitted = unsafe { device.queue_submit(queue, &[submit], fence) };
    let waited =
        submitted.and_then(|()| unsafe { device.wait_for_fences(&[fence], true, u64::MAX) });

    unsafe { device.destroy_fence(fence, None) };
    free_cmd(device);
    free_staging(device);
    waited.map_err(|e| GraphicsError::Vulkan(format!("submit/wait rgba upload: {e}")))?;
    Ok(())
}

fn composite_quad_vertices(rect: &LaidOutView, extent: vk::Extent2D) -> Vec<TextVertex> {
    let ew = extent.width.max(1) as f32;
    let eh = extent.height.max(1) as f32;
    let to_ndc = |px: f32, py: f32| -> [f32; 2] { [2.0 * px / ew - 1.0, 2.0 * py / eh - 1.0] };

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

    vec![tl, tr, br, tl, br, bl]
}

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

    let buffer = unsafe { device.create_buffer(&buffer_info, None) }
        .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (composite vtx): {e}")))?;

    let req = unsafe { device.get_buffer_memory_requirements(buffer) };
    let mem_type = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
        .ok_or_else(|| {
            unsafe { device.destroy_buffer(buffer, None) };
            GraphicsError::Vulkan("no host-visible memory for a composite vertex buffer".to_owned())
        })?;
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_type);

    let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateMemory (composite vtx): {e}"
            )));
        }
    };

    if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            device.free_memory(memory, None);
            device.destroy_buffer(buffer, None);
        }
        return Err(GraphicsError::Vulkan(format!(
            "vkBindBufferMemory (composite vtx): {e}"
        )));
    }

    unsafe {
        let ptr = match device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) {
            Ok(p) => p,
            Err(e) => {
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

fn build_composite_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    descriptor_set_layout: vk::DescriptorSetLayout,
) -> Result<(vk::PipelineLayout, vk::Pipeline), GraphicsError> {
    let vert_words = read_spirv(COMPOSITE_VERT_SPV)?;
    let frag_words = read_spirv(COMPOSITE_FRAG_SPV)?;
    let make_module = |words: &[u32]| -> Result<vk::ShaderModule, GraphicsError> {
        let info = vk::ShaderModuleCreateInfo::default().code(words);

        unsafe { device.create_shader_module(&info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateShaderModule (composite): {e}")))
    };
    let vert_module = make_module(&vert_words)?;
    let frag_module = match make_module(&frag_words) {
        Ok(m) => m,
        Err(e) => {
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

    let pipeline_layout = match unsafe { device.create_pipeline_layout(&layout_info, None) } {
        Ok(l) => l,
        Err(e) => {
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

    let pipeline = match unsafe {
        device.create_graphics_pipelines(
            vk::PipelineCache::null(),
            std::slice::from_ref(&pipeline_info),
            None,
        )
    } {
        Ok(p) => p[0],
        Err((_, e)) => {
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

    unsafe {
        device.destroy_shader_module(frag_module, None);
        device.destroy_shader_module(vert_module, None);
    }
    Ok((pipeline_layout, pipeline))
}

fn build_text_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    descriptor_set_layout: vk::DescriptorSetLayout,
) -> Result<(vk::PipelineLayout, vk::Pipeline), GraphicsError> {
    let vert_words = read_spirv(TEXT_VERT_SPV)?;
    let frag_words = read_spirv(TEXT_FRAG_SPV)?;
    let make_module = |words: &[u32]| -> Result<vk::ShaderModule, GraphicsError> {
        let info = vk::ShaderModuleCreateInfo::default().code(words);

        unsafe { device.create_shader_module(&info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateShaderModule (text): {e}")))
    };
    let vert_module = make_module(&vert_words)?;
    let frag_module = match make_module(&frag_words) {
        Ok(m) => m,
        Err(e) => {
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

    let pipeline_layout = match unsafe { device.create_pipeline_layout(&layout_info, None) } {
        Ok(l) => l,
        Err(e) => {
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

    let pipeline = match unsafe {
        device.create_graphics_pipelines(
            vk::PipelineCache::null(),
            std::slice::from_ref(&pipeline_info),
            None,
        )
    } {
        Ok(p) => p[0],
        Err((_, e)) => {
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

    unsafe {
        device.destroy_shader_module(frag_module, None);
        device.destroy_shader_module(vert_module, None);
    }
    Ok((pipeline_layout, pipeline))
}

use crate::font::RasterFont;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct TextVertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
struct GlyphInfo {
    ax: u32,
    ay: u32,
    aw: u32,
    ah: u32,

    bearing_x: f32,
    bearing_y: f32,

    advance: f32,
}

struct GlyphAtlas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    glyphs: std::collections::HashMap<char, GlyphInfo>,
    ascent: f32,
    line_height: f32,
}

const ATLAS_CHARS: std::ops::RangeInclusive<u8> = 32..=126;

pub(crate) fn discover_font_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("ECLIPSE_FONT") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }

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

    const FONT_DIRS: [&str; 4] = [
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        "/usr/share/fonts/truetype",
        "/run/host/fonts",
    ];
    for dir in FONT_DIRS {
        if let Some(p) = first_font_in_dir(std::path::Path::new(dir)) {
            return Some(p);
        }
    }
    None
}

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

    subdirs.into_iter().find_map(|d| first_font_in_dir(&d))
}

fn build_glyph_atlas(font: &RasterFont, max_width: u32) -> Option<GlyphAtlas> {
    let mut scaled = font.scaled(TEXT_PX)?;
    let ascent = scaled.ascent();
    let line_height = scaled.height() + scaled.line_gap();

    const PAD: u32 = 1;

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
        let advance = scaled.advance(ch);
        if let Some(glyph) = scaled.glyph(ch) {
            let placement = glyph.placement();
            let w = placement.width;
            let h = placement.height;
            let mut pixels = vec![0u8; (w * h) as usize];
            glyph.draw(|x, y, coverage| {
                let idx = (y * w + x) as usize;
                if idx < pixels.len() {
                    pixels[idx] = (coverage.clamp(0.0, 1.0) * 255.0) as u8;
                }
            });
            rasters.push(Raster {
                ch,
                w,
                h,
                pixels,
                bearing_x: placement.left as f32,
                bearing_y: placement.top as f32,
                advance,
            });
        } else {
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

    let max_width = max_width.max(1);
    let mut pen_x = PAD;
    let mut pen_y = PAD;
    let mut row_h = 0u32;
    let mut atlas_w = 0u32;
    let mut placements: Vec<(char, GlyphInfo)> = Vec::with_capacity(rasters.len());
    for r in &rasters {
        if r.w == 0 || r.h == 0 {
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

        let mut pen_x = v.x + TEXT_PAD_X;
        let baseline_y = v.y + (v.h - atlas.line_height).max(0.0) * 0.5 + atlas.ascent;
        for ch in text.chars() {
            let Some(g) = atlas.glyphs.get(&ch) else {
                continue;
            };
            if g.aw > 0 && g.ah > 0 {
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

struct Swapchain {
    swapchain: vk::SwapchainKHR,
    image_views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    extent: vk::Extent2D,
}

struct VulkanRenderer {
    _entry: ash::Entry,
    instance: ash::Instance,
    surface_loader: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical_device: vk::PhysicalDevice,

    device: ash::Device,
    queue: vk::Queue,
    swapchain_loader: khr::swapchain::Device,
    render_pass: vk::RenderPass,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    image_available: vk::Semaphore,
    render_finished: vk::Semaphore,
    in_flight: vk::Fence,

    quad_pipeline_layout: vk::PipelineLayout,
    quad_pipeline: vk::Pipeline,

    quad_vertex_buffer: vk::Buffer,
    quad_vertex_memory: vk::DeviceMemory,
    quad_vertex_capacity: u32,

    memory_properties: vk::PhysicalDeviceMemoryProperties,

    text: Option<TextRenderer>,

    composite: Option<CanvasCompositor>,

    drawn_canvases: Vec<crate::framework::DrawnCanvas>,

    swapchain: Swapchain,
    swapchain_format: vk::Format,
    swapchain_extent: vk::Extent2D,

    needs_recreate: bool,
}

impl VulkanRenderer {
    fn new(window: &Window) -> Result<Self, GraphicsError> {
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

        let surface_extensions = ash_window::enumerate_required_extensions(display_handle)
            .map_err(|e| {
                GraphicsError::Vulkan(format!(
                    "no Vulkan surface extension for this display server: {e}"
                ))
            })?;

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"Eclipse")
            .api_version(vk::API_VERSION_1_0);
        let instance_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(surface_extensions);

        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateInstance failed: {e}")))?;

        match Self::build(entry, instance, display_handle, window_handle, window) {
            Ok(renderer) => Ok(renderer),
            Err(boxed) => {
                let (e, instance) = *boxed;

                unsafe {
                    instance.destroy_instance(None);
                }
                Err(e)
            }
        }
    }

    fn build(
        entry: ash::Entry,
        instance: ash::Instance,
        display_handle: raw_window_handle::RawDisplayHandle,
        window_handle: raw_window_handle::RawWindowHandle,
        window: &Window,
    ) -> Result<Self, Box<(GraphicsError, ash::Instance)>> {
        let surface_loader = khr::surface::Instance::new(&entry, &instance);

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

        let (physical_device, queue_family_index) =
            match Self::pick_device(&instance, &surface_loader, surface) {
                Ok(v) => v,
                Err(e) => {
                    unsafe { surface_loader.destroy_surface(surface, None) };
                    return Err(Box::new((e, instance)));
                }
            };

        let queue_priorities = [1.0_f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities);
        let device_extensions = [khr::swapchain::NAME.as_ptr()];
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue_info))
            .enabled_extension_names(&device_extensions);

        let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
            Ok(d) => d,
            Err(e) => {
                unsafe { surface_loader.destroy_surface(surface, None) };
                return Err(Box::new((
                    GraphicsError::Vulkan(format!("vkCreateDevice failed: {e}")),
                    instance,
                )));
            }
        };

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
                unsafe {
                    device.destroy_device(None);
                    surface_loader.destroy_surface(surface, None);
                }
                Err(Box::new((e, instance)))
            }
        }
    }

    fn pick_device(
        instance: &ash::Instance,
        surface_loader: &khr::surface::Instance,
        surface: vk::SurfaceKHR,
    ) -> Result<(vk::PhysicalDevice, u32), GraphicsError> {
        let devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| GraphicsError::Vulkan(format!("vkEnumeratePhysicalDevices: {e}")))?;
        if devices.is_empty() {
            return Err(GraphicsError::Vulkan(
                "no Vulkan physical devices found".to_owned(),
            ));
        }

        let mut fallback: Option<(vk::PhysicalDevice, u32)> = None;
        for &pd in &devices {
            let exts = match unsafe { instance.enumerate_device_extension_properties(pd) } {
                Ok(e) => e,
                Err(_) => continue,
            };
            let has_swapchain = exts.iter().any(|e| {
                let name = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
                name == khr::swapchain::NAME
            });
            if !has_swapchain {
                continue;
            }

            let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            for (i, family) in families.iter().enumerate() {
                let index = i as u32;
                if !family.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                    continue;
                }

                let present_ok = unsafe {
                    surface_loader.get_physical_device_surface_support(pd, index, surface)
                }
                .unwrap_or(false);
                if !present_ok {
                    continue;
                }

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

    fn create_quad_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
    ) -> Result<(vk::PipelineLayout, vk::Pipeline), GraphicsError> {
        let vert_words = read_spirv(QUAD_VERT_SPV)?;
        let frag_words = read_spirv(QUAD_FRAG_SPV)?;

        let make_module = |words: &[u32]| -> Result<vk::ShaderModule, GraphicsError> {
            let info = vk::ShaderModuleCreateInfo::default().code(words);
            unsafe { device.create_shader_module(&info, None) }
                .map_err(|e| GraphicsError::Vulkan(format!("vkCreateShaderModule: {e}")))
        };
        let vert_module = make_module(&vert_words)?;
        let frag_module = match make_module(&frag_words) {
            Ok(m) => m,
            Err(e) => {
                unsafe { device.destroy_shader_module(vert_module, None) };
                return Err(e);
            }
        };

        let result = Self::build_quad_pipeline_inner(device, render_pass, vert_module, frag_module);

        unsafe {
            device.destroy_shader_module(frag_module, None);
            device.destroy_shader_module(vert_module, None);
        }
        result
    }

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
            .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
            .alpha_blend_op(vk::BlendOp::ADD);
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(std::slice::from_ref(&blend_attachment));

        let layout_info = vk::PipelineLayoutCreateInfo::default();

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

        let pipeline = match unsafe {
            device.create_graphics_pipelines(
                vk::PipelineCache::null(),
                std::slice::from_ref(&pipeline_info),
                None,
            )
        } {
            Ok(p) => p[0],
            Err((_, e)) => {
                unsafe { device.destroy_pipeline_layout(pipeline_layout, None) };
                return Err(GraphicsError::Vulkan(format!(
                    "vkCreateGraphicsPipelines: {e}"
                )));
            }
        };
        Ok((pipeline_layout, pipeline))
    }

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
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let swapchain_loader = khr::swapchain::Device::new(instance, device);

        let formats =
            unsafe { surface_loader.get_physical_device_surface_formats(physical_device, surface) }
                .map_err(|e| GraphicsError::Vulkan(format!("get surface formats: {e}")))?;
        let surface_format = choose_surface_format(&formats)
            .ok_or_else(|| GraphicsError::Vulkan("surface advertises no formats".to_owned()))?;

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

        let render_pass = unsafe { device.create_render_pass(&render_pass_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateRenderPass: {e}")))?;

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
                unsafe { device.destroy_render_pass(render_pass, None) };
                return Err(e);
            }
        };
        let extent = swapchain.extent;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(queue_family_index);

        let command_pool = match unsafe { device.create_command_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
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

        let command_buffers = match unsafe { device.allocate_command_buffers(&alloc_info) } {
            Ok(b) => b,
            Err(e) => {
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

        let sem_info = vk::SemaphoreCreateInfo::default();
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);

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
                unsafe {
                    device.destroy_command_pool(command_pool, None);
                    swapchain.destroy(device, &swapchain_loader);
                    device.destroy_render_pass(render_pass, None);
                }
                return Err(GraphicsError::Vulkan(format!("create sync objects: {e}")));
            }
        };

        let (quad_pipeline_layout, quad_pipeline) =
            match Self::create_quad_pipeline(device, render_pass) {
                Ok(p) => p,
                Err(e) => {
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

        let text =
            match TextRenderer::new(device, queue, command_pool, render_pass, &memory_properties) {
                Ok(t) => t,
                Err(e) => {
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
        let caps = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }
        .map_err(|e| GraphicsError::Vulkan(format!("get surface capabilities: {e}")))?;
        let extent = choose_swap_extent(&caps, window_width, window_height);
        let image_count = choose_image_count(&caps);

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

        let swapchain = unsafe { swapchain_loader.create_swapchain(&create_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateSwapchainKHR: {e}")))?;

        let images = match unsafe { swapchain_loader.get_swapchain_images(swapchain) } {
            Ok(i) => i,
            Err(e) => {
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

            let view = match unsafe { device.create_image_view(&view_info, None) } {
                Ok(v) => v,
                Err(e) => {
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

            let fb = match unsafe { device.create_framebuffer(&fb_info, None) } {
                Ok(f) => f,
                Err(e) => {
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

    fn frame_count(&self) -> usize {
        self.swapchain.framebuffers.len()
    }

    fn hit_test_at(&self, x: f32, y: f32) -> Option<ViewHandle> {
        let nodes = crate::framework::view_registry::snapshot_tree();
        if nodes.is_empty() {
            return None;
        }
        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(&nodes, self.swapchain.extent, measure);
        hit_test(&views, x, y)
    }

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

    fn first_view_center(&self) -> Option<(ViewHandle, f32, f32)> {
        let nodes = crate::framework::view_registry::snapshot_tree();
        if nodes.is_empty() {
            return None;
        }
        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(&nodes, self.swapchain.extent, measure);

        nodes
            .iter()
            .zip(views.iter())
            .rev()
            .find(|(n, _)| n.children.is_empty())
            .map(|(_, v)| (v.handle, v.x + v.w / 2.0, v.y + v.h / 2.0))
    }

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

    fn set_drawn_canvases(&mut self, drawn: Vec<crate::framework::DrawnCanvas>) {
        for d in self.drawn_canvases.drain(..) {
            let _ = crate::framework::canvas_registry::free(d.canvas);
        }
        self.drawn_canvases = drawn;
    }

    fn mark_resized(&mut self, width: u32, height: u32) {
        if width != 0 && height != 0 {
            self.needs_recreate = true;
        }
    }

    fn recreate_swapchain(&mut self, window: &Window) -> Result<(), GraphicsError> {
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }

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

        unsafe {
            let old = std::mem::replace(&mut self.swapchain, new_swapchain);
            old.destroy(&self.device, &self.swapchain_loader);
        }
        self.swapchain_extent = self.swapchain.extent;
        self.needs_recreate = false;
        Ok(())
    }

    fn draw_frame(&mut self, window: &Window) -> Result<(), GraphicsError> {
        if self.needs_recreate {
            self.recreate_swapchain(window)?;
            if self.swapchain.framebuffers.is_empty() {
                return Ok(());
            }
        }

        unsafe {
            self.device
                .wait_for_fences(&[self.in_flight], true, u64::MAX)
                .map_err(|e| GraphicsError::Vulkan(format!("wait_for_fences: {e}")))?;
        }

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
                return Ok(());
            }
            Err(e) => return Err(GraphicsError::Vulkan(format!("acquire_next_image: {e}"))),
        };

        unsafe {
            self.device
                .reset_fences(&[self.in_flight])
                .map_err(|e| GraphicsError::Vulkan(format!("reset_fences: {e}")))?;
        }

        let nodes = crate::framework::view_registry::snapshot_tree();
        let extent = self.swapchain.extent;

        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(&nodes, extent, measure);

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

        let mem_props = self.memory_properties;
        let text_vertex_count = if let Some(text) = self.text.as_mut() {
            let tverts = build_text_vertices(&views, &text.atlas, extent);
            text.upload(&self.device, &mem_props, &tverts)?
        } else {
            0
        };

        let queue = self.queue;
        let command_pool = self.command_pool;
        let composite_count = if let Some(composite) = self.composite.as_mut() {
            unsafe { composite.begin_frame(&self.device)? };
            for d in &self.drawn_canvases {
                let Some(rect) = views.iter().find(|v| v.handle == d.view) else {
                    continue;
                };

                let snapshot = crate::framework::canvas_registry::with_canvas(d.canvas, |c| {
                    let (w, h) = c.dimensions();
                    (w, h, c.rgba())
                });
                let Ok((tw, th, rgba)) = snapshot else {
                    continue;
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

        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], self.in_flight)
                .map_err(|e| GraphicsError::Vulkan(format!("queue_submit: {e}")))?;
        }

        window.pre_present_notify();

        let swapchains = [self.swapchain.swapchain];
        let image_indices = [image_index];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal_semaphores)
            .swapchains(&swapchains)
            .image_indices(&image_indices);

        let present = unsafe {
            self.swapchain_loader
                .queue_present(self.queue, &present_info)
        };
        match present {
            Ok(false) => {}
            Ok(true) => self.needs_recreate = true,
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

    fn upload_vertices(&mut self, verts: &[QuadVertex]) -> Result<u32, GraphicsError> {
        let count: u32 = verts.len().try_into().map_err(|_| {
            GraphicsError::Vulkan("too many quad vertices for one frame".to_owned())
        })?;
        if count == 0 {
            return Ok(0);
        }

        if count > self.quad_vertex_capacity {
            let size =
                (count as vk::DeviceSize) * std::mem::size_of::<QuadVertex>() as vk::DeviceSize;
            let buffer_info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);

            let buffer = unsafe { self.device.create_buffer(&buffer_info, None) }
                .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (vertex): {e}")))?;

            let req = unsafe { self.device.get_buffer_memory_requirements(buffer) };
            let mem_type =
                find_host_visible_memory_type(&self.memory_properties, req.memory_type_bits)
                    .ok_or_else(|| {
                        unsafe { self.device.destroy_buffer(buffer, None) };
                        GraphicsError::Vulkan(
                            "no HOST_VISIBLE|HOST_COHERENT memory type for the vertex buffer"
                                .to_owned(),
                        )
                    })?;
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mem_type);

            let memory = match unsafe { self.device.allocate_memory(&alloc_info, None) } {
                Ok(m) => m,
                Err(e) => {
                    unsafe { self.device.destroy_buffer(buffer, None) };
                    return Err(GraphicsError::Vulkan(format!(
                        "vkAllocateMemory (vertex): {e}"
                    )));
                }
            };

            if let Err(e) = unsafe { self.device.bind_buffer_memory(buffer, memory, 0) } {
                unsafe {
                    self.device.free_memory(memory, None);
                    self.device.destroy_buffer(buffer, None);
                }
                return Err(GraphicsError::Vulkan(format!("vkBindBufferMemory: {e}")));
            }

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

        let copy_bytes =
            (count as vk::DeviceSize) * std::mem::size_of::<QuadVertex>() as vk::DeviceSize;

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

    fn record_draw(
        &self,
        image_index: usize,
        vertex_count: u32,
        text_vertex_count: u32,
    ) -> Result<(), GraphicsError> {
        let cmd = self.command_buffer;

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
    unsafe fn destroy(&self, device: &ash::Device, loader: &khr::swapchain::Device) {
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

    atlas: GlyphAtlas,

    vertex_buffer: vk::Buffer,
    vertex_memory: vk::DeviceMemory,
    vertex_capacity: u32,
}

impl TextRenderer {
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
        let font = match RasterFont::try_from_vec(bytes) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(path = %font_path.display(), error = %e, "font parse failed; text disabled");
                return Ok(None);
            }
        };

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

    #[allow(clippy::too_many_arguments)]
    fn build_gpu(
        device: &ash::Device,
        queue: vk::Queue,
        command_pool: vk::CommandPool,
        render_pass: vk::RenderPass,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        atlas: GlyphAtlas,
    ) -> Result<Self, GraphicsError> {
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

        let atlas_image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateImage (atlas): {e}")))?;

        let req = unsafe { device.get_image_memory_requirements(atlas_image) };
        let mem_type = find_device_local_memory_type(memory_properties, req.memory_type_bits)
            .ok_or_else(|| {
                unsafe { device.destroy_image(atlas_image, None) };
                GraphicsError::Vulkan("no memory type for the glyph atlas image".to_owned())
            })?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(mem_type);

        let atlas_memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { device.destroy_image(atlas_image, None) };
                return Err(GraphicsError::Vulkan(format!(
                    "vkAllocateMemory (atlas): {e}"
                )));
            }
        };

        if let Err(e) = unsafe { device.bind_image_memory(atlas_image, atlas_memory, 0) } {
            unsafe {
                device.free_memory(atlas_memory, None);
                device.destroy_image(atlas_image, None);
            }
            return Err(GraphicsError::Vulkan(format!("vkBindImageMemory: {e}")));
        }

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
            unsafe {
                device.free_memory(atlas_memory, None);
                device.destroy_image(atlas_image, None);
            }
            return Err(e);
        }

        Self::finish_gpu(device, render_pass, atlas, atlas_image, atlas_memory)
    }

    fn finish_gpu(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        atlas: GlyphAtlas,
        atlas_image: vk::Image,
        atlas_memory: vk::DeviceMemory,
    ) -> Result<Self, GraphicsError> {
        let free_image = |device: &ash::Device| unsafe {
            device.free_memory(atlas_memory, None);
            device.destroy_image(atlas_image, None);
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

        let sampler = match unsafe { device.create_sampler(&sampler_info, None) } {
            Ok(s) => s,
            Err(e) => {
                unsafe { device.destroy_image_view(atlas_view, None) };
                free_image(device);
                return Err(GraphicsError::Vulkan(format!("vkCreateSampler: {e}")));
            }
        };

        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let dsl_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));

        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&dsl_info, None) } {
                Ok(l) => l,
                Err(e) => {
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

        let descriptor_pool = match unsafe { device.create_descriptor_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
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

        let descriptor_set = match unsafe { device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
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

        let image_info = vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(atlas_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&image_info));

        unsafe { device.update_descriptor_sets(std::slice::from_ref(&write), &[]) };

        let (pipeline_layout, pipeline) =
            match build_text_pipeline(device, render_pass, descriptor_set_layout) {
                Ok(p) => p,
                Err(e) => {
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

            let buffer = unsafe { device.create_buffer(&buffer_info, None) }
                .map_err(|e| GraphicsError::Vulkan(format!("vkCreateBuffer (text): {e}")))?;

            let req = unsafe { device.get_buffer_memory_requirements(buffer) };
            let mem_type = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
                .ok_or_else(|| {
                unsafe { device.destroy_buffer(buffer, None) };
                GraphicsError::Vulkan(
                    "no host-visible memory for the text vertex buffer".to_owned(),
                )
            })?;
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mem_type);

            let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
                Ok(m) => m,
                Err(e) => {
                    unsafe { device.destroy_buffer(buffer, None) };
                    return Err(GraphicsError::Vulkan(format!(
                        "vkAllocateMemory (text): {e}"
                    )));
                }
            };

            if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
                unsafe {
                    device.free_memory(memory, None);
                    device.destroy_buffer(buffer, None);
                }
                return Err(GraphicsError::Vulkan(format!(
                    "vkBindBufferMemory (text): {e}"
                )));
            }

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

    unsafe fn destroy(&self, device: &ash::Device) {
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

struct CompositeTexture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,

    descriptor_set: vk::DescriptorSet,
    vertex_buffer: vk::Buffer,
    vertex_memory: vk::DeviceMemory,
    vertex_count: u32,
}

struct CanvasCompositor {
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,

    descriptor_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,

    textures: Vec<CompositeTexture>,
}

impl CanvasCompositor {
    fn new(device: &ash::Device, render_pass: vk::RenderPass) -> Result<Self, GraphicsError> {
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);

        let sampler = unsafe { device.create_sampler(&sampler_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateSampler (composite): {e}")))?;

        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let dsl_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));

        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&dsl_info, None) } {
                Ok(l) => l,
                Err(e) => {
                    unsafe { device.destroy_sampler(sampler, None) };
                    return Err(GraphicsError::Vulkan(format!(
                        "vkCreateDescriptorSetLayout (composite): {e}"
                    )));
                }
            };

        let pool_size = vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(MAX_COMPOSITE_VIEWS as u32);
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(MAX_COMPOSITE_VIEWS as u32)
            .pool_sizes(std::slice::from_ref(&pool_size));

        let descriptor_pool = match unsafe { device.create_descriptor_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
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

    unsafe fn begin_frame(&mut self, device: &ash::Device) -> Result<(), GraphicsError> {
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
            return Ok(());
        }

        let expected = (tex_w as usize) * (tex_h as usize) * 4;
        if tex_w == 0 || tex_h == 0 || rgba.len() < expected {
            return Ok(());
        }

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

        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateImage (composite): {e}")))?;

        let req = unsafe { device.get_image_memory_requirements(image) };
        let mem_type = find_device_local_memory_type(memory_properties, req.memory_type_bits)
            .ok_or_else(|| {
                unsafe { device.destroy_image(image, None) };
                GraphicsError::Vulkan("no memory type for a composite texture".to_owned())
            })?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(mem_type);

        let memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { device.destroy_image(image, None) };
                return Err(GraphicsError::Vulkan(format!(
                    "vkAllocateMemory (composite): {e}"
                )));
            }
        };

        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            unsafe {
                device.free_memory(memory, None);
                device.destroy_image(image, None);
            }
            return Err(GraphicsError::Vulkan(format!(
                "vkBindImageMemory (composite): {e}"
            )));
        }

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
            unsafe {
                device.free_memory(memory, None);
                device.destroy_image(image, None);
            }
            return Err(e);
        }

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

        let view = match unsafe { device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    device.free_memory(memory, None);
                    device.destroy_image(image, None);
                }
                return Err(GraphicsError::Vulkan(format!(
                    "vkCreateImageView (composite): {e}"
                )));
            }
        };

        let set_layouts = [self.descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&set_layouts);

        let descriptor_set = match unsafe { device.allocate_descriptor_sets(&ds_alloc) } {
            Ok(sets) => sets[0],
            Err(e) => {
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

        unsafe { device.update_descriptor_sets(std::slice::from_ref(&write), &[]) };

        let verts = composite_quad_vertices(rect, extent);
        let (vertex_buffer, vertex_memory, vertex_count) =
            match upload_composite_vertices(device, memory_properties, &verts) {
                Ok(v) => v,
                Err(e) => {
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

    unsafe fn record(&self, device: &ash::Device, cmd: vk::CommandBuffer) {
        if self.textures.is_empty() {
            return;
        }

        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);

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

    fn texture_count(&self) -> usize {
        self.textures.len()
    }

    unsafe fn destroy(&self, device: &ash::Device) {
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
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_semaphore(self.image_available, None);
            self.device.destroy_semaphore(self.render_finished, None);
            self.device.destroy_fence(self.in_flight, None);
            self.device.destroy_command_pool(self.command_pool, None);

            if let Some(text) = self.text.as_ref() {
                text.destroy(&self.device);
            }

            if let Some(composite) = self.composite.as_ref() {
                composite.destroy(&self.device);
            }

            for d in &self.drawn_canvases {
                let _ = crate::framework::canvas_registry::free(d.canvas);
            }

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

use crate::framework::matrix_registry::Affine;
use crate::framework::path_registry::{PathGeometry, Verb};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

#[derive(Debug, Clone, Copy)]
pub struct FillStyle {
    pub argb: i32,

    pub even_odd: bool,
}

impl Default for FillStyle {
    fn default() -> Self {
        Self {
            argb: 0xFF00_0000u32 as i32,
            even_odd: false,
        }
    }
}

fn argb_to_rgba8(argb: i32) -> (u8, u8, u8, u8) {
    let v = argb as u32;
    let a = (v >> 24) as u8;
    let r = (v >> 16) as u8;
    let g = (v >> 8) as u8;
    let b = v as u8;
    (r, g, b, a)
}

fn build_tiny_skia_path(geometry: &PathGeometry) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let pts = &geometry.points;
    let mut i = 0usize;
    for verb in &geometry.verbs {
        let need = verb.point_count() * 2;
        if i + need > pts.len() {
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

fn affine_to_transform(m: &Affine) -> Transform {
    Transform::from_row(m.m[0], m.m[3], m.m[1], m.m[4], m.m[2], m.m[5])
}

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

#[derive(Debug)]
pub enum GraphicsError {
    EventLoop(EventLoopError),

    CreateWindow(OsError),

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
    fn display_refresh_profile_preserves_real_high_refresh_modes() {
        let profile = normalize_display_refresh_profile(
            Some(143_996),
            (1920, 1080),
            [
                ((1920, 1080), 60_000),
                ((1920, 1080), 143_996),
                ((1920, 1080), 120_000),
                ((1920, 1080), 60_000),
                ((1280, 720), 240_000),
                ((1920, 1080), 0),
            ],
        )
        .expect("the current display has usable rates");

        assert_eq!(profile.current_millihertz, Some(143_996));
        assert_eq!(profile.supported_millihertz, vec![60_000, 120_000, 143_996]);
        assert_eq!(profile.current_hz(), Some(143.996));
        assert_eq!(profile.supported_hz(), vec![60.0, 120.0, 143.996]);
        assert_eq!(
            normalize_display_refresh_profile(None, (1920, 1080), [((1280, 720), 240_000)]),
            None,
            "alternate-resolution-only modes must not fabricate current display rates"
        );
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

    #[test]
    fn winit_keycode_maps_credential_keys_to_android_keycodes() {
        use winit::keyboard::{Key, NamedKey};

        assert_eq!(winit_keycode(&Key::Character("a".into())), Some(29));
        assert_eq!(winit_keycode(&Key::Character("z".into())), Some(54));
        assert_eq!(winit_keycode(&Key::Character("A".into())), Some(29));

        assert_eq!(winit_keycode(&Key::Character("0".into())), Some(7));
        assert_eq!(winit_keycode(&Key::Character("9".into())), Some(16));

        assert_eq!(winit_keycode(&Key::Character("@".into())), Some(77));
        assert_eq!(winit_keycode(&Key::Character(".".into())), Some(56));

        assert_eq!(winit_keycode(&Key::Named(NamedKey::Backspace)), Some(67));
        assert_eq!(winit_keycode(&Key::Named(NamedKey::Enter)), Some(66));
        assert_eq!(winit_keycode(&Key::Named(NamedKey::Space)), Some(62));

        assert_eq!(winit_keycode(&Key::Character("#".into())), Some(0));

        assert_eq!(winit_keycode(&Key::Named(NamedKey::F1)), None);
    }

    #[test]
    fn escape_maps_to_android_back_navigation() {
        use winit::keyboard::{Key, NamedKey};

        assert_eq!(winit_keycode(&Key::Named(NamedKey::Escape)), Some(4));
    }

    #[test]
    fn insert_maps_to_android_insert_for_internal_menu_toggle() {
        use winit::keyboard::{Key, NamedKey};

        assert_eq!(winit_keycode(&Key::Named(NamedKey::Insert)), Some(124));
    }

    #[test]
    fn escape_bypasses_chromium_for_activity_back_navigation() {
        use winit::keyboard::{Key, NamedKey};

        assert_eq!(
            active_webview_key_route(&Key::Named(NamedKey::Escape)),
            ActiveWebViewKeyRoute::ActivityBack
        );
    }

    #[test]
    fn focused_text_field_consumes_both_key_edges_and_non_text_keys() {
        assert_eq!(
            engine_key_route(true, true, true, false),
            EngineKeyRoute::TextEdit
        );
        assert_eq!(
            engine_key_route(true, false, true, false),
            EngineKeyRoute::Consume
        );
        assert_eq!(
            engine_key_route(true, true, false, false),
            EngineKeyRoute::Consume
        );
        assert_eq!(
            engine_key_route(true, true, false, true),
            EngineKeyRoute::Engine
        );
        assert_eq!(
            engine_key_route(false, true, true, false),
            EngineKeyRoute::Engine
        );
    }

    #[test]
    fn desktop_mouse_buttons_match_the_apk_generic_motion_mapping() {
        assert_eq!(desktop_mouse_button(MouseButton::Left), Some(0));
        assert_eq!(desktop_mouse_button(MouseButton::Right), Some(1));
        assert_eq!(desktop_mouse_button(MouseButton::Middle), Some(3));
        assert_eq!(desktop_mouse_button(MouseButton::Back), Some(7));
        assert_eq!(desktop_mouse_button(MouseButton::Forward), Some(15));
        assert_eq!(desktop_mouse_button(MouseButton::Other(42)), None);
    }

    #[test]
    fn winit_physical_keys_preserve_linux_evdev_scancodes() {
        use winit::keyboard::{KeyCode, PhysicalKey};
        use winit::platform::scancode::PhysicalKeyExtScancode;

        assert_eq!(PhysicalKey::Code(KeyCode::KeyW).to_scancode(), Some(17));
        assert_eq!(PhysicalKey::Code(KeyCode::KeyA).to_scancode(), Some(30));
        assert_eq!(
            PhysicalKey::Code(KeyCode::ShiftLeft).to_scancode(),
            Some(42)
        );
        assert_eq!(
            PhysicalKey::Code(KeyCode::ControlLeft).to_scancode(),
            Some(29)
        );
        assert_eq!(PhysicalKey::Code(KeyCode::F1).to_scancode(), Some(59));
    }

    #[test]
    fn surface_format_none_when_driver_advertises_none() {
        assert!(choose_surface_format(&[]).is_none());
    }

    #[test]
    fn publish_engine_window_geometry_registers_real_wsi_mapping() {
        use crate::loader::ndk_registry;

        let ptr = 0xECC1_0613_usize;
        ndk_registry::unregister_wsi_window(ptr);

        publish_engine_window_geometry(Some(ptr), 1280, 720);
        assert_eq!(
            ndk_registry::wsi_window_geometry(ptr),
            Some((1280, 720)),
            "the real WSI pointer must resolve to the published geometry (ANativeWindow_getWidth/Height)"
        );

        publish_engine_window_geometry(Some(ptr), 800, 600);
        assert_eq!(
            ndk_registry::wsi_window_geometry(ptr),
            Some((800, 600)),
            "a resize re-publish updates the same WSI entry's geometry"
        );

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
        let c = caps(2, 4, 800, 600);
        let e = choose_swap_extent(&c, 1920, 1080);
        assert_eq!(e.width, 800);
        assert_eq!(e.height, 600);
    }

    #[test]
    fn swap_extent_clamps_window_size_when_current_is_special() {
        let c = caps(2, 4, u32::MAX, u32::MAX);
        let e = choose_swap_extent(&c, 1920, 1080);
        assert_eq!(e.width, 1920);
        assert_eq!(e.height, 1080);

        let big = choose_swap_extent(&c, 9000, 9000);
        assert_eq!(big.width, 4096);
        assert_eq!(big.height, 4096);

        let small = choose_swap_extent(&c, 0, 0);
        assert_eq!(small.width, 1);
        assert_eq!(small.height, 1);
    }

    #[test]
    fn image_count_is_min_plus_one_clamped_to_max() {
        assert_eq!(choose_image_count(&caps(2, 4, 800, 600)), 3);

        assert_eq!(choose_image_count(&caps(3, 3, 800, 600)), 3);

        assert_eq!(choose_image_count(&caps(2, 0, 800, 600)), 3);
    }

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
        let parent = MeasureSpec {
            mode: SpecMode::Exactly,
            size: 800.0,
        };

        let (size, child) = parent.resolve(exact(120), 999.0);
        assert_eq!(size, 120.0);
        assert_eq!(child.mode, SpecMode::Exactly);
        assert_eq!(child.size, 120.0);

        let (size, child) = parent.resolve(MATCH_PARENT, 50.0);
        assert_eq!(size, 800.0);
        assert_eq!(child.mode, SpecMode::Exactly);

        let (size, child) = parent.resolve(WRAP_CONTENT, 200.0);
        assert_eq!(size, 200.0);
        assert_eq!(child.mode, SpecMode::AtMost);
        assert_eq!(child.size, 800.0);

        let (size, _) = parent.resolve(WRAP_CONTENT, 9000.0);
        assert_eq!(size, 800.0);
    }

    #[test]
    fn measure_spec_unspecified_parent_yields_content_size() {
        let parent = MeasureSpec {
            mode: SpecMode::Unspecified,
            size: 0.0,
        };

        let (size, child) = parent.resolve(MATCH_PARENT, 77.0);
        assert_eq!(size, 77.0);
        assert_eq!(child.mode, SpecMode::Unspecified);
    }

    #[test]
    fn root_match_parent_fills_the_swapchain_extent() {
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

        assert_eq!(
            (views[0].x, views[0].y, views[0].w, views[0].h),
            (0.0, 0.0, 400.0, 600.0)
        );

        assert_eq!((views[1].x, views[1].y), (0.0, 0.0));
        assert_eq!((views[1].w, views[1].h), (400.0, 100.0));

        assert_eq!((views[2].x, views[2].y), (0.0, 100.0));
        assert_eq!((views[2].w, views[2].h), (400.0, 100.0));
    }

    #[test]
    fn argb_to_rgba_f32_splits_channels() {
        let c = argb_to_rgba_f32(0xFFFF_0000u32 as i32);
        assert_eq!(c, [1.0, 0.0, 0.0, 1.0]);

        let g = argb_to_rgba_f32(0x8000_FF00u32 as i32);
        assert!((g[0]).abs() < 1e-6);
        assert!((g[1] - 1.0).abs() < 1e-6);
        assert!((g[2]).abs() < 1e-6);
        assert!((g[3] - 128.0 / 255.0).abs() < 1e-6);

        assert_eq!(argb_to_rgba_f32(0x0000_0000), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn background_color_overrides_depth_palette_in_layout() {
        let extent = vk::Extent2D {
            width: 100,
            height: 100,
        };
        let mut n = node("android.view.View", None, 0);
        n.background_color = Some(0xFF00_00FFu32 as i32);
        let views = layout_views(&[n], extent, None);
        assert_eq!(views[0].color, [0.0, 0.0, 1.0, 1.0]);

        let plain = layout_views(&[node("android.view.View", None, 0)], extent, None);
        assert_eq!(plain[0].color, DEPTH_PALETTE[0]);
    }

    #[test]
    fn frame_layout_honors_child_gravity() {
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
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let atlas = synthetic_atlas();
        let measure = TextMeasure { atlas: &atlas };
        let lp = LayoutParams {
            width: WRAP_CONTENT,
            height: WRAP_CONTENT,
            ..Default::default()
        };
        let nodes = [node_lp("android.widget.TextView", Some("AAA"), 0, lp, &[])];
        let views = layout_views(&nodes, extent, Some(measure));

        assert_eq!(views[0].w, 3.0 * 6.0 + 2.0 * TEXT_PAD_X);

        assert_eq!(views[0].h, 8.0);
    }

    #[test]
    fn linear_layout_weight_distributes_leftover_space() {
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
        assert_eq!(gravity_dx(-1, 200.0, 50.0), 0.0, "unspecified → left");
        assert_eq!(gravity_dy(-1, 200.0, 50.0), 0.0, "unspecified → top");

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

        let q = pixel_rect_to_quad(0.0, 0.0, 800.0, 600.0, [1.0; 4], extent);

        assert_eq!(q[0].pos, [-1.0, -1.0]);

        assert_eq!(q[2].pos, [1.0, 1.0]);

        let mid = pixel_rect_to_quad(400.0, 300.0, 0.0, 0.0, [0.0; 4], extent);
        assert_eq!(mid[0].pos, [0.0, 0.0]);
    }

    #[test]
    fn build_quad_vertices_emits_six_per_view() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };

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

        assert!(verts[0..6].iter().all(|v| v.color == views[0].color));
    }

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

        assert_eq!(hit_test(&views, 50.0, 30.0), Some(7));
        assert_eq!(hit_test(&views, 5.0, 30.0), None, "left of the rect");
        assert_eq!(hit_test(&views, 200.0, 30.0), None, "right of the rect");
        assert_eq!(hit_test(&views, 50.0, 5.0), None, "above the rect");
        assert_eq!(hit_test(&views, 50.0, 100.0), None, "below the rect");
    }

    #[test]
    fn hit_test_topmost_last_drawn_wins_for_overlapping_views() {
        let views = [
            lov(1, 0.0, 0.0, 100.0, 100.0, true),
            lov(2, 20.0, 20.0, 40.0, 40.0, true),
        ];
        assert_eq!(
            hit_test(&views, 30.0, 30.0),
            Some(2),
            "topmost overlapping wins"
        );

        assert_eq!(hit_test(&views, 5.0, 5.0), Some(1));
    }

    #[test]
    fn hit_test_ignores_non_clickable_views() {
        let views = [
            lov(1, 0.0, 0.0, 100.0, 100.0, true),
            lov(2, 0.0, 0.0, 100.0, 100.0, false),
        ];
        assert_eq!(
            hit_test(&views, 50.0, 50.0),
            Some(1),
            "the non-clickable top view is ignored, the clickable one below is hit"
        );

        let inert = [lov(9, 0.0, 0.0, 100.0, 100.0, false)];
        assert_eq!(hit_test(&inert, 50.0, 50.0), None);

        assert_eq!(hit_test(&[], 0.0, 0.0), None);
    }

    #[test]
    fn hit_test_rect_is_half_open() {
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

    #[test]
    fn should_complete_tap_requires_press_and_release_on_same_view() {
        assert_eq!(should_complete_tap(Some(7), Some(7)), Some(7));

        assert_eq!(should_complete_tap(Some(7), Some(9)), None);

        assert_eq!(should_complete_tap(Some(7), None), None);

        assert_eq!(should_complete_tap(None, Some(7)), None);
        assert_eq!(should_complete_tap(None, None), None);
    }

    #[test]
    fn embedded_spirv_is_well_formed() {
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

        assert_eq!(find_device_local_memory_type(&props, 0b011), Some(1));

        assert_eq!(find_device_local_memory_type(&props, 0b001), Some(0));

        assert_eq!(find_device_local_memory_type(&props, 0b000), None);
    }

    fn synthetic_atlas() -> GlyphAtlas {
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
        let Some(path) = discover_font_path() else {
            return;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        let Ok(font) = RasterFont::try_from_vec(bytes) else {
            return;
        };
        let atlas = build_glyph_atlas(&font, 1024).expect("atlas builds from a real font");
        assert!(atlas.width > 0 && atlas.height > 0);
        assert_eq!(atlas.pixels.len(), (atlas.width * atlas.height) as usize);

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

        props.memory_types[0].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[1].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[2].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;

        let filter = 0b101;
        assert_eq!(find_host_visible_memory_type(&props, filter), Some(2));

        assert_eq!(find_host_visible_memory_type(&props, 0b001), None);
    }

    fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * w + x) * 4) as usize;
        (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
    }

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
        let (r, g, b, a) = argb_to_rgba8(0x8012_3456u32 as i32);
        assert_eq!((r, g, b, a), (0x12, 0x34, 0x56, 0x80));
    }

    #[test]
    fn affine_to_transform_maps_affine_coefficients() {
        let mut m = Affine::IDENTITY;
        m.set_translate(10.0, 20.0);
        let t = affine_to_transform(&m);
        assert_eq!((t.sx, t.sy, t.tx, t.ty), (1.0, 1.0, 10.0, 20.0));
        assert_eq!((t.kx, t.ky), (0.0, 0.0));
    }

    #[test]
    fn rasterize_filled_rect_has_opaque_interior_and_clear_exterior() {
        let geometry = rect_path(10.0, 10.0, 30.0, 30.0);
        let style = FillStyle {
            argb: 0xFFFF_0000u32 as i32,
            even_odd: false,
        };
        let (rgba, w, h) =
            rasterize_path_rgba(&geometry, &Affine::IDENTITY, style, 40, 40).expect("rasterizes");
        assert_eq!((w, h), (40, 40));
        assert_eq!(rgba.len(), (40 * 40 * 4) as usize);

        assert_eq!(px(&rgba, w, 20, 20), (255, 0, 0, 255));

        assert_eq!(px(&rgba, w, 0, 0), (0, 0, 0, 0));
        assert_eq!(px(&rgba, w, 39, 39), (0, 0, 0, 0));

        assert_eq!(px(&rgba, w, 5, 5), (0, 0, 0, 0));
        assert_eq!(px(&rgba, w, 25, 15).3, 255, "inside the rect is opaque");
    }

    #[test]
    fn rasterize_honors_the_transform() {
        let geometry = rect_path(0.0, 0.0, 10.0, 10.0);
        let mut m = Affine::IDENTITY;
        m.set_translate(20.0, 20.0);
        let style = FillStyle {
            argb: 0xFF00_FF00u32 as i32,
            even_odd: false,
        };
        let (rgba, w, _h) = rasterize_path_rgba(&geometry, &m, style, 40, 40).expect("rasterizes");

        assert_eq!(px(&rgba, w, 25, 25), (0, 255, 0, 255));
        assert_eq!(px(&rgba, w, 5, 5), (0, 0, 0, 0));
    }

    #[test]
    fn empty_path_does_not_rasterize() {
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
        let geometry = PathGeometry {
            verbs: vec![Verb::CubicTo],
            points: vec![1.0, 2.0],
        };
        assert!(build_tiny_skia_path(&geometry).is_none());
    }

    #[test]
    fn even_odd_donut_leaves_a_hole() {
        let mut geometry = rect_path(5.0, 5.0, 45.0, 45.0);
        let inner = rect_path(20.0, 20.0, 30.0, 30.0);
        geometry.verbs.extend(inner.verbs);
        geometry.points.extend(inner.points);
        let style = FillStyle {
            argb: 0xFF00_00FFu32 as i32,
            even_odd: true,
        };
        let (rgba, w, _h) =
            rasterize_path_rgba(&geometry, &Affine::IDENTITY, style, 50, 50).expect("rasterizes");

        assert_eq!(px(&rgba, w, 10, 25), (0, 0, 255, 255));
        assert_eq!(px(&rgba, w, 25, 25).3, 0, "even-odd hole is transparent");
    }

    #[test]
    fn is_custom_view_class_excludes_framework_namespaces() {
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
        let extent = vk::Extent2D {
            width: 200,
            height: 100,
        };

        let rect = lov(1, 0.0, 0.0, 200.0, 100.0, false);
        let verts = composite_quad_vertices(&rect, extent);
        assert_eq!(verts.len(), 6, "two triangles");

        assert_eq!(verts[0].pos, [-1.0, -1.0]);
        assert_eq!(verts[0].uv, [0.0, 0.0]);

        let has_br = verts
            .iter()
            .any(|v| v.pos == [1.0, 1.0] && v.uv == [1.0, 1.0]);
        assert!(has_br, "bottom-right corner present (full-extent rect)");

        for v in &verts {
            assert!(v.uv[0] >= 0.0 && v.uv[0] <= 1.0 && v.uv[1] >= 0.0 && v.uv[1] <= 1.0);
        }
    }

    #[test]
    fn composite_quad_maps_a_sub_rect_into_ndc() {
        let extent = vk::Extent2D {
            width: 200,
            height: 100,
        };
        let rect = lov(2, 0.0, 0.0, 100.0, 100.0, false);
        let verts = composite_quad_vertices(&rect, extent);

        assert_eq!(verts[0].pos, [-1.0, -1.0]);
        let right_edge_present = verts.iter().any(|v| (v.pos[0] - 0.0).abs() < 1e-6);
        assert!(right_edge_present, "x=100px → NDC x=0.0");
    }

    #[test]
    fn rgba_upload_size_is_four_bytes_per_pixel() {
        let (w, h) = (8u32, 5u32);
        let expected = (w as usize) * (h as usize) * 4;
        assert_eq!(expected, 160);

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
        use crate::framework::canvas_registry;
        let h = canvas_registry::allocate(2, 2).expect("allocate canvas");

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

        const { assert!(MAX_COMPOSITE_VIEWS >= 1 && MAX_COMPOSITE_VIEWS <= 256) };
    }

    #[test]
    fn composite_spirv_is_well_formed() {
        for (name, spv) in [
            ("composite.vert", COMPOSITE_VERT_SPV),
            ("composite.frag", COMPOSITE_FRAG_SPV),
        ] {
            let words = read_spirv(spv).unwrap_or_else(|e| panic!("{name} SPIR-V invalid: {e}"));
            assert!(!words.is_empty(), "{name} SPIR-V is empty");
        }
    }

    #[test]
    fn a_centre_click_routes_into_a_webview_that_has_no_measured_frame_rect() {
        let drawn = crate::loader::vk_overlay::resolve_webview_rect(None, 800, 600, 800, 600)
            .expect("the composite draws the centered fallback when no frame rect is cached");

        assert_eq!(drawn, (0, 0, 800, 600));

        const VIEW: i64 = 0x5eed_1234;
        crate::webview::client::publish_composited_screen_rect(
            VIEW,
            (drawn.0 as i32, drawn.1 as i32, drawn.2, drawn.3),
        );

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

        assert!(webview_relative_point(VIEW + 1, 400.0, 300.0).is_none());

        assert!(!relative_point_in(drawn_i32(drawn), 900.0, 300.0).2);
        assert!(!relative_point_in(drawn_i32(drawn), -1.0, 300.0).2);
    }

    fn drawn_i32(r: (u32, u32, u32, u32)) -> (i32, i32, u32, u32) {
        (r.0 as i32, r.1 as i32, r.2, r.3)
    }
}
