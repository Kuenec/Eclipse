use crate::framework::{ActiveTextOverlay, TextSelection};
use crate::graphics::FrameFence;
use crate::text_layout::{FieldStyle, PixelRect, Scroll, Selection, Viewport};
use ash::vk;
use ash::vk::Handle;
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

fn encode_png_rgba(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    fn crc32(buf: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in buf {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn adler32(buf: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &x in buf {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
    fn chunk(out: &mut Vec<u8>, typ: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(typ);
        out.extend_from_slice(data);
        let mut crc_in = typ.to_vec();
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
    }
    let mut out = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);

    let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
        raw.extend_from_slice(row);
    }

    let mut zlib = vec![0x78u8, 0x01];
    let mut i = 0;
    while i < raw.len() {
        let block = (raw.len() - i).min(65535);
        let bfinal = u8::from(i + block >= raw.len());
        zlib.push(bfinal);
        zlib.extend_from_slice(&(block as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw[i..i + block]);
        i += block;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

const CARET_BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);
const SELECTION_HIGHLIGHT_ALPHA: u8 = 0x66;
const TEXT_TEST_FONT: i32 = 46;

fn caret_visible(since_change: Duration) -> bool {
    (since_change.as_millis() / CARET_BLINK_HALF_PERIOD.as_millis()).is_multiple_of(2)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Rgba,
    Bgra,
}

impl ByteOrder {
    fn of_swapchain(format_raw: i32) -> Option<Self> {
        match vk::Format::from_raw(format_raw) {
            vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB => Some(Self::Bgra),
            vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB => Some(Self::Rgba),
            _ => None,
        }
    }

    fn arrange(self, [r, g, b, a]: [u8; 4]) -> [u8; 4] {
        match self {
            Self::Rgba => [r, g, b, a],
            Self::Bgra => [b, g, r, a],
        }
    }
}

fn text_rgba(argb: i32) -> [u8; 4] {
    let [a, r, g, b] = (argb as u32).to_be_bytes();
    [r, g, b, a]
}

#[derive(Debug, Clone, PartialEq)]
struct TextRequest {
    widget: i64,
    text: String,
    selection: Selection,
    font: i32,
    style: FieldStyle,
    viewport: Viewport,
    text_color: i32,
    order: ByteOrder,
}

impl TextRequest {
    fn of(overlay: &ActiveTextOverlay, rect: vk::Rect2D, order: ByteOrder) -> Self {
        let (field_x, field_y, width, height) = overlay.geometry;
        Self {
            widget: overlay.widget,
            text: crate::framework::displayed_text_box_text(&overlay.text, overlay.input_type)
                .into_owned(),
            selection: match overlay.selection {
                TextSelection::All => Selection::All,
                TextSelection::Cursor(utf16) => Selection::Caret(
                    crate::text_layout::char_index_at_utf16(&overlay.text, utf16),
                ),
            },
            font: overlay.font,
            style: FieldStyle::of_text_box(
                overlay.font_size,
                (width, height),
                overlay.multiline,
                overlay.text_wrapped,
                overlay.x_alignment,
                overlay.y_alignment,
            ),
            viewport: Viewport {
                x: rect.offset.x.saturating_sub(field_x),
                y: rect.offset.y.saturating_sub(field_y),
                width: rect.extent.width,
                height: rect.extent.height,
            },
            text_color: overlay.text_color,
            order,
        }
    }
}

struct TextLayer {
    request: TextRequest,
    generation: u64,
    pixels: Vec<[u8; 4]>,
    caret: Option<PixelRect>,
    caret_color: [u8; 4],
}

fn build_text_layer(
    request: &TextRequest,
    generation: u64,
    previous: Scroll,
) -> Option<(TextLayer, Scroll)> {
    let chain = crate::framework::roblox_fonts::face_chain(request.font)?;
    let layout = crate::text_layout::lay_out(
        &request.text,
        request.selection,
        &request.style,
        chain,
        previous,
    );
    let color = text_rgba(request.text_color);
    let highlight = [
        color[0],
        color[1],
        color[2],
        crate::text_layout::scale_byte(color[3], SELECTION_HIGHLIGHT_ALPHA),
    ];
    let pixels = layout
        .paint(request.viewport, color, highlight)
        .into_iter()
        .map(|pixel| request.order.arrange(pixel))
        .collect();
    let layer = TextLayer {
        request: request.clone(),
        generation,
        pixels,
        caret: layout.caret_pixels(request.viewport),
        caret_color: request.order.arrange(color),
    };
    Some((layer, layout.scroll()))
}

struct TextWorkerState {
    wanted: Option<TextRequest>,
    generation: u64,
    changed_at: Instant,
    ready: Option<Arc<TextLayer>>,
}

struct TextWorker {
    state: Mutex<TextWorkerState>,
    wake: Condvar,
}

fn text_worker() -> Option<&'static TextWorker> {
    static WORKER: OnceLock<Option<&'static TextWorker>> = OnceLock::new();
    *WORKER.get_or_init(|| {
        let worker: &'static TextWorker = Box::leak(Box::new(TextWorker {
            state: Mutex::new(TextWorkerState {
                wanted: None,
                generation: 0,
                changed_at: Instant::now(),
                ready: None,
            }),
            wake: Condvar::new(),
        }));
        match std::thread::Builder::new()
            .name("eclipse-text".to_owned())
            .spawn(move || run_text_worker(worker))
        {
            Ok(_) => Some(worker),
            Err(error) => {
                tracing::error!(%error, "vk-overlay: could not start the text layout thread");
                None
            }
        }
    })
}

fn run_text_worker(worker: &TextWorker) {
    let mut built = 0;
    let mut scroll: Option<(i64, Scroll)> = None;
    loop {
        let (request, generation) = {
            let mut state = worker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state.generation == built {
                state = worker
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            (state.wanted.clone(), state.generation)
        };
        built = generation;
        let Some(request) = request else {
            continue;
        };
        let previous = scroll
            .filter(|(widget, _)| *widget == request.widget)
            .map_or_else(Scroll::default, |(_, scroll)| scroll);
        let layer = build_text_layer(&request, generation, previous).map(|(layer, next)| {
            scroll = Some((request.widget, next));
            crate::framework::record_text_scroll(request.widget, next);
            Arc::new(layer)
        });
        worker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready = layer;
    }
}

fn current_text_layer(request: &TextRequest, now: Instant) -> Option<(Arc<TextLayer>, bool)> {
    let worker = text_worker()?;
    let mut state = worker
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.wanted.as_ref() != Some(request) {
        state.wanted = Some(request.clone());
        state.generation += 1;
        state.changed_at = now;
        worker.wake.notify_one();
    }
    let layer = state.ready.clone().filter(|layer| {
        layer.request.widget == request.widget
            && layer.request.viewport == request.viewport
            && layer.request.order == request.order
    })?;
    Some((
        layer,
        caret_visible(now.saturating_duration_since(state.changed_at)),
    ))
}

fn overlay_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("ECLIPSE_NO_VK_OVERLAY").is_none())
}

static HOST_GDPA: AtomicU64 = AtomicU64::new(0);
static HOST_CREATE_DEVICE: AtomicU64 = AtomicU64::new(0);
static HOST_DESTROY_DEVICE: AtomicU64 = AtomicU64::new(0);
static HOST_QUEUE_PRESENT: AtomicU64 = AtomicU64::new(0);
static HOST_CREATE_SWAPCHAIN: AtomicU64 = AtomicU64::new(0);
static HOST_DESTROY_SWAPCHAIN: AtomicU64 = AtomicU64::new(0);
static HOST_GET_SWAPCHAIN_IMAGES: AtomicU64 = AtomicU64::new(0);
static HOST_ACQUIRE_NEXT_IMAGE: AtomicU64 = AtomicU64::new(0);
static HOST_ACQUIRE_NEXT_IMAGE2: AtomicU64 = AtomicU64::new(0);
static PRESENT_COUNT: AtomicU64 = AtomicU64::new(0);

static INSTANCE: AtomicU64 = AtomicU64::new(0);
static PHYSICAL_DEVICE: AtomicU64 = AtomicU64::new(0);
static QUEUE_FAMILY: AtomicU32 = AtomicU32::new(u32::MAX);

const OVERLAY_COPY_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::TRANSFER_SRC.as_raw() | vk::ImageUsageFlags::TRANSFER_DST.as_raw(),
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SwapchainCopies {
    #[default]
    Unsupported,
    Supported,
}

impl SwapchainCopies {
    fn of(usage: vk::ImageUsageFlags) -> Self {
        if usage.contains(OVERLAY_COPY_USAGE) {
            Self::Supported
        } else {
            Self::Unsupported
        }
    }
}

pub(crate) fn set_instance(instance: vk::Instance) {
    INSTANCE.store(instance.as_raw(), Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SwapchainFit {
    #[default]
    Unproven,
    Optimal,
    RebuildPending,
    SuboptimalAccepted,
}

#[derive(Clone, Copy, Debug)]
enum HostReport {
    FramePresented,
    Suboptimal,
    OutOfDate,
}

impl SwapchainFit {
    fn after(self, report: HostReport) -> Self {
        match (self, report) {
            (_, HostReport::OutOfDate) | (Self::Optimal, HostReport::Suboptimal) => {
                Self::RebuildPending
            }
            (Self::Unproven, HostReport::FramePresented) => Self::Optimal,
            (Self::Unproven, HostReport::Suboptimal) => Self::SuboptimalAccepted,
            (
                Self::Optimal | Self::RebuildPending | Self::SuboptimalAccepted,
                HostReport::FramePresented,
            )
            | (Self::RebuildPending | Self::SuboptimalAccepted, HostReport::Suboptimal) => self,
        }
    }
}

#[derive(Default)]
struct OverlayState {
    device: u64,

    swapchain: u64,

    surface: u64,

    fit: SwapchainFit,

    format: i32,

    width: u32,
    height: u32,

    images: Vec<u64>,

    copies: SwapchainCopies,
}

static STATE: Mutex<OverlayState> = Mutex::new(OverlayState {
    device: 0,
    swapchain: 0,
    surface: 0,
    fit: SwapchainFit::Unproven,
    format: 0,
    width: 0,
    height: 0,
    images: Vec::new(),
    copies: SwapchainCopies::Unsupported,
});

fn pfn_to_addr(p: vk::PFN_vkVoidFunction) -> u64 {
    p.map_or(0, |f| f as usize as u64)
}

fn cached(a: &AtomicU64) -> Option<usize> {
    match a.load(Ordering::Relaxed) {
        0 => None,
        v => Some(v as usize),
    }
}

pub(crate) unsafe fn intercept_instance_proc(
    instance: vk::Instance,
    name: &CStr,
    host_gipa: vk::PFN_vkGetInstanceProcAddr,
) -> Option<vk::PFN_vkVoidFunction> {
    if name == c"vkCreateDevice" {
        let host = unsafe { host_gipa(instance, name.as_ptr()) };
        HOST_CREATE_DEVICE.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(Some(unsafe {
            std::mem::transmute::<vk::PFN_vkCreateDevice, unsafe extern "system" fn()>(
                eclipse_vk_create_device,
            )
        }));
    }
    if name == c"vkGetDeviceProcAddr" {
        let host = unsafe { host_gipa(instance, name.as_ptr()) };
        HOST_GDPA.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(Some(unsafe {
            std::mem::transmute::<vk::PFN_vkGetDeviceProcAddr, unsafe extern "system" fn()>(
                eclipse_vk_get_device_proc_addr,
            )
        }));
    }
    None
}

unsafe extern "system" fn eclipse_vk_get_device_proc_addr(
    device: vk::Device,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    let host_gdpa_addr = cached(&HOST_GDPA)?;

    let host_gdpa: vk::PFN_vkGetDeviceProcAddr =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkGetDeviceProcAddr>(host_gdpa_addr) };

    let name = unsafe { CStr::from_ptr(p_name) };

    if name == c"vkDestroyDevice" {
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_DESTROY_DEVICE.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkDestroyDevice, unsafe extern "system" fn()>(
                eclipse_vk_destroy_device,
            )
        });
    }
    if name == c"vkQueuePresentKHR" {
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_QUEUE_PRESENT.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkQueuePresentKHR, unsafe extern "system" fn()>(
                eclipse_vk_queue_present_khr,
            )
        });
    }
    if name == c"vkCreateSwapchainKHR" {
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_CREATE_SWAPCHAIN.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkCreateSwapchainKHR, unsafe extern "system" fn()>(
                eclipse_vk_create_swapchain_khr,
            )
        });
    }
    if name == c"vkDestroySwapchainKHR" {
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_DESTROY_SWAPCHAIN.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkDestroySwapchainKHR, unsafe extern "system" fn()>(
                eclipse_vk_destroy_swapchain_khr,
            )
        });
    }
    if name == c"vkGetSwapchainImagesKHR" {
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_GET_SWAPCHAIN_IMAGES.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkGetSwapchainImagesKHR, unsafe extern "system" fn()>(
                eclipse_vk_get_swapchain_images_khr,
            )
        });
    }
    if name == c"vkAcquireNextImageKHR" {
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_ACQUIRE_NEXT_IMAGE.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkAcquireNextImageKHR, unsafe extern "system" fn()>(
                eclipse_vk_acquire_next_image_khr,
            )
        });
    }
    if name == c"vkAcquireNextImage2KHR" {
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_ACQUIRE_NEXT_IMAGE2.store(pfn_to_addr(host), Ordering::Relaxed);

        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkAcquireNextImage2KHR, unsafe extern "system" fn()>(
                eclipse_vk_acquire_next_image2_khr,
            )
        });
    }

    unsafe { host_gdpa(device, p_name) }
}

unsafe extern "system" fn eclipse_vk_acquire_next_image_khr(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    timeout: u64,
    semaphore: vk::Semaphore,
    fence: vk::Fence,
    p_image_index: *mut u32,
) -> vk::Result {
    let Some(addr) = cached(&HOST_ACQUIRE_NEXT_IMAGE) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let host: vk::PFN_vkAcquireNextImageKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkAcquireNextImageKHR>(addr) };
    unsafe {
        acquire_like_android(swapchain, p_image_index, || {
            host(device, swapchain, timeout, semaphore, fence, p_image_index)
        })
    }
}

unsafe fn acquire_like_android(
    swapchain: vk::SwapchainKHR,
    p_image_index: *mut u32,
    host_acquire: impl FnOnce() -> vk::Result,
) -> vk::Result {
    let engine_index = unsafe { p_image_index.as_ref() }.copied();
    let result = host_acquire();
    match result {
        vk::Result::SUCCESS => result,
        vk::Result::SUBOPTIMAL_KHR => {
            note_host_report(swapchain, HostReport::Suboptimal);
            vk::Result::SUCCESS
        }
        failed => {
            if let (Some(index), Some(slot)) = (engine_index, unsafe { p_image_index.as_mut() }) {
                *slot = index;
            }
            if failed == vk::Result::ERROR_OUT_OF_DATE_KHR {
                note_host_report(swapchain, HostReport::OutOfDate);
            }
            failed
        }
    }
}

fn note_host_report(swapchain: vk::SwapchainKHR, report: HostReport) {
    let mut st = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if st.swapchain == 0 || st.swapchain != swapchain.as_raw() {
        return;
    }
    let fit = st.fit.after(report);
    if fit == SwapchainFit::RebuildPending && st.fit != SwapchainFit::RebuildPending {
        tracing::info!(
            swapchain = format_args!("{:#x}", st.swapchain),
            ?report,
            "vk-overlay: the host reports the engine swapchain unfit; its next surface \
             capability query reports the surface lost so the engine rebuilds the swapchain"
        );
    }
    st.fit = fit;
}

pub(crate) fn take_pending_rebuild(surface: vk::SurfaceKHR) -> bool {
    let mut st = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pending = st.swapchain != 0
        && st.fit == SwapchainFit::RebuildPending
        && st.surface == surface.as_raw();
    if pending {
        st.fit = SwapchainFit::SuboptimalAccepted;
    }
    pending
}

unsafe extern "system" fn eclipse_vk_acquire_next_image2_khr(
    device: vk::Device,
    p_acquire_info: *const vk::AcquireNextImageInfoKHR<'_>,
    p_image_index: *mut u32,
) -> vk::Result {
    let Some(addr) = cached(&HOST_ACQUIRE_NEXT_IMAGE2) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let host: vk::PFN_vkAcquireNextImage2KHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkAcquireNextImage2KHR>(addr) };
    let swapchain =
        unsafe { p_acquire_info.as_ref() }.map_or(vk::SwapchainKHR::null(), |i| i.swapchain);
    unsafe {
        acquire_like_android(swapchain, p_image_index, || {
            host(device, p_acquire_info, p_image_index)
        })
    }
}

unsafe extern "system" fn eclipse_vk_destroy_device(
    device: vk::Device,
    p_allocator: *const vk::AllocationCallbacks<'_>,
) {
    let Some(addr) = cached(&HOST_DESTROY_DEVICE) else {
        tracing::error!("vk-overlay: missing host vkDestroyDevice");
        return;
    };
    let host: vk::PFN_vkDestroyDevice =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkDestroyDevice>(addr) };

    release_overlay_device_resources(device);
    unsafe { host(device, p_allocator) };
}

unsafe extern "system" fn eclipse_vk_create_device(
    physical_device: vk::PhysicalDevice,
    p_create_info: *const vk::DeviceCreateInfo<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_device: *mut vk::Device,
) -> vk::Result {
    let Some(addr) = cached(&HOST_CREATE_DEVICE) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    let host: vk::PFN_vkCreateDevice =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkCreateDevice>(addr) };

    let r = unsafe { host(physical_device, p_create_info, p_allocator, p_device) };
    if r == vk::Result::SUCCESS && !p_device.is_null() {
        let device = unsafe { *p_device };

        PHYSICAL_DEVICE.store(physical_device.as_raw(), Ordering::Relaxed);
        if !p_create_info.is_null() {
            let ci = unsafe { &*p_create_info };
            if ci.queue_create_info_count > 0 && !ci.p_queue_create_infos.is_null() {
                let qci = unsafe { &*ci.p_queue_create_infos };
                QUEUE_FAMILY.store(qci.queue_family_index, Ordering::Relaxed);
            }
        }
        if let Ok(mut st) = STATE.lock() {
            st.device = device.as_raw();
        }
        tracing::info!("vk-overlay: captured engine VkDevice");
    }
    r
}

unsafe extern "system" fn eclipse_vk_create_swapchain_khr(
    device: vk::Device,
    p_create_info: *const vk::SwapchainCreateInfoKHR<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_swapchain: *mut vk::SwapchainKHR,
) -> vk::Result {
    let Some(addr) = cached(&HOST_CREATE_SWAPCHAIN) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    let host: vk::PFN_vkCreateSwapchainKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkCreateSwapchainKHR>(addr) };

    let Some(requested) = (unsafe { p_create_info.as_ref() }) else {
        return unsafe { host(device, p_create_info, p_allocator, p_swapchain) };
    };
    let present_mode = match unsafe {
        super::vulkan_wsi::swapchain_present_mode(requested.surface, requested.present_mode)
    } {
        Ok(present_mode) => present_mode,
        Err(r) => return r,
    };
    let image_usage = match unsafe {
        super::vulkan_wsi::swapchain_usage_with(
            vk::PhysicalDevice::from_raw(PHYSICAL_DEVICE.load(Ordering::Relaxed)),
            requested.surface,
            requested.image_usage,
            OVERLAY_COPY_USAGE,
        )
    } {
        Ok(image_usage) => image_usage,
        Err(r) => return r,
    };
    let info = requested
        .present_mode(present_mode)
        .image_usage(image_usage);

    let r = unsafe { host(device, &info, p_allocator, p_swapchain) };
    if r == vk::Result::SUCCESS && !p_swapchain.is_null() {
        let swapchain = unsafe { *p_swapchain };
        if let Ok(mut st) = STATE.lock() {
            st.swapchain = swapchain.as_raw();
            st.surface = info.surface.as_raw();
            st.fit = SwapchainFit::Unproven;
            st.format = info.image_format.as_raw();
            st.width = info.image_extent.width;
            st.height = info.image_extent.height;
            st.images.clear();
            st.copies = SwapchainCopies::of(info.image_usage);
        }
        tracing::info!(
            format = info.image_format.as_raw(),
            width = info.image_extent.width,
            height = info.image_extent.height,
            requested_present_mode = ?requested.present_mode,
            present_mode = ?info.present_mode,
            image_usage = ?info.image_usage,
            "vk-overlay: captured engine swapchain"
        );
    }
    r
}

unsafe extern "system" fn eclipse_vk_destroy_swapchain_khr(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    p_allocator: *const vk::AllocationCallbacks<'_>,
) {
    let Some(addr) = cached(&HOST_DESTROY_SWAPCHAIN) else {
        tracing::error!("vk-overlay: missing host vkDestroySwapchainKHR");
        return;
    };
    let host: vk::PFN_vkDestroySwapchainKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkDestroySwapchainKHR>(addr) };

    retire_swapchain(swapchain.as_raw());
    unsafe { host(device, swapchain, p_allocator) };
    if let Ok(mut sets) = PRESENT_SEMAPHORES.lock() {
        sets.retain(|set| set.swapchain != swapchain.as_raw());
    }
}

fn retire_swapchain(swapchain: u64) {
    let mut st = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if st.swapchain == swapchain {
        st.swapchain = 0;
        st.images.clear();
    }
}

unsafe extern "system" fn eclipse_vk_get_swapchain_images_khr(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    p_swapchain_image_count: *mut u32,
    p_swapchain_images: *mut vk::Image,
) -> vk::Result {
    let Some(addr) = cached(&HOST_GET_SWAPCHAIN_IMAGES) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    let host: vk::PFN_vkGetSwapchainImagesKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkGetSwapchainImagesKHR>(addr) };

    let r = unsafe {
        host(
            device,
            swapchain,
            p_swapchain_image_count,
            p_swapchain_images,
        )
    };
    if (r == vk::Result::SUCCESS || r == vk::Result::INCOMPLETE)
        && !p_swapchain_images.is_null()
        && !p_swapchain_image_count.is_null()
    {
        let count = unsafe { *p_swapchain_image_count } as usize;
        let images = unsafe { std::slice::from_raw_parts(p_swapchain_images, count) };
        if let Ok(mut st) = STATE.lock() {
            if st.swapchain == swapchain.as_raw() {
                st.images = images.iter().map(|i| i.as_raw()).collect();
                tracing::info!(count, "vk-overlay: captured swapchain images");
            }
        }
    }
    r
}

fn login_field_rect(extent: vk::Extent2D) -> vk::Rect2D {
    let x = 181u32.min(extent.width.saturating_sub(1));
    let y = 149u32.min(extent.height.saturating_sub(1));
    let w = 438u32.min(extent.width - x);
    let h = 46u32.min(extent.height - y);
    vk::Rect2D {
        offset: vk::Offset2D {
            x: x as i32,
            y: y as i32,
        },
        extent: vk::Extent2D {
            width: w,
            height: h,
        },
    }
}

fn resolve_field_rect(
    geom: Option<(i32, i32, u32, u32)>,
    extent: vk::Extent2D,
) -> Option<vk::Rect2D> {
    let (gx, gy, gw, gh) = geom?;
    let span = |origin: i32, length: u32, limit: u32| {
        let start = i64::from(origin).max(0);
        let end = (i64::from(origin) + i64::from(length)).min(i64::from(limit));
        (start < end).then(|| (start as i32, (end - start) as u32))
    };
    let (x, width) = span(gx, gw, extent.width)?;
    let (y, height) = span(gy, gh, extent.height)?;
    Some(vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D { width, height },
    })
}

fn screenshot_enabled() -> bool {
    static SHOT: OnceLock<bool> = OnceLock::new();
    *SHOT.get_or_init(|| std::env::var_os("ECLIPSE_VK_SCREENSHOT").is_some())
}

fn full_surface_rect(extent: vk::Extent2D) -> Option<vk::Rect2D> {
    if extent.width == 0 || extent.height == 0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    })
}

fn select_text_probe_rect(
    geometry: Option<(i32, i32, u32, u32)>,
    extent: vk::Extent2D,
    drawing_text: bool,
    probing: bool,
    full_screenshot: bool,
) -> Option<vk::Rect2D> {
    let live = resolve_field_rect(geometry, extent);
    if drawing_text {
        return live;
    }
    if !probing {
        return None;
    }
    if full_screenshot {
        return full_surface_rect(extent);
    }
    live.or_else(|| {
        let rect = login_field_rect(extent);
        (rect.extent.width != 0 && rect.extent.height != 0).then_some(rect)
    })
}

unsafe fn locate_image_index(pi: &vk::PresentInfoKHR<'_>, our_sc: u64) -> Option<u32> {
    if our_sc == 0
        || pi.swapchain_count == 0
        || pi.p_swapchains.is_null()
        || pi.p_image_indices.is_null()
    {
        return None;
    }
    let n = pi.swapchain_count as usize;

    let swapchains = unsafe { std::slice::from_raw_parts(pi.p_swapchains, n) };
    let indices = unsafe { std::slice::from_raw_parts(pi.p_image_indices, n) };
    swapchains
        .iter()
        .position(|sc| sc.as_raw() == our_sc)
        .map(|i| indices[i])
}

fn probe_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("ECLIPSE_VK_PROBE").is_some())
}

fn fps_probe_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("ECLIPSE_VK_FPS").is_some())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostAccess {
    Write,
    ReadWrite,
}

fn find_host_visible_mem_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    access: HostAccess,
) -> Option<u32> {
    let coherent = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let find = |want: vk::MemoryPropertyFlags| {
        (0..props.memory_type_count).find(|&i| {
            (type_bits & (1u32 << i)) != 0
                && props.memory_types[i as usize].property_flags.contains(want)
        })
    };
    match access {
        HostAccess::Write => find(coherent),
        HostAccess::ReadWrite => {
            find(coherent | vk::MemoryPropertyFlags::HOST_CACHED).or_else(|| find(coherent))
        }
    }
}

static PROBE: Mutex<Option<Probe>> = Mutex::new(None);

static PRESENT_SEMAPHORES: Mutex<Vec<PresentSemaphores>> = Mutex::new(Vec::new());

fn find_device_local_mem_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
) -> Option<u32> {
    let usable = |i: u32| (type_bits & (1u32 << i)) != 0;
    (0..props.memory_type_count)
        .find(|&i| {
            usable(i)
                && props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        })
        .or_else(|| (0..props.memory_type_count).find(|&i| usable(i)))
}

fn rect_copy(rect: vk::Rect2D) -> vk::BufferImageCopy {
    vk::BufferImageCopy::default()
        .image_subresource(color_layers())
        .image_offset(vk::Offset3D {
            x: rect.offset.x,
            y: rect.offset.y,
            z: 0,
        })
        .image_extent(vk::Extent3D {
            width: rect.extent.width,
            height: rect.extent.height,
            depth: 1,
        })
}

fn write_field_probe(data: &[u8], w: usize, h: usize, order: ByteOrder) {
    let png_rgba: Vec<u8> = data
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&pixel| {
            let [r, g, b, _] = order.arrange(pixel);
            [r, g, b, 255]
        })
        .collect();
    let png = encode_png_rgba(&png_rgba, w as u32, h as u32);
    if let Err(error) = std::fs::write("/tmp/eclipse_field_probe.png", png) {
        tracing::warn!(%error, "vk-overlay: could not write the field probe image");
    }

    static LOG_TICK: AtomicU64 = AtomicU64::new(0);
    if !LOG_TICK.fetch_add(1, Ordering::Relaxed).is_multiple_of(60) {
        return;
    }
    const BUCKETS: usize = 64;
    let mut col_ink = [0u32; BUCKETS];
    let mut total_ink = 0u32;
    let (y0, y1) = (h * 3 / 10, h * 7 / 10);
    for y in y0..y1 {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let lum = (u32::from(data[i]) + u32::from(data[i + 1]) + u32::from(data[i + 2])) / 3;
            if lum > 90 {
                total_ink += 1;
                col_ink[x * BUCKETS / w] += 1;
            }
        }
    }
    let max = col_ink.iter().copied().max().unwrap_or(1).max(1);
    let levels = [' ', '.', ':', '-', '=', '+', '*', '#', '@'];
    let spark: String = col_ink
        .iter()
        .map(|&c| levels[(c as usize * (levels.len() - 1) / max as usize).min(levels.len() - 1)])
        .collect();
    tracing::info!(total_ink, "vk-overlay field-probe ink |{spark}|");
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

#[derive(Debug, Clone, Copy)]
struct EngineHandles {
    instance: u64,
    device: u64,
    physical_device: u64,
    queue_family: u32,
}

impl EngineHandles {
    fn current(device: u64) -> Self {
        Self {
            instance: INSTANCE.load(Ordering::Relaxed),
            device,
            physical_device: PHYSICAL_DEVICE.load(Ordering::Relaxed),
            queue_family: QUEUE_FAMILY.load(Ordering::Relaxed),
        }
    }

    fn complete(&self) -> bool {
        self.instance != 0
            && self.device != 0
            && self.physical_device != 0
            && self.queue_family != u32::MAX
    }
}

fn release_probe_for_device(device: vk::Device) {
    let mut probe = match PROBE.lock() {
        Ok(probe) => probe,
        Err(poisoned) => {
            tracing::warn!("vk-overlay: recovering poisoned probe lock during device teardown");
            poisoned.into_inner()
        }
    };
    if probe
        .as_ref()
        .is_some_and(|probe| probe.device.handle() == device)
    {
        *probe = None;
    }
}

fn release_overlay_device_resources(device: vk::Device) {
    release_probe_for_device(device);
    {
        let mut compositor = TEXT_COMPOSITOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owned = match &*compositor {
            TextCompositorSlot::Built(built) => built.device.handle() == device,
            TextCompositorSlot::Unavailable(built_for, _) => *built_for == device.as_raw(),
            TextCompositorSlot::Empty => false,
        };
        if owned {
            *compositor = TextCompositorSlot::Empty;
        }
    }
    if let Ok(mut sets) = PRESENT_SEMAPHORES.lock() {
        sets.retain(|set| set.device.handle() != device);
    }

    let mut state = match STATE.lock() {
        Ok(state) => state,
        Err(poisoned) => {
            tracing::warn!("vk-overlay: recovering poisoned state lock during device teardown");
            poisoned.into_inner()
        }
    };
    if state.device != device.as_raw() {
        return;
    }

    *state = OverlayState::default();
    PHYSICAL_DEVICE.store(0, Ordering::Relaxed);
    QUEUE_FAMILY.store(u32::MAX, Ordering::Relaxed);
    HOST_QUEUE_PRESENT.store(0, Ordering::Relaxed);
    HOST_CREATE_SWAPCHAIN.store(0, Ordering::Relaxed);
    HOST_GET_SWAPCHAIN_IMAGES.store(0, Ordering::Relaxed);
    HOST_ACQUIRE_NEXT_IMAGE.store(0, Ordering::Relaxed);
    HOST_ACQUIRE_NEXT_IMAGE2.store(0, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayBatch {
    NotSubmitted,

    Completed,

    Queued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresentGate {
    Engine,

    Settled,

    Pending,
}

impl PresentGate {
    fn waits(self, engine_waits: &[vk::Semaphore]) -> &[vk::Semaphore] {
        match self {
            Self::Engine => engine_waits,
            Self::Settled | Self::Pending => &[],
        }
    }

    fn after(self, batch: OverlayBatch) -> Self {
        match batch {
            OverlayBatch::NotSubmitted => self,
            OverlayBatch::Completed => Self::Settled,
            OverlayBatch::Queued => Self::Pending,
        }
    }
}

struct PresentSemaphores {
    device: ash::Device,
    swapchain: u64,
    semaphores: Vec<vk::Semaphore>,
}

impl PresentSemaphores {
    fn create(device: ash::Device, swapchain: u64, count: usize) -> Option<Self> {
        let mut set = Self {
            device,
            swapchain,
            semaphores: Vec::with_capacity(count),
        };
        for _ in 0..count {
            let semaphore = unsafe {
                set.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .ok()?;
            set.semaphores.push(semaphore);
        }
        Some(set)
    }

    fn signal(&self, queue: vk::Queue, image_index: u32) -> Option<vk::Semaphore> {
        let semaphore = *self.semaphores.get(image_index as usize)?;
        let signals = [semaphore];
        let batch = vk::SubmitInfo::default().signal_semaphores(&signals);
        unsafe { self.device.queue_submit(queue, &[batch], vk::Fence::null()) }.ok()?;
        Some(semaphore)
    }
}

impl Drop for PresentSemaphores {
    fn drop(&mut self) {
        for semaphore in self.semaphores.drain(..) {
            unsafe { self.device.destroy_semaphore(semaphore, None) };
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PresentTarget {
    device: u64,
    swapchain: u64,
    image_index: u32,
    image_count: usize,
}

fn engine_device(device_raw: u64) -> Option<ash::Device> {
    let entry = super::vulkan_wsi::host_entry()?;
    let instance_raw = INSTANCE.load(Ordering::Relaxed);
    if instance_raw == 0 || device_raw == 0 {
        return None;
    }
    let instance =
        unsafe { ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(instance_raw)) };
    Some(unsafe { ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw(device_raw)) })
}

fn signal_present_semaphore(queue: vk::Queue, target: PresentTarget) -> Option<vk::Semaphore> {
    let mut sets = PRESENT_SEMAPHORES.lock().ok()?;
    let index = match sets
        .iter()
        .position(|set| set.swapchain == target.swapchain)
    {
        Some(index) => index,
        None => {
            let device = engine_device(target.device)?;
            sets.push(PresentSemaphores::create(
                device,
                target.swapchain,
                target.image_count,
            )?);
            sets.len() - 1
        }
    };
    sets[index].signal(queue, target.image_index)
}

unsafe fn present_through_gate(
    gate: PresentGate,
    host: vk::PFN_vkQueuePresentKHR,
    queue: vk::Queue,
    pi: &vk::PresentInfoKHR<'_>,
    target: PresentTarget,
) -> vk::Result {
    let mut info = *pi;
    match gate {
        PresentGate::Engine => return unsafe { host(queue, pi) },
        PresentGate::Settled => {
            info.wait_semaphore_count = 0;
            info.p_wait_semaphores = std::ptr::null();
        }
        PresentGate::Pending => {
            let ready = signal_present_semaphore(queue, target);
            if let Some(ready) = ready.as_ref() {
                info.wait_semaphore_count = 1;
                info.p_wait_semaphores = ready;
                return unsafe { host(queue, &info) };
            }
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "vk-overlay: could not signal the present semaphore; waiting for the queue \
                     on the CPU instead"
                );
            }
            if let Some(device) = engine_device(target.device) {
                if let Err(e) = unsafe { device.queue_wait_idle(queue) } {
                    tracing::error!(
                        error = %e,
                        "vk-overlay: vkQueueWaitIdle before present failed"
                    );
                }
            }
            info.wait_semaphore_count = 0;
            info.p_wait_semaphores = std::ptr::null();
        }
    }
    unsafe { host(queue, &info) }
}

struct Probe {
    device: ash::Device,
    command_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: FrameFence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    rect: vk::Rect2D,
    waits: Vec<vk::Semaphore>,
    wait_stages: Vec<vk::PipelineStageFlags>,
}

unsafe impl Send for Probe {}

impl Probe {
    fn build(entry: &ash::Entry, engine: EngineHandles, rect: vk::Rect2D) -> Option<Probe> {
        if !engine.complete() || rect.extent.width == 0 || rect.extent.height == 0 {
            return None;
        }

        let instance = unsafe {
            ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(engine.instance))
        };
        let device =
            unsafe { ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw(engine.device)) };

        let mem_props = unsafe {
            instance.get_physical_device_memory_properties(vk::PhysicalDevice::from_raw(
                engine.physical_device,
            ))
        };
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(engine.queue_family);

        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }.ok()?;
        let cleanup_pool = |device: &ash::Device| {
            unsafe { device.destroy_command_pool(command_pool, None) };
        };
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);

        let cmd = match unsafe { device.allocate_command_buffers(&alloc) }
            .ok()
            .and_then(|v| v.into_iter().next())
        {
            Some(c) => c,
            None => {
                cleanup_pool(&device);
                return None;
            }
        };

        let fence = match FrameFence::new(&device) {
            Ok(f) => f,
            Err(_) => {
                cleanup_pool(&device);
                return None;
            }
        };
        let size = u64::from(rect.extent.width) * u64::from(rect.extent.height) * 4;
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let buffer = match unsafe { device.create_buffer(&buf_info, None) } {
            Ok(b) => b,
            Err(_) => {
                unsafe { fence.destroy(&device) };
                cleanup_pool(&device);
                return None;
            }
        };

        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let cleanup_buf = |device: &ash::Device| {
            unsafe {
                device.destroy_buffer(buffer, None);
                fence.destroy(device);
            }
            cleanup_pool(device);
        };
        let Some(mt) =
            find_host_visible_mem_type(&mem_props, req.memory_type_bits, HostAccess::ReadWrite)
        else {
            cleanup_buf(&device);
            return None;
        };
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(mt);

        let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(_) => {
                cleanup_buf(&device);
                return None;
            }
        };

        if unsafe { device.bind_buffer_memory(buffer, memory, 0) }.is_err() {
            unsafe { device.free_memory(memory, None) };
            cleanup_buf(&device);
            return None;
        }

        let mapped =
            match unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) } {
                Ok(p) => p.cast::<u8>(),
                Err(_) => {
                    unsafe { device.free_memory(memory, None) };
                    cleanup_buf(&device);
                    return None;
                }
            };
        Some(Probe {
            device,
            command_pool,
            cmd,
            fence,
            buffer,
            memory,
            mapped,
            rect,
            waits: Vec::new(),
            wait_stages: Vec::new(),
        })
    }

    fn gather_waits(&mut self, engine_waits: &[vk::Semaphore], extra: Option<vk::Semaphore>) {
        self.waits.clear();
        self.waits.extend_from_slice(engine_waits);
        self.waits.extend(extra);
        self.wait_stages.clear();
        self.wait_stages
            .resize(self.waits.len(), vk::PipelineStageFlags::TRANSFER);
    }

    unsafe fn snapshot(
        &mut self,
        queue: vk::Queue,
        image_raw: u64,
        engine_waits: &[vk::Semaphore],
        order: ByteOrder,
    ) -> OverlayBatch {
        let image = vk::Image::from_raw(image_raw);
        let range = color_range();

        unsafe {
            if self.fence.retire(&self.device).is_err()
                || self
                    .device
                    .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                    .is_err()
            {
                return OverlayBatch::NotSubmitted;
            }
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            if self.device.begin_command_buffer(self.cmd, &begin).is_err() {
                return OverlayBatch::NotSubmitted;
            }
            let to_src = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_access_mask(vk::AccessFlags::MEMORY_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(range);
            self.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            self.device.cmd_copy_image_to_buffer(
                self.cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                &[rect_copy(self.rect)],
            );
            let readable = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.buffer)
                .size(vk::WHOLE_SIZE);
            let to_present = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(range);
            self.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS | vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &[readable],
                &[to_present],
            );
            if self.device.end_command_buffer(self.cmd).is_err() {
                return OverlayBatch::NotSubmitted;
            }
            self.gather_waits(engine_waits, None);
            let cmds = [self.cmd];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&self.waits)
                .wait_dst_stage_mask(&self.wait_stages)
                .command_buffers(&cmds);
            if self.fence.submit(&self.device, queue, &[submit]).is_err() {
                return OverlayBatch::NotSubmitted;
            }
            if self.fence.retire(&self.device).is_err() {
                return OverlayBatch::Queued;
            }
            let w = self.rect.extent.width as usize;
            let h = self.rect.extent.height as usize;
            let data = std::slice::from_raw_parts(self.mapped, w * h * 4);
            write_field_probe(data, w, h, order);
            OverlayBatch::Completed
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Err(e) = self.fence.retire(&self.device) {
            tracing::warn!(error = %e, "vk-overlay: overlay work did not finish before teardown");
        }
        unsafe {
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
            self.fence.destroy(&self.device);
            self.device.destroy_command_pool(self.command_pool, None);
        }
    }
}

fn ensure_probe(rect: vk::Rect2D) {
    let Ok(mut guard) = PROBE.lock() else {
        return;
    };
    if guard.as_ref().is_some_and(|p| {
        p.rect.offset.x == rect.offset.x
            && p.rect.offset.y == rect.offset.y
            && p.rect.extent.width == rect.extent.width
            && p.rect.extent.height == rect.extent.height
    }) {
        return;
    }
    let device = STATE.lock().map(|s| s.device).unwrap_or(0);
    if device == 0 {
        return;
    }
    let Some(entry) = super::vulkan_wsi::host_entry() else {
        return;
    };

    *guard = None;
    if let Some(p) = Probe::build(entry, EngineHandles::current(device), rect) {
        tracing::info!(
            x = rect.offset.x,
            y = rect.offset.y,
            w = rect.extent.width,
            h = rect.extent.height,
            "vk-overlay: text probe objects built for rect"
        );
        *guard = Some(p);
    }
}

const TEXT_BLEND_SPV: &[u8] = include_bytes!("../../shaders/text_blend.comp.spv");
const TEXT_SLOTS: usize = 3;
const TEXT_BLEND_GROUP_SIZE: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy)]
struct BlendConstants {
    width: u32,
    height: u32,
    caret_color: u32,
    caret_on: u32,
    caret_rect: [u32; 4],
}

impl BlendConstants {
    fn bytes(&self) -> [u8; 32] {
        let words = [
            self.width,
            self.height,
            self.caret_color,
            self.caret_on,
            self.caret_rect[0],
            self.caret_rect[1],
            self.caret_rect[2],
            self.caret_rect[3],
        ];
        let mut bytes = [0u8; 32];
        for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            *chunk = word.to_ne_bytes();
        }
        bytes
    }
}

struct TextSlot {
    cmd: vk::CommandBuffer,
    fence: FrameFence,
    layer: vk::Buffer,
    layer_memory: vk::DeviceMemory,
    layer_mapped: *mut u8,
    descriptor_set: vk::DescriptorSet,
    generation: Option<u64>,
}

struct TextCompositor {
    device: ash::Device,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    rect: vk::Rect2D,
    capacity: u64,
    command_pool: vk::CommandPool,
    set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pixels: vk::Buffer,
    pixels_memory: vk::DeviceMemory,
    slots: Vec<TextSlot>,
    next_slot: usize,
    waits: Vec<vk::Semaphore>,
    wait_stages: Vec<vk::PipelineStageFlags>,
}

unsafe impl Send for TextCompositor {}

enum TextCompositorSlot {
    Empty,
    Built(Box<TextCompositor>),
    Unavailable(u64, vk::Rect2D),
}

static TEXT_COMPOSITOR: Mutex<TextCompositorSlot> = Mutex::new(TextCompositorSlot::Empty);

fn allocate_buffer(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
    usage: vk::BufferUsageFlags,
    memory: impl Fn(&vk::PhysicalDeviceMemoryProperties, u32) -> Option<u32>,
) -> Option<(vk::Buffer, vk::DeviceMemory)> {
    let info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { device.create_buffer(&info, None) }.ok()?;
    let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
    let allocated = memory(memory_properties, requirements.memory_type_bits).and_then(|index| {
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        let memory = unsafe { device.allocate_memory(&alloc, None) }.ok()?;
        if unsafe { device.bind_buffer_memory(buffer, memory, 0) }.is_err() {
            unsafe { device.free_memory(memory, None) };
            return None;
        }
        Some(memory)
    });
    match allocated {
        Some(memory) => Some((buffer, memory)),
        None => {
            unsafe { device.destroy_buffer(buffer, None) };
            None
        }
    }
}

impl TextCompositor {
    fn build(entry: &ash::Entry, engine: EngineHandles, rect: vk::Rect2D) -> Option<Self> {
        if !engine.complete() || rect.extent.width == 0 || rect.extent.height == 0 {
            return None;
        }
        let instance = unsafe {
            ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(engine.instance))
        };
        let physical_device = vk::PhysicalDevice::from_raw(engine.physical_device);
        let families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        if !families
            .get(engine.queue_family as usize)
            .is_some_and(|family| family.queue_flags.contains(vk::QueueFlags::COMPUTE))
        {
            tracing::warn!(
                queue_family = engine.queue_family,
                "vk-overlay: the engine's queue family has no compute support; focused text \
                 is not drawn"
            );
            return None;
        }
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let device =
            unsafe { ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw(engine.device)) };
        let mut compositor = TextCompositor {
            device,
            memory_properties,
            rect,
            capacity: 0,
            command_pool: vk::CommandPool::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            descriptor_pool: vk::DescriptorPool::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            pixels: vk::Buffer::null(),
            pixels_memory: vk::DeviceMemory::null(),
            slots: Vec::with_capacity(TEXT_SLOTS),
            next_slot: 0,
            waits: Vec::new(),
            wait_stages: Vec::new(),
        };
        match unsafe { compositor.create_objects(engine.queue_family) } {
            Ok(()) => Some(compositor),
            Err(error) => {
                tracing::warn!(%error, "vk-overlay: the focused-text compositor could not be built");
                None
            }
        }
    }

    unsafe fn create_objects(&mut self, queue_family: u32) -> Result<(), String> {
        let device = self.device.clone();
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(queue_family);
        self.command_pool = unsafe { device.create_command_pool(&pool_info, None) }
            .map_err(|e| format!("vkCreateCommandPool: {e}"))?;
        let bindings = [0, 1].map(|binding| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        });
        self.set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(|e| format!("vkCreateDescriptorSetLayout: {e}"))?;
        let push_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size(std::mem::size_of::<BlendConstants>() as u32)];
        let set_layouts = [self.set_layout];
        self.pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push_ranges),
                None,
            )
        }
        .map_err(|e| format!("vkCreatePipelineLayout: {e}"))?;
        let code = ash::util::read_spv(&mut std::io::Cursor::new(TEXT_BLEND_SPV))
            .map_err(|e| format!("text blend SPIR-V: {e}"))?;
        let module = unsafe {
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
        }
        .map_err(|e| format!("vkCreateShaderModule: {e}"))?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"main");
        let pipeline_info = vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(self.pipeline_layout);
        let pipelines = unsafe {
            device.create_compute_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        };
        unsafe { device.destroy_shader_module(module, None) };
        self.pipeline = pipelines
            .map_err(|(_, e)| format!("vkCreateComputePipelines: {e}"))?
            .into_iter()
            .next()
            .ok_or("vkCreateComputePipelines returned no pipeline")?;
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(2 * TEXT_SLOTS as u32)];
        self.descriptor_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(TEXT_SLOTS as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .map_err(|e| format!("vkCreateDescriptorPool: {e}"))?;
        let command_buffers = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(TEXT_SLOTS as u32),
            )
        }
        .map_err(|e| format!("vkAllocateCommandBuffers: {e}"))?;
        let layouts = [self.set_layout; TEXT_SLOTS];
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.descriptor_pool)
                    .set_layouts(&layouts),
            )
        }
        .map_err(|e| format!("vkAllocateDescriptorSets: {e}"))?;
        for (cmd, descriptor_set) in command_buffers.into_iter().zip(sets) {
            let fence = FrameFence::new(&device).map_err(|e| format!("vkCreateFence: {e}"))?;
            self.slots.push(TextSlot {
                cmd,
                fence,
                layer: vk::Buffer::null(),
                layer_memory: vk::DeviceMemory::null(),
                layer_mapped: std::ptr::null_mut(),
                descriptor_set,
                generation: None,
            });
        }
        unsafe { self.allocate_field(self.layer_bytes() as u64) }
    }

    unsafe fn fit(&mut self, rect: vk::Rect2D) -> Result<(), String> {
        let needed = field_bytes(rect);
        if needed > self.capacity {
            let grown = needed.max(self.capacity.saturating_mul(2));
            for slot in &mut self.slots {
                slot.fence
                    .retire(&self.device)
                    .map_err(|e| format!("vkWaitForFences: {e}"))?;
            }
            unsafe {
                self.release_field();
                self.allocate_field(grown)?;
            }
        }
        self.rect = rect;
        Ok(())
    }

    unsafe fn allocate_field(&mut self, capacity: u64) -> Result<(), String> {
        let device = self.device.clone();
        (self.pixels, self.pixels_memory) = allocate_buffer(
            &device,
            &self.memory_properties,
            capacity,
            vk::BufferUsageFlags::TRANSFER_SRC
                | vk::BufferUsageFlags::TRANSFER_DST
                | vk::BufferUsageFlags::STORAGE_BUFFER,
            find_device_local_mem_type,
        )
        .ok_or("no memory for the field pixel buffer")?;
        for slot in &mut self.slots {
            (slot.layer, slot.layer_memory) = allocate_buffer(
                &device,
                &self.memory_properties,
                capacity,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                |properties, bits| find_host_visible_mem_type(properties, bits, HostAccess::Write),
            )
            .ok_or("no host-visible memory for the text layer")?;
            slot.layer_mapped = unsafe {
                device.map_memory(slot.layer_memory, 0, capacity, vk::MemoryMapFlags::empty())
            }
            .map_err(|e| format!("vkMapMemory: {e}"))?
            .cast::<u8>();
            slot.generation = None;
            let pixels_info = [vk::DescriptorBufferInfo::default()
                .buffer(self.pixels)
                .range(vk::WHOLE_SIZE)];
            let layer_info = [vk::DescriptorBufferInfo::default()
                .buffer(slot.layer)
                .range(vk::WHOLE_SIZE)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(slot.descriptor_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&pixels_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(slot.descriptor_set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&layer_info),
            ];
            unsafe { device.update_descriptor_sets(&writes, &[]) };
        }
        self.capacity = capacity;
        Ok(())
    }

    unsafe fn release_field(&mut self) {
        self.capacity = 0;
        unsafe {
            for slot in &mut self.slots {
                self.device.destroy_buffer(slot.layer, None);
                self.device.free_memory(slot.layer_memory, None);
                slot.layer = vk::Buffer::null();
                slot.layer_memory = vk::DeviceMemory::null();
                slot.layer_mapped = std::ptr::null_mut();
            }
            self.device.destroy_buffer(self.pixels, None);
            self.device.free_memory(self.pixels_memory, None);
        }
        self.pixels = vk::Buffer::null();
        self.pixels_memory = vk::DeviceMemory::null();
    }

    fn layer_bytes(&self) -> usize {
        field_bytes(self.rect) as usize
    }

    unsafe fn draw(
        &mut self,
        queue: vk::Queue,
        image_raw: u64,
        engine_waits: &[vk::Semaphore],
        layer: &TextLayer,
        caret_on: bool,
    ) -> OverlayBatch {
        let bytes = self.layer_bytes();
        if layer.pixels.len() * 4 != bytes || self.slots.is_empty() {
            return OverlayBatch::NotSubmitted;
        }
        let index = self.next_slot;
        self.next_slot = (index + 1) % self.slots.len();
        let slot = &mut self.slots[index];
        if slot.fence.retire(&self.device).is_err() {
            return OverlayBatch::NotSubmitted;
        }
        if slot.generation != Some(layer.generation) {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    layer.pixels.as_ptr().cast::<u8>(),
                    slot.layer_mapped,
                    bytes,
                )
            };
            slot.generation = Some(layer.generation);
        }
        let caret = layer.caret.filter(|_| caret_on);
        let constants = BlendConstants {
            width: self.rect.extent.width,
            height: self.rect.extent.height,
            caret_color: u32::from_ne_bytes(layer.caret_color),
            caret_on: u32::from(caret.is_some()),
            caret_rect: caret.map_or([0; 4], |rect| [rect.x0, rect.y0, rect.x1, rect.y1]),
        };
        let (cmd, descriptor_set) = (slot.cmd, slot.descriptor_set);
        if unsafe {
            self.record(
                cmd,
                descriptor_set,
                vk::Image::from_raw(image_raw),
                &constants,
            )
        }
        .is_err()
        {
            return OverlayBatch::NotSubmitted;
        }
        self.waits.clear();
        self.waits.extend_from_slice(engine_waits);
        self.wait_stages.clear();
        self.wait_stages
            .resize(self.waits.len(), vk::PipelineStageFlags::TRANSFER);
        let cmds = [cmd];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&self.waits)
            .wait_dst_stage_mask(&self.wait_stages)
            .command_buffers(&cmds);
        match self.slots[index]
            .fence
            .submit(&self.device, queue, &[submit])
        {
            Ok(()) => OverlayBatch::Queued,
            Err(_) => OverlayBatch::NotSubmitted,
        }
    }

    unsafe fn record(
        &self,
        cmd: vk::CommandBuffer,
        descriptor_set: vk::DescriptorSet,
        image: vk::Image,
        constants: &BlendConstants,
    ) -> ash::prelude::VkResult<()> {
        let device = &self.device;
        let image_barrier = |from, to, src_access, dst_access| {
            vk::ImageMemoryBarrier::default()
                .old_layout(from)
                .new_layout(to)
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(color_range())
        };
        let pixels_barrier = |src_access, dst_access| {
            vk::BufferMemoryBarrier::default()
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.pixels)
                .size(vk::WHOLE_SIZE)
        };
        let region = rect_copy(self.rect);
        unsafe {
            device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[pixels_barrier(
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                )],
                &[image_barrier(
                    vk::ImageLayout::PRESENT_SRC_KHR,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::MEMORY_READ,
                    vk::AccessFlags::TRANSFER_READ,
                )],
            );
            device.cmd_copy_image_to_buffer(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.pixels,
                &[region],
            );
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[pixels_barrier(
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                )],
                &[],
            );
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[descriptor_set],
                &[],
            );
            device.cmd_push_constants(
                cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &constants.bytes(),
            );
            device.cmd_dispatch(
                cmd,
                constants.width.div_ceil(TEXT_BLEND_GROUP_SIZE),
                constants.height.div_ceil(TEXT_BLEND_GROUP_SIZE),
                1,
            );
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[pixels_barrier(
                    vk::AccessFlags::SHADER_WRITE,
                    vk::AccessFlags::TRANSFER_READ,
                )],
                &[image_barrier(
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::TRANSFER_READ,
                    vk::AccessFlags::TRANSFER_WRITE,
                )],
            );
            device.cmd_copy_buffer_to_image(
                cmd,
                self.pixels,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[image_barrier(
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::PRESENT_SRC_KHR,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::MEMORY_READ,
                )],
            );
            device.end_command_buffer(cmd)
        }
    }
}

impl Drop for TextCompositor {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Err(e) = slot.fence.retire(&self.device) {
                tracing::warn!(error = %e, "vk-overlay: focused-text work did not finish before teardown");
            }
        }
        unsafe {
            self.release_field();
            for slot in self.slots.drain(..) {
                slot.fence.destroy(&self.device);
            }
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
            self.device.destroy_command_pool(self.command_pool, None);
        }
    }
}

fn field_bytes(rect: vk::Rect2D) -> u64 {
    u64::from(rect.extent.width) * u64::from(rect.extent.height) * 4
}

fn text_test_overlay(extent: vk::Extent2D) -> Option<ActiveTextOverlay> {
    static TEXT: OnceLock<Option<String>> = OnceLock::new();
    let text = TEXT
        .get_or_init(|| std::env::var("ECLIPSE_VK_TEXT_TEST").ok())
        .as_ref()?;
    let rect = login_field_rect(extent);
    Some(ActiveTextOverlay {
        widget: 0,
        text: text.clone(),
        selection: TextSelection::Cursor(text.encode_utf16().count()),
        geometry: (
            rect.offset.x,
            rect.offset.y,
            rect.extent.width,
            rect.extent.height,
        ),
        input_type: 0,
        font: TEXT_TEST_FONT,
        font_size: 25.0,
        multiline: false,
        text_wrapped: false,
        text_color: -1,
        x_alignment: 0,
        y_alignment: 1,
    })
}

struct TextPlan {
    overlay: Option<ActiveTextOverlay>,
    rect: vk::Rect2D,
    order: ByteOrder,
}

fn text_plan(extent: vk::Extent2D, image_raw: u64, format_raw: i32) -> Option<TextPlan> {
    if image_raw == 0 || !(probe_enabled() || overlay_enabled()) {
        return None;
    }
    let Some(order) = ByteOrder::of_swapchain(format_raw) else {
        static FORMAT_WARNED: AtomicBool = AtomicBool::new(false);
        if !FORMAT_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                format = format_raw,
                "vk-overlay: focused text is not drawn on this swapchain format \
                 (expected B8G8R8A8/R8G8B8A8 UNORM/SRGB)"
            );
        }
        return None;
    };
    let live = if overlay_enabled() {
        crate::framework::active_text_overlay()
    } else {
        None
    };
    let overlay = live.or_else(|| text_test_overlay(extent));
    if overlay.is_none() && !probe_enabled() {
        return None;
    }
    let geometry = match &overlay {
        Some(overlay) => Some(overlay.geometry),
        None => crate::framework::textbox_geometry(),
    };
    let rect = select_text_probe_rect(
        geometry,
        extent,
        overlay.is_some(),
        probe_enabled(),
        screenshot_enabled(),
    )?;
    Some(TextPlan {
        overlay,
        rect,
        order,
    })
}

fn draw_text_field(
    queue: vk::Queue,
    image_raw: u64,
    engine_waits: &[vk::Semaphore],
    plan: &TextPlan,
    overlay: &ActiveTextOverlay,
) -> OverlayBatch {
    let request = TextRequest::of(overlay, plan.rect, plan.order);
    let Some((layer, caret_on)) = current_text_layer(&request, Instant::now()) else {
        return OverlayBatch::NotSubmitted;
    };
    let mut slot = TEXT_COMPOSITOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = STATE.lock().map(|state| state.device).unwrap_or(0);
    let Some(compositor) = fitted_compositor(&mut slot, device, plan.rect, || {
        super::vulkan_wsi::host_entry().and_then(|entry| {
            TextCompositor::build(entry, EngineHandles::current(device), plan.rect)
        })
    }) else {
        return OverlayBatch::NotSubmitted;
    };
    unsafe { compositor.draw(queue, image_raw, engine_waits, &layer, caret_on) }
}

fn fitted_compositor(
    slot: &mut TextCompositorSlot,
    device: u64,
    rect: vk::Rect2D,
    build: impl FnOnce() -> Option<TextCompositor>,
) -> Option<&mut TextCompositor> {
    let current = match &*slot {
        TextCompositorSlot::Built(compositor) => compositor.device.handle().as_raw() == device,
        TextCompositorSlot::Unavailable(built_for, failed_rect) => {
            *built_for == device && *failed_rect == rect
        }
        TextCompositorSlot::Empty => false,
    };
    if !current {
        *slot = TextCompositorSlot::Empty;
        *slot = match build() {
            Some(compositor) => TextCompositorSlot::Built(Box::new(compositor)),
            None => TextCompositorSlot::Unavailable(device, rect),
        };
    }
    let fitted = match slot {
        TextCompositorSlot::Built(compositor) => unsafe { compositor.fit(rect) },
        TextCompositorSlot::Unavailable(..) | TextCompositorSlot::Empty => return None,
    };
    if let Err(error) = fitted {
        tracing::warn!(%error, "vk-overlay: the focused-text buffers could not grow");
        *slot = TextCompositorSlot::Unavailable(device, rect);
        return None;
    }
    match slot {
        TextCompositorSlot::Built(compositor) => Some(compositor),
        TextCompositorSlot::Unavailable(..) | TextCompositorSlot::Empty => None,
    }
}

fn snapshot_field(
    queue: vk::Queue,
    image_raw: u64,
    engine_waits: &[vk::Semaphore],
    plan: &TextPlan,
) -> OverlayBatch {
    ensure_probe(plan.rect);
    let Ok(mut guard) = PROBE.lock() else {
        return OverlayBatch::NotSubmitted;
    };
    match guard.as_mut() {
        Some(probe) => unsafe { probe.snapshot(queue, image_raw, engine_waits, plan.order) },
        None => OverlayBatch::NotSubmitted,
    }
}

unsafe fn present_with_overlay(
    host: vk::PFN_vkQueuePresentKHR,
    queue: vk::Queue,
    p_present_info: *const vk::PresentInfoKHR<'_>,
) -> vk::Result {
    if p_present_info.is_null() {
        return unsafe { host(queue, p_present_info) };
    }

    if !overlay_enabled() && !probe_enabled() {
        return unsafe { host(queue, p_present_info) };
    }

    if !probe_enabled() && crate::framework::active_text_field() == 0 {
        return unsafe { host(queue, p_present_info) };
    }

    let pi = unsafe { &*p_present_info };
    let (our_sc, copies) = match STATE.lock() {
        Ok(s) => (s.swapchain, s.copies),
        Err(_) => (0, SwapchainCopies::Unsupported),
    };

    let Some(image_index) = (unsafe { locate_image_index(pi, our_sc) }) else {
        return unsafe { host(queue, p_present_info) };
    };
    if copies == SwapchainCopies::Unsupported {
        static COPIES_WARNED: AtomicBool = AtomicBool::new(false);
        if !COPIES_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "vk-overlay: the host surface does not allow copies into the engine swapchain, \
                 so focused text is not drawn over it"
            );
        }
        return unsafe { host(queue, p_present_info) };
    }

    let (extent, image_raw, format_raw, device_raw, image_count) = match STATE.lock() {
        Ok(st) => (
            vk::Extent2D {
                width: st.width,
                height: st.height,
            },
            st.images.get(image_index as usize).copied().unwrap_or(0),
            st.format,
            st.device,
            st.images.len(),
        ),
        Err(_) => (vk::Extent2D::default(), 0, 0, 0, 0),
    };
    let engine_waits: &[vk::Semaphore] =
        if pi.wait_semaphore_count > 0 && !pi.p_wait_semaphores.is_null() {
            unsafe {
                std::slice::from_raw_parts(pi.p_wait_semaphores, pi.wait_semaphore_count as usize)
            }
        } else {
            &[]
        };

    let mut gate = PresentGate::Engine;

    if let Some(plan) = text_plan(extent, image_raw, format_raw) {
        if let Some(overlay) = &plan.overlay {
            gate = gate.after(draw_text_field(
                queue,
                image_raw,
                gate.waits(engine_waits),
                &plan,
                overlay,
            ));
        }
        if probe_enabled() {
            gate = gate.after(snapshot_field(
                queue,
                image_raw,
                gate.waits(engine_waits),
                &plan,
            ));
        }
    }

    let target = PresentTarget {
        device: device_raw,
        swapchain: our_sc,
        image_index,
        image_count,
    };
    unsafe { present_through_gate(gate, host, queue, pi, target) }
}

unsafe extern "system" fn eclipse_vk_queue_present_khr(
    queue: vk::Queue,
    p_present_info: *const vk::PresentInfoKHR<'_>,
) -> vk::Result {
    let n = PRESENT_COUNT.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        if let Ok(st) = STATE.lock() {
            tracing::info!(
                device_set = st.device != 0,
                swapchain_set = st.swapchain != 0,
                format = st.format,
                width = st.width,
                height = st.height,
                images = st.images.len(),
                "vk-overlay: present seam armed (engine present interposed)"
            );
        }
    }

    if fps_probe_enabled() && n.is_multiple_of(120) {
        static LAST: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
        if let Ok(mut g) = LAST.lock() {
            let now = std::time::Instant::now();
            if let Some((t0, n0)) = *g {
                let dt = now.duration_since(t0).as_secs_f64();
                if dt > 0.0 {
                    tracing::info!(
                        fps = ((n - n0) as f64 / dt) as u32,
                        field_focused = crate::framework::active_text_field() != 0,
                        "vk-overlay present rate"
                    );
                }
            }
            *g = Some((now, n));
        }
    }
    let Some(addr) = cached(&HOST_QUEUE_PRESENT) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    let host: vk::PFN_vkQueuePresentKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkQueuePresentKHR>(addr) };

    crate::framework::engine_presented();
    let result = unsafe { present_with_overlay(host, queue, p_present_info) };
    let report = match result {
        vk::Result::SUCCESS => Some(HostReport::FramePresented),
        vk::Result::SUBOPTIMAL_KHR => Some(HostReport::Suboptimal),
        vk::Result::ERROR_OUT_OF_DATE_KHR => Some(HostReport::OutOfDate),
        _ => None,
    };
    if let Some(report) = report {
        for &swapchain in unsafe { presented_swapchains(p_present_info) } {
            note_host_report(swapchain, report);
        }
    }
    result
}

unsafe fn presented_swapchains<'a>(
    p_present_info: *const vk::PresentInfoKHR<'a>,
) -> &'a [vk::SwapchainKHR] {
    match unsafe { p_present_info.as_ref() } {
        Some(pi) if pi.swapchain_count > 0 && !pi.p_swapchains.is_null() => unsafe {
            std::slice::from_raw_parts(pi.p_swapchains, pi.swapchain_count as usize)
        },
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;

    #[test]
    fn resolve_field_rect_draws_nothing_without_a_live_textbox_session() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let as_tuple = |r: Option<vk::Rect2D>| {
            r.map(|r| (r.offset.x, r.offset.y, r.extent.width, r.extent.height))
        };

        assert_eq!(as_tuple(resolve_field_rect(None, extent)), None);

        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 0, 46)), extent)),
            None
        );
        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 438, 0)), extent)),
            None
        );

        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 300, 390, 46)), extent)),
            Some((181, 300, 390, 46))
        );
        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 438, 46)), extent)),
            Some((181, 149, 438, 46))
        );

        assert_eq!(
            as_tuple(resolve_field_rect(Some((700, 560, 438, 46)), extent)),
            Some((700, 560, 100, 40))
        );

        assert_eq!(
            as_tuple(resolve_field_rect(Some((-5, -7, 300, 40)), extent)),
            Some((0, 0, 295, 33))
        );
        assert_eq!(
            as_tuple(resolve_field_rect(Some((900, 0, 10, 10)), extent)),
            None
        );
    }

    #[test]
    fn full_frame_probe_never_expands_or_invents_a_text_draw_rect() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let as_tuple = |r: Option<vk::Rect2D>| {
            r.map(|r| (r.offset.x, r.offset.y, r.extent.width, r.extent.height))
        };
        let live = Some((181, 300, 390, 46));

        assert_eq!(
            as_tuple(select_text_probe_rect(live, extent, true, true, true)),
            Some((181, 300, 390, 46))
        );

        assert_eq!(
            as_tuple(select_text_probe_rect(live, extent, false, true, true)),
            Some((0, 0, 800, 600))
        );

        assert_eq!(
            as_tuple(select_text_probe_rect(None, extent, true, true, true)),
            None
        );
    }

    fn text_overlay(text: &str, input_type: i32) -> ActiveTextOverlay {
        ActiveTextOverlay {
            widget: 7,
            text: text.to_string(),
            selection: TextSelection::Cursor(text.encode_utf16().count()),
            geometry: (0, 0, 240, 46),
            input_type,
            font: TEXT_TEST_FONT,
            font_size: 25.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 1,
        }
    }

    fn host_font_available() -> bool {
        let available = crate::host_fonts::system_font().is_some();
        if !available {
            eprintln!("SKIP: no host font for the text overlay");
        }
        available
    }

    fn field_rect(overlay: &ActiveTextOverlay, extent: vk::Extent2D) -> vk::Rect2D {
        resolve_field_rect(Some(overlay.geometry), extent).expect("field on screen")
    }

    fn text_layer(overlay: &ActiveTextOverlay, rect: vk::Rect2D, order: ByteOrder) -> TextLayer {
        let request = TextRequest::of(overlay, rect, order);
        build_text_layer(&request, 1, Scroll::default())
            .expect("text layer")
            .0
    }

    fn blend_reference(pixels: &mut [u8], layer: &TextLayer, width: u32, caret_on: bool) {
        let caret = layer.caret.filter(|_| caret_on);
        let caret_ink = crate::text_layout::premultiply(layer.caret_color);
        for (index, (destination, ink)) in pixels
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(&layer.pixels)
            .enumerate()
        {
            let (x, y) = (index as u32 % width, index as u32 / width);
            let mut blended = crate::text_layout::over(*ink, *destination);
            if caret.is_some_and(|rect| x >= rect.x0 && x < rect.x1 && y >= rect.y0 && y < rect.y1)
            {
                blended = crate::text_layout::over(caret_ink, blended);
            }
            blended[3] = destination[3];
            *destination = blended;
        }
    }

    #[test]
    fn readback_memory_prefers_host_cached_types() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 5,
            ..Default::default()
        };
        props.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[2].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[3].property_flags = vk::MemoryPropertyFlags::HOST_VISIBLE
            | vk::MemoryPropertyFlags::HOST_COHERENT
            | vk::MemoryPropertyFlags::HOST_CACHED;
        props.memory_types[4].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL
            | vk::MemoryPropertyFlags::HOST_VISIBLE
            | vk::MemoryPropertyFlags::HOST_COHERENT;
        assert_eq!(
            find_host_visible_mem_type(&props, 0b11111, HostAccess::ReadWrite),
            Some(3)
        );
        assert_eq!(
            find_host_visible_mem_type(&props, 0b11111, HostAccess::Write),
            Some(2)
        );
        assert_eq!(
            find_host_visible_mem_type(&props, 0b10111, HostAccess::ReadWrite),
            Some(2)
        );
    }

    #[test]
    fn caret_blinks_every_half_second_of_wall_time() {
        for (millis, visible) in [
            (0, true),
            (208, true),
            (499, true),
            (500, false),
            (999, false),
            (1000, true),
            (1500, false),
        ] {
            assert_eq!(
                caret_visible(Duration::from_millis(millis)),
                visible,
                "caret visibility {millis} ms after the last edit"
            );
        }
    }

    #[test]
    fn text_colour_follows_the_swapchain_byte_order() {
        let color = text_rgba(0x80FF_4020u32 as i32);
        assert_eq!(color, [0xFF, 0x40, 0x20, 0x80]);
        assert_eq!(ByteOrder::Rgba.arrange(color), [0xFF, 0x40, 0x20, 0x80]);
        assert_eq!(ByteOrder::Bgra.arrange(color), [0x20, 0x40, 0xFF, 0x80]);
        assert_eq!(
            ByteOrder::of_swapchain(vk::Format::B8G8R8A8_SRGB.as_raw()),
            Some(ByteOrder::Bgra)
        );
        assert_eq!(
            ByteOrder::of_swapchain(vk::Format::R8G8B8A8_UNORM.as_raw()),
            Some(ByteOrder::Rgba)
        );
        assert_eq!(
            ByteOrder::of_swapchain(vk::Format::R16G16B16A16_SFLOAT.as_raw()),
            None
        );
    }

    #[test]
    fn a_box_partly_off_screen_is_laid_out_at_its_real_origin() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let mut overlay = text_overlay("hihihi", 0);
        overlay.geometry = (-50, -10, 300, 40);
        let rect = field_rect(&overlay, extent);
        assert_eq!(
            (
                rect.offset.x,
                rect.offset.y,
                rect.extent.width,
                rect.extent.height
            ),
            (0, 0, 250, 30)
        );
        let request = TextRequest::of(&overlay, rect, ByteOrder::Rgba);
        assert_eq!(
            request.viewport,
            Viewport {
                x: 50,
                y: 10,
                width: 250,
                height: 30
            }
        );
        assert_eq!(
            request.style.width, 300.0,
            "the layout uses the whole box, not the visible part"
        );
        if !host_font_available() {
            return;
        }
        let whole = {
            let mut shown = text_overlay("hihihi", 0);
            shown.geometry = (100, 100, 300, 40);
            text_layer(&shown, field_rect(&shown, extent), ByteOrder::Rgba)
        };
        let cropped = text_layer(&overlay, rect, ByteOrder::Rgba);
        for y in 0..30usize {
            for x in 0..250usize {
                assert_eq!(
                    cropped.pixels[y * 250 + x],
                    whole.pixels[(y + 10) * 300 + x + 50],
                    "pixel {x},{y} of the visible part"
                );
            }
        }
    }

    #[test]
    fn an_empty_focused_box_shows_a_caret() {
        if !host_font_available() {
            return;
        }
        let overlay = text_overlay("", 0);
        let rect = field_rect(
            &overlay,
            vk::Extent2D {
                width: 800,
                height: 600,
            },
        );
        let layer = text_layer(&overlay, rect, ByteOrder::Rgba);
        let caret = layer.caret.expect("an empty focused box keeps its caret");
        assert!(caret.x0 < 3 && caret.x1 > caret.x0 && caret.y1 > caret.y0);
        assert!(layer.pixels.iter().all(|pixel| pixel[3] == 0));
    }

    #[test]
    fn the_text_worker_builds_layers_off_the_present_thread_and_restarts_the_blink() {
        if !host_font_available() {
            return;
        }
        let overlay = text_overlay("worker", 0);
        let rect = field_rect(
            &overlay,
            vk::Extent2D {
                width: 800,
                height: 600,
            },
        );
        let request = TextRequest::of(&overlay, rect, ByteOrder::Rgba);
        let edited_at = Instant::now();
        let started = Instant::now();
        let (layer, caret_on) = loop {
            if let Some(ready) = current_text_layer(&request, edited_at) {
                break ready;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the worker never published a layer"
            );
            std::thread::yield_now();
        };
        assert_eq!(layer.request, request);
        assert!(caret_on, "the caret shows right after an edit");
        assert!(layer.pixels.iter().any(|pixel| pixel[3] != 0));
        let blink_at = |millis: u64| {
            current_text_layer(&request, edited_at + Duration::from_millis(millis))
                .expect("layer")
                .1
        };
        assert!(!blink_at(600), "the caret hides 500 ms after the last edit");
        assert!(blink_at(1000), "and shows again after another 500 ms");
        let edited = TextRequest::of(&text_overlay("worker!", 0), rect, ByteOrder::Rgba);
        let (_, after_edit) = current_text_layer(&edited, edited_at + Duration::from_millis(1600))
            .expect("the previous layer stays on screen while the edit is built");
        assert!(
            after_edit,
            "an edit restarts the blink with the caret shown"
        );
    }

    fn gated_engine_frame(
        gpu: &crate::graphics::headless_vulkan::HeadlessGpu,
        rendered: vk::Semaphore,
    ) -> (vk::Event, vk::CommandPool, vk::Semaphore) {
        let release = unsafe {
            gpu.device
                .create_event(&vk::EventCreateInfo::default(), None)
        }
        .expect("vkCreateEvent");
        let engine_done = gpu.semaphore();
        let pool = unsafe {
            gpu.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family),
                None,
            )
        }
        .expect("pool");
        let cmd = unsafe {
            gpu.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .expect("cmd")[0];
        unsafe {
            gpu.device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .expect("begin");
            gpu.device.cmd_wait_events(
                cmd,
                &[release],
                vk::PipelineStageFlags::HOST,
                vk::PipelineStageFlags::ALL_COMMANDS,
                &[vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::HOST_WRITE)
                    .dst_access_mask(vk::AccessFlags::MEMORY_READ)],
                &[],
                &[],
            );
            gpu.device.end_command_buffer(cmd).expect("end");
            let waits = [rendered];
            let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            let cmds = [cmd];
            let signals = [engine_done];
            gpu.device
                .queue_submit(
                    gpu.queue,
                    &[vk::SubmitInfo::default()
                        .wait_semaphores(&waits)
                        .wait_dst_stage_mask(&stages)
                        .command_buffers(&cmds)
                        .signal_semaphores(&signals)],
                    vk::Fence::null(),
                )
                .expect("engine submit");
        }
        (release, pool, engine_done)
    }

    #[test]
    fn focused_text_is_blended_on_the_gpu_without_waiting_for_the_engine_frame() {
        let Some(gpu) = headless_gpu() else {
            return;
        };
        if !host_font_available() {
            return;
        }
        let extent = vk::Extent2D {
            width: 320,
            height: 96,
        };
        for (format, order) in [
            (vk::Format::R8G8B8A8_UNORM, ByteOrder::Rgba),
            (vk::Format::B8G8R8A8_UNORM, ByteOrder::Bgra),
        ] {
            let image = gpu.image(
                format,
                extent.width,
                extent.height,
                vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
            );
            let rendered = engine_frame(&gpu, image, vk::ImageLayout::UNDEFINED);
            let mut overlay = text_overlay("Eclipse 42 ffi", 0);
            overlay.geometry = (16, 20, 240, 46);
            overlay.text_color = 0xFF00_A2FFu32 as i32;
            let rect = field_rect(&overlay, extent);
            let layer = text_layer(&overlay, rect, order);
            let mut compositor = TextCompositor::build(
                &gpu.entry,
                EngineHandles {
                    instance: gpu.instance.handle().as_raw(),
                    device: gpu.device.handle().as_raw(),
                    physical_device: gpu.physical_device.as_raw(),
                    queue_family: gpu.queue_family,
                },
                rect,
            )
            .expect("text compositor");
            let (release, pool, engine_done) = gated_engine_frame(&gpu, rendered);
            let queue = gpu.queue;
            let image_raw = image.as_raw();
            let (sent, received) = std::sync::mpsc::channel();
            let (early, batch) = std::thread::scope(|scope| {
                let compositor = &mut compositor;
                let layer = &layer;
                scope.spawn(move || {
                    let batch =
                        unsafe { compositor.draw(queue, image_raw, &[engine_done], layer, true) };
                    sent.send(batch).expect("send");
                });
                let early = received.recv_timeout(Duration::from_secs(2));
                unsafe { gpu.device.set_event(release) }.expect("vkSetEvent");
                let batch = match &early {
                    Ok(batch) => *batch,
                    Err(_) => received.recv().expect("draw result"),
                };
                (early, batch)
            });
            assert!(
                early.is_ok(),
                "the present hook waited for the engine's GPU work"
            );
            assert_eq!(batch, OverlayBatch::Queued);
            let presents =
                PresentSemaphores::create(gpu.device.clone(), 0x5E, 1).expect("semaphores");
            let ready = presents.signal(gpu.queue, 0).expect("present semaphore");
            let pixels = read_image(
                &gpu,
                image,
                vk::ImageLayout::PRESENT_SRC_KHR,
                extent,
                &[ready],
            );
            unsafe {
                gpu.device.queue_wait_idle(gpu.queue).expect("idle");
                gpu.device.destroy_command_pool(pool, None);
                gpu.device.destroy_event(release, None);
            }
            drop(compositor);
            drop(presents);

            let background = pixels[..4].to_vec();
            let mut expected = background.repeat((rect.extent.width * rect.extent.height) as usize);
            blend_reference(&mut expected, &layer, rect.extent.width, true);
            assert_ne!(expected, background.repeat(expected.len() / 4));
            assert!(
                rect_rows(&pixels, extent, rect) == expected,
                "{format:?} differs from the CPU reference blend"
            );
            assert!(outside_rect_is(&pixels, extent, rect, &background));
            let caret = layer.caret.expect("caret");
            let at = |x: u32, y: u32| {
                let i = (((rect.offset.y as u32 + y) * extent.width + rect.offset.x as u32 + x) * 4)
                    as usize;
                order.arrange([pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]])
            };
            let [r, g, b, _] = at(caret.x0, (caret.y0 + caret.y1) / 2);
            assert_eq!(
                [r, g, b],
                [0, 162, 255],
                "{format:?} caret keeps TextColor3's channels"
            );
        }
    }

    #[test]
    fn a_moved_or_resized_field_keeps_its_compositor_and_blends_at_the_new_rect() {
        let Some(gpu) = headless_gpu() else {
            return;
        };
        if !host_font_available() {
            return;
        }
        let extent = vk::Extent2D {
            width: 320,
            height: 160,
        };
        let image = gpu.image(
            vk::Format::R8G8B8A8_UNORM,
            extent.width,
            extent.height,
            vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
        );
        let mut from = vk::ImageLayout::UNDEFINED;
        let mut slot = TextCompositorSlot::Empty;
        let mut builds = 0;
        for geometry in [
            (16, 20, 200, 40),
            (40, 70, 200, 40),
            (8, 12, 300, 120),
            (30, 30, 120, 30),
        ] {
            let rendered = engine_frame(&gpu, image, from);
            from = vk::ImageLayout::PRESENT_SRC_KHR;
            let mut overlay = text_overlay("Eclipse 42", 0);
            overlay.geometry = geometry;
            overlay.text_color = 0xFF00_A2FFu32 as i32;
            let rect = field_rect(&overlay, extent);
            let layer = text_layer(&overlay, rect, ByteOrder::Rgba);
            let compositor =
                fitted_compositor(&mut slot, gpu.device.handle().as_raw(), rect, || {
                    builds += 1;
                    TextCompositor::build(
                        &gpu.entry,
                        EngineHandles {
                            instance: gpu.instance.handle().as_raw(),
                            device: gpu.device.handle().as_raw(),
                            physical_device: gpu.physical_device.as_raw(),
                            queue_family: gpu.queue_family,
                        },
                        rect,
                    )
                })
                .expect("text compositor");
            let batch =
                unsafe { compositor.draw(gpu.queue, image.as_raw(), &[rendered], &layer, true) };
            assert_eq!(batch, OverlayBatch::Queued);
            let pixels = read_image(&gpu, image, vk::ImageLayout::PRESENT_SRC_KHR, extent, &[]);
            let background = pixels[..4].to_vec();
            let mut expected = background.repeat((rect.extent.width * rect.extent.height) as usize);
            blend_reference(&mut expected, &layer, rect.extent.width, true);
            assert!(
                rect_rows(&pixels, extent, rect) == expected,
                "the field at {geometry:?} differs from the CPU reference blend"
            );
            assert!(outside_rect_is(&pixels, extent, rect, &background));
        }
        assert_eq!(
            builds, 1,
            "moving or resizing the field rebuilt the compositor"
        );
    }

    static HOOK_TEST_LOCK: Mutex<()> = Mutex::new(());
    static STUB_DESTROYED: AtomicU64 = AtomicU64::new(0);
    static STUB_CREATED_PRESENT_MODE: AtomicU32 = AtomicU32::new(u32::MAX);
    static STUB_CREATED_WIDTH: AtomicU32 = AtomicU32::new(0);
    static STUB_CREATED_USAGE: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn stub_function() {}

    unsafe extern "system" fn stub_get_device_proc_addr(
        _device: vk::Device,
        _name: *const c_char,
    ) -> vk::PFN_vkVoidFunction {
        Some(stub_function)
    }

    unsafe extern "system" fn stub_destroy_swapchain(
        _device: vk::Device,
        swapchain: vk::SwapchainKHR,
        _allocator: *const vk::AllocationCallbacks<'_>,
    ) {
        STUB_DESTROYED.store(swapchain.as_raw(), Ordering::SeqCst);
    }

    unsafe extern "system" fn stub_create_swapchain(
        _device: vk::Device,
        info: *const vk::SwapchainCreateInfoKHR<'_>,
        _allocator: *const vk::AllocationCallbacks<'_>,
        swapchain: *mut vk::SwapchainKHR,
    ) -> vk::Result {
        let info = unsafe { &*info };
        STUB_CREATED_PRESENT_MODE.store(info.present_mode.as_raw() as u32, Ordering::SeqCst);
        STUB_CREATED_WIDTH.store(info.image_extent.width, Ordering::SeqCst);
        STUB_CREATED_USAGE.store(info.image_usage.as_raw(), Ordering::SeqCst);
        unsafe { *swapchain = vk::SwapchainKHR::from_raw(CREATED_SWAPCHAIN) };
        vk::Result::SUCCESS
    }

    fn set_state(swapchain: u64, images: Vec<u64>) {
        let mut st = STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.swapchain = swapchain;
        st.images = images;
    }

    fn state_swapchain() -> (u64, usize) {
        let st = STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (st.swapchain, st.images.len())
    }

    static STUB_DEVICE_REQUEST: AtomicU64 = AtomicU64::new(0);

    unsafe extern "system" fn stub_create_device(
        _physical_device: vk::PhysicalDevice,
        info: *const vk::DeviceCreateInfo<'_>,
        _allocator: *const vk::AllocationCallbacks<'_>,
        device: *mut vk::Device,
    ) -> vk::Result {
        STUB_DEVICE_REQUEST.store(info as u64, Ordering::SeqCst);
        unsafe { *device = vk::Device::from_raw(0xD) };
        vk::Result::SUCCESS
    }

    #[test]
    fn the_engine_device_is_created_exactly_as_the_engine_asks() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved =
            HOST_CREATE_DEVICE.swap(stub_create_device as *const () as u64, Ordering::SeqCst);
        let saved_device = STATE.lock().map(|state| state.device).unwrap_or(0);
        let saved_physical = PHYSICAL_DEVICE.load(Ordering::SeqCst);
        let saved_family = QUEUE_FAMILY.load(Ordering::SeqCst);
        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(2)
            .queue_priorities(&priorities)];
        let request = vk::DeviceCreateInfo::default().queue_create_infos(&queues);
        let mut device = vk::Device::null();

        let created = unsafe {
            eclipse_vk_create_device(
                vk::PhysicalDevice::from_raw(0x7),
                &request,
                std::ptr::null(),
                &mut device,
            )
        };
        let forwarded = STUB_DEVICE_REQUEST.load(Ordering::SeqCst);
        let captured = (
            STATE.lock().map(|state| state.device).unwrap_or(0),
            PHYSICAL_DEVICE.load(Ordering::SeqCst),
            QUEUE_FAMILY.load(Ordering::SeqCst),
        );

        if let Ok(mut state) = STATE.lock() {
            state.device = saved_device;
        }
        PHYSICAL_DEVICE.store(saved_physical, Ordering::SeqCst);
        QUEUE_FAMILY.store(saved_family, Ordering::SeqCst);
        HOST_CREATE_DEVICE.store(saved, Ordering::SeqCst);

        assert_eq!(created, vk::Result::SUCCESS);
        assert_eq!(device.as_raw(), 0xD);
        assert_eq!(
            forwarded, &raw const request as u64,
            "the host receives the engine's own device request, with no queue added"
        );
        assert_eq!(captured, (0xD, 0x7, 2));
    }

    static STUB_PRESENTED_FRAME: AtomicU64 = AtomicU64::new(0);

    unsafe extern "system" fn stub_present_frame(
        _queue: vk::Queue,
        info: *const vk::PresentInfoKHR<'_>,
    ) -> vk::Result {
        STUB_PRESENTED_FRAME.store(info as u64, Ordering::SeqCst);
        vk::Result::SUBOPTIMAL_KHR
    }

    #[test]
    fn an_engine_frame_with_nothing_to_draw_reaches_the_host_untouched() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved =
            HOST_QUEUE_PRESENT.swap(stub_present_frame as *const () as u64, Ordering::SeqCst);
        set_state(0xA, vec![0x1, 0x2]);
        if let Ok(mut state) = STATE.lock() {
            state.format = vk::Format::B8G8R8A8_UNORM.as_raw();
        }
        let waits = [vk::Semaphore::from_raw(0x5)];
        let swapchains = [vk::SwapchainKHR::from_raw(0xA)];
        let indices = [1];
        let frame = vk::PresentInfoKHR::default()
            .wait_semaphores(&waits)
            .swapchains(&swapchains)
            .image_indices(&indices);

        let presented = unsafe { eclipse_vk_queue_present_khr(vk::Queue::from_raw(0x9), &frame) };
        let forwarded = STUB_PRESENTED_FRAME.load(Ordering::SeqCst);

        if let Ok(mut state) = STATE.lock() {
            state.format = 0;
        }
        set_state(0, Vec::new());
        HOST_QUEUE_PRESENT.store(saved, Ordering::SeqCst);

        assert_eq!(presented, vk::Result::SUBOPTIMAL_KHR);
        assert_eq!(
            forwarded, &raw const frame as u64,
            "the host presents the engine's own frame, waiting on the engine's semaphores only"
        );
    }

    #[test]
    fn device_proc_addr_interposes_swapchain_destruction() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = HOST_GDPA.swap(
            stub_get_device_proc_addr as *const () as u64,
            Ordering::SeqCst,
        );
        let saved_destroy = HOST_DESTROY_SWAPCHAIN.load(Ordering::SeqCst);
        let hooked = unsafe {
            eclipse_vk_get_device_proc_addr(vk::Device::null(), c"vkDestroySwapchainKHR".as_ptr())
        };
        let host = HOST_DESTROY_SWAPCHAIN.swap(saved_destroy, Ordering::SeqCst);
        HOST_GDPA.store(saved, Ordering::SeqCst);

        assert_eq!(
            hooked.map(|f| f as usize),
            Some(eclipse_vk_destroy_swapchain_khr as *const () as usize)
        );
        assert_eq!(host, stub_function as *const () as u64);
    }

    #[test]
    fn destroying_the_tracked_swapchain_forgets_it() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = HOST_DESTROY_SWAPCHAIN
            .swap(stub_destroy_swapchain as *const () as u64, Ordering::SeqCst);
        set_state(0xA, vec![1, 2, 3]);

        unsafe {
            eclipse_vk_destroy_swapchain_khr(
                vk::Device::null(),
                vk::SwapchainKHR::from_raw(0xA),
                std::ptr::null(),
            )
        };
        let retired = state_swapchain();
        let destroyed = STUB_DESTROYED.load(Ordering::SeqCst);

        set_state(0xB, vec![4, 5]);
        unsafe {
            eclipse_vk_destroy_swapchain_khr(
                vk::Device::null(),
                vk::SwapchainKHR::from_raw(0xA),
                std::ptr::null(),
            )
        };
        let replaced = state_swapchain();

        set_state(0, Vec::new());
        HOST_DESTROY_SWAPCHAIN.store(saved, Ordering::SeqCst);

        assert_eq!(
            retired,
            (0, 0),
            "a destroyed swapchain is no longer a present target"
        );
        assert_eq!(destroyed, 0xA);
        assert_eq!(
            replaced,
            (0xB, 2),
            "retiring an old swapchain keeps its replacement"
        );
    }

    #[test]
    fn swapchain_creation_forwards_the_engine_request_and_tracks_the_swapchain() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved =
            HOST_CREATE_SWAPCHAIN.swap(stub_create_swapchain as *const () as u64, Ordering::SeqCst);
        let info = vk::SwapchainCreateInfoKHR::default()
            .image_extent(vk::Extent2D {
                width: 640,
                height: 480,
            })
            .present_mode(vk::PresentModeKHR::FIFO_RELAXED);
        let mut swapchain = vk::SwapchainKHR::null();
        let created = unsafe {
            eclipse_vk_create_swapchain_khr(
                vk::Device::null(),
                &info,
                std::ptr::null(),
                &mut swapchain,
            )
        };
        let tracked = state_swapchain();
        let forwarded = (
            STUB_CREATED_PRESENT_MODE.load(Ordering::SeqCst),
            STUB_CREATED_WIDTH.load(Ordering::SeqCst),
        );

        set_state(0, Vec::new());
        HOST_CREATE_SWAPCHAIN.store(saved, Ordering::SeqCst);

        assert_eq!(created, vk::Result::SUCCESS);
        assert_eq!(tracked, (0xB, 0));
        assert_eq!(
            forwarded,
            (vk::PresentModeKHR::FIFO_RELAXED.as_raw() as u32, 640),
            "the host receives the engine's swapchain request"
        );
    }

    unsafe extern "system" fn stub_surface_supporting_copies(
        _physical_device: vk::PhysicalDevice,
        _surface: vk::SurfaceKHR,
        caps: *mut vk::SurfaceCapabilitiesKHR,
    ) -> vk::Result {
        unsafe {
            *caps = vk::SurfaceCapabilitiesKHR::default()
                .supported_usage_flags(vk::ImageUsageFlags::COLOR_ATTACHMENT | OVERLAY_COPY_USAGE)
        };
        vk::Result::SUCCESS
    }

    unsafe extern "system" fn stub_surface_without_copies(
        _physical_device: vk::PhysicalDevice,
        _surface: vk::SurfaceKHR,
        caps: *mut vk::SurfaceCapabilitiesKHR,
    ) -> vk::Result {
        unsafe {
            *caps = vk::SurfaceCapabilitiesKHR::default()
                .supported_usage_flags(vk::ImageUsageFlags::COLOR_ATTACHMENT)
        };
        vk::Result::SUCCESS
    }

    fn host_swapchain_usage(
        capabilities: vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR,
    ) -> (vk::ImageUsageFlags, SwapchainCopies) {
        super::super::vulkan_wsi::use_host_surface_capabilities_for_test(Some(capabilities));
        STUB_CREATED_USAGE.store(0, Ordering::SeqCst);
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(vk::SurfaceKHR::from_raw(ENGINE_SURFACE))
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT);
        let mut swapchain = vk::SwapchainKHR::null();
        let created = unsafe {
            eclipse_vk_create_swapchain_khr(
                vk::Device::null(),
                &info,
                std::ptr::null(),
                &mut swapchain,
            )
        };
        super::super::vulkan_wsi::use_host_surface_capabilities_for_test(None);
        assert_eq!(created, vk::Result::SUCCESS);
        let copies = STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .copies;
        (
            vk::ImageUsageFlags::from_raw(STUB_CREATED_USAGE.load(Ordering::SeqCst)),
            copies,
        )
    }

    #[test]
    fn engine_swapchains_allow_the_overlay_copies_where_the_surface_supports_them() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_create =
            HOST_CREATE_SWAPCHAIN.swap(stub_create_swapchain as *const () as u64, Ordering::SeqCst);
        let saved_physical = PHYSICAL_DEVICE.swap(0x7, Ordering::SeqCst);

        let supported = host_swapchain_usage(stub_surface_supporting_copies);
        let unsupported = host_swapchain_usage(stub_surface_without_copies);

        set_state(0, Vec::new());
        PHYSICAL_DEVICE.store(saved_physical, Ordering::SeqCst);
        HOST_CREATE_SWAPCHAIN.store(saved_create, Ordering::SeqCst);

        assert_eq!(
            supported,
            (
                vk::ImageUsageFlags::COLOR_ATTACHMENT | OVERLAY_COPY_USAGE,
                SwapchainCopies::Supported
            ),
            "the text overlay copies out of and back into the engine's images"
        );
        assert_eq!(
            unsupported,
            (
                vk::ImageUsageFlags::COLOR_ATTACHMENT,
                SwapchainCopies::Unsupported
            ),
            "usage the surface does not support never reaches the host, and the overlay skips it"
        );
    }

    const SUBOPTIMAL_SWAPCHAIN: u64 = 0x50;
    const OUT_OF_DATE_SWAPCHAIN: u64 = 0x51;
    const RETIRED_SWAPCHAIN: u64 = 0x52;
    const CREATED_SWAPCHAIN: u64 = 0xB;
    const OUT_OF_DATE_AFTER_PULL_SWAPCHAIN: u64 = 0x53;
    const ACQUIRED_IMAGE: u32 = 2;

    static STUB_ENGINE_ACQUIRE: AtomicI32 = AtomicI32::new(0);
    static STUB_ENGINE_PRESENT: AtomicI32 = AtomicI32::new(0);

    fn stub_host_acquire(swapchain: vk::SwapchainKHR, image_index: *mut u32) -> vk::Result {
        match swapchain.as_raw() {
            SUBOPTIMAL_SWAPCHAIN => {
                unsafe { *image_index = ACQUIRED_IMAGE };
                vk::Result::SUBOPTIMAL_KHR
            }
            OUT_OF_DATE_SWAPCHAIN | RETIRED_SWAPCHAIN => vk::Result::ERROR_OUT_OF_DATE_KHR,
            CREATED_SWAPCHAIN => {
                let reported = vk::Result::from_raw(STUB_ENGINE_ACQUIRE.load(Ordering::SeqCst));
                if matches!(reported, vk::Result::SUCCESS | vk::Result::SUBOPTIMAL_KHR) {
                    unsafe { *image_index = ACQUIRED_IMAGE };
                }
                reported
            }
            OUT_OF_DATE_AFTER_PULL_SWAPCHAIN => {
                unsafe { *image_index = ACQUIRED_IMAGE };
                vk::Result::ERROR_OUT_OF_DATE_KHR
            }
            _ => vk::Result::ERROR_SURFACE_LOST_KHR,
        }
    }

    unsafe extern "system" fn stub_acquire_image(
        _device: vk::Device,
        swapchain: vk::SwapchainKHR,
        _timeout: u64,
        _semaphore: vk::Semaphore,
        _fence: vk::Fence,
        image_index: *mut u32,
    ) -> vk::Result {
        stub_host_acquire(swapchain, image_index)
    }

    unsafe extern "system" fn stub_acquire_image2(
        _device: vk::Device,
        info: *const vk::AcquireNextImageInfoKHR<'_>,
        image_index: *mut u32,
    ) -> vk::Result {
        stub_host_acquire(unsafe { (*info).swapchain }, image_index)
    }

    fn engine_acquires(swapchain: u64) -> [(vk::Result, u32); 2] {
        let swapchain = vk::SwapchainKHR::from_raw(swapchain);
        let mut first = u32::MAX;
        let acquired = unsafe {
            eclipse_vk_acquire_next_image_khr(
                vk::Device::null(),
                swapchain,
                u64::MAX,
                vk::Semaphore::null(),
                vk::Fence::null(),
                &mut first,
            )
        };
        let mut second = u32::MAX;
        let info = vk::AcquireNextImageInfoKHR::default()
            .swapchain(swapchain)
            .timeout(u64::MAX)
            .device_mask(1);
        let acquired2 =
            unsafe { eclipse_vk_acquire_next_image2_khr(vk::Device::null(), &info, &mut second) };
        [(acquired, first), (acquired2, second)]
    }

    #[test]
    fn a_suboptimal_host_swapchain_hands_the_engine_its_image_like_android() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved =
            HOST_ACQUIRE_NEXT_IMAGE.swap(stub_acquire_image as *const () as u64, Ordering::SeqCst);
        let saved2 = HOST_ACQUIRE_NEXT_IMAGE2
            .swap(stub_acquire_image2 as *const () as u64, Ordering::SeqCst);

        let suboptimal = engine_acquires(SUBOPTIMAL_SWAPCHAIN);
        let out_of_date = engine_acquires(OUT_OF_DATE_SWAPCHAIN);

        HOST_ACQUIRE_NEXT_IMAGE.store(saved, Ordering::SeqCst);
        HOST_ACQUIRE_NEXT_IMAGE2.store(saved2, Ordering::SeqCst);

        assert_eq!(
            suboptimal,
            [(vk::Result::SUCCESS, ACQUIRED_IMAGE); 2],
            "an image acquired from a suboptimal swapchain reaches the engine as a plain success"
        );
        assert_eq!(
            out_of_date,
            [(vk::Result::ERROR_OUT_OF_DATE_KHR, u32::MAX); 2],
            "an out-of-date swapchain still reports the error and acquires no image"
        );
    }

    #[test]
    fn a_failed_host_acquire_leaves_the_engine_image_index_alone_like_android() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved =
            HOST_ACQUIRE_NEXT_IMAGE.swap(stub_acquire_image as *const () as u64, Ordering::SeqCst);
        let saved2 = HOST_ACQUIRE_NEXT_IMAGE2
            .swap(stub_acquire_image2 as *const () as u64, Ordering::SeqCst);

        let failed = engine_acquires(OUT_OF_DATE_AFTER_PULL_SWAPCHAIN);

        HOST_ACQUIRE_NEXT_IMAGE.store(saved, Ordering::SeqCst);
        HOST_ACQUIRE_NEXT_IMAGE2.store(saved2, Ordering::SeqCst);

        assert_eq!(
            failed,
            [(vk::Result::ERROR_OUT_OF_DATE_KHR, u32::MAX); 2],
            "an acquire that fails after the host wrote an index keeps the engine's own index, \
             so the engine never presents an image it did not acquire"
        );
    }

    const ENGINE_SURFACE: u64 = 0x5F;
    const OTHER_SURFACE: u64 = 0x60;
    const HOST_MIN_IMAGE_COUNT: u32 = 3;
    const HOST_EXTENT: vk::Extent2D = vk::Extent2D {
        width: 1280,
        height: 720,
    };

    unsafe extern "system" fn stub_surface_capabilities(
        _physical_device: vk::PhysicalDevice,
        _surface: vk::SurfaceKHR,
        caps: *mut vk::SurfaceCapabilitiesKHR,
    ) -> vk::Result {
        unsafe {
            *caps = vk::SurfaceCapabilitiesKHR::default()
                .min_image_count(HOST_MIN_IMAGE_COUNT)
                .current_extent(HOST_EXTENT)
        };
        vk::Result::SUCCESS
    }

    unsafe extern "system" fn stub_engine_present(
        _queue: vk::Queue,
        _info: *const vk::PresentInfoKHR<'_>,
    ) -> vk::Result {
        vk::Result::from_raw(STUB_ENGINE_PRESENT.load(Ordering::SeqCst))
    }

    fn engine_swapchain_on(surface: u64) -> vk::SwapchainKHR {
        let info = vk::SwapchainCreateInfoKHR::default().surface(vk::SurfaceKHR::from_raw(surface));
        let mut swapchain = vk::SwapchainKHR::null();
        let created = unsafe {
            eclipse_vk_create_swapchain_khr(
                vk::Device::null(),
                &info,
                std::ptr::null(),
                &mut swapchain,
            )
        };
        assert_eq!(created, vk::Result::SUCCESS);
        swapchain
    }

    fn engine_presents(swapchain: vk::SwapchainKHR) -> vk::Result {
        let swapchains = [swapchain];
        let image_indices = [0];
        let info = vk::PresentInfoKHR::default()
            .swapchains(&swapchains)
            .image_indices(&image_indices);
        unsafe { eclipse_vk_queue_present_khr(vk::Queue::null(), &info) }
    }

    fn engine_queries_surface(surface: u64) -> (vk::Result, vk::SurfaceCapabilitiesKHR) {
        let mut caps = vk::SurfaceCapabilitiesKHR::default();
        let queried = unsafe {
            super::super::vulkan_wsi::eclipse_vk_get_physical_device_surface_capabilities_khr(
                vk::PhysicalDevice::null(),
                vk::SurfaceKHR::from_raw(surface),
                &mut caps,
            )
        };
        (queried, caps)
    }

    struct ScriptedHost {
        saved: [(&'static AtomicU64, u64); 5],
    }

    impl ScriptedHost {
        fn install() -> Self {
            let stub = |slot: &'static AtomicU64, f: *const ()| {
                (slot, slot.swap(f as u64, Ordering::SeqCst))
            };
            super::super::vulkan_wsi::use_host_surface_capabilities_for_test(Some(
                stub_surface_capabilities,
            ));
            let host = Self {
                saved: [
                    stub(&HOST_ACQUIRE_NEXT_IMAGE, stub_acquire_image as *const ()),
                    stub(&HOST_ACQUIRE_NEXT_IMAGE2, stub_acquire_image2 as *const ()),
                    stub(&HOST_CREATE_SWAPCHAIN, stub_create_swapchain as *const ()),
                    stub(&HOST_DESTROY_SWAPCHAIN, stub_destroy_swapchain as *const ()),
                    stub(&HOST_QUEUE_PRESENT, stub_engine_present as *const ()),
                ],
            };
            host.reports(
                vk::Result::ERROR_OUT_OF_DATE_KHR,
                vk::Result::ERROR_OUT_OF_DATE_KHR,
            );
            host
        }

        fn reports(&self, acquire: vk::Result, present: vk::Result) {
            STUB_ENGINE_ACQUIRE.store(acquire.as_raw(), Ordering::SeqCst);
            STUB_ENGINE_PRESENT.store(present.as_raw(), Ordering::SeqCst);
        }
    }

    impl Drop for ScriptedHost {
        fn drop(&mut self) {
            for (slot, saved) in self.saved {
                slot.store(saved, Ordering::SeqCst);
            }
            super::super::vulkan_wsi::use_host_surface_capabilities_for_test(None);
            set_state(0, Vec::new());
        }
    }

    #[test]
    fn an_out_of_date_engine_swapchain_reports_its_surface_lost_to_the_next_query() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _host = ScriptedHost::install();
        let swapchain = engine_swapchain_on(ENGINE_SURFACE);

        let retired = engine_acquires(RETIRED_SWAPCHAIN);
        let (after_retired, _) = engine_queries_surface(ENGINE_SURFACE);
        let current = engine_acquires(swapchain.as_raw());
        let (other_surface, _) = engine_queries_surface(OTHER_SURFACE);
        let (first, lost_caps) = engine_queries_surface(ENGINE_SURFACE);
        let (second, _) = engine_queries_surface(ENGINE_SURFACE);
        let flagged = engine_acquires(swapchain.as_raw());
        let _replacement = engine_swapchain_on(ENGINE_SURFACE);
        let (after_replacement, _) = engine_queries_surface(ENGINE_SURFACE);

        assert_eq!(
            [retired, current],
            [[(vk::Result::ERROR_OUT_OF_DATE_KHR, u32::MAX); 2]; 2],
            "the engine still sees the out-of-date result of its acquire"
        );
        assert_eq!(
            after_retired,
            vk::Result::SUCCESS,
            "a swapchain the engine already replaced leaves its surface alone"
        );
        assert_eq!(other_surface, vk::Result::SUCCESS);
        assert_eq!(
            [first, second],
            [vk::Result::ERROR_SURFACE_LOST_KHR, vk::Result::SUCCESS],
            "the engine's next query of the surface reports it lost once, so the engine drops \
             the swapchain and builds a new one"
        );
        assert_eq!(
            (lost_caps.min_image_count, lost_caps.current_extent),
            (HOST_MIN_IMAGE_COUNT, HOST_EXTENT),
            "the surface-lost answer still carries the host's capabilities, because one of the \
             engine's queries builds a swapchain from them without checking the result"
        );
        assert_eq!(flagged, [(vk::Result::ERROR_OUT_OF_DATE_KHR, u32::MAX); 2]);
        assert_eq!(
            after_replacement,
            vk::Result::SUCCESS,
            "a swapchain the engine builds before its next query clears the pending answer"
        );
    }

    #[test]
    fn an_out_of_date_present_reports_the_surface_lost_unless_the_engine_dropped_the_swapchain() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _host = ScriptedHost::install();

        let dropped = engine_swapchain_on(ENGINE_SURFACE);
        let dropped_present = engine_presents(dropped);
        unsafe { eclipse_vk_destroy_swapchain_khr(vk::Device::null(), dropped, std::ptr::null()) };
        let (after_drop, _) = engine_queries_surface(ENGINE_SURFACE);

        let current = engine_swapchain_on(ENGINE_SURFACE);
        let current_present = engine_presents(current);
        let (after_present, _) = engine_queries_surface(ENGINE_SURFACE);

        assert_eq!(
            [dropped_present, current_present],
            [vk::Result::ERROR_OUT_OF_DATE_KHR; 2]
        );
        assert_eq!(
            after_drop,
            vk::Result::SUCCESS,
            "a swapchain the engine destroyed itself needs no rebuild"
        );
        assert_eq!(
            after_present,
            vk::Result::ERROR_SURFACE_LOST_KHR,
            "an out-of-date present makes the engine rebuild its swapchain"
        );
    }

    #[test]
    fn acquire_failures_other_than_out_of_date_leave_the_surface_alone() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = ScriptedHost::install();
        let swapchain = engine_swapchain_on(ENGINE_SURFACE);
        let failures = [
            vk::Result::ERROR_SURFACE_LOST_KHR,
            vk::Result::TIMEOUT,
            vk::Result::NOT_READY,
        ];

        let answers = failures.map(|failure| {
            host.reports(failure, vk::Result::SUCCESS);
            let acquired = engine_acquires(swapchain.as_raw());
            let (queried, _) = engine_queries_surface(ENGINE_SURFACE);
            (acquired, queried)
        });

        assert_eq!(
            answers,
            failures.map(|failure| ([(failure, u32::MAX); 2], vk::Result::SUCCESS)),
            "the engine sees each failure as the host reported it, and its surface still answers \
             normally, because only an out-of-date swapchain needs a rebuild"
        );
    }

    fn engine_frame_on(swapchain: vk::SwapchainKHR) -> ([(vk::Result, u32); 2], vk::Result) {
        (
            engine_acquires(swapchain.as_raw()),
            engine_presents(swapchain),
        )
    }

    #[test]
    fn a_swapchain_that_turns_suboptimal_asks_the_engine_for_one_rebuild() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = ScriptedHost::install();
        let swapchain = engine_swapchain_on(ENGINE_SURFACE);

        host.reports(vk::Result::SUCCESS, vk::Result::SUCCESS);
        let optimal = [engine_frame_on(swapchain), engine_frame_on(swapchain)];
        let (while_optimal, _) = engine_queries_surface(ENGINE_SURFACE);
        host.reports(vk::Result::SUBOPTIMAL_KHR, vk::Result::SUCCESS);
        let suboptimal = engine_frame_on(swapchain);
        let (first, _) = engine_queries_surface(ENGINE_SURFACE);
        host.reports(vk::Result::SUCCESS, vk::Result::SUCCESS);
        let _optimal_again = [engine_frame_on(swapchain), engine_frame_on(swapchain)];
        host.reports(vk::Result::SUBOPTIMAL_KHR, vk::Result::SUCCESS);
        let _suboptimal_again = [engine_frame_on(swapchain), engine_frame_on(swapchain)];
        let (second, _) = engine_queries_surface(ENGINE_SURFACE);

        let frame = (
            [(vk::Result::SUCCESS, ACQUIRED_IMAGE); 2],
            vk::Result::SUCCESS,
        );
        assert_eq!(optimal, [frame; 2]);
        assert_eq!(
            suboptimal, frame,
            "the engine acquires its image from a suboptimal swapchain as on Android"
        );
        assert_eq!(
            [while_optimal, first, second],
            [
                vk::Result::SUCCESS,
                vk::Result::ERROR_SURFACE_LOST_KHR,
                vk::Result::SUCCESS,
            ],
            "a swapchain that turns suboptimal after optimal frames makes the engine rebuild it \
             once, so the host can allocate buffers that fit the surface, and never again while \
             the engine keeps it, even after more optimal frames, so the engine cannot rebuild \
             in a loop"
        );
    }

    #[test]
    fn a_swapchain_suboptimal_from_its_first_frame_never_asks_for_a_rebuild() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = ScriptedHost::install();
        let swapchain = engine_swapchain_on(ENGINE_SURFACE);

        host.reports(vk::Result::SUBOPTIMAL_KHR, vk::Result::SUCCESS);
        let _born_suboptimal = engine_frame_on(swapchain);
        host.reports(vk::Result::SUCCESS, vk::Result::SUCCESS);
        let _optimal = [engine_frame_on(swapchain), engine_frame_on(swapchain)];
        host.reports(vk::Result::SUBOPTIMAL_KHR, vk::Result::SUBOPTIMAL_KHR);
        let _suboptimal_again = engine_frame_on(swapchain);
        let (queried, _) = engine_queries_surface(ENGINE_SURFACE);

        assert_eq!(
            queried,
            vk::Result::SUCCESS,
            "a swapchain the host reports suboptimal from its first frame never asks for a \
             rebuild, so the engine cannot rebuild in a loop"
        );
    }

    #[test]
    fn a_suboptimal_present_after_an_optimal_frame_asks_the_engine_for_a_rebuild() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = ScriptedHost::install();
        let swapchain = engine_swapchain_on(ENGINE_SURFACE);

        host.reports(vk::Result::SUCCESS, vk::Result::SUCCESS);
        let _optimal = engine_frame_on(swapchain);
        host.reports(vk::Result::SUCCESS, vk::Result::SUBOPTIMAL_KHR);
        let presented = engine_frame_on(swapchain);
        let (queried, _) = engine_queries_surface(ENGINE_SURFACE);

        assert_eq!(
            presented,
            (
                [(vk::Result::SUCCESS, ACQUIRED_IMAGE); 2],
                vk::Result::SUBOPTIMAL_KHR
            ),
            "the engine sees a suboptimal present as Android reports it"
        );
        assert_eq!(
            queried,
            vk::Result::ERROR_SURFACE_LOST_KHR,
            "a driver that reports suboptimal only from present still gets a rebuilt swapchain"
        );
    }

    fn presented_optimally(host: &ScriptedHost, swapchain: vk::SwapchainKHR) {
        host.reports(vk::Result::SUCCESS, vk::Result::SUCCESS);
        let _optimal = engine_frame_on(swapchain);
    }

    fn suboptimal_from_creation(host: &ScriptedHost, swapchain: vk::SwapchainKHR) {
        host.reports(vk::Result::SUBOPTIMAL_KHR, vk::Result::SUBOPTIMAL_KHR);
        let _suboptimal = engine_frame_on(swapchain);
    }

    fn rebuild_asked_for_turning_suboptimal(host: &ScriptedHost, swapchain: vk::SwapchainKHR) {
        presented_optimally(host, swapchain);
        host.reports(vk::Result::SUBOPTIMAL_KHR, vk::Result::SUCCESS);
        let _suboptimal = engine_frame_on(swapchain);
        let _rebuild_asked = engine_queries_surface(ENGINE_SURFACE);
    }

    fn out_of_date_on_acquire(host: &ScriptedHost, swapchain: vk::SwapchainKHR) {
        host.reports(
            vk::Result::ERROR_OUT_OF_DATE_KHR,
            vk::Result::ERROR_OUT_OF_DATE_KHR,
        );
        let _out_of_date = engine_acquires(swapchain.as_raw());
    }

    fn out_of_date_on_present(host: &ScriptedHost, swapchain: vk::SwapchainKHR) {
        host.reports(vk::Result::SUCCESS, vk::Result::ERROR_OUT_OF_DATE_KHR);
        let _out_of_date = engine_frame_on(swapchain);
    }

    #[test]
    fn an_out_of_date_swapchain_is_rebuilt_whatever_the_host_reported_before() {
        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = ScriptedHost::install();
        let histories: [fn(&ScriptedHost, vk::SwapchainKHR); 3] = [
            presented_optimally,
            suboptimal_from_creation,
            rebuild_asked_for_turning_suboptimal,
        ];
        let failures: [fn(&ScriptedHost, vk::SwapchainKHR); 2] =
            [out_of_date_on_acquire, out_of_date_on_present];

        let answers = histories.map(|history| {
            failures.map(|failure| {
                let swapchain = engine_swapchain_on(ENGINE_SURFACE);
                history(&host, swapchain);
                failure(&host, swapchain);
                engine_queries_surface(ENGINE_SURFACE).0
            })
        });

        assert_eq!(
            answers,
            [[vk::Result::ERROR_SURFACE_LOST_KHR; 2]; 3],
            "an out-of-date swapchain cannot present, so the engine rebuilds it whether the game \
             was running on it when a monitor changed, it was suboptimal from creation, or it \
             already asked for a rebuild"
        );
    }

    static STUB_QUERIED_PHYSICAL_DEVICE: AtomicU64 = AtomicU64::new(0);

    unsafe fn stub_host_present_modes(
        physical_device: vk::PhysicalDevice,
        modes: &[vk::PresentModeKHR],
        count: *mut u32,
        out: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        STUB_QUERIED_PHYSICAL_DEVICE.store(physical_device.as_raw(), Ordering::SeqCst);
        let count = unsafe { &mut *count };
        if out.is_null() {
            *count = modes.len() as u32;
            return vk::Result::SUCCESS;
        }
        let written = modes.len().min(*count as usize);
        unsafe { std::ptr::copy_nonoverlapping(modes.as_ptr(), out, written) };
        *count = written as u32;
        vk::Result::SUCCESS
    }

    unsafe extern "system" fn stub_host_without_tearing(
        physical_device: vk::PhysicalDevice,
        _surface: vk::SurfaceKHR,
        count: *mut u32,
        out: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        let modes = [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::FIFO];
        unsafe { stub_host_present_modes(physical_device, &modes, count, out) }
    }

    unsafe extern "system" fn stub_host_with_tearing(
        physical_device: vk::PhysicalDevice,
        _surface: vk::SurfaceKHR,
        count: *mut u32,
        out: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        let modes = [
            vk::PresentModeKHR::MAILBOX,
            vk::PresentModeKHR::FIFO,
            vk::PresentModeKHR::IMMEDIATE,
        ];
        unsafe { stub_host_present_modes(physical_device, &modes, count, out) }
    }

    fn engine_present_modes(physical_device: u64) -> Vec<vk::PresentModeKHR> {
        let physical_device = vk::PhysicalDevice::from_raw(physical_device);
        let query =
            super::super::vulkan_wsi::eclipse_vk_get_physical_device_surface_present_modes_khr;
        let mut count = 0;
        let counted = unsafe {
            query(
                physical_device,
                vk::SurfaceKHR::null(),
                &mut count,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(counted, vk::Result::SUCCESS);
        let mut modes = vec![vk::PresentModeKHR::FIFO_RELAXED; count as usize];
        let listed = unsafe {
            query(
                physical_device,
                vk::SurfaceKHR::null(),
                &mut count,
                modes.as_mut_ptr(),
            )
        };
        assert_eq!(listed, vk::Result::SUCCESS);
        modes
    }

    fn host_swapchain_mode(requested: vk::PresentModeKHR) -> vk::PresentModeKHR {
        STUB_CREATED_PRESENT_MODE.store(u32::MAX, Ordering::SeqCst);
        let info = vk::SwapchainCreateInfoKHR::default().present_mode(requested);
        let mut swapchain = vk::SwapchainKHR::null();
        let created = unsafe {
            eclipse_vk_create_swapchain_khr(
                vk::Device::null(),
                &info,
                std::ptr::null(),
                &mut swapchain,
            )
        };
        assert_eq!(created, vk::Result::SUCCESS);
        vk::PresentModeKHR::from_raw(STUB_CREATED_PRESENT_MODE.load(Ordering::SeqCst) as i32)
    }

    #[test]
    fn immediate_swapchains_follow_the_host_modes_of_the_device_the_engine_asked_about() {
        use super::super::vulkan_wsi::use_host_present_modes_for_test;

        let _serial = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_create =
            HOST_CREATE_SWAPCHAIN.swap(stub_create_swapchain as *const () as u64, Ordering::SeqCst);
        let saved_physical = PHYSICAL_DEVICE.swap(0, Ordering::SeqCst);

        use_host_present_modes_for_test(Some(stub_host_without_tearing));
        let before_asking = host_swapchain_mode(vk::PresentModeKHR::IMMEDIATE);
        let advertised = engine_present_modes(0x5);
        let without_tearing = host_swapchain_mode(vk::PresentModeKHR::IMMEDIATE);
        let queried = STUB_QUERIED_PHYSICAL_DEVICE.load(Ordering::SeqCst);
        let fifo = host_swapchain_mode(vk::PresentModeKHR::FIFO);

        use_host_present_modes_for_test(Some(stub_host_with_tearing));
        engine_present_modes(0x6);
        let with_tearing = host_swapchain_mode(vk::PresentModeKHR::IMMEDIATE);

        use_host_present_modes_for_test(None);
        set_state(0, Vec::new());
        PHYSICAL_DEVICE.store(saved_physical, Ordering::SeqCst);
        HOST_CREATE_SWAPCHAIN.store(saved_create, Ordering::SeqCst);

        assert_eq!(
            before_asking,
            vk::PresentModeKHR::IMMEDIATE,
            "a mode the engine did not learn from Eclipse reaches the host unchanged"
        );
        assert!(advertised.contains(&vk::PresentModeKHR::IMMEDIATE));
        assert_eq!(
            without_tearing,
            vk::PresentModeKHR::MAILBOX,
            "the immediate mode Eclipse advertised is carried out through mailbox"
        );
        assert_eq!(
            queried, 0x5,
            "the host is asked about the device the engine asked about, not the overlay's device"
        );
        assert_eq!(fifo, vk::PresentModeKHR::FIFO);
        assert_eq!(
            with_tearing,
            vk::PresentModeKHR::IMMEDIATE,
            "a host that can tear receives the engine's immediate mode"
        );
    }

    #[test]
    fn overlay_teardown_never_waits_on_the_whole_engine_device() {
        let source = include_str!("vk_overlay.rs");
        let body = |header: &str| {
            source
                .split_once(header)
                .unwrap_or_else(|| panic!("{header} missing"))
                .1
                .split_once("\n}\n")
                .expect("drop body")
                .0
                .to_owned()
        };
        let probe = body("impl Drop for Probe {");
        assert!(
            !probe.contains(concat!("device_wait", "_idle")),
            "Probe teardown must not synchronize every queue of the engine's device"
        );
        assert!(probe.contains("self.fence.retire(&self.device)"));
    }

    fn headless_gpu() -> Option<crate::graphics::headless_vulkan::HeadlessGpu> {
        match crate::graphics::headless_vulkan::HeadlessGpu::new() {
            Ok(gpu) => Some(gpu),
            Err(e) => {
                eprintln!("SKIP: no headless Vulkan device ({e})");
                None
            }
        }
    }

    fn image_barrier(
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        image: vk::Image,
        from: vk::ImageLayout,
        to: vk::ImageLayout,
    ) {
        let barrier = vk::ImageMemoryBarrier::default()
            .old_layout(from)
            .new_layout(to)
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color_range());
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            )
        };
    }

    fn engine_frame(
        gpu: &crate::graphics::headless_vulkan::HeadlessGpu,
        image: vk::Image,
        from: vk::ImageLayout,
    ) -> vk::Semaphore {
        let rendered = gpu.semaphore();
        gpu.run(&[], &[rendered], |device, cmd| unsafe {
            image_barrier(
                device,
                cmd,
                image,
                from,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            device.cmd_clear_color_image(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: [0.2, 0.4, 0.6, 1.0],
                },
                &[color_range()],
            );
            image_barrier(
                device,
                cmd,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::PRESENT_SRC_KHR,
            );
        });
        rendered
    }

    fn read_image(
        gpu: &crate::graphics::headless_vulkan::HeadlessGpu,
        image: vk::Image,
        layout: vk::ImageLayout,
        extent: vk::Extent2D,
        waits: &[vk::Semaphore],
    ) -> Vec<u8> {
        let len = (extent.width * extent.height * 4) as usize;
        let (buffer, memory) = gpu.host_buffer(len as u64, vk::BufferUsageFlags::TRANSFER_DST);
        gpu.run(waits, &[], |device, cmd| unsafe {
            image_barrier(
                device,
                cmd,
                image,
                layout,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(color_layers())
                .image_extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                });
            device.cmd_copy_image_to_buffer(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );
            image_barrier(
                device,
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                layout,
            );
        });
        gpu.read(memory, len)
    }

    fn rect_rows(pixels: &[u8], extent: vk::Extent2D, rect: vk::Rect2D) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..rect.extent.height {
            let row = (rect.offset.y as u32 + y) * extent.width + rect.offset.x as u32;
            let start = (row * 4) as usize;
            out.extend_from_slice(&pixels[start..start + (rect.extent.width * 4) as usize]);
        }
        out
    }

    fn outside_rect_is(
        pixels: &[u8],
        extent: vk::Extent2D,
        rect: vk::Rect2D,
        background: &[u8],
    ) -> bool {
        (0..extent.height).all(|y| {
            (0..extent.width).all(|x| {
                let inside = x >= rect.offset.x as u32
                    && x < rect.offset.x as u32 + rect.extent.width
                    && y >= rect.offset.y as u32
                    && y < rect.offset.y as u32 + rect.extent.height;
                let i = ((y * extent.width + x) * 4) as usize;
                inside || pixels[i..i + 4] == *background
            })
        })
    }

    #[test]
    fn device_destruction_releases_overlay_children_before_host_device() {
        let source = include_str!("vk_overlay.rs");

        assert!(source.contains("if name == c\"vkDestroyDevice\""));
        let release = source
            .find("release_overlay_device_resources(device)")
            .expect("device destruction must release overlay children");
        let host_destroy = source
            .find("host(device, p_allocator)")
            .expect("device destruction must call the host driver");
        assert!(release < host_destroy);
    }
}
