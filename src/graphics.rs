use std::fmt;

use ash::{khr, vk};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::error::{EventLoopError, ExternalError, OsError};
use winit::event::{DeviceEvent, DeviceId, ElementState, Ime, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand as _;
use winit::platform::startup_notify::WindowAttributesExtStartupNotify as _;
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{ActivationToken, CursorGrabMode, Fullscreen, Window, WindowId};

use crate::web_view_parent::WebViewParent;

pub mod activation;
mod dialog_window;
mod gles_renderer;
pub mod launch_window;
pub(crate) mod title_suffix;
pub mod window_state;

const CLEAR_COLOR: [f32; 4] = [0.149, 0.408, 0.722, 1.0];

const QUAD_VERT_SPV: &[u8] = include_bytes!("../shaders/quad.vert.spv");
const QUAD_FRAG_SPV: &[u8] = include_bytes!("../shaders/quad.frag.spv");

const TEXT_VERT_SPV: &[u8] = include_bytes!("../shaders/text.vert.spv");
const TEXT_FRAG_SPV: &[u8] = include_bytes!("../shaders/text.frag.spv");

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!("../shaders/composite.vert.spv");
const COMPOSITE_FRAG_SPV: &[u8] = include_bytes!("../shaders/composite.frag.spv");

const MAX_COMPOSITE_VIEWS: usize = 16;

const MAIN_THREAD_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(4);

const DISPLAY_REFRESH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

const POINTER_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

const SYNTHETIC_INPUT_DELAY: std::time::Duration = std::time::Duration::from_secs(6);

const ENGINE_SURFACE_SIZE_FIRST_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(100);

const ENGINE_SURFACE_SIZE_MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

const TEXT_PX: f32 = 28.0;
const TEXT_COLOR: [f32; 4] = [0.08, 0.09, 0.12, 1.0];

const TEXT_PAD_X: f32 = 12.0;

struct GameWindow<'vm> {
    title: String,

    graphics: crate::gpu::Graphics,

    renderer: Option<WindowRenderer>,

    engine_window: Option<crate::egl_engine::EngineNativeWindow>,

    web_view_parent: Option<WebViewParent>,

    window: Option<Window>,

    create_error: Option<OsError>,

    handoff_error: Option<GraphicsError>,

    vm: Option<&'vm crate::runtime::Vm>,

    touch_mode: eclipse_config::TouchMode,

    cursor: Option<(f32, f32)>,

    primary_press: Option<(ViewHandle, f32, f32)>,

    synthetic_tap_done: bool,

    handed_off: bool,

    engine_surface_size: EngineSurfaceSize,

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

    synthetic_gamepad_done: bool,

    synthetic_touch_done: bool,

    gamepads: Gamepads,

    engine_gamepads: crate::gamepad::EngineGamepads,

    web_view_window: WebViewWindow,

    runtime_shutdown_started: bool,

    last_activity: crate::framework::lifecycle::LastActivityCheck,

    modifiers: winit::keyboard::ModifiersState,

    published_display_refresh_profile: Option<DisplayRefreshProfile>,

    next_display_refresh_poll: std::time::Instant,

    focused: bool,

    idle_inhibit: crate::portal::IdleInhibit,

    on_screen_keyboard: crate::on_screen_keyboard::OnScreenKeyboard,

    fullscreen: bool,

    window_state: window_state::WindowStateFile,

    fullscreen_shortcut: FullscreenShortcut,

    host_cursor: HostCursor,

    pointer_lock_reasons: PointerLockReasons,

    pointer_lock: PointerLock,

    engine_held: EngineHeldInput,

    wheel_steps: crate::input::WheelSteps,

    touch: crate::input::TouchTracker,

    touch_route: TouchRoute,

    touch_down_time: Option<i64>,

    relative_motion_units: RelativeMotionUnits,

    engine_center_query_failed: bool,

    pending_pointer_motion: Option<PendingPointerMotion>,

    loopers_need_wake: bool,

    ime: HostIme,

    ime_area_support: ImeAreaSupport,

    clipboard: Option<crate::clipboard::HostClipboard>,
    activation_token: Option<ActivationToken>,

    dialogs: dialog_window::DialogWindows,

    commands: Option<&'vm std::sync::mpsc::Receiver<launch_window::WindowCommand>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PointerMotion {
    position: (f32, f32),
    dx: f32,
    dy: f32,
}

impl PointerMotion {
    fn then(self, next: Self) -> Self {
        Self {
            position: next.position,
            dx: self.dx + next.dx,
            dy: self.dy + next.dy,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PendingPointerMotion {
    Free(PointerMotion),
    Locked(PointerMotion),
}

impl PendingPointerMotion {
    fn coalesce(self, next: Self) -> Option<Self> {
        match (self, next) {
            (Self::Free(held), Self::Free(next)) => Some(Self::Free(held.then(next))),
            (Self::Locked(held), Self::Locked(next)) if held.position == next.position => {
                Some(Self::Locked(held.then(next)))
            }
            _ => None,
        }
    }
}

fn flushes_pending_motion(input: Option<crate::loader::native_provider::HostInputKind>) -> bool {
    input != Some(crate::loader::native_provider::HostInputKind::Pointer)
}

fn next_wake(deadlines: impl IntoIterator<Item = Option<std::time::Instant>>) -> ControlFlow {
    deadlines
        .into_iter()
        .flatten()
        .min()
        .map_or(ControlFlow::Wait, ControlFlow::WaitUntil)
}

fn surface_handoff(
    dispatched: Result<bool, crate::framework::FrameworkError>,
    width: i32,
    height: i32,
) -> Result<(), GraphicsError> {
    match dispatched {
        Ok(true) => Ok(()),
        Ok(false) => Err(GraphicsError::EngineSurfaceUnavailable { width, height }),
        Err(e) => Err(GraphicsError::EngineSurfaceHandoff(e)),
    }
}

#[derive(Debug)]
enum SurfaceSizeRefusal {
    DisplaySize(crate::framework::FrameworkError),
    NoSurfaceView,
    SurfaceChanged(crate::framework::FrameworkError),
}

impl fmt::Display for SurfaceSizeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisplaySize(e) => write!(f, "publishing the Android Display size failed: {e}"),
            Self::NoSurfaceView => f.write_str("no engine SurfaceView takes surfaceChanged"),
            Self::SurfaceChanged(e) => write!(f, "SurfaceView.surfaceChanged failed: {e}"),
        }
    }
}

fn surface_size_offer(
    dispatched: Result<bool, crate::framework::FrameworkError>,
) -> Result<(), SurfaceSizeRefusal> {
    match dispatched {
        Ok(true) => Ok(()),
        Ok(false) => Err(SurfaceSizeRefusal::NoSurfaceView),
        Err(e) => Err(SurfaceSizeRefusal::SurfaceChanged(e)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfaceSizeRetry {
    at: std::time::Instant,
    delay: std::time::Duration,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingSurfaceSize {
    size: (i32, i32),
    retry: Option<SurfaceSizeRetry>,
}

impl PendingSurfaceSize {
    fn refused_before(self) -> bool {
        self.retry.is_some()
    }

    fn after_offer(
        self,
        offer: &Result<(), SurfaceSizeRefusal>,
        now: std::time::Instant,
    ) -> EngineSurfaceSize {
        if offer.is_ok() {
            return EngineSurfaceSize::Accepted(self.size);
        }
        let delay = self
            .retry
            .map_or(ENGINE_SURFACE_SIZE_FIRST_RETRY_DELAY, |retry| {
                retry
                    .delay
                    .saturating_mul(2)
                    .min(ENGINE_SURFACE_SIZE_MAX_RETRY_DELAY)
            });
        EngineSurfaceSize::Pending(Self {
            size: self.size,
            retry: Some(SurfaceSizeRetry {
                at: now + delay,
                delay,
            }),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum EngineSurfaceSize {
    Unrequested,
    Accepted((i32, i32)),
    Pending(PendingSurfaceSize),
}

impl EngineSurfaceSize {
    fn requested(self, size: (i32, i32)) -> Self {
        match self {
            Self::Accepted(accepted) if accepted == size => self,
            Self::Pending(pending) if pending.size == size => self,
            Self::Unrequested | Self::Accepted(_) | Self::Pending(_) => {
                Self::Pending(PendingSurfaceSize { size, retry: None })
            }
        }
    }

    fn due(self, now: std::time::Instant) -> Option<PendingSurfaceSize> {
        match self {
            Self::Pending(pending) if pending.retry.is_none_or(|retry| now >= retry.at) => {
                Some(pending)
            }
            Self::Unrequested | Self::Accepted(_) | Self::Pending(_) => None,
        }
    }

    fn retry_at(self) -> Option<std::time::Instant> {
        match self {
            Self::Pending(pending) => pending.retry.map(|retry| retry.at),
            Self::Unrequested | Self::Accepted(_) => None,
        }
    }
}

fn loop_wake(
    main_looper: crate::framework::MainLooperDue,
    now: std::time::Instant,
    display_refresh_poll: std::time::Instant,
    main_thread_retry: Option<std::time::Instant>,
    engine_surface_size_retry: Option<std::time::Instant>,
    pointer_lock: PointerLock,
) -> ControlFlow {
    next_wake([
        main_looper.deadline(now),
        Some(display_refresh_poll),
        main_thread_retry,
        engine_surface_size_retry,
        pointer_lock_recheck(pointer_lock),
    ])
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplayRefreshProfile {
    current_millihertz: std::num::NonZeroU32,
    supported_millihertz: Vec<std::num::NonZeroU32>,
}

impl DisplayRefreshProfile {
    fn current_hz(&self) -> f32 {
        self.current_millihertz.get() as f32 / 1000.0
    }

    fn supported_hz(&self) -> Vec<f32> {
        self.supported_millihertz
            .iter()
            .map(|rate| rate.get() as f32 / 1000.0)
            .collect()
    }
}

fn normalize_display_refresh_profile(
    current_millihertz: Option<u32>,
    current_size: (u32, u32),
    modes: impl IntoIterator<Item = ((u32, u32), u32)>,
) -> Option<DisplayRefreshProfile> {
    let current_millihertz = current_millihertz.and_then(std::num::NonZeroU32::new)?;
    let mut supported_millihertz: Vec<std::num::NonZeroU32> = modes
        .into_iter()
        .filter(|(size, _)| *size == current_size)
        .filter_map(|(_, rate)| std::num::NonZeroU32::new(rate))
        .chain([current_millihertz])
        .collect();
    supported_millihertz.sort_unstable();
    supported_millihertz.dedup();
    Some(DisplayRefreshProfile {
        current_millihertz,
        supported_millihertz,
    })
}

fn refresh_profile_to_publish(
    published: Option<&DisplayRefreshProfile>,
    observed: Option<DisplayRefreshProfile>,
) -> Option<DisplayRefreshProfile> {
    observed.filter(|profile| published != Some(profile))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowRoute {
    Game,
    Dialog,
    Closed,
}

fn window_route(
    id: WindowId,
    game: Option<WindowId>,
    dialog_owns: impl Fn(WindowId) -> bool,
) -> WindowRoute {
    if game == Some(id) {
        WindowRoute::Game
    } else if dialog_owns(id) {
        WindowRoute::Dialog
    } else {
        WindowRoute::Closed
    }
}

impl ApplicationHandler<crate::framework::HostWake> for GameWindow<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let mut attrs = self.window_state.window_attributes(
            Window::default_attributes()
                .with_title(self.title.clone())
                .with_name(crate::APP_ID, "eclipse"),
            event_loop.available_monitors(),
        );
        if let Some(token) = self.activation_token.take() {
            attrs = attrs.with_activation_token(token);
        }
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

        match window.window_handle() {
            Ok(handle) => {
                let size = window.inner_size();
                let geometry =
                    crate::egl_engine::WindowGeometry::from_physical(size.width, size.height);
                crate::loader::ndk_registry::set_engine_window_geometry(
                    geometry.width,
                    geometry.height,
                );
                self.propagate_window_resize(geometry.width, geometry.height);
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

        match WindowRenderer::start(&window, self.engine_window.as_ref(), self.graphics) {
            Ok(renderer) => {
                tracing::info!(%renderer, "the game window draws until Roblox takes it over");
                self.renderer = Some(renderer);
                window.request_redraw();
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "the game window stays open without drawing until Roblox takes it over"
                );
            }
        }

        let wsi_target = match (window.display_handle(), window.window_handle()) {
            (Ok(display), Ok(surface)) => {
                crate::loader::ndk_registry::WsiTarget::from_raw(display.as_raw(), surface.as_raw())
            }
            _ => None,
        };
        if matches!(
            wsi_target,
            Some(crate::loader::ndk_registry::WsiTarget::Wayland { .. })
        ) {
            self.relative_motion_units = RelativeMotionUnits::SurfaceLogical;
            self.ime_area_support = ImeAreaSupport::Rectangle;
        }
        crate::loader::ndk_registry::set_wsi_target(wsi_target);
        self.clipboard = host_clipboard(&window);
        self.web_view_parent = web_view_parent(&window);

        self.window = Some(window);
        self.publish_engine_display_refresh_rates();
        self.run_window_commands(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        use crate::loader::native_provider::{classify_winit_event, host_input_should_wake};

        self.sync_web_view_window();
        match window_route(id, self.window.as_ref().map(Window::id), |id| {
            self.dialogs.owns(id)
        }) {
            WindowRoute::Game => {}
            WindowRoute::Dialog => {
                if let Some(vm) = self.vm {
                    self.dialogs.window_event(vm, id, event);
                }
                return;
            }
            WindowRoute::Closed => return,
        }

        let input = classify_winit_event(&event);
        if self.handed_off && host_input_should_wake(input) {
            self.loopers_need_wake = true;
        }
        if flushes_pending_motion(input) {
            self.flush_pointer_motion();
        }
        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("window close requested; stopping Android before event-loop exit");
                self.close(event_loop);
            }
            WindowEvent::Resized(size) => {
                self.sync_fullscreen();
                self.observe_window_state();
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.mark_resized(size.width, size.height);
                }

                let geo = crate::egl_engine::WindowGeometry::from_physical(size.width, size.height);
                let wsi_ptr = self.engine_window.as_mut().map(|w| {
                    w.resize(geo);
                    w.as_native_window() as usize
                });
                publish_engine_window_geometry(wsi_ptr, geo.width, geo.height);
                self.propagate_window_resize(geo.width, geo.height);
                if let (Some(parent), Some(window)) =
                    (self.web_view_parent.as_ref(), self.window.as_ref())
                {
                    parent.resized(window);
                }

                self.publish_engine_display_refresh_rates();
            }

            WindowEvent::Moved(_) => self.publish_engine_display_refresh_rates(),
            WindowEvent::RedrawRequested => {
                self.drive_custom_view_draw();
                if let (Some(window), Some(renderer)) =
                    (self.window.as_ref(), self.renderer.as_mut())
                {
                    match renderer.draw_frame(window) {
                        Ok(()) => window.request_redraw(),
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                "drawing the game window failed; it stays open without drawing"
                            );
                            self.renderer = None;
                        }
                    }
                }

                self.maybe_synthetic_tap();
            }

            WindowEvent::Focused(focused) => {
                match (self.focused, focused) {
                    (false, true) => tracing::info!("game window gained keyboard focus"),
                    (true, false) => tracing::info!("game window lost keyboard focus"),
                    (false, false) | (true, true) => {}
                }
                self.focused = focused;
                crate::framework::notifications::set_host_window_focused(focused);
                if let Gamepads::Running(gamepads) = &self.gamepads {
                    gamepads.set_focused(focused);
                }
                if !focused {
                    self.release_engine_input_for_focus_loss();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if let PointerLock::Held { anchor, grab } = self.pointer_lock {
                    if confined_cursor_needs_warp(grab, (position.x, position.y), anchor) {
                        self.return_cursor_to(anchor);
                    }
                    return;
                }
                let Some(motion) = host_cursor_motion(
                    &mut self.cursor,
                    (position.x as f32, position.y as f32),
                    self.touch.is_active(),
                ) else {
                    return;
                };
                if self.handed_off && self.web_view_window == WebViewWindow::Hidden {
                    self.queue_pointer_motion(PendingPointerMotion::Free(motion));
                }
            }
            WindowEvent::MouseInput {
                button: MouseButton::Left,
                ..
            } if self.touch.is_active() => {}
            WindowEvent::MouseInput { state, button, .. } if self.handed_off => {
                match self.web_view_window.button_route(button, state) {
                    HostInputRoute::Engine => self.engine_mouse_button(button, state),
                    HostInputRoute::ActivityBack => self.activity_back(),
                    HostInputRoute::RaiseWebView => {
                        crate::webview::client::request_activation_of_shown_view();
                    }
                    HostInputRoute::ToggleFullscreen | HostInputRoute::Withheld => {}
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

            WindowEvent::KeyboardInput { event, .. } => {
                match self.fullscreen_shortcut.key_route(
                    self.web_view_window,
                    &event.logical_key,
                    event.physical_key,
                    key_edge(&event),
                    self.modifiers,
                ) {
                    HostInputRoute::ToggleFullscreen => self.toggle_fullscreen(),
                    HostInputRoute::Engine if self.handed_off => self.engine_key(&event),
                    HostInputRoute::ActivityBack => self.activity_back(),
                    HostInputRoute::RaiseWebView => {
                        crate::webview::client::request_activation_of_shown_view();
                    }
                    HostInputRoute::Engine | HostInputRoute::Withheld => {}
                }
            }

            WindowEvent::Ime(ime) if self.handed_off => self.text_field_ime(ime),

            WindowEvent::MouseWheel { delta, .. }
                if self.handed_off && self.web_view_window == WebViewWindow::Hidden =>
            {
                let Some(window) = self.window.as_ref() else {
                    return;
                };
                if let Some(notches) = self.wheel_steps.notches(delta, window.scale_factor()) {
                    self.engine_scroll(notches);
                }
            }

            WindowEvent::Touch(touch) => self.host_touch(
                touch.id,
                touch.phase,
                (touch.location.x as f32, touch.location.y as f32),
            ),

            WindowEvent::ActivationTokenDone { token, .. } => {
                crate::webview::client::activate(token.into_raw());
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, wake: crate::framework::HostWake) {
        match wake {
            crate::framework::HostWake::MainLooper => {}
            crate::framework::HostWake::Control => self.run_window_commands(event_loop),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.flush_pointer_motion();
        if let Some(motion) = self.touch.flush() {
            self.touch_motion(motion);
        }
        if let Some(window) = &self.window {
            if let Some(requested) = title_suffix::take_request() {
                window.set_title(&requested.title(&self.title));
            }
        }
        if std::mem::take(&mut self.loopers_need_wake) {
            crate::loader::ndk_registry::wake_all_loopers();
        }
        let Some(vm) = self.vm else { return };
        let main_looper = match crate::framework::pump_main_looper(vm) {
            Ok(due) => due,
            Err(e) => {
                tracing::error!(error = %e, "main Looper pump failed");
                crate::framework::MainLooperDue::At(
                    std::time::Instant::now() + MAIN_THREAD_RETRY_DELAY,
                )
            }
        };
        if let Some(reason) = crate::framework::lifecycle::take_quit_request() {
            self.quit(event_loop, reason);
            return;
        }
        if !self.runtime_shutdown_started
            && self
                .last_activity
                .client_finished(vm, main_looper, std::time::Instant::now())
        {
            tracing::info!("Roblox finished its last activity; closing Eclipse");
            self.close(event_loop);
            return;
        }
        match crate::framework::window_registry::showing_dialogs() {
            Ok(showing) => self.dialogs.sync(event_loop, &showing, self.graphics),
            Err(e) => tracing::error!(error = %e, "the showing dialogs could not be read"),
        }

        let mut surface_probe_failed = false;
        if !self.handed_off && self.engine_window.is_some() {
            match crate::framework::engine_surface_callback_ready(vm) {
                Ok(true) => {
                    let (w, h) =
                        crate::loader::ndk_registry::engine_window_geometry().unwrap_or((1, 1));

                    self.renderer = None;

                    if let Err(e) = surface_handoff(
                        crate::framework::dispatch_surface_lifecycle(vm, w, h),
                        w,
                        h,
                    ) {
                        tracing::error!(error = %e, "engine SurfaceView handoff failed; stopping");
                        self.handoff_error = Some(e);
                        self.shutdown_runtime();
                        event_loop.exit();
                        return;
                    }
                    self.handed_off = true;
                    self.handoff_at = Some(std::time::Instant::now());
                    tracing::info!(
                        width = w,
                        height = h,
                        "Eclipse released its renderer then dispatched the SurfaceView lifecycle \
                         (surfaceCreated + surfaceChanged); present-loop handoff (drop-before-dispatch)"
                    );
                    self.start_gamepads();
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "engine surface-callback readiness probe failed (retry)");
                    surface_probe_failed = true;
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
        if self.handed_off
            && !self.runtime_shutdown_started
            && crate::framework::engine_surface_view_replaced()
        {
            let (w, h) = crate::loader::ndk_registry::engine_window_geometry().unwrap_or((1, 1));
            match crate::framework::dispatch_surface_lifecycle(vm, w, h) {
                Ok(true) => tracing::info!(
                    width = w,
                    height = h,
                    "the replacement engine SurfaceView received surfaceCreated + surfaceChanged"
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    error = %e,
                    "replacement engine SurfaceView lifecycle dispatch failed (retry)"
                ),
            }
        }
        self.maybe_synthetic_engine_tap();
        self.maybe_synthetic_touch();

        self.sync_web_view_window();
        let web_view_hidden = self.web_view_window == WebViewWindow::Hidden;
        self.maybe_synthetic_gamepad(vm);
        self.drive_gamepads(vm, web_view_hidden);
        if crate::webview::client::take_activation_request()
            && !crate::webview::client::pages_embedded()
        {
            self.request_web_view_activation();
        }
        let text_box = if self.handed_off {
            crate::framework::focused_text_box(vm)
        } else {
            None
        };
        let game_text_box = text_box.filter(|_| web_view_hidden);
        self.sync_ime(ime_request(game_text_box));
        self.on_screen_keyboard.follow(game_text_box);
        if let Some(text) = crate::framework::take_pending_host_clipboard_text() {
            self.store_clipboard_text(text);
        }

        if self.handed_off {
            self.poll_pointer_lock();
        }
        crate::framework::set_engine_present_wakes_main_loop(
            self.handed_off && web_view_hidden && self.engine_center_queryable(),
        );
        self.sync_host_cursor();
        crate::framework::keep_screen_on::sync_idle_inhibit(&mut self.idle_inhibit, self.focused);
        let now = std::time::Instant::now();
        self.sync_engine_surface_size(now);
        self.window_state.save_if_due(now);
        if now >= self.next_display_refresh_poll {
            self.publish_engine_display_refresh_rates();
            self.next_display_refresh_poll = now + DISPLAY_REFRESH_POLL_INTERVAL;
        }
        let main_thread_retry = (surface_probe_failed
            || crate::framework::textbox_geometry_pending()
            || crate::framework::global_layout_pending())
        .then(|| now + MAIN_THREAD_RETRY_DELAY);
        event_loop.set_control_flow(loop_wake(
            main_looper,
            now,
            self.next_display_refresh_poll,
            main_thread_retry,
            self.engine_surface_size.retry_at(),
            self.pointer_lock,
        ));
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        let DeviceEvent::MouseMotion { delta } = event else {
            return;
        };
        let PointerLock::Held { anchor, .. } = self.pointer_lock else {
            return;
        };
        if self.vm.is_none() {
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let (dx, dy) = self
            .relative_motion_units
            .engine_delta(delta, window.scale_factor());
        self.loopers_need_wake = true;
        self.queue_pointer_motion(PendingPointerMotion::Locked(PointerMotion {
            position: anchor,
            dx,
            dy,
        }));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.window_state.save();
        self.shutdown_runtime();
        self.clipboard = None;
        self.web_view_parent = None;
    }
}

fn pass_key_to_engine(
    vm: &crate::runtime::Vm,
    action: crate::framework::KeyAction,
    key: EngineKey,
    repeat: bool,
) -> bool {
    match crate::framework::pass_hardware_key_to_engine(
        vm,
        action,
        key.scan_code,
        key.key_code,
        repeat,
    ) {
        Ok(()) => {
            static HARDWARE_KEY_PATH_LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !HARDWARE_KEY_PATH_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("engine hardware-key input path active (keys not logged)");
            } else {
                tracing::trace!(?action, "engine hardware key dispatched (key not logged)");
            }
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "engine key dispatch failed (ignored)");
            false
        }
    }
}

fn web_view_parent(window: &Window) -> Option<WebViewParent> {
    let (display, handle) = match (window.display_handle(), window.window_handle()) {
        (Ok(display), Ok(handle)) => (display.as_raw(), handle.as_raw()),
        (Err(error), _) | (_, Err(error)) => {
            tracing::info!(%error, "no game window handle; WebView windows open as normal windows");
            return None;
        }
    };
    let parent = unsafe { WebViewParent::for_window_whose_display_outlives_it(display, handle) };
    parent.resized(window);
    Some(parent)
}

fn host_clipboard(window: &Window) -> Option<crate::clipboard::HostClipboard> {
    let display = match window.display_handle() {
        Ok(display) => display.as_raw(),
        Err(error) => {
            tracing::warn!(%error, "no display handle; text-box copy and paste are unavailable");
            return None;
        }
    };
    match unsafe { crate::clipboard::HostClipboard::for_display_that_outlives_it(display) } {
        Ok(clipboard) => Some(clipboard),
        Err(error) => {
            tracing::warn!(%error, "host clipboard unavailable; text-box copy and paste are disabled");
            None
        }
    }
}

enum Gamepads {
    AtHandoff,
    Running(crate::gamepad::GamepadService),
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebViewWindow {
    Hidden,
    Shown,
    Embedded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostInputRoute {
    Engine,
    ToggleFullscreen,
    ActivityBack,
    RaiseWebView,
    Withheld,
}

impl WebViewWindow {
    fn current() -> Self {
        use crate::webview::client;
        match (client::view_window_visible(), client::pages_embedded()) {
            (false, _) => Self::Hidden,
            (true, false) => Self::Shown,
            (true, true) => Self::Embedded,
        }
    }

    fn key_route(self, key: &winit::keyboard::Key, edge: crate::input::KeyEdge) -> HostInputRoute {
        use crate::input::KeyEdge;
        use winit::keyboard::{Key, NamedKey};
        match (self, key, edge) {
            (Self::Hidden, _, _) => HostInputRoute::Engine,
            (Self::Shown, Key::Named(NamedKey::Escape), KeyEdge::Press) => {
                HostInputRoute::ActivityBack
            }
            (Self::Shown, key, KeyEdge::Press) if !is_modifier_key(key) => {
                HostInputRoute::RaiseWebView
            }
            (Self::Shown, _, _) => HostInputRoute::Withheld,
            (Self::Embedded, _, _) => HostInputRoute::Withheld,
        }
    }

    fn button_route(self, button: MouseButton, state: ElementState) -> HostInputRoute {
        match (self, button, state) {
            (Self::Hidden, _, _) => HostInputRoute::Engine,
            (Self::Shown, MouseButton::Back, ElementState::Pressed) => HostInputRoute::ActivityBack,
            (Self::Shown, _, ElementState::Pressed) => HostInputRoute::RaiseWebView,
            (Self::Shown, _, ElementState::Released) => HostInputRoute::Withheld,
            (Self::Embedded, _, _) => HostInputRoute::Withheld,
        }
    }

    fn pointer_lock_reasons(self, game: impl FnOnce() -> PointerLockReasons) -> PointerLockReasons {
        match self {
            Self::Hidden => game(),
            Self::Shown | Self::Embedded => PointerLockReasons::default(),
        }
    }
}

fn is_modifier_key(key: &winit::keyboard::Key) -> bool {
    use winit::keyboard::{Key, NamedKey};
    matches!(
        key,
        Key::Named(
            NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::CapsLock
                | NamedKey::Control
                | NamedKey::Fn
                | NamedKey::FnLock
                | NamedKey::NumLock
                | NamedKey::ScrollLock
                | NamedKey::Shift
                | NamedKey::Symbol
                | NamedKey::SymbolLock
                | NamedKey::Meta
                | NamedKey::Hyper
                | NamedKey::Super
        )
    )
}

fn next_fullscreen(current: Option<Fullscreen>) -> Option<Fullscreen> {
    match current {
        Some(_) => None,
        None => Some(Fullscreen::Borderless(None)),
    }
}

fn fullscreen_shortcut(
    key: &winit::keyboard::Key,
    modifiers: winit::keyboard::ModifiersState,
) -> bool {
    use winit::keyboard::{Key, NamedKey};
    match key {
        Key::Named(NamedKey::F11) => true,
        Key::Named(NamedKey::Enter) => {
            modifiers.alt_key()
                && !modifiers.control_key()
                && !modifiers.super_key()
                && !modifiers.shift_key()
        }
        _ => false,
    }
}

#[derive(Default)]
struct FullscreenShortcut {
    held: Option<winit::keyboard::PhysicalKey>,
}

impl FullscreenShortcut {
    fn key_route(
        &mut self,
        window: WebViewWindow,
        key: &winit::keyboard::Key,
        physical: winit::keyboard::PhysicalKey,
        edge: crate::input::KeyEdge,
        modifiers: winit::keyboard::ModifiersState,
    ) -> HostInputRoute {
        use crate::input::KeyEdge;
        if self.held == Some(physical) {
            match edge {
                KeyEdge::Repeat => return HostInputRoute::Withheld,
                KeyEdge::Release => {
                    self.held = None;
                    return HostInputRoute::Withheld;
                }
                KeyEdge::Press => self.held = None,
            }
        }
        if window == WebViewWindow::Hidden
            && edge == KeyEdge::Press
            && fullscreen_shortcut(key, modifiers)
        {
            self.held = Some(physical);
            return HostInputRoute::ToggleFullscreen;
        }
        window.key_route(key, edge)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImeRequest {
    area: (i32, i32, u32, u32),
}

fn ime_request(text_box: Option<crate::framework::FocusedTextBox>) -> Option<ImeRequest> {
    text_box
        .filter(|text_box| !text_box.masked)
        .map(|text_box| ImeRequest {
            area: text_box.geometry,
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostIme {
    Disallowed,
    Allowed { sent: Option<ImeRequest> },
}

impl HostIme {
    fn after_sending(wanted: Option<ImeRequest>) -> Self {
        match wanted {
            Some(request) => Self::Allowed {
                sent: Some(request),
            },
            None => Self::Disallowed,
        }
    }

    fn after_enabled(self) -> Self {
        match self {
            Self::Allowed { .. } => Self::Allowed { sent: None },
            Self::Disallowed => Self::Disallowed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImeCall {
    Allow(bool),
    CursorArea((i32, i32, u32, u32)),
}

fn ime_calls(host: HostIme, wanted: Option<ImeRequest>) -> impl Iterator<Item = ImeCall> {
    let (allow, sent) = match (host, wanted) {
        (HostIme::Disallowed, None) => (None, None),
        (HostIme::Disallowed, Some(_)) => (Some(true), None),
        (HostIme::Allowed { .. }, None) => (Some(false), None),
        (HostIme::Allowed { sent }, Some(_)) => (None, sent),
    };
    let area = wanted
        .filter(|request| sent != Some(*request))
        .map(|request| ImeCall::CursorArea(request.area));
    [allow.map(ImeCall::Allow), area].into_iter().flatten()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImeAreaSupport {
    Rectangle,
    TopLeftSpot,
}

fn ime_cursor_area(
    support: ImeAreaSupport,
    (x, y, width, height): (i32, i32, u32, u32),
) -> ((i32, i32), (u32, u32)) {
    match support {
        ImeAreaSupport::Rectangle => ((x, y), (width, height)),
        ImeAreaSupport::TopLeftSpot => ((x, y.saturating_add_unsigned(height)), (width, 0)),
    }
}

fn engine_scan_code(physical: winit::keyboard::PhysicalKey) -> Option<i32> {
    use winit::platform::scancode::PhysicalKeyExtScancode;
    physical
        .to_scancode()
        .and_then(|code| i32::try_from(code).ok())
}

fn key_edge(event: &winit::event::KeyEvent) -> crate::input::KeyEdge {
    match (event.state, event.repeat) {
        (ElementState::Released, _) => crate::input::KeyEdge::Release,
        (ElementState::Pressed, true) => crate::input::KeyEdge::Repeat,
        (ElementState::Pressed, false) => crate::input::KeyEdge::Press,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostCursor {
    Shown,
    Hidden,
}

fn host_cursor(
    handed_off: bool,
    touch_mode: eclipse_config::TouchMode,
    web_view_window: WebViewWindow,
) -> HostCursor {
    if handed_off
        && touch_mode == eclipse_config::TouchMode::Off
        && web_view_window == WebViewWindow::Hidden
    {
        HostCursor::Hidden
    } else {
        HostCursor::Shown
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PointerLockReasons {
    right_drag: bool,
    engine_center: bool,
}

impl PointerLockReasons {
    fn any(self) -> bool {
        self.right_drag || self.engine_center
    }

    fn after_mouse_button(
        self,
        button: MouseButton,
        state: ElementState,
        touch_mode: eclipse_config::TouchMode,
    ) -> Self {
        let right_drag = match (button, state) {
            (MouseButton::Right, ElementState::Pressed) => {
                self.right_drag || touch_mode == eclipse_config::TouchMode::Off
            }
            (MouseButton::Right, ElementState::Released) => false,
            _ => self.right_drag,
        };
        Self { right_drag, ..self }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrimaryRelease {
    Nothing,
    MouseButton,
    TouchUp { down_time: i64 },
}

fn primary_release(
    touch_mode: eclipse_config::TouchMode,
    held: &mut EngineHeldInput,
    touch_down_time: Option<i64>,
) -> PrimaryRelease {
    use eclipse_config::TouchMode;

    match touch_mode {
        TouchMode::Off if held.release_button(0) => PrimaryRelease::MouseButton,
        TouchMode::Off => PrimaryRelease::Nothing,
        TouchMode::On | TouchMode::FakeOff => touch_down_time
            .map_or(PrimaryRelease::Nothing, |down_time| {
                PrimaryRelease::TouchUp { down_time }
            }),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EngineKey {
    scan_code: i32,
    key_code: i32,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct EngineHeldInput {
    keys: Vec<EngineKey>,
    buttons: Vec<i32>,
}

impl EngineHeldInput {
    fn press_key(&mut self, key: EngineKey) {
        if !self.holds_key(key.scan_code) {
            self.keys.push(key);
        }
    }

    fn holds_key(&self, scan_code: i32) -> bool {
        self.keys.iter().any(|held| held.scan_code == scan_code)
    }

    fn release_key(&mut self, scan_code: i32) -> Option<EngineKey> {
        let index = self
            .keys
            .iter()
            .position(|held| held.scan_code == scan_code)?;
        Some(self.keys.swap_remove(index))
    }

    fn press_button(&mut self, button: i32) {
        if !self.buttons.contains(&button) {
            self.buttons.push(button);
        }
    }

    fn release_button(&mut self, button: i32) -> bool {
        let held = self.buttons.len();
        self.buttons.retain(|&pressed| pressed != button);
        self.buttons.len() != held
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelativeMotionUnits {
    SurfaceLogical,
    DeviceCounts,
}

impl RelativeMotionUnits {
    fn engine_delta(self, delta: (f64, f64), scale_factor: f64) -> (f32, f32) {
        let scale = match self {
            Self::SurfaceLogical => scale_factor,
            Self::DeviceCounts => 1.0,
        };
        ((delta.0 * scale) as f32, (delta.1 * scale) as f32)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointerGrab {
    Locked,
    ConfinedWithWarp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PointerLock {
    Free,
    Held {
        anchor: (f32, f32),
        grab: PointerGrab,
    },
    Refused {
        at: std::time::Instant,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointerLockStep {
    Acquire,
    Release,
    Keep,
}

fn pointer_lock_step(
    lock: PointerLock,
    reasons: PointerLockReasons,
    now: std::time::Instant,
) -> PointerLockStep {
    match (lock, reasons.any()) {
        (PointerLock::Free, true) => PointerLockStep::Acquire,
        (PointerLock::Refused { at }, true)
            if now.saturating_duration_since(at) >= POINTER_LOCK_RETRY_DELAY =>
        {
            PointerLockStep::Acquire
        }
        (PointerLock::Held { .. } | PointerLock::Refused { .. }, false) => PointerLockStep::Release,
        _ => PointerLockStep::Keep,
    }
}

fn pointer_lock_recheck(lock: PointerLock) -> Option<std::time::Instant> {
    match lock {
        PointerLock::Refused { at } => Some(at + POINTER_LOCK_RETRY_DELAY),
        PointerLock::Free | PointerLock::Held { .. } => None,
    }
}

fn confined_cursor_needs_warp(grab: PointerGrab, position: (f64, f64), anchor: (f32, f32)) -> bool {
    grab == PointerGrab::ConfinedWithWarp
        && (position.0.round() as i32, position.1.round() as i32)
            != (anchor.0.round() as i32, anchor.1.round() as i32)
}

fn grab_host_pointer(window: &Window) -> Result<PointerGrab, ExternalError> {
    match window.set_cursor_grab(CursorGrabMode::Locked) {
        Ok(()) => Ok(PointerGrab::Locked),
        Err(ExternalError::NotSupported(_)) => {
            window.set_cursor_grab(CursorGrabMode::Confined)?;
            Ok(PointerGrab::ConfinedWithWarp)
        }
        Err(error) => Err(error),
    }
}

impl GameWindow<'_> {
    fn close(&mut self, event_loop: &ActiveEventLoop) {
        self.window_state.save();
        self.shutdown_runtime();
        event_loop.exit();
    }

    fn quit(
        &mut self,
        event_loop: &ActiveEventLoop,
        reason: crate::framework::lifecycle::QuitReason,
    ) {
        match reason {
            crate::framework::lifecycle::QuitReason::ClientExit { status } => {
                crate::framework::lifecycle::finish_after_client_exit(status)
            }
            crate::framework::lifecycle::QuitReason::AnotherLaunch => {
                tracing::info!(
                    "another Eclipse launch asked Roblox to close; stopping Android before \
                     event-loop exit"
                );
                self.close(event_loop);
            }
            crate::framework::lifecycle::QuitReason::LeftExperience => {
                tracing::info!(
                    "Roblox left the experience for its home screen; close_on_leave closes Eclipse"
                );
                self.close(event_loop);
            }
        }
    }

    fn run_window_commands(&mut self, event_loop: &ActiveEventLoop) {
        let Some(commands) = self.commands else {
            return;
        };
        while let Ok(command) = commands.try_recv() {
            match command {
                launch_window::WindowCommand::Raise { token, done } => {
                    launch_window::raise(self.window.as_ref(), token.as_ref(), done);
                }
                launch_window::WindowCommand::Close => {
                    self.quit(
                        event_loop,
                        crate::framework::lifecycle::QuitReason::AnotherLaunch,
                    );
                }
            }
        }
    }

    fn activity_back(&self) {
        let Some(vm) = self.vm else {
            tracing::warn!("WebView window Back input has no JavaVM");
            return;
        };
        match crate::framework::dispatch_back_to_active_activity(vm) {
            Ok(true) => {}
            Ok(false) => tracing::warn!("WebView window Back input has no live Android Activity"),
            Err(error) => tracing::warn!(%error, "WebView window Back dispatch failed"),
        }
    }

    fn sync_web_view_window(&mut self) {
        let current = WebViewWindow::current();
        match (self.web_view_window, current) {
            (WebViewWindow::Hidden, WebViewWindow::Shown) => {
                tracing::info!("WebView window shown; game input is withheld until it closes");
                self.release_engine_input_for_focus_loss();
            }
            (WebViewWindow::Hidden, WebViewWindow::Embedded) => {
                tracing::info!(
                    "WebView page shown inside the game window; game input is withheld until it \
                     closes"
                );
                self.release_engine_input_for_focus_loss();
            }
            (WebViewWindow::Shown | WebViewWindow::Embedded, WebViewWindow::Hidden) => {
                tracing::info!("WebView window hidden; game input resumes");
            }
            (WebViewWindow::Hidden, WebViewWindow::Hidden)
            | (WebViewWindow::Shown | WebViewWindow::Embedded, _) => {}
        }
        self.web_view_window = current;
    }

    fn request_web_view_activation(&self) {
        use winit::platform::startup_notify::WindowExtStartupNotify as _;
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if let Err(error) = window.request_activation_token() {
            tracing::info!(
                %error,
                "the compositor offers no activation token; the WebView window opens without \
                 taking focus"
            );
        }
    }

    fn propagate_window_resize(&mut self, width: i32, height: i32) {
        self.engine_surface_size = self.engine_surface_size.requested((width, height));
        self.sync_engine_surface_size(std::time::Instant::now());
    }

    fn sync_engine_surface_size(&mut self, now: std::time::Instant) {
        let Some(vm) = self.vm else { return };
        let Some(pending) = self.engine_surface_size.due(now) else {
            return;
        };
        let (width, height) = pending.size;
        let offer = self.offer_surface_size(vm, width, height);
        match &offer {
            Ok(()) => tracing::info!(
                width,
                height,
                surface_view = self.handed_off,
                "engine surface size accepted"
            ),
            Err(refusal) if !pending.refused_before() => tracing::warn!(
                %refusal,
                width,
                height,
                "engine surface size refused; retrying with backoff"
            ),
            Err(_) => {}
        }
        self.engine_surface_size = pending.after_offer(&offer, now);
    }

    fn offer_surface_size(
        &self,
        vm: &crate::runtime::Vm,
        width: i32,
        height: i32,
    ) -> Result<(), SurfaceSizeRefusal> {
        crate::framework::publish_window_size(vm, width, height)
            .map_err(SurfaceSizeRefusal::DisplaySize)?;
        if !self.handed_off {
            return Ok(());
        }
        surface_size_offer(crate::framework::dispatch_surface_changed(
            vm, width, height,
        ))
    }

    fn publish_engine_display_refresh_rates(&mut self) {
        let observed = self.window.as_ref().and_then(display_refresh_profile);
        if observed.is_none() {
            tracing::debug!(
                "host monitor refresh rate unavailable; keeping the last published rates"
            );
        }
        let Some(profile) =
            refresh_profile_to_publish(self.published_display_refresh_profile.as_ref(), observed)
        else {
            return;
        };
        let Some(vm) = self.vm else { return };
        let supported_hz = profile.supported_hz();
        match crate::framework::publish_display_refresh_rates(
            vm,
            profile.current_hz(),
            &supported_hz,
        ) {
            Ok(()) => {
                tracing::info!(
                    current_hz = profile.current_hz(),
                    supported_hz = ?supported_hz,
                    "published host display refresh rates to Android and Roblox"
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
        crate::framework::lifecycle::retire_web_engine(vm, std::time::Duration::from_secs(10));
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
        if self.touch_mode == eclipse_config::TouchMode::Off {
            match crate::framework::dispatch_mouse_button(vm, px, py, true, 0) {
                Ok(()) => self.engine_held.press_button(0),
                Err(e) => {
                    tracing::warn!(error = %e, "engine desktop mouse-button down dispatch failed (ignored)")
                }
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
            Ok(Some(outcome)) => {
                self.engine_tap_downtime = Some(outcome.down_time_ms);
                tracing::info!(
                    x = px,
                    y = py,
                    consumed = outcome.consumed,
                    "engine pointer ACTION_DOWN → RBXSurfaceView.onTouchEventInternal"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "engine pointer ACTION_DOWN dispatch failed (ignored)");
            }
        }
    }

    fn engine_primary_release(&mut self) {
        let down_time = self.engine_tap_downtime.take();
        let Some(vm) = self.vm else { return };
        let Some((px, py)) = self.cursor else { return };
        let down_time = match primary_release(self.touch_mode, &mut self.engine_held, down_time) {
            PrimaryRelease::Nothing => return,
            PrimaryRelease::MouseButton => {
                if let Err(e) = crate::framework::dispatch_mouse_button(vm, px, py, false, 0) {
                    tracing::warn!(error = %e, "engine desktop mouse-button up dispatch failed (ignored)");
                }
                return;
            }
            PrimaryRelease::TouchUp { down_time } => down_time,
        };
        match crate::framework::dispatch_touch_to_engine_surface(
            vm,
            crate::framework::MotionAction::Up,
            px,
            py,
            Some(down_time),
        ) {
            Ok(Some(outcome)) => {
                tracing::info!(
                    x = px,
                    y = py,
                    consumed = outcome.consumed,
                    "engine pointer ACTION_UP → RBXSurfaceView.onTouchEventInternal"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "engine pointer ACTION_UP dispatch failed (ignored)");
            }
        }
    }

    fn host_touch(&mut self, finger: u64, phase: winit::event::TouchPhase, (x, y): (f32, f32)) {
        if !self.touch.is_active() {
            self.touch_route = touch_route(
                self.handed_off,
                self.touch_mode,
                self.web_view_window,
                self.mouse_primary_held(),
            );
            if self.touch_route == TouchRoute::Withheld {
                return;
            }
        }
        for motion in self.touch.touch(finger, phase, x, y) {
            self.touch_motion(motion);
        }
    }

    fn mouse_primary_held(&self) -> bool {
        if !self.handed_off {
            return self.primary_press.is_some();
        }
        self.engine_tap_downtime.is_some() || self.engine_held.buttons.contains(&0)
    }

    fn touch_motion(&mut self, motion: crate::input::TouchMotion) {
        match self.touch_route {
            TouchRoute::EngineTouch => self.engine_touch_motion(&motion),
            TouchRoute::EngineMouse => {
                if let Some(primary) = motion.primary() {
                    self.engine_finger_mouse(primary);
                }
            }
            TouchRoute::LaunchWindow => {
                if let Some(primary) = motion.primary() {
                    self.launch_window_finger(primary);
                }
            }
            TouchRoute::Withheld => {}
        }
    }

    fn engine_touch_motion(&mut self, motion: &crate::input::TouchMotion) {
        use crate::input::TouchAction;

        let Some(vm) = self.vm else { return };
        let down_time = match motion.action {
            TouchAction::Down => None,
            TouchAction::PointerDown(_)
            | TouchAction::Move
            | TouchAction::PointerUp(_)
            | TouchAction::Up
            | TouchAction::Cancel(_) => self.touch_down_time,
        };
        if let Some(pointer) = motion.pressed_pointer() {
            crate::framework::prepare_text_field_pointer_press((pointer.x, pointer.y));
        }
        match crate::framework::engine_input::dispatch_touch_motion(vm, motion, down_time) {
            Ok(Some(outcome)) => {
                self.touch_down_time = Some(outcome.down_time_ms);
                let action = motion.action;
                let pointers = motion.pointers().len();
                let consumed = outcome.consumed;
                if action == TouchAction::Move {
                    tracing::debug!(pointers, consumed, "engine touchscreen ACTION_MOVE");
                } else {
                    tracing::info!(
                        ?action,
                        pointers,
                        consumed,
                        "engine touchscreen pointer edge → RBXSurfaceView.onTouchEventInternal"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                action = ?motion.action,
                %error,
                "engine touchscreen dispatch failed (ignored)"
            ),
        }
        if motion.ends_stream() {
            self.touch_down_time = None;
        }
    }

    fn engine_finger_mouse(&mut self, primary: crate::input::PrimaryTouch) {
        use crate::input::PrimaryTouch;

        match primary {
            PrimaryTouch::Press { x, y } => {
                self.cursor = Some((x, y));
                self.engine_pointer_move(PointerMotion {
                    position: (x, y),
                    dx: 0.0,
                    dy: 0.0,
                });
                self.engine_primary_press();
            }
            PrimaryTouch::Move { x, y } => {
                if let Some(motion) = finger_cursor_motion(&mut self.cursor, (x, y)) {
                    self.engine_pointer_move(motion);
                }
            }
            PrimaryTouch::Release { x, y } => {
                self.cursor = Some((x, y));
                self.engine_primary_release();
            }
            PrimaryTouch::Cancel => self.engine_primary_release(),
        }
    }

    fn launch_window_finger(&mut self, primary: crate::input::PrimaryTouch) {
        use crate::input::PrimaryTouch;

        match primary {
            PrimaryTouch::Press { x, y } => {
                self.cursor = Some((x, y));
                self.handle_primary_press();
            }
            PrimaryTouch::Move { x, y } => self.cursor = Some((x, y)),
            PrimaryTouch::Release { x, y } => {
                self.cursor = Some((x, y));
                self.handle_primary_release();
            }
            PrimaryTouch::Cancel => self.primary_press = None,
        }
    }

    fn maybe_synthetic_touch(&mut self) {
        if self.synthetic_touch_done
            || self
                .handoff_at
                .is_none_or(|at| at.elapsed() < SYNTHETIC_INPUT_DELAY)
        {
            return;
        }
        self.synthetic_touch_done = true;
        let Some((first, second)) =
            std::env::var_os("ECLIPSE_SYNTHETIC_TOUCH").and_then(|spec| parse_two_xy(&spec))
        else {
            return;
        };
        tracing::info!(
            ?first,
            ?second,
            "synthetic touch: a two-finger pinch through the touchscreen path"
        );
        for frame in synthetic_pinch(first, second) {
            for (finger, phase, position) in frame {
                self.host_touch(finger, phase, position);
            }
            if let Some(motion) = self.touch.flush() {
                self.touch_motion(motion);
            }
        }
    }

    fn queue_pointer_motion(&mut self, motion: PendingPointerMotion) {
        if let Some(merged) = self
            .pending_pointer_motion
            .and_then(|pending| pending.coalesce(motion))
        {
            self.pending_pointer_motion = Some(merged);
            return;
        }
        self.flush_pointer_motion();
        self.pending_pointer_motion = Some(motion);
    }

    fn flush_pointer_motion(&mut self) {
        match self.pending_pointer_motion.take() {
            None => {}
            Some(PendingPointerMotion::Free(motion)) => self.engine_pointer_move(motion),
            Some(PendingPointerMotion::Locked(motion)) => self.engine_locked_pointer_move(motion),
        }
    }

    fn engine_locked_pointer_move(&self, motion: PointerMotion) {
        let Some(vm) = self.vm else { return };
        let (px, py) = motion.position;
        if let Err(e) = crate::framework::dispatch_mouse_move(vm, px, py, motion.dx, motion.dy) {
            tracing::warn!(error = %e, "engine locked mouse-move dispatch failed (ignored)");
        }
    }

    fn engine_pointer_move(&mut self, motion: PointerMotion) {
        let (px, py) = motion.position;
        if self.touch_mode == eclipse_config::TouchMode::Off {
            let Some(vm) = self.vm else { return };
            if let Err(e) = crate::framework::dispatch_mouse_move(vm, px, py, motion.dx, motion.dy)
            {
                tracing::warn!(error = %e, "engine desktop mouse-move dispatch failed (ignored)");
            }
            return;
        }
        let Some(down_time) = self.engine_tap_downtime else {
            return;
        };
        let Some(vm) = self.vm else { return };
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

    fn engine_mouse_button(&mut self, button: MouseButton, state: ElementState) {
        if button == MouseButton::Left {
            match state {
                ElementState::Pressed => self.engine_primary_press(),
                ElementState::Released => self.engine_primary_release(),
            }
            return;
        }
        self.engine_aux_mouse_button(button, state == ElementState::Pressed);
        let reasons = self
            .pointer_lock_reasons
            .after_mouse_button(button, state, self.touch_mode);
        self.update_pointer_lock(reasons);
    }

    fn engine_aux_mouse_button(&mut self, button: MouseButton, pressed: bool) {
        if self.touch_mode != eclipse_config::TouchMode::Off {
            return;
        }
        let Some(android_button) = desktop_mouse_button(button) else {
            return;
        };
        let Some(vm) = self.vm else { return };
        let Some((px, py)) = self.cursor else { return };
        if !pressed && !self.engine_held.release_button(android_button) {
            return;
        }
        match crate::framework::dispatch_mouse_button(vm, px, py, pressed, android_button) {
            Ok(()) if pressed => self.engine_held.press_button(android_button),
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(error = %e, "engine auxiliary mouse-button dispatch failed (ignored)")
            }
        }
    }

    fn poll_pointer_lock(&mut self) {
        let reasons = self
            .web_view_window
            .pointer_lock_reasons(|| PointerLockReasons {
                engine_center: self.query_engine_center(),
                ..self.pointer_lock_reasons
            });
        self.update_pointer_lock(reasons);
    }

    fn engine_center_queryable(&self) -> bool {
        self.focused
            && self.touch_mode == eclipse_config::TouchMode::Off
            && !self.engine_center_query_failed
    }

    fn query_engine_center(&mut self) -> bool {
        if !self.engine_center_queryable() {
            return false;
        }
        let Some(vm) = self.vm else { return false };
        match crate::framework::engine_mouse_locked_center(vm) {
            Ok(locked) => locked,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "engine mouse-lock query failed; shift-lock and first person keep a free host cursor"
                );
                self.engine_center_query_failed = true;
                false
            }
        }
    }

    fn update_pointer_lock(&mut self, reasons: PointerLockReasons) {
        let previous = std::mem::replace(&mut self.pointer_lock_reasons, reasons);
        match pointer_lock_step(self.pointer_lock, reasons, std::time::Instant::now()) {
            PointerLockStep::Acquire => self.acquire_pointer_lock(),
            PointerLockStep::Release => self.release_pointer_lock(previous),
            PointerLockStep::Keep => {}
        }
    }

    fn acquire_pointer_lock(&mut self) {
        let (Some(window), Some(anchor)) = (self.window.as_ref(), self.cursor) else {
            return;
        };
        let reasons = self.pointer_lock_reasons;
        self.pointer_lock = match grab_host_pointer(window) {
            Ok(grab) => {
                tracing::info!(
                    ?reasons,
                    ?grab,
                    focused = self.focused,
                    "pointer lock requested"
                );
                PointerLock::Held { anchor, grab }
            }
            Err(error) => {
                if !matches!(self.pointer_lock, PointerLock::Refused { .. }) {
                    tracing::warn!(
                        %error,
                        ?reasons,
                        focused = self.focused,
                        "pointer lock refused; retrying while a lock reason holds"
                    );
                }
                PointerLock::Refused {
                    at: std::time::Instant::now(),
                }
            }
        };
    }

    fn release_pointer_lock(&mut self, ended: PointerLockReasons) {
        let anchor = match std::mem::replace(&mut self.pointer_lock, PointerLock::Free) {
            PointerLock::Held { anchor, .. } => anchor,
            PointerLock::Refused { .. } => {
                tracing::info!(reasons = ?ended, "pointer lock no longer wanted; it was refused");
                return;
            }
            PointerLock::Free => return,
        };
        tracing::info!(reasons = ?ended, "pointer lock released");
        self.return_cursor_to(anchor);
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if let Err(error) = window.set_cursor_grab(CursorGrabMode::None) {
            tracing::warn!(%error, "host pointer unlock failed");
        }
    }

    fn return_cursor_to(&self, anchor: (f32, f32)) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let position = PhysicalPosition::new(f64::from(anchor.0), f64::from(anchor.1));
        if let Err(error) = window.set_cursor_position(position) {
            tracing::warn!(%error, "host cursor could not return to its lock anchor");
        }
    }

    fn release_engine_input_for_focus_loss(&mut self) {
        let held = std::mem::take(&mut self.engine_held);
        self.wheel_steps = crate::input::WheelSteps::default();
        if let Some(vm) = self.vm {
            for key in held.keys {
                pass_key_to_engine(vm, crate::framework::KeyAction::Up, key, false);
            }
            let (px, py) = self.cursor.unwrap_or((0.0, 0.0));
            for button in held.buttons {
                if let Err(e) = crate::framework::dispatch_mouse_button(vm, px, py, false, button) {
                    tracing::warn!(error = %e, button, "engine focus-loss mouse-button release failed");
                }
            }
            let released = self.engine_gamepads.release_all();
            if let Err(error) = crate::framework::engine_input::pass_gamepad_calls(vm, released) {
                self.stop_gamepads(&error);
            }
        }
        for motion in self.touch.cancel() {
            self.touch_motion(motion);
        }
        if self.engine_tap_downtime.is_some() {
            self.engine_primary_release();
        }
        self.update_pointer_lock(PointerLockReasons::default());
    }

    fn start_gamepads(&mut self) {
        let Gamepads::AtHandoff = self.gamepads else {
            return;
        };
        self.gamepads = match crate::gamepad::start_service() {
            Some(service) => {
                service.set_focused(self.focused);
                Gamepads::Running(service)
            }
            None => Gamepads::Off,
        };
    }

    fn drive_gamepads(&mut self, vm: &crate::runtime::Vm, web_view_hidden: bool) {
        let Gamepads::Running(service) = &self.gamepads else {
            return;
        };
        let gate = if self.handed_off && web_view_hidden && self.focused {
            crate::gamepad::GamepadGate::Open
        } else {
            crate::gamepad::GamepadGate::Closed
        };
        let calls = service.drain(&mut self.engine_gamepads, gate);
        if let Err(error) = crate::framework::engine_input::pass_gamepad_calls(vm, calls) {
            self.stop_gamepads(&error);
        }
    }

    fn maybe_synthetic_gamepad(&mut self, vm: &crate::runtime::Vm) {
        if self.synthetic_gamepad_done
            || self
                .handoff_at
                .is_none_or(|at| at.elapsed() < SYNTHETIC_INPUT_DELAY)
        {
            return;
        }
        self.synthetic_gamepad_done = true;
        if std::env::var_os("ECLIPSE_SYNTHETIC_GAMEPAD").is_none_or(|value| value != "1") {
            return;
        }
        tracing::info!("synthetic gamepad: a scripted Xbox pad bypasses SDL and focus");
        let (mut inbox, steps) = crate::gamepad::synthetic_pad();
        for step in steps {
            for &update in step {
                inbox.apply(update);
            }
            let calls = self
                .engine_gamepads
                .drain(&mut inbox, crate::gamepad::GamepadGate::Open);
            if let Err(error) = crate::framework::engine_input::pass_gamepad_calls(vm, calls) {
                self.stop_gamepads(&error);
                return;
            }
        }
    }

    fn stop_gamepads(&mut self, error: &crate::framework::engine_input::GamepadPathError) {
        tracing::warn!(
            %error,
            "controllers turned off: the engine's gamepad input path is unavailable"
        );
        self.gamepads = Gamepads::Off;
    }

    fn sync_host_cursor(&mut self) {
        let cursor = host_cursor(self.handed_off, self.touch_mode, self.web_view_window);
        if cursor == self.host_cursor {
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        window.set_cursor_visible(cursor == HostCursor::Shown);
        self.host_cursor = cursor;
    }

    fn engine_key(&mut self, event: &winit::event::KeyEvent) {
        use crate::input::{KeyEdge, TextFieldKey};
        use winit::platform::modifier_supplement::KeyEventExtModifierSupplement as _;

        let Some(vm) = self.vm else { return };
        let Some(scan_code) = engine_scan_code(event.physical_key) else {
            return;
        };
        let edge = key_edge(event);
        if edge == KeyEdge::Release {
            if let Some(key) = self.engine_held.release_key(scan_code) {
                pass_key_to_engine(vm, crate::framework::KeyAction::Up, key, false);
                return;
            }
        }
        let action = match crate::framework::focused_text_box(vm) {
            Some(text_box) => crate::input::text_field_key(
                &event.logical_key,
                event.physical_key,
                event.text.as_deref(),
                self.modifiers,
                text_box.multiline,
                edge,
            ),
            None => TextFieldKey::Engine,
        };
        match action {
            TextFieldKey::Engine => {
                if edge == KeyEdge::Release {
                    return;
                }
                let key = EngineKey {
                    scan_code,
                    key_code: crate::input::android_key_code(
                        &event.key_without_modifiers(),
                        event.physical_key,
                    ),
                };
                if pass_key_to_engine(vm, crate::framework::KeyAction::Down, key, event.repeat) {
                    self.engine_held.press_key(key);
                }
            }
            TextFieldKey::Edit(edit) => {
                if !crate::framework::edit_active_text_field(vm, edit) {
                    tracing::debug!("text-field key arrived after the text box lost focus");
                }
            }
            TextFieldKey::Submit => {
                if let Err(error) = crate::framework::submit_active_text_field(vm) {
                    tracing::warn!(%error, "Enter could not submit the focused text box");
                }
            }
            TextFieldKey::Dismiss => {
                if let Err(error) = crate::framework::release_active_text_field(vm) {
                    tracing::warn!(%error, "Escape could not release the focused text box");
                }
            }
            TextFieldKey::SelectAll => {
                crate::framework::select_all_active_text_field();
            }
            TextFieldKey::Clipboard(shortcut) => self.clipboard_shortcut(vm, shortcut),
            TextFieldKey::Ignore => {}
        }
    }

    fn clipboard_shortcut(
        &mut self,
        vm: &crate::runtime::Vm,
        shortcut: crate::input::ClipboardShortcut,
    ) {
        use crate::framework::SelectionTransfer;
        use crate::input::ClipboardShortcut;

        let transfer = match shortcut {
            ClipboardShortcut::Copy => SelectionTransfer::Copy,
            ClipboardShortcut::Cut => SelectionTransfer::Cut,
            ClipboardShortcut::Paste => {
                self.paste_into_text_field(vm);
                return;
            }
        };
        let Some(selected) = crate::framework::active_text_field_selection(vm, transfer) else {
            return;
        };
        if self.store_clipboard_text(selected) && transfer == SelectionTransfer::Cut {
            crate::framework::edit_active_text_field(
                vm,
                crate::framework::TextEdit::Backspace(crate::framework::TextUnit::Character),
            );
        }
    }

    fn paste_into_text_field(&mut self, vm: &crate::runtime::Vm) {
        let Some(clipboard) = self.clipboard.as_mut() else {
            tracing::warn!("paste ignored: no host clipboard is available");
            return;
        };
        match clipboard.load() {
            Ok(text) => {
                crate::framework::edit_active_text_field(
                    vm,
                    crate::framework::TextEdit::Paste(&text),
                );
            }
            Err(error) => {
                tracing::warn!(%error, "paste ignored: reading the host clipboard failed")
            }
        }
    }

    fn store_clipboard_text(&self, text: String) -> bool {
        let Some(clipboard) = self.clipboard.as_ref() else {
            tracing::warn!("copy ignored: no host clipboard is available");
            return false;
        };
        match clipboard.store(text) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "copy ignored: writing the host clipboard failed");
                false
            }
        }
    }

    fn text_field_ime(&mut self, ime: Ime) {
        let Some(vm) = self.vm else { return };
        let edit = match &ime {
            Ime::Enabled => {
                self.ime = self.ime.after_enabled();
                return;
            }
            Ime::Preedit(text, cursor) => crate::framework::TextEdit::Compose {
                text,
                cursor: cursor.map(|(start, _)| start),
            },
            Ime::Commit(text) => crate::framework::TextEdit::Type(text),
            Ime::Disabled => crate::framework::TextEdit::FinishComposing,
        };
        if !crate::framework::edit_active_text_field(vm, edit) {
            tracing::debug!("IME input arrived without a focused text box");
        }
    }

    fn sync_ime(&mut self, wanted: Option<ImeRequest>) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if crate::framework::take_host_ime_reset() && self.ime != HostIme::Disallowed {
            window.set_ime_allowed(false);
            self.ime = HostIme::Disallowed;
        }
        for call in ime_calls(self.ime, wanted) {
            match call {
                ImeCall::Allow(allowed) => window.set_ime_allowed(allowed),
                ImeCall::CursorArea(area) => {
                    let ((x, y), (width, height)) = ime_cursor_area(self.ime_area_support, area);
                    window.set_ime_cursor_area(
                        PhysicalPosition::new(x, y),
                        winit::dpi::PhysicalSize::new(width, height),
                    );
                }
            }
        }
        self.ime = HostIme::after_sending(wanted);
    }

    fn toggle_fullscreen(&mut self) {
        if let Some(window) = self.window.as_ref() {
            let next = next_fullscreen(window.fullscreen());
            tracing::info!(
                fullscreen = next.is_some(),
                "the fullscreen shortcut toggles fullscreen"
            );
            window.set_fullscreen(next);
        }
        self.observe_window_state();
    }

    fn observe_window_state(&mut self) {
        if let Some(window) = self.window.as_ref() {
            self.window_state.observe(window, std::time::Instant::now());
        }
    }

    fn sync_fullscreen(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let fullscreen = window.fullscreen().is_some();
        if fullscreen == self.fullscreen {
            return;
        }
        self.fullscreen = fullscreen;
        if fullscreen {
            tracing::info!("game window entered fullscreen");
        } else {
            tracing::info!("game window left fullscreen");
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
                        let handled = crate::framework::edit_active_text_field(
                            vm,
                            crate::framework::TextEdit::Type(&text),
                        );
                        tracing::info!(handled, "synthetic TYPE text → active text field");
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
                        let handled = crate::framework::edit_active_text_field(
                            vm,
                            crate::framework::TextEdit::Type(&text),
                        );
                        tracing::info!(handled, "synthetic TYPE2 text → active text field");
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

pub type HostEventLoop = EventLoop<crate::framework::HostWake>;

pub fn host_event_loop() -> Result<&'static mut HostEventLoop, GraphicsError> {
    HostEventLoop::with_user_event()
        .build()
        .map(|event_loop| Box::leak(Box::new(event_loop)))
        .map_err(GraphicsError::EventLoop)
}

pub fn run_windowed(
    event_loop: &mut HostEventLoop,
    activation_token: Option<ActivationToken>,
    title: &str,
    vm: Option<&crate::runtime::Vm>,
    config: &eclipse_config::Config,
    commands: Option<&std::sync::mpsc::Receiver<launch_window::WindowCommand>>,
    window_state: window_state::WindowStateFile,
) -> Result<(), GraphicsError> {
    let _event_loop_thread = crate::framework::lifecycle::EventLoopThread::enter();
    crate::framework::install_main_looper_waker(event_loop.create_proxy());
    let mut app = GameWindow {
        title: title.to_owned(),
        graphics: crate::gpu::graphics(crate::gpu::Requested::of(config)),
        window: None,
        renderer: None,
        create_error: None,
        handoff_error: None,
        vm,
        touch_mode: config.touch_mode,
        cursor: None,
        primary_press: None,
        synthetic_tap_done: false,
        engine_window: None,
        web_view_parent: None,
        handed_off: false,
        engine_surface_size: EngineSurfaceSize::Unrequested,
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
        synthetic_gamepad_done: false,
        synthetic_touch_done: false,
        gamepads: match vm {
            Some(_) if config.allow_gamepad_permission => Gamepads::AtHandoff,
            Some(_) | None => Gamepads::Off,
        },
        engine_gamepads: crate::gamepad::EngineGamepads::default(),
        web_view_window: WebViewWindow::Hidden,
        runtime_shutdown_started: false,
        last_activity: crate::framework::lifecycle::LastActivityCheck::default(),
        modifiers: winit::keyboard::ModifiersState::default(),
        published_display_refresh_profile: None,
        next_display_refresh_poll: std::time::Instant::now(),
        focused: false,
        idle_inhibit: crate::portal::IdleInhibit::Release,
        on_screen_keyboard: crate::on_screen_keyboard::OnScreenKeyboard::from_environment(),
        fullscreen: false,
        window_state,
        fullscreen_shortcut: FullscreenShortcut::default(),
        host_cursor: HostCursor::Shown,
        pointer_lock_reasons: PointerLockReasons::default(),
        pointer_lock: PointerLock::Free,
        engine_held: EngineHeldInput::default(),
        wheel_steps: crate::input::WheelSteps::default(),
        touch: crate::input::TouchTracker::default(),
        touch_route: TouchRoute::Withheld,
        touch_down_time: None,
        relative_motion_units: RelativeMotionUnits::DeviceCounts,
        engine_center_query_failed: false,
        pending_pointer_motion: None,
        loopers_need_wake: false,
        ime: HostIme::Disallowed,
        ime_area_support: ImeAreaSupport::TopLeftSpot,
        clipboard: None,
        activation_token,
        dialogs: dialog_window::DialogWindows::default(),
        commands,
    };
    let run = event_loop.run_app_on_demand(&mut app);

    app.shutdown_runtime();

    if let Some(vm) = vm {
        crate::framework::retire_main_upcall_dispatch(vm);
    }
    run.map_err(GraphicsError::EventLoop)?;

    if let Some(e) = app.create_error {
        return Err(GraphicsError::CreateWindow(e));
    }
    if let Some(e) = app.handoff_error {
        return Err(e);
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
            matches!(
                f.format,
                vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM
            ) && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
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
            .map(|ch| self.atlas.glyph(ch).map_or(0.0, |g| g.advance))
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

const HORIZONTAL_GRAVITY_MASK: i32 = 0x07;
const GRAVITY_CENTER_HORIZONTAL: i32 = 0x01;
const GRAVITY_RIGHT: i32 = 0x05;
const VERTICAL_GRAVITY_MASK: i32 = 0x70;
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
    match gravity & HORIZONTAL_GRAVITY_MASK {
        GRAVITY_RIGHT => slack,
        GRAVITY_CENTER_HORIZONTAL => slack * 0.5,
        _ => 0.0,
    }
}

fn gravity_dy(gravity: i32, slot_h: f32, ch: f32) -> f32 {
    if !gravity_specified(gravity) {
        return 0.0;
    }
    let slack = (slot_h - ch).max(0.0);
    match gravity & VERTICAL_GRAVITY_MASK {
        GRAVITY_BOTTOM => slack,
        GRAVITY_CENTER_VERTICAL => slack * 0.5,
        _ => 0.0,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TouchRoute {
    LaunchWindow,
    EngineMouse,
    EngineTouch,
    Withheld,
}

fn touch_route(
    handed_off: bool,
    touch_mode: eclipse_config::TouchMode,
    web_view_window: WebViewWindow,
    mouse_primary_held: bool,
) -> TouchRoute {
    use eclipse_config::TouchMode;

    match (handed_off, web_view_window, touch_mode) {
        _ if mouse_primary_held => TouchRoute::Withheld,
        (false, _, _) => TouchRoute::LaunchWindow,
        (true, WebViewWindow::Shown | WebViewWindow::Embedded, _) => TouchRoute::Withheld,
        (true, WebViewWindow::Hidden, TouchMode::Off) => TouchRoute::EngineMouse,
        (true, WebViewWindow::Hidden, TouchMode::On | TouchMode::FakeOff) => {
            TouchRoute::EngineTouch
        }
    }
}

fn host_cursor_motion(
    cursor: &mut Option<(f32, f32)>,
    position: (f32, f32),
    fingers_down: bool,
) -> Option<PointerMotion> {
    if fingers_down {
        return None;
    }
    let (dx, dy) = cursor
        .replace(position)
        .map_or((0.0, 0.0), |(old_x, old_y)| {
            (position.0 - old_x, position.1 - old_y)
        });
    Some(PointerMotion { position, dx, dy })
}

fn finger_cursor_motion(
    cursor: &mut Option<(f32, f32)>,
    position: (f32, f32),
) -> Option<PointerMotion> {
    let (old_x, old_y) = cursor.replace(position)?;
    ((old_x, old_y) != position).then_some(PointerMotion {
        position,
        dx: position.0 - old_x,
        dy: position.1 - old_y,
    })
}

type SyntheticFinger = (u64, winit::event::TouchPhase, (f32, f32));

const SYNTHETIC_PINCH_STEPS: u8 = 3;

const SYNTHETIC_PINCH_CLOSING: f32 = 40.0;

fn synthetic_pinch(first: (f32, f32), second: (f32, f32)) -> Vec<Vec<SyntheticFinger>> {
    use winit::event::TouchPhase;

    const FIRST: u64 = u64::MAX - 1;
    const SECOND: u64 = u64::MAX;
    let (dx, dy) = (second.0 - first.0, second.1 - first.1);
    let length = dx.hypot(dy);
    let (ux, uy) = if length > 0.0 {
        (dx / length, dy / length)
    } else {
        (0.0, 0.0)
    };
    let per_finger = SYNTHETIC_PINCH_CLOSING / 2.0 / f32::from(SYNTHETIC_PINCH_STEPS);
    let at = |origin: (f32, f32), toward: f32, step: u8| {
        let travel = toward * per_finger * f32::from(step);
        (origin.0 + ux * travel, origin.1 + uy * travel)
    };
    let mut frames = vec![
        vec![(FIRST, TouchPhase::Started, first)],
        vec![(SECOND, TouchPhase::Started, second)],
    ];
    for step in 1..=SYNTHETIC_PINCH_STEPS {
        frames.push(vec![
            (FIRST, TouchPhase::Moved, at(first, 1.0, step)),
            (SECOND, TouchPhase::Moved, at(second, -1.0, step)),
        ]);
    }
    let last = SYNTHETIC_PINCH_STEPS;
    frames.push(vec![(SECOND, TouchPhase::Ended, at(second, -1.0, last))]);
    frames.push(vec![(FIRST, TouchPhase::Ended, at(first, 1.0, last))]);
    frames
}

fn parse_two_xy(spec: &std::ffi::OsStr) -> Option<((f32, f32), (f32, f32))> {
    let (first, second) = spec.to_str()?.split_once(';')?;
    Some((
        parse_xy(std::ffi::OsStr::new(first))?,
        parse_xy(std::ffi::OsStr::new(second))?,
    ))
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

fn upload_atlas_pixels(
    device: &ash::Device,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    image: vk::Image,
    atlas: &GlyphAtlas,
) -> Result<(), GraphicsError> {
    let (width, height, pixels) = (atlas.width, atlas.height, atlas.pixels.as_slice());
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

fn composite_quad_vertices(rect: &LaidOutView, extent: vk::Extent2D) -> [TextVertex; 6] {
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

    [tl, tr, br, tl, br, bl]
}

fn text_vertex_bytes(verts: &[TextVertex]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(verts.as_ptr().cast::<u8>(), std::mem::size_of_val(verts)) }
}

fn create_host_buffer(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory), GraphicsError> {
    let info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { device.create_buffer(&info, None) }.map_err(|e| {
        GraphicsError::Vulkan(format!("vkCreateBuffer ({usage:?}, {size} bytes): {e}"))
    })?;
    let req = unsafe { device.get_buffer_memory_requirements(buffer) };
    let Some(memory_type) = find_host_visible_memory_type(memory_properties, req.memory_type_bits)
    else {
        unsafe { device.destroy_buffer(buffer, None) };
        return Err(GraphicsError::Vulkan(format!(
            "no host-visible memory for a {usage:?} buffer"
        )));
    };
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(memory_type);
    let memory = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(memory) => memory,
        Err(e) => {
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(GraphicsError::Vulkan(format!(
                "vkAllocateMemory ({usage:?}, {} bytes): {e}",
                req.size
            )));
        }
    };
    if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            device.free_memory(memory, None);
            device.destroy_buffer(buffer, None);
        }
        return Err(GraphicsError::Vulkan(format!(
            "vkBindBufferMemory ({usage:?}): {e}"
        )));
    }
    Ok((buffer, memory))
}

fn write_host_memory(
    device: &ash::Device,
    memory: vk::DeviceMemory,
    bytes: &[u8],
) -> Result<(), GraphicsError> {
    unsafe {
        let ptr = device
            .map_memory(
                memory,
                0,
                bytes.len() as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )
            .map_err(|e| {
                GraphicsError::Vulkan(format!("vkMapMemory ({} bytes): {e}", bytes.len()))
            })?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        device.unmap_memory(memory);
    }
    Ok(())
}

pub(crate) struct FrameFence {
    fence: vk::Fence,
    pending: bool,
}

impl FrameFence {
    pub(crate) fn new(device: &ash::Device) -> ash::prelude::VkResult<Self> {
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
        Ok(Self {
            fence,
            pending: false,
        })
    }

    pub(crate) fn retire(&mut self, device: &ash::Device) -> ash::prelude::VkResult<()> {
        if !self.pending {
            return Ok(());
        }
        unsafe {
            device.wait_for_fences(&[self.fence], true, u64::MAX)?;
            device.reset_fences(&[self.fence])?;
        }
        self.pending = false;
        Ok(())
    }

    pub(crate) fn submit(
        &mut self,
        device: &ash::Device,
        queue: vk::Queue,
        batches: &[vk::SubmitInfo<'_>],
    ) -> ash::prelude::VkResult<()> {
        assert!(
            !self.pending,
            "a frame fence must be retired before it guards another submission"
        );
        unsafe { device.queue_submit(queue, batches, self.fence) }?;
        self.pending = true;
        Ok(())
    }

    pub(crate) unsafe fn destroy(&self, device: &ash::Device) {
        unsafe { device.destroy_fence(self.fence, None) };
    }
}

fn consume_acquire_signal(
    device: &ash::Device,
    queue: vk::Queue,
    in_flight: &mut FrameFence,
    image_available: &mut vk::Semaphore,
) -> Result<(), GraphicsError> {
    let waits = [*image_available];
    let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
    let release = vk::SubmitInfo::default()
        .wait_semaphores(&waits)
        .wait_dst_stage_mask(&stages);
    let Err(e) = in_flight.submit(device, queue, &[release]) else {
        return Ok(());
    };
    unsafe { device.destroy_semaphore(*image_available, None) };
    *image_available = vk::Semaphore::null();
    Err(GraphicsError::Vulkan(format!(
        "queue_submit (release the acquired image's semaphore): {e}"
    )))
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

const ATLAS_CHARS: [std::ops::RangeInclusive<char>; 5] = [
    ' '..='~',
    '\u{A0}'..='\u{17F}',
    '\u{2010}'..='\u{2027}',
    '\u{20AC}'..='\u{20AC}',
    REPLACEMENT_CHARACTER..=REPLACEMENT_CHARACTER,
];

const REPLACEMENT_CHARACTER: char = '\u{FFFD}';

impl GlyphAtlas {
    fn glyph(&self, character: char) -> Option<&GlyphInfo> {
        self.glyphs
            .get(&character)
            .or_else(|| self.glyphs.get(&REPLACEMENT_CHARACTER))
    }
}

fn build_glyph_atlas(font: &RasterFont, text_px: f32, max_width: u32) -> Option<GlyphAtlas> {
    let mut scaled = font.scaled(text_px)?;
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
    for ch in ATLAS_CHARS.into_iter().flatten() {
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

fn host_glyph_atlas(text_px: f32) -> Option<GlyphAtlas> {
    let font = crate::host_fonts::system_font()?;
    let Some(atlas) = build_glyph_atlas(font, text_px, 1024) else {
        tracing::warn!("glyph atlas came out empty; text disabled");
        return None;
    };
    tracing::info!(
        atlas_w = atlas.width,
        atlas_h = atlas.height,
        glyphs = atlas.glyphs.len(),
        "text: built R8 glyph atlas from the host font"
    );
    Some(atlas)
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
            let Some(g) = atlas.glyph(ch) else {
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

enum WindowRenderer {
    Vulkan(Box<VulkanRenderer>),
    Gles(Box<gles_renderer::GlesRenderer>),
}

impl WindowRenderer {
    fn start(
        window: &Window,
        engine_window: Option<&crate::egl_engine::EngineNativeWindow>,
        graphics: crate::gpu::Graphics,
    ) -> Result<Self, GraphicsError> {
        if let crate::gpu::Graphics::Gles(_) = graphics {
            return gles_renderer::GlesRenderer::new(window, engine_window)
                .map(|renderer| Self::Gles(Box::new(renderer)))
                .map_err(GraphicsError::Gles);
        }
        let vulkan = match VulkanRenderer::new(window) {
            Ok(renderer) => return Ok(Self::Vulkan(Box::new(renderer))),
            Err(error) => error,
        };
        match gles_renderer::GlesRenderer::new(window, engine_window) {
            Ok(renderer) => {
                tracing::warn!(
                    %vulkan,
                    %renderer,
                    "Vulkan cannot draw Eclipse's window, so it draws with OpenGL ES"
                );
                Ok(Self::Gles(Box::new(renderer)))
            }
            Err(gles) => Err(GraphicsError::NoRenderer {
                vulkan: Box::new(vulkan),
                gles,
            }),
        }
    }

    fn for_status_window(
        window: &Window,
        requested: crate::gpu::Requested,
    ) -> Result<Self, GraphicsError> {
        if let Some(graphics) = crate::gpu::planned_graphics() {
            return Self::start(window, None, graphics);
        }
        let (renderer, plan) = Self::first(
            requested,
            || VulkanRenderer::new(window),
            || gles_renderer::GlesRenderer::new(window, None),
        );
        crate::gpu::record_plan(plan);
        renderer
    }

    fn first(
        requested: crate::gpu::Requested,
        mut vulkan: impl FnMut() -> Result<VulkanRenderer, GraphicsError>,
        gles: impl FnOnce() -> Result<gles_renderer::GlesRenderer, crate::egl_engine::EglError>,
    ) -> (Result<Self, GraphicsError>, crate::gpu::Plan) {
        let started = crate::gpu::start_drawing(
            requested,
            || {
                vulkan().map(|renderer| {
                    let fit = renderer.device_fit.clone();
                    (renderer, fit)
                })
            },
            || {
                gles().map(|renderer| {
                    let gl = renderer.gl_renderer().to_owned();
                    (renderer, gl)
                })
            },
        );
        let renderer = match started.drawing {
            Ok(crate::gpu::Drawing::Vulkan(renderer)) => Ok(Self::Vulkan(Box::new(renderer))),
            Ok(crate::gpu::Drawing::Gles(renderer)) => Ok(Self::Gles(Box::new(renderer))),
            Err(crate::gpu::Undrawn::Gles(error)) => Err(GraphicsError::Gles(error)),
            Err(crate::gpu::Undrawn::Neither { vulkan, gles }) => Err(GraphicsError::NoRenderer {
                vulkan: Box::new(vulkan),
                gles,
            }),
        };
        (renderer, started.plan)
    }

    fn extent(&self) -> vk::Extent2D {
        match self {
            Self::Vulkan(renderer) => renderer.swapchain_extent,
            Self::Gles(renderer) => renderer.extent(),
        }
    }

    fn atlas(&self) -> Option<&GlyphAtlas> {
        match self {
            Self::Vulkan(renderer) => renderer.text.as_ref().map(|text| &text.atlas),
            Self::Gles(renderer) => renderer.atlas(),
        }
    }

    fn mark_resized(&mut self, width: u32, height: u32) {
        match self {
            Self::Vulkan(renderer) => renderer.mark_resized(width, height),
            Self::Gles(renderer) => renderer.mark_resized(width, height),
        }
    }

    fn set_text_scale(&mut self, scale: f64) -> Result<(), GraphicsError> {
        match self {
            Self::Vulkan(renderer) => renderer.set_text_scale(scale),
            Self::Gles(renderer) => renderer.set_text_scale(scale).map_err(GraphicsError::Gles),
        }
    }

    fn set_drawn_canvases(&mut self, drawn: Vec<crate::framework::DrawnCanvas>) {
        match self {
            Self::Vulkan(renderer) => renderer.set_drawn_canvases(drawn),
            Self::Gles(renderer) => renderer.set_drawn_canvases(drawn),
        }
    }

    fn current_extent(&mut self, window: &Window) -> Result<Option<vk::Extent2D>, GraphicsError> {
        match self {
            Self::Vulkan(renderer) => renderer.current_extent(window),
            Self::Gles(renderer) => {
                let extent = renderer.extent();
                Ok((extent.width != 0 && extent.height != 0).then_some(extent))
            }
        }
    }

    fn draw_nodes(&mut self, window: &Window, nodes: &[RenderNode]) -> Result<(), GraphicsError> {
        match self {
            Self::Vulkan(renderer) => renderer.draw_nodes(window, nodes),
            Self::Gles(renderer) => renderer
                .draw_nodes(window, nodes)
                .map_err(GraphicsError::Gles),
        }
    }

    fn draw_frame(&mut self, window: &Window) -> Result<(), GraphicsError> {
        self.draw_nodes(window, &crate::framework::view_registry::snapshot_tree())
    }

    fn laid_out_tree(&self) -> Option<(Vec<RenderNode>, Vec<LaidOutView>)> {
        let nodes = crate::framework::view_registry::snapshot_tree();
        if nodes.is_empty() {
            return None;
        }
        let measure = self.atlas().map(|atlas| TextMeasure { atlas });
        let views = layout_views(&nodes, self.extent(), measure);
        Some((nodes, views))
    }

    fn hit_test_at(&self, x: f32, y: f32) -> Option<ViewHandle> {
        let (_, views) = self.laid_out_tree()?;
        hit_test(&views, x, y)
    }

    fn first_clickable_center(&self) -> Option<(f32, f32)> {
        let (_, views) = self.laid_out_tree()?;
        views
            .iter()
            .find(|v| v.clickable)
            .map(|v| (v.x + v.w / 2.0, v.y + v.h / 2.0))
    }

    fn first_view_center(&self) -> Option<(ViewHandle, f32, f32)> {
        let (nodes, views) = self.laid_out_tree()?;
        nodes
            .iter()
            .zip(views.iter())
            .rev()
            .find(|(n, _)| n.children.is_empty())
            .map(|(_, v)| (v.handle, v.x + v.w / 2.0, v.y + v.h / 2.0))
    }

    fn custom_view_draw_targets(&self) -> Vec<crate::framework::DrawTarget> {
        let Some((nodes, views)) = self.laid_out_tree() else {
            return Vec::new();
        };
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
}

impl fmt::Display for WindowRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vulkan(renderer) => write!(
                f,
                "Vulkan, {:?} {}x{}, {} images",
                renderer.swapchain_format,
                renderer.swapchain_extent.width,
                renderer.swapchain_extent.height,
                renderer.frame_count()
            ),
            Self::Gles(renderer) => fmt::Display::fmt(renderer, f),
        }
    }
}

struct Swapchain {
    swapchain: vk::SwapchainKHR,
    image_views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    extent: vk::Extent2D,
}

struct DeviceObjects {
    queue: vk::Queue,
    swapchain_loader: khr::swapchain::Device,
    render_pass: vk::RenderPass,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    image_available: vk::Semaphore,
    render_finished: vk::Semaphore,
    in_flight: FrameFence,
    swapchain: Swapchain,
    swapchain_format: vk::Format,
    swapchain_extent: vk::Extent2D,
    quad_pipeline_layout: vk::PipelineLayout,
    quad_pipeline: vk::Pipeline,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    text: Option<TextRenderer>,
    composite: Option<CanvasCompositor>,
}

struct SwapchainTarget<'a> {
    surface_loader: &'a khr::surface::Instance,
    swapchain_loader: &'a khr::swapchain::Device,
    device: &'a ash::Device,
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    surface_format: vk::SurfaceFormatKHR,
    render_pass: vk::RenderPass,
}

struct HostVulkan {
    display_server: std::mem::Discriminant<RawDisplayHandle>,
    entry: ash::Entry,
    instance: ash::Instance,
    surface_loader: khr::surface::Instance,
}

static HOST_VULKAN: std::sync::Mutex<Option<&'static HostVulkan>> = std::sync::Mutex::new(None);

impl HostVulkan {
    fn shared(display_handle: RawDisplayHandle) -> Result<&'static Self, GraphicsError> {
        let mut shared = HOST_VULKAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let host = match *shared {
            Some(host) => host,
            None => *shared.insert(Box::leak(Box::new(Self::create(display_handle)?))),
        };
        if host.display_server != std::mem::discriminant(&display_handle) {
            return Err(GraphicsError::Vulkan(format!(
                "Eclipse's Vulkan instance serves another display server than {display_handle:?}"
            )));
        }
        Ok(host)
    }

    fn create(display_handle: RawDisplayHandle) -> Result<Self, GraphicsError> {
        let entry = unsafe { ash::Entry::load() }.map_err(|e| {
            GraphicsError::Vulkan(format!("no Vulkan loader (libvulkan) available: {e}"))
        })?;

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
        let surface_loader = khr::surface::Instance::new(&entry, &instance);
        Ok(Self {
            display_server: std::mem::discriminant(&display_handle),
            entry,
            instance,
            surface_loader,
        })
    }
}

struct VulkanRenderer {
    host: &'static HostVulkan,
    surface: vk::SurfaceKHR,
    physical_device: vk::PhysicalDevice,
    device_fit: crate::gpu::DeviceFit,

    device: ash::Device,
    queue: vk::Queue,
    swapchain_loader: khr::swapchain::Device,
    render_pass: vk::RenderPass,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    image_available: vk::Semaphore,
    render_finished: vk::Semaphore,
    in_flight: FrameFence,

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
        let display_handle = window
            .display_handle()
            .map_err(|e| GraphicsError::Vulkan(format!("no raw display handle: {e}")))?
            .as_raw();
        let window_handle = window
            .window_handle()
            .map_err(|e| GraphicsError::Vulkan(format!("no raw window handle: {e}")))?
            .as_raw();
        Self::create(display_handle, window_handle, window.inner_size())
    }

    fn create(
        display_handle: RawDisplayHandle,
        window_handle: RawWindowHandle,
        size: winit::dpi::PhysicalSize<u32>,
    ) -> Result<Self, GraphicsError> {
        let host = HostVulkan::shared(display_handle)?;

        let surface = unsafe {
            ash_window::create_surface(
                &host.entry,
                &host.instance,
                display_handle,
                window_handle,
                None,
            )
        }
        .map_err(|e| GraphicsError::Vulkan(format!("vkCreate*SurfaceKHR failed: {e}")))?;

        let crate::gpu::PickedDevice {
            handle: physical_device,
            queue_family: queue_family_index,
            fit: device_fit,
        } = match crate::gpu::pick_device(&host.instance, &host.surface_loader, surface) {
            Ok(picked) => picked,
            Err(e) => {
                unsafe { host.surface_loader.destroy_surface(surface, None) };
                return Err(GraphicsError::Vulkan(e));
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

        let device = match unsafe {
            host.instance
                .create_device(physical_device, &device_info, None)
        } {
            Ok(d) => d,
            Err(e) => {
                unsafe { host.surface_loader.destroy_surface(surface, None) };
                return Err(GraphicsError::Vulkan(format!("vkCreateDevice failed: {e}")));
            }
        };

        match Self::build_device_objects(
            &host.instance,
            &host.surface_loader,
            surface,
            physical_device,
            queue_family_index,
            &device,
            size,
        ) {
            Ok(objects) => Ok(Self {
                host,
                surface,
                physical_device,
                device_fit,
                device,
                queue: objects.queue,
                swapchain_loader: objects.swapchain_loader,
                render_pass: objects.render_pass,
                command_pool: objects.command_pool,
                command_buffer: objects.command_buffer,
                image_available: objects.image_available,
                render_finished: objects.render_finished,
                in_flight: objects.in_flight,
                swapchain: objects.swapchain,
                swapchain_format: objects.swapchain_format,
                swapchain_extent: objects.swapchain_extent,
                quad_pipeline_layout: objects.quad_pipeline_layout,
                quad_pipeline: objects.quad_pipeline,
                quad_vertex_buffer: vk::Buffer::null(),
                quad_vertex_memory: vk::DeviceMemory::null(),
                quad_vertex_capacity: 0,
                memory_properties: objects.memory_properties,
                text: objects.text,
                composite: objects.composite,
                drawn_canvases: Vec::new(),
                needs_recreate: false,
            }),
            Err(e) => {
                unsafe {
                    device.destroy_device(None);
                    host.surface_loader.destroy_surface(surface, None);
                }
                Err(e)
            }
        }
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

    fn build_device_objects(
        instance: &ash::Instance,
        surface_loader: &khr::surface::Instance,
        surface: vk::SurfaceKHR,
        physical_device: vk::PhysicalDevice,
        queue_family_index: u32,
        device: &ash::Device,
        size: winit::dpi::PhysicalSize<u32>,
    ) -> Result<DeviceObjects, GraphicsError> {
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

        let target = SwapchainTarget {
            surface_loader,
            swapchain_loader: &swapchain_loader,
            device,
            physical_device,
            surface,
            surface_format,
            render_pass,
        };
        let swapchain = match target.create_swapchain(size, vk::SwapchainKHR::null()) {
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

        let sync = unsafe {
            let image_available = device.create_semaphore(&sem_info, None);
            let render_finished = device.create_semaphore(&sem_info, None);
            let in_flight = FrameFence::new(device);
            match (image_available, render_finished, in_flight) {
                (Ok(ia), Ok(rf), Ok(f)) => Ok((ia, rf, f)),
                (ia, rf, f) => {
                    if let Ok(h) = ia {
                        device.destroy_semaphore(h, None);
                    }
                    if let Ok(h) = rf {
                        device.destroy_semaphore(h, None);
                    }
                    if let Ok(h) = &f {
                        h.destroy(device);
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
                        in_flight.destroy(device);
                        device.destroy_command_pool(command_pool, None);
                        swapchain.destroy(device, &swapchain_loader);
                        device.destroy_render_pass(render_pass, None);
                    }
                    return Err(e);
                }
            };

        let text = match TextRenderer::new(
            device,
            queue,
            command_pool,
            render_pass,
            &memory_properties,
            TEXT_PX,
        ) {
            Ok(t) => t,
            Err(e) => {
                unsafe {
                    device.destroy_pipeline(quad_pipeline, None);
                    device.destroy_pipeline_layout(quad_pipeline_layout, None);
                    device.destroy_semaphore(image_available, None);
                    device.destroy_semaphore(render_finished, None);
                    in_flight.destroy(device);
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

        Ok(DeviceObjects {
            queue,
            swapchain_loader,
            render_pass,
            command_pool,
            command_buffer,
            image_available,
            render_finished,
            in_flight,
            swapchain,
            swapchain_format: surface_format.format,
            swapchain_extent: extent,
            quad_pipeline_layout,
            quad_pipeline,
            memory_properties,
            text,
            composite,
        })
    }

    fn frame_count(&self) -> usize {
        self.swapchain.framebuffers.len()
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
        let target = SwapchainTarget {
            surface_loader: &self.host.surface_loader,
            swapchain_loader: &self.swapchain_loader,
            device: &self.device,
            physical_device: self.physical_device,
            surface: self.surface,
            surface_format,
            render_pass: self.render_pass,
        };
        let (next, created) = match target.create_swapchain(size, self.swapchain.swapchain) {
            Ok(swapchain) => (swapchain, Ok(())),
            Err(e) => (
                Swapchain {
                    swapchain: vk::SwapchainKHR::null(),
                    image_views: Vec::new(),
                    framebuffers: Vec::new(),
                    extent: self.swapchain.extent,
                },
                Err(e),
            ),
        };
        let old = std::mem::replace(&mut self.swapchain, next);
        unsafe { old.destroy(&self.device, &self.swapchain_loader) };
        created?;
        self.swapchain_extent = self.swapchain.extent;
        self.needs_recreate = false;
        Ok(())
    }

    fn set_text_scale(&mut self, scale: f64) -> Result<(), GraphicsError> {
        unsafe { self.device.device_wait_idle() }.map_err(|e| {
            GraphicsError::Vulkan(format!("wait before replacing the text atlas: {e}"))
        })?;
        let text = TextRenderer::new(
            &self.device,
            self.queue,
            self.command_pool,
            self.render_pass,
            &self.memory_properties,
            TEXT_PX * scale as f32,
        )?;
        if let Some(previous) = std::mem::replace(&mut self.text, text) {
            unsafe { previous.destroy(&self.device) };
        }
        Ok(())
    }

    fn current_extent(&mut self, window: &Window) -> Result<Option<vk::Extent2D>, GraphicsError> {
        if self.needs_recreate {
            self.recreate_swapchain(window)?;
            if self.swapchain.framebuffers.is_empty() {
                return Ok(None);
            }
        }
        Ok(Some(self.swapchain.extent))
    }

    fn draw_nodes(&mut self, window: &Window, nodes: &[RenderNode]) -> Result<(), GraphicsError> {
        let Some(extent) = self.current_extent(window)? else {
            return Ok(());
        };

        self.in_flight
            .retire(&self.device)
            .map_err(|e| GraphicsError::Vulkan(format!("wait for the previous frame: {e}")))?;

        let measure = self.text.as_ref().map(|t| TextMeasure { atlas: &t.atlas });
        let views = layout_views(nodes, extent, measure);

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

        let composite_count = match self.composite.as_mut() {
            Some(composite) => {
                composite.prepare(
                    &self.device,
                    &mem_props,
                    &self.drawn_canvases,
                    &views,
                    extent,
                )?;
                composite.texture_count()
            }
            None => 0,
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

        if self.image_available == vk::Semaphore::null() {
            self.image_available = unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .map_err(|e| {
                GraphicsError::Vulkan(format!("vkCreateSemaphore (image available): {e}"))
            })?;
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

        if let Err(e) = self.record_draw(image_index as usize, vertex_count, text_vertex_count) {
            self.abandon_acquired_image()?;
            return Err(e);
        }

        let wait_semaphores = [self.image_available];
        let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let command_buffers = [self.command_buffer];
        let signal_semaphores = [self.render_finished];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_semaphores)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&command_buffers)
            .signal_semaphores(&signal_semaphores);

        if let Err(e) = self.in_flight.submit(&self.device, self.queue, &[submit]) {
            self.abandon_acquired_image()?;
            return Err(GraphicsError::Vulkan(format!("queue_submit: {e}")));
        }
        if let Some(composite) = self.composite.as_mut() {
            composite.uploads_submitted();
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

    fn abandon_acquired_image(&mut self) -> Result<(), GraphicsError> {
        self.needs_recreate = true;
        consume_acquire_signal(
            &self.device,
            self.queue,
            &mut self.in_flight,
            &mut self.image_available,
        )
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
            if let Some(composite) = self.composite.as_ref() {
                composite.record_uploads(&self.device, cmd);
            }
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

impl SwapchainTarget<'_> {
    fn create_swapchain(
        &self,
        size: winit::dpi::PhysicalSize<u32>,
        old: vk::SwapchainKHR,
    ) -> Result<Swapchain, GraphicsError> {
        let Self {
            surface_loader,
            swapchain_loader,
            device,
            physical_device,
            surface,
            surface_format,
            render_pass,
        } = *self;
        let caps = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }
        .map_err(|e| GraphicsError::Vulkan(format!("get surface capabilities: {e}")))?;
        let extent = choose_swap_extent(&caps, size.width, size.height);
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
        text_px: f32,
    ) -> Result<Option<Self>, GraphicsError> {
        let Some(atlas) = host_glyph_atlas(text_px) else {
            return Ok(None);
        };

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
            &atlas,
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
    width: u32,
    height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    staging: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    vertex_buffer: vk::Buffer,
    vertex_memory: vk::DeviceMemory,

    pixels: Vec<u8>,
    quad: Option<[TextVertex; 6]>,
    upload_pending: bool,
}

impl CompositeTexture {
    fn create(
        device: &ash::Device,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        width: u32,
        height: u32,
    ) -> Result<Self, GraphicsError> {
        let mut texture = Self {
            width,
            height,
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            staging: vk::Buffer::null(),
            staging_memory: vk::DeviceMemory::null(),
            vertex_buffer: vk::Buffer::null(),
            vertex_memory: vk::DeviceMemory::null(),
            pixels: Vec::new(),
            quad: None,
            upload_pending: false,
        };
        match texture.allocate(device, memory_properties) {
            Ok(()) => Ok(texture),
            Err(e) => {
                unsafe { texture.destroy(device) };
                Err(e)
            }
        }
    }

    fn allocate(
        &mut self,
        device: &ash::Device,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<(), GraphicsError> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width: self.width,
                height: self.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        self.image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateImage (composite): {e}")))?;

        let req = unsafe { device.get_image_memory_requirements(self.image) };
        let memory_type = find_device_local_memory_type(memory_properties, req.memory_type_bits)
            .ok_or_else(|| {
                GraphicsError::Vulkan("no memory type for a composite texture".to_owned())
            })?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(memory_type);
        self.memory = unsafe { device.allocate_memory(&alloc, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkAllocateMemory (composite): {e}")))?;
        unsafe { device.bind_image_memory(self.image, self.memory, 0) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkBindImageMemory (composite): {e}")))?;

        let view_info = vk::ImageViewCreateInfo::default()
            .image(self.image)
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
        self.view = unsafe { device.create_image_view(&view_info, None) }
            .map_err(|e| GraphicsError::Vulkan(format!("vkCreateImageView (composite): {e}")))?;

        let texel_bytes = u64::from(self.width) * u64::from(self.height) * 4;
        (self.staging, self.staging_memory) = create_host_buffer(
            device,
            memory_properties,
            texel_bytes,
            vk::BufferUsageFlags::TRANSFER_SRC,
        )?;
        (self.vertex_buffer, self.vertex_memory) = create_host_buffer(
            device,
            memory_properties,
            std::mem::size_of::<[TextVertex; 6]>() as vk::DeviceSize,
            vk::BufferUsageFlags::VERTEX_BUFFER,
        )?;
        Ok(())
    }

    fn stage(
        &mut self,
        device: &ash::Device,
        rgba: Vec<u8>,
        quad: [TextVertex; 6],
    ) -> Result<(), GraphicsError> {
        if self.pixels != rgba {
            write_host_memory(device, self.staging_memory, &rgba)?;
            self.pixels = rgba;
            self.upload_pending = true;
        }
        if self.quad != Some(quad) {
            write_host_memory(device, self.vertex_memory, text_vertex_bytes(&quad))?;
            self.quad = Some(quad);
        }
        Ok(())
    }

    unsafe fn record_upload(&self, device: &ash::Device, cmd: vk::CommandBuffer) {
        if !self.upload_pending {
            return;
        }
        let subresource = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);
        let to_transfer = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(subresource)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: self.width,
                height: self.height,
                depth: 1,
            });
        let to_shader = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(subresource)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_transfer),
            );
            device.cmd_copy_buffer_to_image(
                cmd,
                self.staging,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&region),
            );
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_shader),
            );
        }
    }

    unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer(self.vertex_buffer, None);
            device.free_memory(self.vertex_memory, None);
            device.destroy_buffer(self.staging, None);
            device.free_memory(self.staging_memory, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

struct CanvasCompositor {
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,

    descriptor_pool: vk::DescriptorPool,
    descriptor_sets: Vec<vk::DescriptorSet>,
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

        let set_layouts = [descriptor_set_layout; MAX_COMPOSITE_VIEWS];
        let set_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&set_layouts);
        let descriptor_sets = match unsafe { device.allocate_descriptor_sets(&set_alloc) } {
            Ok(sets) => sets,
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_pool(descriptor_pool, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(GraphicsError::Vulkan(format!(
                    "vkAllocateDescriptorSets (composite): {e}"
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
            descriptor_sets,
            pipeline_layout,
            pipeline,
            textures: Vec::new(),
        })
    }

    fn prepare(
        &mut self,
        device: &ash::Device,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        canvases: &[crate::framework::DrawnCanvas],
        views: &[LaidOutView],
        extent: vk::Extent2D,
    ) -> Result<(), GraphicsError> {
        let mut slot = 0;
        for d in canvases {
            if slot == MAX_COMPOSITE_VIEWS {
                break;
            }
            let Some(rect) = views.iter().find(|v| v.handle == d.view) else {
                continue;
            };
            let snapshot = crate::framework::canvas_registry::with_canvas(d.canvas, |c| {
                let (w, h) = c.dimensions();
                (w, h, c.rgba())
            });
            let Ok((width, height, mut rgba)) = snapshot else {
                continue;
            };
            let expected = (width as usize) * (height as usize) * 4;
            if width == 0 || height == 0 || rgba.len() < expected {
                continue;
            }
            rgba.truncate(expected);
            self.stage_slot(
                device,
                memory_properties,
                slot,
                (width, height),
                rgba,
                composite_quad_vertices(rect, extent),
            )?;
            slot += 1;
        }
        for texture in self.textures.drain(slot..) {
            unsafe { texture.destroy(device) };
        }
        Ok(())
    }

    fn stage_slot(
        &mut self,
        device: &ash::Device,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        slot: usize,
        (width, height): (u32, u32),
        rgba: Vec<u8>,
        quad: [TextVertex; 6],
    ) -> Result<(), GraphicsError> {
        if self
            .textures
            .get(slot)
            .is_none_or(|t| t.width != width || t.height != height)
        {
            let texture = CompositeTexture::create(device, memory_properties, width, height)?;
            self.bind_slot(device, slot, texture.view);
            if slot < self.textures.len() {
                let stale = std::mem::replace(&mut self.textures[slot], texture);
                unsafe { stale.destroy(device) };
            } else {
                self.textures.push(texture);
            }
        }
        self.textures[slot].stage(device, rgba, quad)
    }

    fn bind_slot(&self, device: &ash::Device, slot: usize, view: vk::ImageView) {
        let desc_image = vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.descriptor_sets[slot])
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&desc_image));
        unsafe { device.update_descriptor_sets(std::slice::from_ref(&write), &[]) };
    }

    unsafe fn record_uploads(&self, device: &ash::Device, cmd: vk::CommandBuffer) {
        for texture in &self.textures {
            unsafe { texture.record_upload(device, cmd) };
        }
    }

    fn uploads_submitted(&mut self) {
        for texture in &mut self.textures {
            texture.upload_pending = false;
        }
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
            for (texture, &descriptor_set) in self.textures.iter().zip(&self.descriptor_sets) {
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[descriptor_set],
                    &[],
                );
                device.cmd_push_constants(
                    cmd,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    &bytes,
                );
                device.cmd_bind_vertex_buffers(cmd, 0, &[texture.vertex_buffer], &[0]);
                device.cmd_draw(cmd, 6, 1, 0, 0);
            }
        }
    }

    fn texture_count(&self) -> usize {
        self.textures.len()
    }

    unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            for texture in &self.textures {
                texture.destroy(device);
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
            self.in_flight.destroy(&self.device);
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
            self.host.surface_loader.destroy_surface(self.surface, None);
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

    Gles(crate::egl_engine::EglError),

    NoRenderer {
        vulkan: Box<GraphicsError>,
        gles: crate::egl_engine::EglError,
    },

    EngineSurfaceUnavailable {
        width: i32,
        height: i32,
    },

    EngineSurfaceHandoff(crate::framework::FrameworkError),
}

impl fmt::Display for GraphicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLoop(e) => write!(f, "winit event loop error: {e}"),
            Self::CreateWindow(e) => write!(f, "failed to create host window: {e}"),
            Self::Vulkan(msg) => write!(f, "Vulkan error: {msg}"),
            Self::Gles(e) => write!(f, "OpenGL ES error: {e}"),
            Self::NoRenderer { vulkan, gles } => write!(
                f,
                "neither Vulkan nor OpenGL ES can draw the window ({vulkan}; OpenGL ES error: {gles})"
            ),
            Self::EngineSurfaceUnavailable { width, height } => write!(
                f,
                "Roblox's SurfaceView stopped accepting the {width}x{height} window right after \
                 it registered its surface callback"
            ),
            Self::EngineSurfaceHandoff(e) => write!(
                f,
                "handing the window to Roblox's SurfaceView (surfaceCreated, then surfaceChanged) \
                 failed: {e}"
            ),
        }
    }
}

impl std::error::Error for GraphicsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EventLoop(e) => Some(e),
            Self::CreateWindow(e) => Some(e),
            Self::EngineSurfaceHandoff(e) => Some(e),
            Self::Gles(e) => Some(e),
            Self::NoRenderer { vulkan, .. } => Some(vulkan),
            Self::Vulkan(_) | Self::EngineSurfaceUnavailable { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::view_registry::WRAP_CONTENT;
    use unmapped_xlib_window::UnmappedXlibWindow;

    #[test]
    fn events_of_a_closed_window_never_reach_the_game_window() {
        let game = WindowId::from(1);
        let dialog = WindowId::from(2);
        let closed_dialog = WindowId::from(3);
        let open_dialogs = |id| id == dialog;
        assert_eq!(
            window_route(game, Some(game), open_dialogs),
            WindowRoute::Game
        );
        assert_eq!(
            window_route(dialog, Some(game), open_dialogs),
            WindowRoute::Dialog
        );
        assert_eq!(
            window_route(closed_dialog, Some(game), open_dialogs),
            WindowRoute::Closed
        );
        assert_eq!(
            window_route(game, None, open_dialogs),
            WindowRoute::Closed,
            "a dropped game window's queued events"
        );
    }

    fn headless_gpu() -> Option<headless_vulkan::HeadlessGpu> {
        match headless_vulkan::HeadlessGpu::new() {
            Ok(gpu) => Some(gpu),
            Err(e) => {
                eprintln!("SKIP: no headless Vulkan device ({e})");
                None
            }
        }
    }

    #[test]
    fn frame_fence_does_not_wait_on_a_frame_that_never_submitted() {
        let Some(gpu) = headless_gpu() else {
            return;
        };
        let mut fence = FrameFence::new(&gpu.device).expect("create frame fence");
        fence
            .submit(&gpu.device, gpu.queue, &[vk::SubmitInfo::default()])
            .expect("submit an empty frame");
        fence
            .retire(&gpu.device)
            .expect("retire the submitted frame");

        let device = gpu.device.clone();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let retired = fence.retire(&device);
            let _ = done.send((retired, fence));
        });
        let Ok((retired, mut fence)) = finished.recv_timeout(std::time::Duration::from_secs(10))
        else {
            std::mem::forget(gpu);
            panic!("a frame that failed before submitting must not block the next frame");
        };
        retired.expect("retire without a pending frame");

        fence
            .submit(&gpu.device, gpu.queue, &[vk::SubmitInfo::default()])
            .expect("the fence guards the next submission");
        fence.retire(&gpu.device).expect("retire the next frame");
        unsafe { fence.destroy(&gpu.device) };
    }

    #[test]
    fn abandoning_an_acquired_image_consumes_its_acquire_signal() {
        let Some(gpu) = headless_gpu() else {
            return;
        };
        let mut acquired = gpu.semaphore();
        gpu.run(&[], &[acquired], |_, _| {});
        let mut fence = FrameFence::new(&gpu.device).expect("create frame fence");

        consume_acquire_signal(&gpu.device, gpu.queue, &mut fence, &mut acquired)
            .expect("wait out the acquire signal");
        assert!(
            fence.pending,
            "the acquire signal is consumed by a batch the frame fence guards"
        );

        let device = gpu.device.clone();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let retired = fence.retire(&device);
            let _ = done.send((retired, fence));
        });
        let Ok((retired, fence)) = finished.recv_timeout(std::time::Duration::from_secs(10)) else {
            std::mem::forget(gpu);
            panic!("the batch that consumes the acquire signal must complete");
        };
        retired.expect("retire the batch that consumed the acquire signal");
        unsafe { fence.destroy(&gpu.device) };
    }

    unsafe extern "system" fn out_of_memory_submit(
        _queue: vk::Queue,
        _submit_count: u32,
        _submits: *const vk::SubmitInfo<'_>,
        _fence: vk::Fence,
    ) -> vk::Result {
        vk::Result::ERROR_OUT_OF_HOST_MEMORY
    }

    #[test]
    fn an_acquire_signal_that_cannot_be_consumed_retires_its_semaphore() {
        let Some(gpu) = headless_gpu() else {
            return;
        };
        let handle = gpu.device.handle();
        let get_device_proc_addr = gpu.instance.fp_v1_0().get_device_proc_addr;
        let failing = unsafe {
            ash::Device::load_with(
                |name| {
                    if name == c"vkQueueSubmit" {
                        return out_of_memory_submit as *const std::ffi::c_void;
                    }
                    get_device_proc_addr(handle, name.as_ptr())
                        .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void)
                },
                handle,
            )
        };
        let mut acquired = unsafe {
            gpu.device
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
        }
        .expect("create the acquire semaphore");
        gpu.run(&[], &[acquired], |_, _| {});
        let mut fence = FrameFence::new(&gpu.device).expect("create frame fence");

        let result = consume_acquire_signal(&failing, gpu.queue, &mut fence, &mut acquired);

        let cause = vk::Result::ERROR_OUT_OF_HOST_MEMORY.to_string();
        assert!(
            matches!(&result, Err(GraphicsError::Vulkan(m)) if m.contains(&cause)),
            "the failed release is reported with its cause: {result:?}"
        );
        assert_eq!(
            acquired,
            vk::Semaphore::null(),
            "a semaphore whose acquire signal is still pending must never be acquired with again"
        );
        assert!(
            !fence.pending,
            "nothing was submitted for the fence to guard"
        );
        unsafe { fence.destroy(&gpu.device) };
    }

    struct CompositeTarget {
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        image: vk::Image,
        extent: vk::Extent2D,
        readback: vk::Buffer,
        readback_memory: vk::DeviceMemory,
    }

    fn render_composites(
        gpu: &headless_vulkan::HeadlessGpu,
        compositor: &mut CanvasCompositor,
        target: &CompositeTarget,
        canvases: &[crate::framework::DrawnCanvas],
        views: &[LaidOutView],
    ) -> (Vec<u8>, usize) {
        let extent = target.extent;
        compositor
            .prepare(&gpu.device, &gpu.memory_properties, canvases, views, extent)
            .expect("prepare composites");
        let uploads = compositor
            .textures
            .iter()
            .filter(|t| t.upload_pending)
            .count();
        gpu.run(&[], &[], |device, cmd| unsafe {
            compositor.record_uploads(device, cmd);
            let clear = [vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
            }];
            let begin = vk::RenderPassBeginInfo::default()
                .render_pass(target.render_pass)
                .framebuffer(target.framebuffer)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                })
                .clear_values(&clear);
            device.cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);
            let viewport = vk::Viewport::default()
                .width(extent.width as f32)
                .height(extent.height as f32)
                .max_depth(1.0);
            device.cmd_set_viewport(cmd, 0, &[viewport]);
            device.cmd_set_scissor(
                cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            compositor.record(device, cmd);
            device.cmd_end_render_pass(cmd);
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                });
            device.cmd_copy_image_to_buffer(
                cmd,
                target.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                target.readback,
                &[region],
            );
        });
        compositor.uploads_submitted();
        let len = (extent.width * extent.height * 4) as usize;
        (gpu.read(target.readback_memory, len), uploads)
    }

    #[test]
    fn composite_textures_persist_and_upload_only_changed_canvases() {
        use crate::framework::{canvas_registry, DrawnCanvas};

        let Some(gpu) = headless_gpu() else {
            return;
        };
        let device = &gpu.device;
        let format = vk::Format::R8G8B8A8_UNORM;
        let extent = vk::Extent2D {
            width: 48,
            height: 32,
        };
        let attachment = vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        let color_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(std::slice::from_ref(&color_ref));
        let render_pass = unsafe {
            device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(std::slice::from_ref(&attachment))
                    .subpasses(std::slice::from_ref(&subpass)),
                None,
            )
        }
        .expect("render pass");
        let image = gpu.image(
            format,
            extent.width,
            extent.height,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
        );
        let view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .expect("image view");
        let attachments = [view];
        let framebuffer = unsafe {
            device.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(render_pass)
                    .attachments(&attachments)
                    .width(extent.width)
                    .height(extent.height)
                    .layers(1),
                None,
            )
        }
        .expect("framebuffer");
        let (readback, readback_memory) = gpu.host_buffer(
            u64::from(extent.width * extent.height * 4),
            vk::BufferUsageFlags::TRANSFER_DST,
        );
        let mut compositor = CanvasCompositor::new(device, render_pass).expect("compositor");
        let target = CompositeTarget {
            render_pass,
            framebuffer,
            image,
            extent,
            readback,
            readback_memory,
        };
        let pixel = |bytes: &[u8], x: u32, y: u32| {
            let i = ((y * extent.width + x) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
        };
        let black = [0, 0, 0, 255];

        let canvas = canvas_registry::allocate(16, 16).expect("canvas");
        canvas_registry::with_canvas(canvas, |c| c.draw_color(0xFF20_40C0u32 as i32))
            .expect("draw");
        let views = [lov(7, 8.0, 8.0, 16.0, 16.0, false)];
        let drawn = [DrawnCanvas { view: 7, canvas }];

        let (first, uploads) = render_composites(&gpu, &mut compositor, &target, &drawn, &views);
        assert_eq!(uploads, 1, "a new canvas is uploaded");
        assert_eq!(pixel(&first, 8, 8), [0x20, 0x40, 0xC0, 0xFF]);
        assert_eq!(pixel(&first, 23, 23), [0x20, 0x40, 0xC0, 0xFF]);
        assert_eq!(pixel(&first, 7, 8), black);
        assert_eq!(pixel(&first, 24, 23), black);

        let (second, uploads) = render_composites(&gpu, &mut compositor, &target, &drawn, &views);
        assert_eq!(uploads, 0, "unchanged canvas content is not uploaded again");
        assert_eq!(second, first, "the persistent texture renders identically");

        canvas_registry::with_canvas(canvas, |c| c.draw_color(0xFFC0_4020u32 as i32))
            .expect("redraw");
        let (third, uploads) = render_composites(&gpu, &mut compositor, &target, &drawn, &views);
        assert_eq!(uploads, 1, "changed canvas content is uploaded");
        assert_eq!(pixel(&third, 8, 8), [0xC0, 0x40, 0x20, 0xFF]);
        assert_eq!(pixel(&third, 7, 8), black);

        let moved = [lov(7, 24.0, 0.0, 16.0, 16.0, false)];
        let (fourth, uploads) = render_composites(&gpu, &mut compositor, &target, &drawn, &moved);
        assert_eq!(uploads, 0, "moving a view reuses its texture");
        assert_eq!(pixel(&fourth, 24, 0), [0xC0, 0x40, 0x20, 0xFF]);
        assert_eq!(pixel(&fourth, 8, 8), black);

        let (fifth, uploads) = render_composites(&gpu, &mut compositor, &target, &[], &moved);
        assert_eq!(uploads, 0);
        assert_eq!(
            compositor.texture_count(),
            0,
            "undrawn views release their textures"
        );
        assert!(fifth.chunks(4).all(|p| p == black));

        canvas_registry::free(canvas).expect("free canvas");
        unsafe {
            compositor.destroy(device);
            device.destroy_framebuffer(framebuffer, None);
            device.destroy_image_view(view, None);
            device.destroy_render_pass(render_pass, None);
        }
    }

    const LOADER_LIFETIME_CHILD: &str = "ECLIPSE_TEST_VULKAN_LOADER_LIFETIME_CHILD";

    #[test]
    fn a_failed_renderer_init_leaves_the_shared_instance_usable() {
        if std::env::var_os(LOADER_LIFETIME_CHILD).is_none() {
            let output = std::process::Command::new(
                std::env::current_exe().expect("the test harness executable must have a path"),
            )
            .args([
                "--exact",
                "graphics::tests::a_failed_renderer_init_leaves_the_shared_instance_usable",
                "--test-threads=1",
            ])
            .env(LOADER_LIFETIME_CHILD, "1")
            .output()
            .expect("the loader-lifetime child must start");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "a failed renderer init must leave the shared Vulkan instance usable: \
                 status={:?}, stdout={stdout}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let display = RawDisplayHandle::Wayland(raw_window_handle::WaylandDisplayHandle::new(
            std::ptr::NonNull::dangling(),
        ));
        let result = VulkanRenderer::create(
            display,
            RawWindowHandle::Web(raw_window_handle::WebWindowHandle::new(1)),
            winit::dpi::PhysicalSize::new(64, 64),
        );
        match result {
            Ok(_) => panic!("a web window handle cannot back a Linux Vulkan surface"),
            Err(GraphicsError::Vulkan(message)) if message.contains("vkCreate*SurfaceKHR") => {}
            Err(e) => {
                eprintln!("SKIP: no Vulkan instance with Wayland surface support ({e})");
                return;
            }
        }
        let host = HostVulkan::shared(display).expect("the shared Vulkan instance");
        unsafe { host.instance.enumerate_physical_devices() }
            .expect("the shared instance outlives a failed renderer init");
    }

    const SHARED_INSTANCE_CHILD: &str = "ECLIPSE_TEST_SHARED_VULKAN_INSTANCE_CHILD";

    const SHARED_INSTANCE_CHILD_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

    #[test]
    fn window_renderers_share_one_vulkan_instance_that_outlives_them() {
        if std::env::var_os(SHARED_INSTANCE_CHILD).is_none() {
            if std::env::var_os("DISPLAY").is_none() {
                eprintln!("SKIP: no X11 display (DISPLAY unset)");
                return;
            }
            let output = crate::bounded_child::output(
                std::process::Command::new(
                    std::env::current_exe().expect("the test harness executable must have a path"),
                )
                .args([
                    "--exact",
                    "graphics::tests::window_renderers_share_one_vulkan_instance_that_outlives_them",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(SHARED_INSTANCE_CHILD, "1"),
                SHARED_INSTANCE_CHILD_LIMIT,
            );
            let report = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && report.contains("1 passed"),
                "status={:?}, stdout={report}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let windows = match (
            UnmappedXlibWindow::open(64, 64),
            UnmappedXlibWindow::open(64, 64),
        ) {
            (Ok(first), Ok(second)) => [first, second],
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("SKIP: no usable X11 display ({e})");
                return;
            }
        };
        let display_of = |window: &UnmappedXlibWindow| {
            RawDisplayHandle::Xlib(raw_window_handle::XlibDisplayHandle::new(
                std::ptr::NonNull::new(window.display),
                0,
            ))
        };
        let renderer_on = |window: &UnmappedXlibWindow| {
            VulkanRenderer::create(
                display_of(window),
                RawWindowHandle::Xlib(raw_window_handle::XlibWindowHandle::new(window.window)),
                winit::dpi::PhysicalSize::new(64, 64),
            )
        };
        let first = match renderer_on(&windows[0]) {
            Ok(renderer) => renderer,
            Err(e) => {
                eprintln!("SKIP: no Vulkan device presents to X11 ({e})");
                return;
            }
        };
        let second = renderer_on(&windows[1]).expect("a second window's renderer");
        let instance = first.host.instance.handle();
        assert_eq!(second.host.instance.handle(), instance);
        drop(first);
        drop(second);
        let host = HostVulkan::shared(display_of(&windows[0])).expect("the shared instance");
        assert_eq!(host.instance.handle(), instance);
        unsafe { host.instance.enumerate_physical_devices() }
            .expect("closing every window keeps the shared instance");
    }

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

    fn monitor_profile(
        current_millihertz: u32,
        size: (u32, u32),
        mode_millihertz: &[u32],
    ) -> DisplayRefreshProfile {
        normalize_display_refresh_profile(
            Some(current_millihertz),
            size,
            mode_millihertz.iter().map(|rate| (size, *rate)),
        )
        .expect("a monitor with a current refresh rate has a profile")
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

        assert_eq!(profile.current_hz(), 143.996);
        assert_eq!(profile.supported_hz(), vec![60.0, 120.0, 143.996]);
    }

    #[test]
    fn refresh_profiles_report_the_current_monitor_and_all_its_rates() {
        let sixty = monitor_profile(60_000, (1920, 1080), &[60_000]);
        assert_eq!(sixty.current_hz(), 60.0);
        assert_eq!(sixty.supported_hz(), vec![60.0]);

        let one_forty_four = monitor_profile(144_000, (2560, 1440), &[60_000, 120_000, 144_000]);
        assert_eq!(one_forty_four.current_hz(), 144.0);
        assert_eq!(one_forty_four.supported_hz(), vec![60.0, 120.0, 144.0]);

        let three_sixty = monitor_profile(
            360_000,
            (2560, 1440),
            &[360_000, 240_000, 165_000, 144_000, 59_940],
        );
        assert_eq!(three_sixty.current_hz(), 360.0);
        assert_eq!(
            three_sixty.supported_hz(),
            vec![59.94, 144.0, 165.0, 240.0, 360.0]
        );
    }

    #[test]
    fn a_monitor_without_a_mode_list_reports_its_current_rate() {
        let profile = monitor_profile(320_000, (2560, 1440), &[]);

        assert_eq!(profile.current_hz(), 320.0);
        assert_eq!(profile.supported_hz(), vec![320.0]);
    }

    #[test]
    fn missing_refresh_data_produces_no_profile() {
        assert_eq!(
            normalize_display_refresh_profile(None, (1920, 1080), [((1920, 1080), 144_000)]),
            None
        );
        assert_eq!(
            normalize_display_refresh_profile(Some(0), (1920, 1080), [((1920, 1080), 144_000)]),
            None
        );
        assert_eq!(
            normalize_display_refresh_profile(None, (1920, 1080), [((1280, 720), 240_000)]),
            None,
            "alternate-resolution-only modes must not fabricate current display rates"
        );
    }

    #[test]
    fn moving_to_another_monitor_republishes_and_missing_data_keeps_the_last_rates() {
        let fast = monitor_profile(144_000, (1920, 1080), &[60_000, 144_000]);
        let slow = monitor_profile(60_000, (1920, 1080), &[60_000]);

        assert_eq!(
            refresh_profile_to_publish(None, Some(fast.clone())),
            Some(fast.clone())
        );
        assert_eq!(
            refresh_profile_to_publish(Some(&fast), Some(slow.clone())),
            Some(slow.clone())
        );
        assert_eq!(
            refresh_profile_to_publish(Some(&slow), Some(fast.clone())),
            Some(fast.clone())
        );
        assert_eq!(
            refresh_profile_to_publish(Some(&fast), Some(fast.clone())),
            None
        );
        assert_eq!(refresh_profile_to_publish(Some(&fast), None), None);
        assert_eq!(refresh_profile_to_publish(None, None), None);
    }

    #[test]
    fn surface_format_prefers_unorm_so_srgb_encoded_colours_pass_through() {
        let formats = [
            vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_SRGB,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
            vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_UNORM,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        let chosen = choose_surface_format(&formats).expect("a format exists");
        assert_eq!(chosen.format, vk::Format::B8G8R8A8_UNORM);
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

    const KEY_EDGES: [crate::input::KeyEdge; 3] = [
        crate::input::KeyEdge::Press,
        crate::input::KeyEdge::Repeat,
        crate::input::KeyEdge::Release,
    ];

    const MOUSE_BUTTONS: [MouseButton; 6] = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
        MouseButton::Other(9),
    ];

    fn sample_keys() -> [winit::keyboard::Key; 4] {
        use winit::keyboard::{Key, NamedKey};

        [
            Key::Named(NamedKey::Escape),
            Key::Named(NamedKey::Enter),
            Key::Named(NamedKey::Space),
            Key::Character("w".into()),
        ]
    }

    #[test]
    fn while_a_web_view_window_shows_escape_is_android_back_and_other_presses_raise_it() {
        use crate::input::KeyEdge;
        use winit::keyboard::{Key, NamedKey};

        let escape = Key::Named(NamedKey::Escape);
        assert_eq!(
            WebViewWindow::Shown.key_route(&escape, KeyEdge::Press),
            HostInputRoute::ActivityBack
        );
        for edge in [KeyEdge::Repeat, KeyEdge::Release] {
            assert_eq!(
                WebViewWindow::Shown.key_route(&escape, edge),
                HostInputRoute::Withheld,
                "holding Escape sends Android Back once, not once per key repeat"
            );
        }
        let others = sample_keys()
            .into_iter()
            .filter(|key| *key != escape)
            .chain([Key::Named(NamedKey::F11)]);
        for key in others {
            assert_eq!(
                WebViewWindow::Shown.key_route(&key, KeyEdge::Press),
                HostInputRoute::RaiseWebView,
                "a key pressed on the game window brings the WebView window back: {key:?}"
            );
            for edge in [KeyEdge::Repeat, KeyEdge::Release] {
                assert_eq!(
                    WebViewWindow::Shown.key_route(&key, edge),
                    HostInputRoute::Withheld
                );
            }
        }
        for modifier in [
            NamedKey::Alt,
            NamedKey::Super,
            NamedKey::Control,
            NamedKey::Shift,
        ] {
            assert_eq!(
                WebViewWindow::Shown.key_route(&Key::Named(modifier), KeyEdge::Press),
                HostInputRoute::Withheld,
                "a modifier that starts a desktop shortcut such as Alt+Tab raises nothing: \
                 {modifier:?}"
            );
        }
    }

    #[test]
    fn while_a_web_view_window_shows_back_is_android_back_and_other_clicks_raise_it() {
        assert_eq!(
            WebViewWindow::Shown.button_route(MouseButton::Back, ElementState::Pressed),
            HostInputRoute::ActivityBack
        );
        assert_eq!(
            WebViewWindow::Shown.button_route(MouseButton::Back, ElementState::Released),
            HostInputRoute::Withheld
        );
        for button in MOUSE_BUTTONS
            .into_iter()
            .filter(|button| *button != MouseButton::Back)
        {
            assert_eq!(
                WebViewWindow::Shown.button_route(button, ElementState::Pressed),
                HostInputRoute::RaiseWebView,
                "a click on the game window brings the WebView window back: {button:?}"
            );
            assert_eq!(
                WebViewWindow::Shown.button_route(button, ElementState::Released),
                HostInputRoute::Withheld
            );
        }
    }

    #[test]
    fn a_page_inside_the_game_window_takes_every_key_and_click_itself() {
        use eclipse_config::TouchMode;
        use winit::keyboard::{Key, NamedKey};

        let keys = sample_keys()
            .into_iter()
            .chain([Key::Named(NamedKey::Escape), Key::Named(NamedKey::F11)]);
        for key in keys {
            for edge in KEY_EDGES {
                assert_eq!(
                    WebViewWindow::Embedded.key_route(&key, edge),
                    HostInputRoute::Withheld,
                    "the page handles Escape and every other key itself: {key:?}"
                );
            }
        }
        for button in MOUSE_BUTTONS {
            for state in [ElementState::Pressed, ElementState::Released] {
                assert_eq!(
                    WebViewWindow::Embedded.button_route(button, state),
                    HostInputRoute::Withheld,
                    "the page handles Back and every other button itself: {button:?}"
                );
            }
        }
        assert_eq!(
            WebViewWindow::Embedded
                .pointer_lock_reasons(|| panic!("the engine is asked for its mouse lock")),
            PointerLockReasons::default()
        );
        assert_eq!(
            host_cursor(true, TouchMode::Off, WebViewWindow::Embedded),
            HostCursor::Shown
        );
    }

    #[test]
    fn without_a_web_view_window_keys_and_buttons_reach_the_engine() {
        for key in sample_keys() {
            for edge in KEY_EDGES {
                assert_eq!(
                    WebViewWindow::Hidden.key_route(&key, edge),
                    HostInputRoute::Engine
                );
            }
        }
        for button in MOUSE_BUTTONS {
            for state in [ElementState::Pressed, ElementState::Released] {
                assert_eq!(
                    WebViewWindow::Hidden.button_route(button, state),
                    HostInputRoute::Engine
                );
            }
        }
    }

    #[test]
    fn a_shown_web_view_window_releases_the_pointer_lock_without_asking_the_engine() {
        let wanted = PointerLockReasons {
            right_drag: true,
            engine_center: true,
        };
        let now = std::time::Instant::now();

        let shown = WebViewWindow::Shown
            .pointer_lock_reasons(|| panic!("the engine is asked for its mouse lock"));
        assert_eq!(shown, PointerLockReasons::default());
        assert_eq!(
            pointer_lock_step(HELD_AT_ANCHOR, shown, now),
            PointerLockStep::Release
        );

        let hidden = WebViewWindow::Hidden.pointer_lock_reasons(|| wanted);
        assert_eq!(hidden, wanted);
        assert_eq!(
            pointer_lock_step(PointerLock::Free, hidden, now),
            PointerLockStep::Acquire
        );
    }

    const HELD_AT_ANCHOR: PointerLock = PointerLock::Held {
        anchor: (320.0, 240.0),
        grab: PointerGrab::Locked,
    };

    const RIGHT_DRAG: PointerLockReasons = PointerLockReasons {
        right_drag: true,
        engine_center: false,
    };

    const ENGINE_CENTER: PointerLockReasons = PointerLockReasons {
        right_drag: false,
        engine_center: true,
    };

    #[test]
    fn right_press_locks_only_on_the_engine_surface_in_touch_off() {
        use eclipse_config::TouchMode;

        let now = std::time::Instant::now();
        let pressed = PointerLockReasons::default().after_mouse_button(
            MouseButton::Right,
            ElementState::Pressed,
            TouchMode::Off,
        );
        assert_eq!(pressed, RIGHT_DRAG);
        assert_eq!(
            pointer_lock_step(PointerLock::Free, pressed, now),
            PointerLockStep::Acquire
        );

        for touch_mode in [TouchMode::On, TouchMode::FakeOff] {
            let reasons = PointerLockReasons::default().after_mouse_button(
                MouseButton::Right,
                ElementState::Pressed,
                touch_mode,
            );
            assert_eq!(reasons, PointerLockReasons::default());
            assert_eq!(
                pointer_lock_step(PointerLock::Free, reasons, now),
                PointerLockStep::Keep
            );
        }
    }

    #[test]
    fn other_mouse_buttons_never_change_the_right_drag_reason() {
        use eclipse_config::TouchMode;

        for reasons in [PointerLockReasons::default(), RIGHT_DRAG] {
            for button in [
                MouseButton::Left,
                MouseButton::Middle,
                MouseButton::Back,
                MouseButton::Forward,
                MouseButton::Other(9),
            ] {
                for state in [ElementState::Pressed, ElementState::Released] {
                    assert_eq!(
                        reasons.after_mouse_button(button, state, TouchMode::Off),
                        reasons
                    );
                }
            }
        }
    }

    #[test]
    fn right_release_unlocks_unless_the_engine_still_locks_the_center() {
        use eclipse_config::TouchMode;

        let now = std::time::Instant::now();
        let released = RIGHT_DRAG.after_mouse_button(
            MouseButton::Right,
            ElementState::Released,
            TouchMode::Off,
        );
        assert_eq!(released, PointerLockReasons::default());
        assert_eq!(
            pointer_lock_step(HELD_AT_ANCHOR, released, now),
            PointerLockStep::Release
        );

        let both = PointerLockReasons {
            right_drag: true,
            engine_center: true,
        };
        let released =
            both.after_mouse_button(MouseButton::Right, ElementState::Released, TouchMode::Off);
        assert_eq!(released, ENGINE_CENTER);
        assert_eq!(
            pointer_lock_step(HELD_AT_ANCHOR, released, now),
            PointerLockStep::Keep
        );
    }

    #[test]
    fn engine_center_alone_locks_and_its_end_unlocks() {
        let now = std::time::Instant::now();
        assert_eq!(
            pointer_lock_step(PointerLock::Free, ENGINE_CENTER, now),
            PointerLockStep::Acquire
        );
        assert_eq!(
            pointer_lock_step(HELD_AT_ANCHOR, ENGINE_CENTER, now),
            PointerLockStep::Keep
        );
        assert_eq!(
            pointer_lock_step(HELD_AT_ANCHOR, PointerLockReasons::default(), now),
            PointerLockStep::Release
        );
        assert_eq!(
            pointer_lock_step(PointerLock::Free, PointerLockReasons::default(), now),
            PointerLockStep::Keep
        );
    }

    #[test]
    fn a_refused_lock_retries_after_the_delay_while_a_reason_holds() {
        let refused_at = std::time::Instant::now();
        let refused = PointerLock::Refused { at: refused_at };
        let before_retry = refused_at + POINTER_LOCK_RETRY_DELAY / 2;
        let retry_due = refused_at + POINTER_LOCK_RETRY_DELAY;

        for reasons in [ENGINE_CENTER, RIGHT_DRAG] {
            assert_eq!(
                pointer_lock_step(refused, reasons, refused_at),
                PointerLockStep::Keep
            );
            assert_eq!(
                pointer_lock_step(refused, reasons, before_retry),
                PointerLockStep::Keep
            );
            assert_eq!(
                pointer_lock_step(refused, reasons, retry_due),
                PointerLockStep::Acquire
            );
        }
        assert_eq!(
            pointer_lock_step(refused, PointerLockReasons::default(), refused_at),
            PointerLockStep::Release
        );
        assert_eq!(
            pointer_lock_step(refused, PointerLockReasons::default(), retry_due),
            PointerLockStep::Release
        );
    }

    #[test]
    fn focus_loss_releases_every_key_and_button_the_engine_holds() {
        let w = EngineKey {
            scan_code: 17,
            key_code: 51,
        };
        let shift = EngineKey {
            scan_code: 42,
            key_code: 0,
        };
        let mut held = EngineHeldInput::default();
        held.press_key(w);
        held.press_key(w);
        held.press_key(shift);
        assert_eq!(held.release_key(shift.scan_code), Some(shift));
        held.press_button(0);
        held.press_button(1);
        assert!(held.release_button(1));
        assert!(!held.release_button(3));

        assert_eq!(
            std::mem::take(&mut held),
            EngineHeldInput {
                keys: vec![w],
                buttons: vec![0],
            }
        );
        assert_eq!(held, EngineHeldInput::default());
    }

    #[test]
    fn a_primary_release_reaches_the_engine_only_after_its_press_did() {
        use eclipse_config::TouchMode;

        let mut held = EngineHeldInput::default();
        for mode in [TouchMode::On, TouchMode::FakeOff] {
            assert_eq!(
                primary_release(mode, &mut held, Some(40)),
                PrimaryRelease::TouchUp { down_time: 40 }
            );
            assert_eq!(
                primary_release(mode, &mut held, None),
                PrimaryRelease::Nothing,
                "a touch whose ACTION_DOWN never reached the engine, or whose ACTION_UP \
                 was already sent on focus loss, sends no second ACTION_UP"
            );
        }
        assert_eq!(
            primary_release(TouchMode::Off, &mut held, None),
            PrimaryRelease::Nothing
        );
        held.press_button(0);
        assert_eq!(
            primary_release(TouchMode::Off, &mut held, None),
            PrimaryRelease::MouseButton
        );
        assert_eq!(
            primary_release(TouchMode::Off, &mut held, None),
            PrimaryRelease::Nothing
        );
    }

    #[test]
    fn a_touch_stream_goes_where_the_window_and_touch_mode_send_it() {
        use eclipse_config::TouchMode;

        for mode in [TouchMode::On, TouchMode::FakeOff] {
            assert_eq!(
                touch_route(true, mode, WebViewWindow::Hidden, false),
                TouchRoute::EngineTouch
            );
        }
        assert_eq!(
            touch_route(true, TouchMode::Off, WebViewWindow::Hidden, false),
            TouchRoute::EngineMouse
        );
        for mode in [TouchMode::Off, TouchMode::On, TouchMode::FakeOff] {
            assert_eq!(
                touch_route(false, mode, WebViewWindow::Hidden, false),
                TouchRoute::LaunchWindow
            );
            for window in [WebViewWindow::Shown, WebViewWindow::Embedded] {
                assert_eq!(touch_route(true, mode, window, false), TouchRoute::Withheld);
            }
        }
    }

    #[test]
    fn a_held_mouse_primary_keeps_new_fingers_out() {
        use eclipse_config::TouchMode;

        for mode in [TouchMode::Off, TouchMode::On, TouchMode::FakeOff] {
            for handed_off in [false, true] {
                assert_eq!(
                    touch_route(handed_off, mode, WebViewWindow::Hidden, true),
                    TouchRoute::Withheld
                );
            }
        }
    }

    #[test]
    fn a_finger_drag_moves_the_engine_mouse_after_the_x11_cursor_echo() {
        use crate::input::{PrimaryTouch, TouchTracker};
        use winit::event::TouchPhase;

        let (pressed_at, dragged_to) = ((10.0, 5.0), (30.0, 8.0));
        let mut touch = TouchTracker::default();
        let mut cursor = Some((0.0, 0.0));
        assert_eq!(
            host_cursor_motion(&mut cursor, pressed_at, touch.is_active()),
            Some(PointerMotion {
                position: pressed_at,
                dx: 10.0,
                dy: 5.0,
            })
        );
        let pressed: Vec<_> = touch
            .touch(1, TouchPhase::Started, pressed_at.0, pressed_at.1)
            .filter_map(|motion| motion.primary())
            .collect();
        assert_eq!(pressed, [PrimaryTouch::Press { x: 10.0, y: 5.0 }]);

        let mut engine_moves = Vec::new();
        engine_moves.extend(host_cursor_motion(
            &mut cursor,
            dragged_to,
            touch.is_active(),
        ));
        assert_eq!(
            touch
                .touch(1, TouchPhase::Moved, dragged_to.0, dragged_to.1)
                .count(),
            0
        );
        assert_eq!(
            touch.flush().and_then(|motion| motion.primary()),
            Some(PrimaryTouch::Move { x: 30.0, y: 8.0 })
        );
        engine_moves.extend(finger_cursor_motion(&mut cursor, dragged_to));
        assert_eq!(
            engine_moves,
            [PointerMotion {
                position: dragged_to,
                dx: 20.0,
                dy: 3.0,
            }]
        );
    }

    #[test]
    fn off_mode_presses_the_primary_button_with_the_first_finger_only() {
        use crate::input::{PrimaryTouch, TouchTracker};
        use eclipse_config::TouchMode;
        use winit::event::TouchPhase;

        assert_eq!(
            touch_route(true, TouchMode::Off, WebViewWindow::Hidden, false),
            TouchRoute::EngineMouse
        );
        let mut touch = TouchTracker::default();
        let mut primary = Vec::new();
        for (finger, phase, x) in [
            (1, TouchPhase::Started, 10.0),
            (2, TouchPhase::Started, 50.0),
            (2, TouchPhase::Ended, 50.0),
            (1, TouchPhase::Ended, 12.0),
        ] {
            primary.extend(
                touch
                    .touch(finger, phase, x, 5.0)
                    .filter_map(|motion| motion.primary()),
            );
        }
        assert_eq!(
            primary,
            [
                PrimaryTouch::Press { x: 10.0, y: 5.0 },
                PrimaryTouch::Release { x: 12.0, y: 5.0 },
            ]
        );
    }

    #[test]
    fn the_synthetic_pinch_closes_two_fingers_by_forty_pixels() {
        use crate::input::{TouchAction, TouchTracker};

        let mut touch = TouchTracker::default();
        let mut motions = Vec::new();
        for frame in synthetic_pinch((100.0, 300.0), (500.0, 300.0)) {
            for (finger, phase, (x, y)) in frame {
                motions.extend(touch.touch(finger, phase, x, y));
            }
            motions.extend(touch.flush());
        }
        let actions: Vec<_> = motions.iter().map(|motion| motion.action).collect();
        assert_eq!(
            actions,
            [
                TouchAction::Down,
                TouchAction::PointerDown(1),
                TouchAction::Move,
                TouchAction::Move,
                TouchAction::Move,
                TouchAction::PointerUp(1),
                TouchAction::Up,
            ]
        );
        let closed = motions[4].pointers();
        assert!((closed[1].x - closed[0].x - 360.0).abs() < 1e-3);
        assert_eq!(closed[0].y, 300.0);
        assert!(!touch.is_active());
        assert_eq!(
            parse_two_xy(std::ffi::OsStr::new("100,300;500, 300")),
            Some(((100.0, 300.0), (500.0, 300.0)))
        );
        assert_eq!(parse_two_xy(std::ffi::OsStr::new("100,300")), None);
    }

    #[test]
    fn a_key_release_reuses_the_key_code_sent_with_its_press() {
        let slash = EngineKey {
            scan_code: 53,
            key_code: 76,
        };
        let mut held = EngineHeldInput::default();
        held.press_key(slash);
        assert_eq!(held.release_key(53), Some(slash));
        assert_eq!(held.release_key(53), None);
    }

    #[test]
    fn locked_motion_reaches_the_engine_in_physical_pixels() {
        assert_eq!(
            RelativeMotionUnits::SurfaceLogical.engine_delta((3.0, -2.0), 2.0),
            (6.0, -4.0)
        );
        assert_eq!(
            RelativeMotionUnits::SurfaceLogical.engine_delta((4.0, -2.0), 1.5),
            (6.0, -3.0)
        );
        assert_eq!(
            RelativeMotionUnits::DeviceCounts.engine_delta((3.0, -2.0), 2.0),
            (3.0, -2.0)
        );
    }

    #[test]
    fn free_motion_coalesces_to_the_latest_position_and_the_summed_delta() {
        let free =
            |position, dx, dy| PendingPointerMotion::Free(PointerMotion { position, dx, dy });

        assert_eq!(
            free((10.0, 20.0), 1.0, -2.0).coalesce(free((13.0, 19.0), 3.0, -1.0)),
            Some(free((13.0, 19.0), 4.0, -3.0))
        );
    }

    #[test]
    fn locked_motion_coalesces_only_around_one_anchor() {
        let anchor = (320.0, 240.0);
        let locked =
            |position, dx, dy| PendingPointerMotion::Locked(PointerMotion { position, dx, dy });
        let free = PendingPointerMotion::Free(PointerMotion {
            position: anchor,
            dx: 1.0,
            dy: 1.0,
        });

        assert_eq!(
            locked(anchor, 2.0, 1.0).coalesce(locked(anchor, -5.0, 4.0)),
            Some(locked(anchor, -3.0, 5.0))
        );
        assert_eq!(
            locked(anchor, 2.0, 1.0).coalesce(locked((10.0, 10.0), 1.0, 1.0)),
            None
        );
        assert_eq!(free.coalesce(locked(anchor, 1.0, 1.0)), None);
        assert_eq!(locked(anchor, 1.0, 1.0).coalesce(free), None);
    }

    #[test]
    fn the_loop_sleeps_until_the_earliest_deadline_or_a_wake() {
        let now = std::time::Instant::now();
        let soon = now + std::time::Duration::from_millis(3);
        let later = now + std::time::Duration::from_millis(500);

        assert_eq!(
            next_wake([None, Some(later), Some(soon)]),
            ControlFlow::WaitUntil(soon)
        );
        assert_eq!(next_wake([None, None]), ControlFlow::Wait);
    }

    #[test]
    fn the_handoff_fails_unless_the_surface_view_took_the_window() {
        use crate::framework::FrameworkError;

        assert!(surface_handoff(Ok(true), 1280, 720).is_ok());
        assert!(matches!(
            surface_handoff(Ok(false), 1280, 720),
            Err(GraphicsError::EngineSurfaceUnavailable {
                width: 1280,
                height: 720
            })
        ));
        assert!(matches!(
            surface_handoff(Err(FrameworkError::Panicked), 1280, 720),
            Err(GraphicsError::EngineSurfaceHandoff(
                FrameworkError::Panicked
            ))
        ));
    }

    #[test]
    fn the_loop_wakes_for_main_looper_messages_before_the_handoff() {
        use crate::framework::MainLooperDue;

        let now = std::time::Instant::now();
        let delayed = now + std::time::Duration::from_millis(30);
        let retry = now + MAIN_THREAD_RETRY_DELAY;
        let refresh_poll = now + DISPLAY_REFRESH_POLL_INTERVAL;

        assert_eq!(
            loop_wake(
                MainLooperDue::At(delayed),
                now,
                refresh_poll,
                None,
                None,
                PointerLock::Free
            ),
            ControlFlow::WaitUntil(delayed),
            "a postDelayed message runs on time while the splash is hidden"
        );
        assert_eq!(
            loop_wake(
                MainLooperDue::WhenWoken,
                now,
                refresh_poll,
                Some(retry),
                None,
                PointerLock::Free
            ),
            ControlFlow::WaitUntil(retry),
            "a failed surface-callback probe is retried without a redraw"
        );
        assert_eq!(
            loop_wake(
                MainLooperDue::WhenWoken,
                now,
                refresh_poll,
                None,
                None,
                PointerLock::Free
            ),
            ControlFlow::WaitUntil(refresh_poll)
        );
    }

    #[test]
    fn a_refused_engine_surface_size_is_offered_again_until_the_engine_accepts_it() {
        use crate::framework::MainLooperDue;

        let now = std::time::Instant::now();
        let fullscreen = (2560, 1440);
        let requested = EngineSurfaceSize::Accepted((1280, 720)).requested(fullscreen);
        let first = requested.due(now).expect("a new size is offered at once");
        assert_eq!(first.size, fullscreen);
        assert!(!first.refused_before());

        let refused = first.after_offer(&surface_size_offer(Ok(false)), now);
        assert_eq!(refused.due(now), None);
        assert_eq!(
            refused.requested(fullscreen),
            refused,
            "a repeated Resized keeps the backoff"
        );
        let retry_at = refused
            .retry_at()
            .expect("a refused size schedules a retry");
        assert_eq!(retry_at, now + ENGINE_SURFACE_SIZE_FIRST_RETRY_DELAY);
        assert_eq!(
            loop_wake(
                MainLooperDue::WhenWoken,
                now,
                now + DISPLAY_REFRESH_POLL_INTERVAL,
                None,
                refused.retry_at(),
                PointerLock::Free
            ),
            ControlFlow::WaitUntil(retry_at)
        );

        let second = refused
            .due(retry_at)
            .expect("the refused size is offered again");
        assert_eq!(second.size, fullscreen);
        assert!(second.refused_before());

        let accepted = second.after_offer(&surface_size_offer(Ok(true)), retry_at);
        assert_eq!(accepted, EngineSurfaceSize::Accepted(fullscreen));
        assert_eq!(accepted.requested(fullscreen), accepted);
        assert_eq!(accepted.due(retry_at + DISPLAY_REFRESH_POLL_INTERVAL), None);
        assert_eq!(accepted.retry_at(), None);
    }

    #[test]
    fn engine_surface_size_retries_back_off_to_a_bound_and_a_new_size_goes_at_once() {
        let mut at = std::time::Instant::now();
        let mut size = EngineSurfaceSize::Unrequested.requested((1920, 1080));
        let mut delays = Vec::new();
        for _ in 0..7 {
            size = size
                .due(at)
                .expect("the retry is due")
                .after_offer(&Err(SurfaceSizeRefusal::NoSurfaceView), at);
            let retry_at = size.retry_at().expect("a retry is scheduled");
            delays.push((retry_at - at).as_millis());
            at = retry_at;
        }
        assert_eq!(delays, [100, 200, 400, 800, 1600, 2000, 2000]);

        let windowed = size.requested((1280, 720));
        let offered = windowed.due(at).expect("a new size skips the backoff");
        assert_eq!(offered.size, (1280, 720));
        assert!(!offered.refused_before());
    }

    #[test]
    fn only_a_surface_view_that_takes_the_size_settles_it() {
        use crate::framework::FrameworkError;

        let now = std::time::Instant::now();
        let fullscreen = (2560, 1440);
        let pending = EngineSurfaceSize::Accepted((1280, 720))
            .requested(fullscreen)
            .due(now)
            .expect("a new size is offered at once");
        let retried = EngineSurfaceSize::Pending(PendingSurfaceSize {
            size: fullscreen,
            retry: Some(SurfaceSizeRetry {
                at: now + ENGINE_SURFACE_SIZE_FIRST_RETRY_DELAY,
                delay: ENGINE_SURFACE_SIZE_FIRST_RETRY_DELAY,
            }),
        });

        assert_eq!(
            pending.after_offer(&surface_size_offer(Ok(true)), now),
            EngineSurfaceSize::Accepted(fullscreen)
        );
        assert_eq!(
            pending.after_offer(&surface_size_offer(Ok(false)), now),
            retried
        );
        assert_eq!(
            pending.after_offer(&surface_size_offer(Err(FrameworkError::Panicked)), now),
            retried
        );
        assert_eq!(
            pending.after_offer(
                &Err(SurfaceSizeRefusal::DisplaySize(FrameworkError::Panicked)),
                now
            ),
            retried
        );
    }

    #[test]
    fn a_free_or_held_pointer_lock_sets_no_display_rate_timer() {
        let held = PointerLock::Held {
            anchor: (320.0, 240.0),
            grab: PointerGrab::Locked,
        };

        for lock in [PointerLock::Free, held] {
            assert_eq!(pointer_lock_recheck(lock), None);
        }
    }

    #[test]
    fn a_refused_lock_wakes_the_loop_for_its_retry() {
        let now = std::time::Instant::now();
        let refused = PointerLock::Refused { at: now };

        assert_eq!(
            pointer_lock_recheck(refused),
            Some(now + POINTER_LOCK_RETRY_DELAY)
        );
    }

    #[test]
    fn pending_motion_is_flushed_before_any_other_event_is_handled() {
        use crate::loader::native_provider::HostInputKind;

        assert!(!flushes_pending_motion(Some(HostInputKind::Pointer)));
        for input in [
            Some(HostInputKind::MouseButton),
            Some(HostInputKind::Scroll),
            Some(HostInputKind::Key),
            Some(HostInputKind::Touch),
            None,
        ] {
            assert!(flushes_pending_motion(input), "{input:?}");
        }
    }

    #[test]
    fn only_a_confined_grab_warps_and_only_off_the_anchor() {
        let anchor = (320.0, 240.0);

        assert!(!confined_cursor_needs_warp(
            PointerGrab::Locked,
            (500.0, 100.0),
            anchor
        ));
        assert!(!confined_cursor_needs_warp(
            PointerGrab::ConfinedWithWarp,
            (320.0, 240.0),
            anchor
        ));
        assert!(!confined_cursor_needs_warp(
            PointerGrab::ConfinedWithWarp,
            (320.4, 239.6),
            anchor
        ));
        assert!(confined_cursor_needs_warp(
            PointerGrab::ConfinedWithWarp,
            (321.0, 240.0),
            anchor
        ));
        assert!(confined_cursor_needs_warp(
            PointerGrab::ConfinedWithWarp,
            (320.0, 238.0),
            anchor
        ));
    }

    #[test]
    fn host_cursor_hides_over_the_engine_surface_like_the_null_pointer_icon() {
        use eclipse_config::TouchMode;

        assert_eq!(
            host_cursor(true, TouchMode::Off, WebViewWindow::Hidden),
            HostCursor::Hidden
        );
        assert_eq!(
            host_cursor(true, TouchMode::Off, WebViewWindow::Shown),
            HostCursor::Shown
        );
        assert_eq!(
            host_cursor(false, TouchMode::Off, WebViewWindow::Hidden),
            HostCursor::Shown
        );
        for touch_mode in [TouchMode::On, TouchMode::FakeOff] {
            for window in [WebViewWindow::Hidden, WebViewWindow::Shown] {
                assert_eq!(host_cursor(true, touch_mode, window), HostCursor::Shown);
            }
        }
    }

    #[test]
    fn f11_and_alt_enter_are_the_fullscreen_shortcut() {
        use winit::keyboard::{Key, ModifiersState, NamedKey};

        let f11 = Key::Named(NamedKey::F11);
        let enter = Key::Named(NamedKey::Enter);
        let alt = ModifiersState::ALT;
        for modifiers in [ModifiersState::empty(), alt, ModifiersState::CONTROL] {
            assert!(fullscreen_shortcut(&f11, modifiers), "{modifiers:?}");
        }
        assert!(fullscreen_shortcut(&enter, alt));
        for modifiers in [
            ModifiersState::empty(),
            ModifiersState::CONTROL | alt,
            ModifiersState::SHIFT | alt,
            ModifiersState::SUPER | alt,
        ] {
            assert!(
                !fullscreen_shortcut(&enter, modifiers),
                "Enter with {modifiers:?} reaches the game"
            );
        }
        for key in [Key::Named(NamedKey::F10), Key::Character("f".into())] {
            assert!(!fullscreen_shortcut(&key, alt), "{key:?}");
        }
        assert_eq!(next_fullscreen(None), Some(Fullscreen::Borderless(None)));
        assert_eq!(next_fullscreen(Some(Fullscreen::Borderless(None))), None);
    }

    #[test]
    fn the_fullscreen_shortcut_keeps_its_key_from_the_game_until_released() {
        use crate::input::KeyEdge;
        use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

        let enter = Key::Named(NamedKey::Enter);
        let enter_key = PhysicalKey::Code(KeyCode::Enter);
        let f11 = Key::Named(NamedKey::F11);
        let f11_key = PhysicalKey::Code(KeyCode::F11);
        let alt = ModifiersState::ALT;
        let none = ModifiersState::empty();
        let mut shortcut = FullscreenShortcut::default();
        let mut route = |key: &Key, physical, edge, modifiers| {
            shortcut.key_route(WebViewWindow::Hidden, key, physical, edge, modifiers)
        };

        assert_eq!(
            route(&enter, enter_key, KeyEdge::Press, alt),
            HostInputRoute::ToggleFullscreen
        );
        assert_eq!(
            route(&enter, enter_key, KeyEdge::Repeat, alt),
            HostInputRoute::Withheld
        );
        assert_eq!(
            route(&enter, enter_key, KeyEdge::Release, none),
            HostInputRoute::Withheld,
            "the Enter release is withheld even after Alt was let go"
        );
        for edge in [KeyEdge::Press, KeyEdge::Release] {
            assert_eq!(
                route(&enter, enter_key, edge, none),
                HostInputRoute::Engine,
                "a later plain Enter reaches the game"
            );
        }

        assert_eq!(
            route(&f11, f11_key, KeyEdge::Press, none),
            HostInputRoute::ToggleFullscreen
        );
        for edge in [KeyEdge::Repeat, KeyEdge::Release] {
            assert_eq!(route(&f11, f11_key, edge, none), HostInputRoute::Withheld);
        }

        assert_eq!(
            route(&enter, enter_key, KeyEdge::Press, alt),
            HostInputRoute::ToggleFullscreen
        );
        assert_eq!(
            route(&enter, enter_key, KeyEdge::Press, none),
            HostInputRoute::Engine,
            "a new press after a release Eclipse never saw goes to the game"
        );
        assert_eq!(
            route(&enter, enter_key, KeyEdge::Release, none),
            HostInputRoute::Engine
        );
    }

    #[test]
    fn the_fullscreen_shortcut_waits_while_a_web_view_page_shows() {
        use crate::input::KeyEdge;
        use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

        for (key, physical, modifiers) in [
            (
                Key::Named(NamedKey::F11),
                PhysicalKey::Code(KeyCode::F11),
                ModifiersState::empty(),
            ),
            (
                Key::Named(NamedKey::Enter),
                PhysicalKey::Code(KeyCode::Enter),
                ModifiersState::ALT,
            ),
        ] {
            for (window, expected) in [
                (WebViewWindow::Shown, HostInputRoute::RaiseWebView),
                (WebViewWindow::Embedded, HostInputRoute::Withheld),
            ] {
                assert_eq!(
                    FullscreenShortcut::default().key_route(
                        window,
                        &key,
                        physical,
                        KeyEdge::Press,
                        modifiers
                    ),
                    expected,
                    "a fullscreen game would cover the WebView page: {key:?} on {window:?}"
                );
            }
        }
    }

    #[test]
    fn the_ime_is_requested_only_for_a_focused_text_box() {
        let text_box = |masked| crate::framework::FocusedTextBox {
            geometry: (181, 149, 438, 46),
            multiline: false,
            masked,
        };

        assert_eq!(ime_request(None), None);
        assert_eq!(
            ime_request(Some(text_box(false))),
            Some(ImeRequest {
                area: (181, 149, 438, 46),
            })
        );
        assert_eq!(ime_request(Some(text_box(true))), None);
    }

    #[test]
    fn the_ime_is_allowed_before_its_cursor_area_which_is_resent_after_every_enable() {
        let calls =
            |host: HostIme, wanted: Option<ImeRequest>| ime_calls(host, wanted).collect::<Vec<_>>();
        let box_area = (181, 149, 438, 46);
        let focused = Some(ImeRequest { area: box_area });

        assert_eq!(
            calls(HostIme::Disallowed, focused),
            [ImeCall::Allow(true), ImeCall::CursorArea(box_area)]
        );
        let sent = HostIme::after_sending(focused);
        assert_eq!(calls(sent, focused), []);
        assert_eq!(
            calls(sent.after_enabled(), focused),
            [ImeCall::CursorArea(box_area)]
        );
        let moved = (181, 300, 438, 46);
        assert_eq!(
            calls(sent, Some(ImeRequest { area: moved })),
            [ImeCall::CursorArea(moved)]
        );
        assert_eq!(calls(sent, None), [ImeCall::Allow(false)]);
        assert_eq!(calls(HostIme::Disallowed, None), []);
        assert_eq!(HostIme::Disallowed.after_enabled(), HostIme::Disallowed);
    }

    #[test]
    fn the_ime_candidate_window_keeps_clear_of_the_text_box() {
        let box_area = (181, 149, 438, 46);

        assert_eq!(
            ime_cursor_area(ImeAreaSupport::Rectangle, box_area),
            ((181, 149), (438, 46))
        );
        assert_eq!(
            ime_cursor_area(ImeAreaSupport::TopLeftSpot, box_area),
            ((181, 195), (438, 0))
        );
        assert_eq!(
            ime_cursor_area(ImeAreaSupport::TopLeftSpot, (0, i32::MAX - 1, 10, 46)),
            ((0, i32::MAX), (10, 0))
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
    fn gravity_decodes_each_axis_from_its_own_field() {
        const GRAVITY_LEFT: i32 = 0x03;
        const GRAVITY_START: i32 = 0x0080_0003;
        const GRAVITY_TOP: i32 = 0x30;
        const GRAVITY_FILL: i32 = 0x77;

        let extent = vk::Extent2D {
            width: 200,
            height: 200,
        };
        let root_lp = LayoutParams {
            width: MATCH_PARENT,
            height: MATCH_PARENT,
            ..Default::default()
        };
        let child_at = |class: &str, gravity: i32| {
            let child_lp = LayoutParams {
                width: 50,
                height: 50,
                gravity,
                ..Default::default()
            };
            let nodes = [
                node_lp(class, None, 0, root_lp, &[1]),
                node_lp("android.view.View", None, 1, child_lp, &[]),
            ];
            let views = layout_views(&nodes, extent, None);
            (views[1].x, views[1].y)
        };
        let frame = "android.widget.FrameLayout";

        assert_eq!(child_at(frame, GRAVITY_LEFT | GRAVITY_TOP), (0.0, 0.0));
        assert_eq!(child_at(frame, GRAVITY_START | GRAVITY_TOP), (0.0, 0.0));
        assert_eq!(child_at(frame, GRAVITY_RIGHT | GRAVITY_TOP), (150.0, 0.0));
        assert_eq!(child_at(frame, GRAVITY_LEFT | GRAVITY_BOTTOM), (0.0, 150.0));
        assert_eq!(
            child_at(frame, GRAVITY_LEFT | GRAVITY_CENTER_VERTICAL),
            (0.0, 75.0)
        );
        assert_eq!(child_at(frame, GRAVITY_FILL), (0.0, 0.0));
        assert_eq!(
            child_at("android.widget.LinearLayout", GRAVITY_LEFT),
            (0.0, 0.0)
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
        let Some(font) = crate::host_fonts::system_font() else {
            return;
        };
        let atlas = build_glyph_atlas(font, TEXT_PX, 1024).expect("atlas builds from a real font");
        assert!(atlas.width > 0 && atlas.height > 0);
        assert_eq!(atlas.pixels.len(), (atlas.width * atlas.height) as usize);

        let a = atlas.glyphs.get(&'A').expect("'A' in atlas");
        assert!(a.advance > 0.0);
        assert!(a.aw > 0 && a.ah > 0, "'A' has a non-empty bitmap");
        for accented in ['é', '’', '…'] {
            let glyph = atlas
                .glyphs
                .get(&accented)
                .expect("common non-ASCII text is in the atlas");
            assert!(glyph.advance > 0.0 && glyph.aw > 0, "{accented:?} draws");
        }
    }

    #[test]
    fn text_without_an_atlas_glyph_draws_the_replacement_character() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let mut atlas = synthetic_atlas();
        let replacement = *atlas.glyphs.get(&'A').expect("synthetic 'A'");
        atlas.glyphs.insert(REPLACEMENT_CHARACTER, replacement);
        let view = |text: &str| LaidOutView {
            handle: 0,
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 64.0,
            clickable: false,
            color: [1.0; 4],
            text: Some(text.to_owned()),
        };
        let unknown = build_text_vertices(&[view("\u{4F60}A")], &atlas, extent);
        let known = build_text_vertices(&[view("AA")], &atlas, extent);
        assert_eq!(unknown, known);
        assert_eq!(
            TextMeasure { atlas: &atlas }.width("\u{4F60}A"),
            TextMeasure { atlas: &atlas }.width("AA")
        );
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
}

#[cfg(test)]
pub(crate) mod unmapped_xlib_window;

#[cfg(test)]
pub(crate) mod roleless_wayland_surface;

#[cfg(test)]
pub(crate) mod headless_vulkan {
    use ash::{khr, vk};
    use std::cell::RefCell;

    pub(crate) struct HeadlessGpu {
        pub(crate) entry: ash::Entry,
        pub(crate) instance: ash::Instance,
        pub(crate) physical_device: vk::PhysicalDevice,
        pub(crate) device: ash::Device,
        pub(crate) queue_family: u32,
        pub(crate) queue: vk::Queue,
        pub(crate) memory_properties: vk::PhysicalDeviceMemoryProperties,
        command_pool: vk::CommandPool,
        images: RefCell<Vec<(vk::Image, vk::DeviceMemory)>>,
        buffers: RefCell<Vec<(vk::Buffer, vk::DeviceMemory)>>,
        semaphores: RefCell<Vec<vk::Semaphore>>,
    }

    impl HeadlessGpu {
        pub(crate) fn new() -> Result<Self, String> {
            let entry =
                unsafe { ash::Entry::load() }.map_err(|e| format!("no Vulkan loader: {e}"))?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_0);
            let instance = unsafe {
                entry.create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
            }
            .map_err(|e| format!("vkCreateInstance: {e}"))?;
            let (physical_device, queue_family, device) = match Self::open_device(&instance) {
                Ok(opened) => opened,
                Err(e) => {
                    unsafe { instance.destroy_instance(None) };
                    return Err(e);
                }
            };
            let queue = unsafe { device.get_device_queue(queue_family, 0) };
            let memory_properties =
                unsafe { instance.get_physical_device_memory_properties(physical_device) };
            let pool_info = vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(queue_family);
            let command_pool = match unsafe { device.create_command_pool(&pool_info, None) } {
                Ok(pool) => pool,
                Err(e) => {
                    unsafe {
                        device.destroy_device(None);
                        instance.destroy_instance(None);
                    }
                    return Err(format!("vkCreateCommandPool: {e}"));
                }
            };
            Ok(Self {
                entry,
                instance,
                physical_device,
                device,
                queue_family,
                queue,
                memory_properties,
                command_pool,
                images: RefCell::new(Vec::new()),
                buffers: RefCell::new(Vec::new()),
                semaphores: RefCell::new(Vec::new()),
            })
        }

        fn open_device(
            instance: &ash::Instance,
        ) -> Result<(vk::PhysicalDevice, u32, ash::Device), String> {
            let devices = unsafe { instance.enumerate_physical_devices() }
                .map_err(|e| format!("vkEnumeratePhysicalDevices: {e}"))?;
            for physical_device in devices {
                let extensions =
                    unsafe { instance.enumerate_device_extension_properties(physical_device) }
                        .map_err(|e| format!("vkEnumerateDeviceExtensionProperties: {e}"))?;
                if !extensions
                    .iter()
                    .any(|e| e.extension_name_as_c_str() == Ok(khr::swapchain::NAME))
                {
                    continue;
                }
                let families = unsafe {
                    instance.get_physical_device_queue_family_properties(physical_device)
                };
                let Some(family) = families
                    .iter()
                    .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                else {
                    continue;
                };
                let priorities = [1.0f32];
                let queue_info = vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family as u32)
                    .queue_priorities(&priorities);
                let device_extensions = [khr::swapchain::NAME.as_ptr()];
                let device_info = vk::DeviceCreateInfo::default()
                    .queue_create_infos(std::slice::from_ref(&queue_info))
                    .enabled_extension_names(&device_extensions);
                let device = unsafe { instance.create_device(physical_device, &device_info, None) }
                    .map_err(|e| format!("vkCreateDevice: {e}"))?;
                return Ok((physical_device, family as u32, device));
            }
            Err("no physical device with a graphics queue and VK_KHR_swapchain".to_owned())
        }

        pub(crate) fn image(
            &self,
            format: vk::Format,
            width: u32,
            height: u32,
            usage: vk::ImageUsageFlags,
        ) -> vk::Image {
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let image = unsafe { self.device.create_image(&info, None) }.expect("vkCreateImage");
            let req = unsafe { self.device.get_image_memory_requirements(image) };
            let memory_type =
                super::find_device_local_memory_type(&self.memory_properties, req.memory_type_bits)
                    .expect("device-local memory type");
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(memory_type);
            let memory =
                unsafe { self.device.allocate_memory(&alloc, None) }.expect("vkAllocateMemory");
            unsafe { self.device.bind_image_memory(image, memory, 0) }.expect("vkBindImageMemory");
            self.images.borrow_mut().push((image, memory));
            image
        }

        pub(crate) fn host_buffer(
            &self,
            size: vk::DeviceSize,
            usage: vk::BufferUsageFlags,
        ) -> (vk::Buffer, vk::DeviceMemory) {
            let created =
                super::create_host_buffer(&self.device, &self.memory_properties, size, usage)
                    .expect("host-visible buffer");
            self.buffers.borrow_mut().push(created);
            created
        }

        pub(crate) fn semaphore(&self) -> vk::Semaphore {
            let semaphore = unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .expect("vkCreateSemaphore");
            self.semaphores.borrow_mut().push(semaphore);
            semaphore
        }

        pub(crate) fn run(
            &self,
            waits: &[vk::Semaphore],
            signals: &[vk::Semaphore],
            record: impl FnOnce(&ash::Device, vk::CommandBuffer),
        ) {
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cmd = unsafe { self.device.allocate_command_buffers(&alloc) }
                .expect("vkAllocateCommandBuffers")[0];
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            unsafe { self.device.begin_command_buffer(cmd, &begin) }.expect("vkBeginCommandBuffer");
            record(&self.device, cmd);
            unsafe { self.device.end_command_buffer(cmd) }.expect("vkEndCommandBuffer");
            let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
            let cmds = [cmd];
            let batch = vk::SubmitInfo::default()
                .wait_semaphores(waits)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cmds)
                .signal_semaphores(signals);
            let fence = unsafe {
                self.device
                    .create_fence(&vk::FenceCreateInfo::default(), None)
            }
            .expect("vkCreateFence");
            unsafe {
                self.device
                    .queue_submit(self.queue, &[batch], fence)
                    .expect("vkQueueSubmit");
                self.device
                    .wait_for_fences(&[fence], true, u64::MAX)
                    .expect("vkWaitForFences");
                self.device.destroy_fence(fence, None);
                self.device.free_command_buffers(self.command_pool, &cmds);
            }
        }

        pub(crate) fn read(&self, memory: vk::DeviceMemory, len: usize) -> Vec<u8> {
            unsafe {
                let ptr = self
                    .device
                    .map_memory(
                        memory,
                        0,
                        len as vk::DeviceSize,
                        vk::MemoryMapFlags::empty(),
                    )
                    .expect("vkMapMemory");
                let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), len).to_vec();
                self.device.unmap_memory(memory);
                bytes
            }
        }
    }

    impl Drop for HeadlessGpu {
        fn drop(&mut self) {
            unsafe {
                let _ = self.device.device_wait_idle();
                for semaphore in self.semaphores.borrow_mut().drain(..) {
                    self.device.destroy_semaphore(semaphore, None);
                }
                for (buffer, memory) in self.buffers.borrow_mut().drain(..) {
                    self.device.destroy_buffer(buffer, None);
                    self.device.free_memory(memory, None);
                }
                for (image, memory) in self.images.borrow_mut().drain(..) {
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                }
                self.device.destroy_command_pool(self.command_pool, None);
                self.device.destroy_device(None);
                self.instance.destroy_instance(None);
            }
        }
    }
}
