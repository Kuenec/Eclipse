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
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::application::ApplicationHandler;
use winit::error::{EventLoopError, OsError};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

/// 2026-06-05: the clear color presented each frame — a Roblox-ish blue (linear-ish sRGB),
/// distinct from black so a working present is visually unambiguous on the dev host.
const CLEAR_COLOR: [f32; 4] = [0.149, 0.408, 0.722, 1.0];

/// The host game window + event loop (winit application state).
struct GameWindow {
    title: String,
    /// `None` until the event loop is `resumed` and the window is created.
    window: Option<Window>,
    /// The Vulkan surface + swapchain bound to [`Self::window`]. `None` if Vulkan init failed
    /// (no ICD / unsupported display) — the window then stays open and blank (no crash).
    renderer: Option<VulkanRenderer>,
    /// Set if window creation failed, so [`run_windowed`] can surface a typed error.
    create_error: Option<OsError>,
}

impl ApplicationHandler for GameWindow {
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

        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("window close requested; exiting event loop");
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.mark_resized(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(window), Some(renderer)) =
                    (self.window.as_ref(), self.renderer.as_mut())
                {
                    if let Err(e) = renderer.draw_frame(window) {
                        tracing::error!(error = %e, "Vulkan frame draw failed");
                    }
                    // Drive a continuous clear-and-present loop so the surface keeps presenting.
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

/// Open the host game window and run the winit event loop until the window is closed.
///
/// MUST be called on the process main thread (winit requires the event loop there on Linux);
/// `eclipse run` calls this from `main` after the ART VM is booted. Returns when the window is
/// closed, or a typed [`GraphicsError`] if the event loop or window cannot be created. A Vulkan
/// init failure is NOT returned here — it is logged and the window stays open blank (no crash).
pub fn run_windowed(title: &str) -> Result<(), GraphicsError> {
    let event_loop = EventLoop::new().map_err(GraphicsError::EventLoop)?;
    let mut app = GameWindow {
        title: title.to_owned(),
        window: None,
        renderer: None,
        create_error: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(GraphicsError::EventLoop)?;
    // run_app returns Ok even if `resumed` failed to create the window; surface that as an error.
    if let Some(e) = app.create_error {
        return Err(GraphicsError::CreateWindow(e));
    }
    Ok(())
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
        ),
        GraphicsError,
    > {
        // SAFETY: queue family index was validated by `pick_device`; queue 0 always exists when
        // the family was requested with one priority.
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
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

        self.record_clear(image_index as usize)?;

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

    /// Record the clear-only render pass into the (reset) command buffer for `image_index`.
    fn record_clear(&self, image_index: usize) -> Result<(), GraphicsError> {
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
        // SAFETY: framebuffer index is in range (acquired image), render pass + cmd are valid.
        unsafe {
            self.device
                .cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);
            // Clear-only: no draw calls. The CLEAR load-op paints CLEAR_COLOR; STORE keeps it.
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
            self.swapchain.destroy(&self.device, &self.swapchain_loader);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
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

    #[test]
    fn surface_format_none_when_driver_advertises_none() {
        assert!(choose_surface_format(&[]).is_none());
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
}
