use crate::font::{RasterFont, ScaledFont};
use ash::vk;
use ash::vk::Handle;
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

fn overlay_font() -> Option<&'static RasterFont> {
    static FONT: OnceLock<Option<RasterFont>> = OnceLock::new();
    FONT.get_or_init(|| {
        let path = crate::graphics::discover_font_path()?;
        let bytes = std::fs::read(path).ok()?;
        RasterFont::try_from_vec(bytes).ok()
    })
    .as_ref()
}

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

const OVERLAY_TEXT_PADDING: f32 = 10.0;
const MAX_OVERLAY_FONT_SIZE: f32 = 100.0;

fn overlay_font_size(font_size: f32) -> f32 {
    if font_size.is_finite() && font_size > 0.0 {
        font_size.min(MAX_OVERLAY_FONT_SIZE)
    } else {
        14.0
    }
}

fn blend_text_pixel(buf: &mut [u8], index: usize, color: [u8; 4], coverage: f32) {
    let alpha = coverage.clamp(0.0, 1.0) * f32::from(color[3]) / 255.0;
    for channel in 0..3 {
        let background = f32::from(buf[index + channel]);
        buf[index + channel] =
            (f32::from(color[channel]) * alpha + background * (1.0 - alpha)) as u8;
    }
}

fn visible_line_end(
    text: &str,
    scaled: &mut ScaledFont,
    width: f32,
    wrapped: bool,
) -> (usize, usize, bool) {
    let mut line_width = 0.0;
    for (index, character) in text.char_indices() {
        if character == '\n' {
            return (index, index + character.len_utf8(), false);
        }
        let advance = scaled.advance(character);
        if line_width + advance > width {
            if wrapped {
                let end = if index == 0 {
                    character.len_utf8()
                } else {
                    index
                };
                return (end, end, false);
            }
            return (index, index, true);
        }
        line_width += advance;
    }
    (text.len(), text.len(), false)
}

fn draw_text_onto_rgba(
    buf: &mut [u8],
    w: u32,
    h: u32,
    overlay: &crate::framework::ActiveTextOverlay,
) {
    if overlay.text.is_empty() {
        return;
    }
    let Some(font) = overlay_font() else {
        return;
    };
    let scale = overlay_font_size(overlay.font_size);
    let Some(mut scaled) = font.scaled(scale) else {
        return;
    };
    let ascent = scaled.ascent();
    let line_height = (scaled.height() + scaled.line_gap().max(0.0)).max(scale);
    let color_bytes = (overlay.text_color as u32).to_be_bytes();
    let color = [
        color_bytes[1],
        color_bytes[2],
        color_bytes[3],
        color_bytes[0],
    ];
    let available_width = (w as f32 - OVERLAY_TEXT_PADDING * 2.0).max(1.0);
    let available_height = (h as f32 - OVERLAY_TEXT_PADDING * 2.0).max(line_height);
    let maximum_lines = if overlay.multiline {
        (available_height / line_height).floor().max(1.0) as usize
    } else {
        1
    };
    let first_baseline = if overlay.multiline || overlay.y_alignment == 0 {
        OVERLAY_TEXT_PADDING + ascent
    } else if overlay.y_alignment == 2 {
        h as f32 - OVERLAY_TEXT_PADDING - scale + ascent
    } else {
        (h as f32 - scale) * 0.5 + ascent
    };
    let mut remaining = overlay.text.as_str();
    let mut caret = None;
    let mut complete = false;

    for line_index in 0..maximum_lines {
        let (draw_end, consumed, clipped_line) = visible_line_end(
            remaining,
            &mut scaled,
            available_width,
            overlay.text_wrapped,
        );
        let line = &remaining[..draw_end];
        let line_width = line
            .chars()
            .map(|character| scaled.advance(character))
            .sum::<f32>();
        let mut pen_x = match overlay.x_alignment {
            1 => (w as f32 - OVERLAY_TEXT_PADDING - line_width).max(OVERLAY_TEXT_PADDING),
            2 => ((w as f32 - line_width) * 0.5).max(OVERLAY_TEXT_PADDING),
            _ => OVERLAY_TEXT_PADDING,
        };
        let baseline_y = first_baseline + line_index as f32 * line_height;

        for character in line.chars() {
            if character == '\u{2022}' {
                let radius = (scale * 0.13).max(2.0);
                let center_x = (pen_x + radius) as i32;
                let center_y = (baseline_y - ascent + scale * 0.5) as i32;
                let integer_radius = radius as i32;
                for delta_y in -integer_radius..=integer_radius {
                    for delta_x in -integer_radius..=integer_radius {
                        if (delta_x * delta_x + delta_y * delta_y) as f32 <= radius * radius {
                            let pixel_x = center_x + delta_x;
                            let pixel_y = center_y + delta_y;
                            if pixel_x >= 0
                                && pixel_y >= 0
                                && (pixel_x as u32) < w
                                && (pixel_y as u32) < h
                            {
                                let index = ((pixel_y as u32 * w + pixel_x as u32) * 4) as usize;
                                if index + 2 < buf.len() {
                                    blend_text_pixel(buf, index, color, 1.0);
                                }
                            }
                        }
                    }
                }
                pen_x += radius * 3.0;
                continue;
            }
            let advance = scaled.advance(character);
            if let Some(glyph) = scaled.glyph_at(character, pen_x, baseline_y) {
                let placement = glyph.placement();
                glyph.draw(|glyph_x, glyph_y, coverage| {
                    let pixel_x = placement.left + glyph_x as i32;
                    let pixel_y = placement.top + glyph_y as i32;
                    if pixel_x >= 0 && pixel_y >= 0 && (pixel_x as u32) < w && (pixel_y as u32) < h
                    {
                        let index = ((pixel_y as u32 * w + pixel_x as u32) * 4) as usize;
                        blend_text_pixel(buf, index, color, coverage);
                    }
                });
            }
            pen_x += advance;
        }

        if clipped_line {
            break;
        }
        if consumed == remaining.len() {
            caret = Some((pen_x, baseline_y));
            complete = true;
            break;
        }
        remaining = &remaining[consumed..];
        if remaining.is_empty() {
            caret = Some((OVERLAY_TEXT_PADDING, baseline_y + line_height));
            complete = line_index + 1 < maximum_lines;
            break;
        }
    }

    static BLINK: AtomicU64 = AtomicU64::new(0);
    if complete && (BLINK.fetch_add(1, Ordering::Relaxed) / 30).is_multiple_of(2) {
        if let Some((pen_x, baseline_y)) = caret {
            let cx = pen_x as i32 + 1;
            let y0 = (baseline_y - scale * 0.72).max(0.0) as u32;
            let y1 = ((baseline_y + scale * 0.08) as u32).min(h);
            for cy in y0..y1 {
                for dx in 0..2 {
                    let px = cx + dx;
                    if px >= 0 && (px as u32) < w {
                        let idx = ((cy * w + px as u32) * 4) as usize;
                        if idx + 2 < buf.len() {
                            blend_text_pixel(buf, idx, color, 1.0);
                        }
                    }
                }
            }
        }
    }
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
static HOST_GET_SWAPCHAIN_IMAGES: AtomicU64 = AtomicU64::new(0);
static HOST_ACQUIRE_NEXT_IMAGE: AtomicU64 = AtomicU64::new(0);
static HOST_ACQUIRE_NEXT_IMAGE2: AtomicU64 = AtomicU64::new(0);
static PRESENT_COUNT: AtomicU64 = AtomicU64::new(0);

static INSTANCE: AtomicU64 = AtomicU64::new(0);
static PHYSICAL_DEVICE: AtomicU64 = AtomicU64::new(0);
static QUEUE_FAMILY: AtomicU32 = AtomicU32::new(u32::MAX);

pub(crate) fn set_instance(instance: vk::Instance) {
    INSTANCE.store(instance.as_raw(), Ordering::Relaxed);
}

#[derive(Default)]
struct OverlayState {
    device: u64,

    swapchain: u64,

    format: i32,

    width: u32,
    height: u32,

    images: Vec<u64>,
}

static STATE: Mutex<OverlayState> = Mutex::new(OverlayState {
    device: 0,
    swapchain: 0,
    format: 0,
    width: 0,
    height: 0,
    images: Vec::new(),
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
    let _swapchain = swapchain_lock();
    unsafe { host(device, swapchain, timeout, semaphore, fence, p_image_index) }
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
    let _swapchain = swapchain_lock();
    unsafe { host(device, p_acquire_info, p_image_index) }
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

    let create_info = unsafe { p_create_info.as_ref() };
    let engine_queues = create_info
        .filter(|ci| ci.queue_create_info_count > 0 && !ci.p_queue_create_infos.is_null())
        .map(|ci| unsafe {
            std::slice::from_raw_parts(ci.p_queue_create_infos, ci.queue_create_info_count as usize)
        });
    let reservation = engine_queues.and_then(|infos| {
        let available = queue_family_queue_count(physical_device, infos[0].queue_family_index)?;
        reserve_web_present_queue(available, infos[0].queue_count).map(|index| (index, infos))
    });
    let (r, reserved_index) = match (reservation, create_info) {
        (Some((index, infos)), Some(ci)) => {
            let priorities = vec![1.0f32; index as usize + 1];
            let mut queue_infos = infos.to_vec();
            queue_infos[0] = queue_infos[0].queue_priorities(&priorities);
            let widened = ci.queue_create_infos(&queue_infos);
            (
                unsafe { host(physical_device, &widened, p_allocator, p_device) },
                index,
            )
        }
        _ => (
            unsafe { host(physical_device, p_create_info, p_allocator, p_device) },
            u32::MAX,
        ),
    };
    if r == vk::Result::SUCCESS && !p_device.is_null() {
        let device = unsafe { *p_device };

        PHYSICAL_DEVICE.store(physical_device.as_raw(), Ordering::Relaxed);
        WEB_PRESENT_QUEUE_INDEX.store(reserved_index, Ordering::Relaxed);
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

    let r = unsafe { host(device, p_create_info, p_allocator, p_swapchain) };
    if r == vk::Result::SUCCESS && !p_create_info.is_null() && !p_swapchain.is_null() {
        let info = unsafe { &*p_create_info };
        let swapchain = unsafe { *p_swapchain };
        if let Ok(mut st) = STATE.lock() {
            st.swapchain = swapchain.as_raw();
            st.format = info.image_format.as_raw();
            st.width = info.image_extent.width;
            st.height = info.image_extent.height;
            st.images.clear();
        }
        tracing::info!(
            format = info.image_format.as_raw(),
            width = info.image_extent.width,
            height = info.image_extent.height,
            "vk-overlay: captured engine swapchain"
        );
    }
    r
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
    if gw == 0 || gh == 0 {
        return None;
    }
    let x = (gx.max(0) as u32).min(extent.width.saturating_sub(1));
    let y = (gy.max(0) as u32).min(extent.height.saturating_sub(1));
    let w = gw.min(extent.width - x).max(1);
    let h = gh.min(extent.height - y).max(1);
    Some(vk::Rect2D {
        offset: vk::Offset2D {
            x: x as i32,
            y: y as i32,
        },
        extent: vk::Extent2D {
            width: w,
            height: h,
        },
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

fn mask_overlay_text(text: String, input_type: i32) -> String {
    if matches!(input_type, 0..=4 | 7 | 8) {
        text
    } else {
        "\u{2022}".repeat(text.chars().count())
    }
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

fn find_host_visible_mem_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
) -> Option<u32> {
    let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    (0..props.memory_type_count).find(|&i| {
        (type_bits & (1u32 << i)) != 0
            && props.memory_types[i as usize].property_flags.contains(want)
    })
}

static PROBE: Mutex<Option<Probe>> = Mutex::new(None);

static WEB_COMPOSITE: Mutex<Option<Probe>> = Mutex::new(None);

static WEB_COMPOSITE_LAST: AtomicU64 = AtomicU64::new(0);

static SWAPCHAIN_LOCK: Mutex<()> = Mutex::new(());

static WEB_PRESENT_QUEUE_INDEX: AtomicU32 = AtomicU32::new(u32::MAX);

static LAST_ENGINE_PRESENT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

static WEB_PRESENTER: Mutex<Option<WebPresenter>> = Mutex::new(None);

const WEB_PRESENT_ENGINE_GAP: std::time::Duration = std::time::Duration::from_millis(12);

fn swapchain_lock() -> std::sync::MutexGuard<'static, ()> {
    SWAPCHAIN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn note_engine_present() {
    if let Ok(mut last) = LAST_ENGINE_PRESENT.lock() {
        *last = Some(std::time::Instant::now());
    }
}

fn engine_presented_within(gap: std::time::Duration) -> bool {
    LAST_ENGINE_PRESENT
        .lock()
        .ok()
        .and_then(|last| *last)
        .is_some_and(|at| at.elapsed() < gap)
}

fn reserve_web_present_queue(family_queue_count: u32, engine_queue_count: u32) -> Option<u32> {
    (engine_queue_count > 0 && family_queue_count > engine_queue_count)
        .then_some(engine_queue_count)
}

fn queue_family_queue_count(physical_device: vk::PhysicalDevice, family: u32) -> Option<u32> {
    let entry = super::vulkan_wsi::host_entry()?;
    let instance_raw = INSTANCE.load(Ordering::Relaxed);
    if instance_raw == 0 {
        return None;
    }
    let instance =
        unsafe { ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(instance_raw)) };
    let families = unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
    families.get(family as usize).map(|f| f.queue_count)
}

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

struct WebPresenter {
    device: ash::Device,
    device_raw: u64,
    queue: vk::Queue,
    acquire_next_image: vk::PFN_vkAcquireNextImageKHR,
    queue_present: vk::PFN_vkQueuePresentKHR,
    swapchain: u64,
    extent: vk::Extent2D,
    images: Vec<u64>,
    command_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    acquire: vk::Semaphore,
    done: Vec<vk::Semaphore>,
    saved: vk::Image,
    saved_memory: vk::DeviceMemory,
    saved_valid: bool,
    frame: vk::Buffer,
    frame_memory: vk::DeviceMemory,
    frame_mapped: *mut u8,
    frame_rect: vk::Rect2D,
    frame_key: u64,
}

unsafe impl Send for WebPresenter {}

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

unsafe fn record_save_engine_frame(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    saved: vk::Image,
    extent: vk::Extent2D,
) {
    let to_src = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_range());
    let saved_writable = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_access_mask(vk::AccessFlags::TRANSFER_READ)
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(saved)
        .subresource_range(color_range());
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_src, saved_writable],
        );
        let copy = vk::ImageCopy::default()
            .src_subresource(color_layers())
            .dst_subresource(color_layers())
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            });
        device.cmd_copy_image(
            cmd,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            saved,
            vk::ImageLayout::GENERAL,
            &[copy],
        );
    }
}

impl WebPresenter {
    #[allow(clippy::too_many_arguments)]
    fn build(
        entry: &ash::Entry,
        instance_raw: u64,
        device_raw: u64,
        physical_raw: u64,
        queue_family: u32,
        queue_index: u32,
        swapchain: u64,
        extent: vk::Extent2D,
        format: vk::Format,
        images: Vec<u64>,
    ) -> Option<WebPresenter> {
        if instance_raw == 0
            || device_raw == 0
            || physical_raw == 0
            || queue_family == u32::MAX
            || swapchain == 0
            || extent.width == 0
            || extent.height == 0
            || images.is_empty()
        {
            return None;
        }
        let host_gdpa_addr = cached(&HOST_GDPA)?;
        let host_gdpa: vk::PFN_vkGetDeviceProcAddr =
            unsafe { std::mem::transmute::<usize, vk::PFN_vkGetDeviceProcAddr>(host_gdpa_addr) };
        let device_handle = vk::Device::from_raw(device_raw);
        let acquire_next_image: vk::PFN_vkAcquireNextImageKHR = unsafe {
            std::mem::transmute::<vk::PFN_vkVoidFunction, Option<vk::PFN_vkAcquireNextImageKHR>>(
                host_gdpa(device_handle, c"vkAcquireNextImageKHR".as_ptr()),
            )
        }?;
        let queue_present: vk::PFN_vkQueuePresentKHR = unsafe {
            std::mem::transmute::<vk::PFN_vkVoidFunction, Option<vk::PFN_vkQueuePresentKHR>>(
                host_gdpa(device_handle, c"vkQueuePresentKHR".as_ptr()),
            )
        }?;

        let instance =
            unsafe { ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(instance_raw)) };
        let device = unsafe { ash::Device::load(instance.fp_v1_0(), device_handle) };
        let mem_props = unsafe {
            instance
                .get_physical_device_memory_properties(vk::PhysicalDevice::from_raw(physical_raw))
        };
        let queue = unsafe { device.get_device_queue(queue_family, queue_index) };

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(queue_family);
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }.ok()?;
        let mut presenter = WebPresenter {
            device: device.clone(),
            device_raw,
            queue,
            acquire_next_image,
            queue_present,
            swapchain,
            extent,
            images,
            command_pool,
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            acquire: vk::Semaphore::null(),
            done: Vec::new(),
            saved: vk::Image::null(),
            saved_memory: vk::DeviceMemory::null(),
            saved_valid: false,
            frame: vk::Buffer::null(),
            frame_memory: vk::DeviceMemory::null(),
            frame_mapped: std::ptr::null_mut(),
            frame_rect: vk::Rect2D::default(),
            frame_key: 0,
        };
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        presenter.cmd = unsafe { device.allocate_command_buffers(&alloc) }
            .ok()?
            .into_iter()
            .next()?;
        presenter.fence = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )
        }
        .ok()?;
        presenter.acquire =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }.ok()?;
        for _ in 0..presenter.images.len() {
            let done =
                unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
                    .ok()?;
            presenter.done.push(done);
        }

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        presenter.saved = unsafe { device.create_image(&image_info, None) }.ok()?;
        let image_req = unsafe { device.get_image_memory_requirements(presenter.saved) };
        let image_mt = find_device_local_mem_type(&mem_props, image_req.memory_type_bits)?;
        presenter.saved_memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(image_req.size)
                    .memory_type_index(image_mt),
                None,
            )
        }
        .ok()?;
        unsafe { device.bind_image_memory(presenter.saved, presenter.saved_memory, 0) }.ok()?;

        let frame_bytes = u64::from(extent.width) * u64::from(extent.height) * 4;
        let buffer_info = vk::BufferCreateInfo::default()
            .size(frame_bytes)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        presenter.frame = unsafe { device.create_buffer(&buffer_info, None) }.ok()?;
        let buffer_req = unsafe { device.get_buffer_memory_requirements(presenter.frame) };
        let buffer_mt = find_host_visible_mem_type(&mem_props, buffer_req.memory_type_bits)?;
        presenter.frame_memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(buffer_req.size)
                    .memory_type_index(buffer_mt),
                None,
            )
        }
        .ok()?;
        unsafe { device.bind_buffer_memory(presenter.frame, presenter.frame_memory, 0) }.ok()?;
        presenter.frame_mapped = unsafe {
            device.map_memory(
                presenter.frame_memory,
                0,
                frame_bytes,
                vk::MemoryMapFlags::empty(),
            )
        }
        .ok()?
        .cast::<u8>();

        unsafe { presenter.initialize_saved_layout() }.then_some(presenter)
    }

    unsafe fn initialize_saved_layout(&mut self) -> bool {
        unsafe {
            if self.device.reset_fences(&[self.fence]).is_err() {
                return false;
            }
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            if self.device.begin_command_buffer(self.cmd, &begin).is_err() {
                return false;
            }
            let to_general = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.saved)
                .subresource_range(color_range());
            self.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_general],
            );
            if self.device.end_command_buffer(self.cmd).is_err() {
                return false;
            }
            let cmds = [self.cmd];
            let submit = vk::SubmitInfo::default().command_buffers(&cmds);
            self.device
                .queue_submit(self.queue, &[submit], self.fence)
                .is_ok()
                && self
                    .device
                    .wait_for_fences(&[self.fence], true, u64::MAX)
                    .is_ok()
        }
    }

    fn saved_target(&self, extent: vk::Extent2D) -> Option<vk::Image> {
        (self.extent == extent).then_some(self.saved)
    }

    fn matches(&self, swapchain: u64, extent: vk::Extent2D, images: &[u64]) -> bool {
        self.swapchain == swapchain && self.extent == extent && self.images == images
    }

    unsafe fn fill(
        &mut self,
        key: u64,
        rect: vk::Rect2D,
        src: &[u8],
        src_stride: usize,
        swizzle: bool,
    ) -> bool {
        if rect.extent.width == 0
            || rect.extent.height == 0
            || rect.extent.width > self.extent.width
            || rect.extent.height > self.extent.height
        {
            return false;
        }
        if self.frame_key == key && self.frame_rect == rect {
            return true;
        }
        if unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .is_err()
        } {
            return false;
        }
        let row_bytes = rect.extent.width as usize * 4;
        let rows = rect.extent.height as usize;
        let dst = unsafe { std::slice::from_raw_parts_mut(self.frame_mapped, row_bytes * rows) };
        if !bgra_rows_into(dst, row_bytes, src, src_stride, rows, row_bytes, swizzle) {
            return false;
        }
        self.frame_key = key;
        self.frame_rect = rect;
        true
    }

    unsafe fn present(&mut self) -> bool {
        let mut image_index = 0u32;
        let acquired = unsafe {
            (self.acquire_next_image)(
                vk::Device::from_raw(self.device_raw),
                vk::SwapchainKHR::from_raw(self.swapchain),
                0,
                self.acquire,
                vk::Fence::null(),
                &mut image_index,
            )
        };
        if acquired != vk::Result::SUCCESS && acquired != vk::Result::SUBOPTIMAL_KHR {
            return false;
        }
        let Some(&image_raw) = self.images.get(image_index as usize) else {
            return false;
        };
        let Some(&done) = self.done.get(image_index as usize) else {
            return false;
        };
        let image = vk::Image::from_raw(image_raw);
        let full = vk::Extent3D {
            width: self.extent.width,
            height: self.extent.height,
            depth: 1,
        };
        unsafe {
            if self.device.reset_fences(&[self.fence]).is_err()
                || self
                    .device
                    .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                    .is_err()
            {
                return false;
            }
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            if self.device.begin_command_buffer(self.cmd, &begin).is_err() {
                return false;
            }
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(color_range());
            let saved_readable = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.saved)
                .subresource_range(color_range());
            self.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE | vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst, saved_readable],
            );
            if self.saved_valid {
                let copy = vk::ImageCopy::default()
                    .src_subresource(color_layers())
                    .dst_subresource(color_layers())
                    .extent(full);
                self.device.cmd_copy_image(
                    self.cmd,
                    self.saved,
                    vk::ImageLayout::GENERAL,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[copy],
                );
                let ordered = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(color_range());
                self.device.cmd_pipeline_barrier(
                    self.cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[ordered],
                );
            } else {
                self.device.cmd_clear_color_image(
                    self.cmd,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 1.0],
                    },
                    &[color_range()],
                );
            }
            let region = vk::BufferImageCopy::default()
                .image_subresource(color_layers())
                .image_offset(vk::Offset3D {
                    x: self.frame_rect.offset.x,
                    y: self.frame_rect.offset.y,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: self.frame_rect.extent.width,
                    height: self.frame_rect.extent.height,
                    depth: 1,
                });
            self.device.cmd_copy_buffer_to_image(
                self.cmd,
                self.frame,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            let to_present = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(color_range());
            self.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );
            if self.device.end_command_buffer(self.cmd).is_err() {
                return false;
            }
            let waits = [self.acquire];
            let wait_stages = [vk::PipelineStageFlags::TRANSFER];
            let cmds = [self.cmd];
            let signals = [done];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmds)
                .signal_semaphores(&signals);
            if self
                .device
                .queue_submit(self.queue, &[submit], self.fence)
                .is_err()
            {
                return false;
            }
            let swapchains = [vk::SwapchainKHR::from_raw(self.swapchain)];
            let indices = [image_index];
            let present = vk::PresentInfoKHR::default()
                .wait_semaphores(&signals)
                .swapchains(&swapchains)
                .image_indices(&indices);
            let presented = (self.queue_present)(self.queue, &present);
            let _ = self.device.wait_for_fences(&[self.fence], true, u64::MAX);
            presented == vk::Result::SUCCESS || presented == vk::Result::SUBOPTIMAL_KHR
        }
    }
}

impl Drop for WebPresenter {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            for done in self.done.drain(..) {
                self.device.destroy_semaphore(done, None);
            }
            self.device.destroy_semaphore(self.acquire, None);
            self.device.destroy_buffer(self.frame, None);
            self.device.free_memory(self.frame_memory, None);
            self.device.destroy_image(self.saved, None);
            self.device.free_memory(self.saved_memory, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
        }
    }
}

fn ensure_web_presenter() -> bool {
    let queue_index = WEB_PRESENT_QUEUE_INDEX.load(Ordering::Relaxed);
    if queue_index == u32::MAX {
        return false;
    }
    let (device, swapchain, extent, format, images) = match STATE.lock() {
        Ok(st) => (
            st.device,
            st.swapchain,
            vk::Extent2D {
                width: st.width,
                height: st.height,
            },
            vk::Format::from_raw(st.format),
            st.images.clone(),
        ),
        Err(_) => return false,
    };
    if device == 0 || swapchain == 0 || images.is_empty() {
        return false;
    }
    let Ok(mut slot) = WEB_PRESENTER.lock() else {
        return false;
    };
    if slot
        .as_ref()
        .is_some_and(|p| p.matches(swapchain, extent, &images))
    {
        return true;
    }
    *slot = None;
    let Some(entry) = super::vulkan_wsi::host_entry() else {
        return false;
    };
    match WebPresenter::build(
        entry,
        INSTANCE.load(Ordering::Relaxed),
        device,
        PHYSICAL_DEVICE.load(Ordering::Relaxed),
        QUEUE_FAMILY.load(Ordering::Relaxed),
        queue_index,
        swapchain,
        extent,
        format,
        images,
    ) {
        Some(presenter) => {
            tracing::info!(
                queue_index,
                width = extent.width,
                height = extent.height,
                "vk-overlay: independent WebView presenter armed on Eclipse's reserved queue"
            );
            *slot = Some(presenter);
            true
        }
        None => {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "vk-overlay: independent WebView presenter unavailable; WebView frames \
                     will only reach the screen when the engine presents"
                );
            }
            false
        }
    }
}

fn engine_frame_save_target(extent: vk::Extent2D) -> Option<vk::Image> {
    if !ensure_web_presenter() {
        return None;
    }
    WEB_PRESENTER
        .lock()
        .ok()?
        .as_ref()
        .and_then(|p| p.saved_target(extent))
}

fn mark_engine_frame_saved() {
    if let Ok(mut slot) = WEB_PRESENTER.lock() {
        if let Some(p) = slot.as_mut() {
            p.saved_valid = true;
        }
    }
}

pub(crate) fn present_staged_webview_frame(view: i64) {
    if crate::webview::client::active_view() != view
        || engine_presented_within(WEB_PRESENT_ENGINE_GAP)
    {
        return;
    }
    let _swapchain = swapchain_lock();
    if engine_presented_within(WEB_PRESENT_ENGINE_GAP) || !ensure_web_presenter() {
        return;
    }
    let (extent, format_raw) = match STATE.lock() {
        Ok(st) => (
            vk::Extent2D {
                width: st.width,
                height: st.height,
            },
            st.format,
        ),
        Err(_) => return,
    };
    let swizzle = match classify_swapchain_format(format_raw) {
        CompositeFormat::Bgra => false,
        CompositeFormat::RgbaSwizzle => true,
        CompositeFormat::Unsupported => return,
    };
    let Ok(mut slot) = WEB_PRESENTER.lock() else {
        return;
    };
    let Some(presenter) = slot.as_mut() else {
        return;
    };
    let filled = crate::webview::client::with_latest_frame(view, |stage| {
        if stage.bytes.is_empty() {
            return false;
        }
        let Some((x, y, w, h)) = resolve_webview_rect(
            crate::webview::client::composited_rect(),
            extent.width,
            extent.height,
            stage.width,
            stage.height,
        ) else {
            return false;
        };
        let rect = vk::Rect2D {
            offset: vk::Offset2D {
                x: x as i32,
                y: y as i32,
            },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        };
        let key = (u64::from(stage.generation) << 32) | u64::from(stage.seq);
        unsafe { presenter.fill(key, rect, &stage.bytes, stage.stride as usize, swizzle) }
    })
    .unwrap_or(false);
    if filled {
        unsafe { presenter.present() };
    }
}

fn release_probe_for_device(slot: &'static Mutex<Option<Probe>>, device: vk::Device) {
    let mut probe = match slot.lock() {
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
    release_probe_for_device(&PROBE, device);
    release_probe_for_device(&WEB_COMPOSITE, device);
    if let Ok(mut slot) = WEB_PRESENTER.lock() {
        if slot
            .as_ref()
            .is_some_and(|p| p.device_raw == device.as_raw())
        {
            *slot = None;
        }
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
    WEB_PRESENT_QUEUE_INDEX.store(u32::MAX, Ordering::Relaxed);
    WEB_COMPOSITE_LAST.store(0, Ordering::Relaxed);
}

struct Probe {
    device: ash::Device,
    command_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    rect: vk::Rect2D,
}

unsafe impl Send for Probe {}

impl Probe {
    fn build(
        entry: &ash::Entry,
        instance_raw: u64,
        device_raw: u64,
        physical_raw: u64,
        queue_family: u32,
        rect: vk::Rect2D,
    ) -> Option<Probe> {
        if instance_raw == 0
            || device_raw == 0
            || physical_raw == 0
            || queue_family == u32::MAX
            || rect.extent.width == 0
            || rect.extent.height == 0
        {
            return None;
        }

        let instance =
            unsafe { ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(instance_raw)) };
        let device =
            unsafe { ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw(device_raw)) };

        let mem_props = unsafe {
            instance
                .get_physical_device_memory_properties(vk::PhysicalDevice::from_raw(physical_raw))
        };
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(queue_family);

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

        let fence = match unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )
        } {
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
                unsafe { device.destroy_fence(fence, None) };
                cleanup_pool(&device);
                return None;
            }
        };

        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let cleanup_buf = |device: &ash::Device| {
            unsafe {
                device.destroy_buffer(buffer, None);
                device.destroy_fence(fence, None);
            }
            cleanup_pool(device);
        };
        let Some(mt) = find_host_visible_mem_type(&mem_props, req.memory_type_bits) else {
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
        })
    }

    unsafe fn capture(
        &self,
        queue: vk::Queue,
        image_raw: u64,
        engine_waits: &[vk::Semaphore],
        draw_text: Option<&crate::framework::ActiveTextOverlay>,
        write_probe: bool,
    ) -> bool {
        let image = vk::Image::from_raw(image_raw);
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);

        unsafe {
            if self
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .is_err()
                || self.device.reset_fences(&[self.fence]).is_err()
                || self
                    .device
                    .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                    .is_err()
            {
                return false;
            }
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            if self.device.begin_command_buffer(self.cmd, &begin).is_err() {
                return false;
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
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D {
                    x: self.rect.offset.x,
                    y: self.rect.offset.y,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: self.rect.extent.width,
                    height: self.rect.extent.height,
                    depth: 1,
                });
            self.device.cmd_copy_image_to_buffer(
                self.cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                &[region],
            );
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
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );
            if self.device.end_command_buffer(self.cmd).is_err() {
                return false;
            }
            let wait_stages = vec![vk::PipelineStageFlags::TRANSFER; engine_waits.len()];
            let cmds = [self.cmd];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(engine_waits)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmds);
            if self
                .device
                .queue_submit(queue, &[submit], self.fence)
                .is_err()
            {
                return false;
            }
            if self
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .is_err()
            {
                return false;
            }

            if let Some(text) = draw_text {
                {
                    let size =
                        (self.rect.extent.width as usize) * (self.rect.extent.height as usize) * 4;
                    let buf = std::slice::from_raw_parts_mut(self.mapped, size);
                    draw_text_onto_rgba(buf, self.rect.extent.width, self.rect.extent.height, text);
                }
                let recorded = self.device.reset_fences(&[self.fence]).is_ok()
                    && self
                        .device
                        .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                        .is_ok()
                    && self.device.begin_command_buffer(self.cmd, &begin).is_ok();
                if recorded {
                    let to_dst = vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .src_access_mask(vk::AccessFlags::MEMORY_READ)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
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
                        &[to_dst],
                    );

                    self.device.cmd_copy_buffer_to_image(
                        self.cmd,
                        self.buffer,
                        image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                    let back = vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::MEMORY_READ)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image)
                        .subresource_range(range);
                    self.device.cmd_pipeline_barrier(
                        self.cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::ALL_COMMANDS,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[back],
                    );
                    if self.device.end_command_buffer(self.cmd).is_ok() {
                        let cmds2 = [self.cmd];
                        let submit2 = vk::SubmitInfo::default().command_buffers(&cmds2);
                        let _ = self.device.queue_submit(queue, &[submit2], self.fence);
                        let _ = self.device.wait_for_fences(&[self.fence], true, u64::MAX);
                    }
                }
            }
            if write_probe {
                let w = self.rect.extent.width as usize;
                let h = self.rect.extent.height as usize;
                let size = w * h * 4;

                {
                    let data = std::slice::from_raw_parts(self.mapped, size);
                    let mut png_rgba = data.to_vec();
                    for px in png_rgba.as_chunks_mut::<4>().0 {
                        px[3] = 255;
                    }
                    let png = encode_png_rgba(&png_rgba, w as u32, h as u32);
                    let _ = std::fs::write("/tmp/eclipse_field_probe.png", png);
                }

                static LOG_TICK: AtomicU64 = AtomicU64::new(0);
                if LOG_TICK.fetch_add(1, Ordering::Relaxed).is_multiple_of(60) {
                    let data = std::slice::from_raw_parts(self.mapped, size);
                    const BUCKETS: usize = 64;
                    let mut col_ink = [0u32; BUCKETS];
                    let mut total_ink = 0u32;
                    let (y0, y1) = (h * 3 / 10, h * 7 / 10);
                    for y in y0..y1 {
                        for x in 0..w {
                            let i = (y * w + x) * 4;
                            let lum = (u32::from(data[i])
                                + u32::from(data[i + 1])
                                + u32::from(data[i + 2]))
                                / 3;
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
                        .map(|&c| {
                            levels[(c as usize * (levels.len() - 1) / max as usize)
                                .min(levels.len() - 1)]
                        })
                        .collect();
                    tracing::info!(total_ink, "vk-overlay field-probe ink |{spark}|");
                }
            }
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn upload_bgra(
        &self,
        queue: vk::Queue,
        image_raw: u64,
        engine_waits: &[vk::Semaphore],
        src: &[u8],
        src_stride: usize,
        swizzle: bool,
        refresh: bool,
        save_into: Option<(vk::Image, vk::Extent2D)>,
    ) -> bool {
        let image = vk::Image::from_raw(image_raw);
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);

        unsafe {
            if self
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .is_err()
                || self.device.reset_fences(&[self.fence]).is_err()
                || self
                    .device
                    .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                    .is_err()
            {
                return false;
            }
            if refresh {
                let w = self.rect.extent.width as usize;
                let h = self.rect.extent.height as usize;
                let dst = std::slice::from_raw_parts_mut(self.mapped, w * h * 4);
                if !bgra_rows_into(dst, w * 4, src, src_stride, h, w * 4, swizzle) {
                    return false;
                }
            }
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            if self.device.begin_command_buffer(self.cmd, &begin).is_err() {
                return false;
            }
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::MEMORY_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
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
                &[to_dst],
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D {
                    x: self.rect.offset.x,
                    y: self.rect.offset.y,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: self.rect.extent.width,
                    height: self.rect.extent.height,
                    depth: 1,
                });
            self.device.cmd_copy_buffer_to_image(
                self.cmd,
                self.buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            let mut presented_from = vk::ImageLayout::TRANSFER_DST_OPTIMAL;
            if let Some((saved, extent)) = save_into {
                record_save_engine_frame(&self.device, self.cmd, image, saved, extent);
                presented_from = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
            }
            let back = vk::ImageMemoryBarrier::default()
                .old_layout(presented_from)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(range);
            self.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back],
            );
            if self.device.end_command_buffer(self.cmd).is_err() {
                return false;
            }
            let wait_stages = vec![vk::PipelineStageFlags::TRANSFER; engine_waits.len()];
            let cmds = [self.cmd];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(engine_waits)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmds);
            if self
                .device
                .queue_submit(queue, &[submit], self.fence)
                .is_err()
            {
                return false;
            }
            if self
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .is_err()
            {
                return false;
            }
        }
        true
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
        }
    }
}

fn ensure_probe_in(slot: &'static Mutex<Option<Probe>>, rect: vk::Rect2D) -> bool {
    let Ok(mut guard) = slot.lock() else {
        return false;
    };
    if guard.as_ref().is_some_and(|p| {
        p.rect.offset.x == rect.offset.x
            && p.rect.offset.y == rect.offset.y
            && p.rect.extent.width == rect.extent.width
            && p.rect.extent.height == rect.extent.height
    }) {
        return false;
    }
    let device = STATE.lock().map(|s| s.device).unwrap_or(0);
    if device == 0 {
        return false;
    }
    let Some(entry) = super::vulkan_wsi::host_entry() else {
        return false;
    };

    *guard = None;
    if let Some(p) = Probe::build(
        entry,
        INSTANCE.load(Ordering::Relaxed),
        device,
        PHYSICAL_DEVICE.load(Ordering::Relaxed),
        QUEUE_FAMILY.load(Ordering::Relaxed),
        rect,
    ) {
        tracing::info!(
            x = rect.offset.x,
            y = rect.offset.y,
            w = rect.extent.width,
            h = rect.extent.height,
            "vk-overlay: overlay/composite objects built for rect"
        );
        *guard = Some(p);
        return true;
    }
    false
}

fn ensure_probe(rect: vk::Rect2D) {
    let _ = ensure_probe_in(&PROBE, rect);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompositeFormat {
    Bgra,

    RgbaSwizzle,

    Unsupported,
}

fn classify_swapchain_format(raw: i32) -> CompositeFormat {
    let f = vk::Format::from_raw(raw);
    if f == vk::Format::B8G8R8A8_UNORM || f == vk::Format::B8G8R8A8_SRGB {
        CompositeFormat::Bgra
    } else if f == vk::Format::R8G8B8A8_UNORM || f == vk::Format::R8G8B8A8_SRGB {
        CompositeFormat::RgbaSwizzle
    } else {
        CompositeFormat::Unsupported
    }
}

fn bgra_rows_into(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    rows: usize,
    row_bytes: usize,
    swizzle: bool,
) -> bool {
    for r in 0..rows {
        let Some(src_start) = r.checked_mul(src_stride) else {
            return false;
        };
        let Some(dst_start) = r.checked_mul(dst_stride) else {
            return false;
        };
        let Some(srow) = src.get(src_start..src_start + row_bytes) else {
            return false;
        };
        let Some(drow) = dst.get_mut(dst_start..dst_start + row_bytes) else {
            return false;
        };
        if swizzle {
            let (destination_pixels, _) = drow.as_chunks_mut::<4>();
            let (source_pixels, _) = srow.as_chunks::<4>();
            for (d, s) in destination_pixels.iter_mut().zip(source_pixels) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
        } else {
            drow.copy_from_slice(srow);
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn clamp_webview_rect(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    extent_w: u32,
    extent_h: u32,
    stage_w: u32,
    stage_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    if extent_w == 0 || extent_h == 0 || stage_w == 0 || stage_h == 0 || w == 0 || h == 0 {
        return None;
    }
    let cx = u32::try_from(x.max(0)).ok()?;
    let cy = u32::try_from(y.max(0)).ok()?;
    if cx >= extent_w || cy >= extent_h {
        return None;
    }
    let cw = w.min(stage_w).min(extent_w - cx);
    let ch = h.min(stage_h).min(extent_h - cy);
    if cw == 0 || ch == 0 {
        return None;
    }
    Some((cx, cy, cw, ch))
}

pub(crate) fn resolve_webview_rect(
    cached: Option<(i32, i32, u32, u32)>,
    extent_w: u32,
    extent_h: u32,
    stage_w: u32,
    stage_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    let (x, y, w, h) = match cached {
        Some(r) => r,
        None => {
            let w = stage_w.min(extent_w);
            let h = stage_h.min(extent_h);
            (
                ((extent_w - w) / 2) as i32,
                ((extent_h - h) / 2) as i32,
                w,
                h,
            )
        }
    };
    clamp_webview_rect(x, y, w, h, extent_w, extent_h, stage_w, stage_h)
}

fn composite_webview_frame(
    queue: vk::Queue,
    view: i64,
    image_raw: u64,
    extent: vk::Extent2D,
    format_raw: i32,
    engine_waits: &[vk::Semaphore],
) -> bool {
    let swizzle = match classify_swapchain_format(format_raw) {
        CompositeFormat::Bgra => false,
        CompositeFormat::RgbaSwizzle => true,
        CompositeFormat::Unsupported => {
            static FORMAT_WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !FORMAT_WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    format = format_raw,
                    "vk-overlay: webview composite skipped — unsupported swapchain format \
                     (expected B8G8R8A8/R8G8B8A8 UNORM/SRGB)"
                );
            }
            return false;
        }
    };

    let (consumed, drawn) = crate::webview::client::with_latest_frame(view, |stage| {
        if stage.bytes.is_empty() {
            return (false, None);
        }
        let Some((cx, cy, cw, ch)) = resolve_webview_rect(
            crate::webview::client::composited_rect(),
            extent.width,
            extent.height,
            stage.width,
            stage.height,
        ) else {
            return (false, None);
        };
        let rect = vk::Rect2D {
            offset: vk::Offset2D {
                x: cx as i32,
                y: cy as i32,
            },
            extent: vk::Extent2D {
                width: cw,
                height: ch,
            },
        };
        let rebuilt = ensure_probe_in(&WEB_COMPOSITE, rect);
        let key = (u64::from(stage.generation) << 32) | u64::from(stage.seq);
        let refresh = rebuilt || WEB_COMPOSITE_LAST.load(Ordering::Relaxed) != key;
        let save_into = engine_frame_save_target(extent).map(|saved| (saved, extent));
        let consumed = match WEB_COMPOSITE.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(p) => unsafe {
                    p.upload_bgra(
                        queue,
                        image_raw,
                        engine_waits,
                        &stage.bytes,
                        stage.stride as usize,
                        swizzle,
                        refresh,
                        save_into,
                    )
                },
                None => false,
            },
            Err(_) => false,
        };
        if consumed && save_into.is_some() {
            mark_engine_frame_saved();
        }
        if consumed && refresh {
            WEB_COMPOSITE_LAST.store(key, Ordering::Relaxed);
        }

        (consumed, consumed.then_some((cx as i32, cy as i32, cw, ch)))
    })
    .unwrap_or((false, None));

    if let Some(rect) = drawn {
        crate::webview::client::publish_composited_screen_rect(view, rect);
    }
    consumed
}

unsafe fn present_with_overlay(
    host: vk::PFN_vkQueuePresentKHR,
    queue: vk::Queue,
    p_present_info: *const vk::PresentInfoKHR<'_>,
) -> vk::Result {
    if p_present_info.is_null() {
        return unsafe { host(queue, p_present_info) };
    }

    let webview_live = crate::webview::client::active_view() != 0;
    if !overlay_enabled() && !probe_enabled() && !webview_live {
        return unsafe { host(queue, p_present_info) };
    }

    if !probe_enabled() && crate::framework::active_text_field() == 0 && !webview_live {
        return unsafe { host(queue, p_present_info) };
    }

    let pi = unsafe { &*p_present_info };
    let our_sc = match STATE.lock() {
        Ok(s) => s.swapchain,
        Err(_) => 0,
    };

    let Some(image_index) = (unsafe { locate_image_index(pi, our_sc) }) else {
        return unsafe { host(queue, p_present_info) };
    };

    let (extent, image_raw, format_raw) = match STATE.lock() {
        Ok(st) => (
            vk::Extent2D {
                width: st.width,
                height: st.height,
            },
            st.images.get(image_index as usize).copied().unwrap_or(0),
            st.format,
        ),
        Err(_) => (vk::Extent2D::default(), 0, 0),
    };
    let engine_waits: &[vk::Semaphore] =
        if pi.wait_semaphore_count > 0 && !pi.p_wait_semaphores.is_null() {
            unsafe {
                std::slice::from_raw_parts(pi.p_wait_semaphores, pi.wait_semaphore_count as usize)
            }
        } else {
            &[]
        };

    let mut waits_consumed = false;

    if webview_live && image_raw != 0 {
        let view = crate::webview::client::active_view();
        if view != 0
            && composite_webview_frame(queue, view, image_raw, extent, format_raw, engine_waits)
        {
            waits_consumed = true;
        }
    }

    if probe_enabled() || overlay_enabled() {
        let live = if overlay_enabled() {
            crate::framework::active_text_overlay().filter(|overlay| !overlay.text.is_empty())
        } else {
            None
        };
        let text_test = live
            .is_none()
            .then(|| std::env::var("ECLIPSE_VK_TEXT_TEST").ok())
            .flatten();
        let (draw_text, geometry) = if let Some(mut overlay) = live {
            overlay.text = mask_overlay_text(overlay.text, overlay.input_type);
            let geometry = overlay.geometry;
            (Some(overlay), Some(geometry))
        } else if let Some(text) = text_test {
            let rect = login_field_rect(extent);
            (
                Some(crate::framework::ActiveTextOverlay {
                    text,
                    geometry: (
                        rect.offset.x,
                        rect.offset.y,
                        rect.extent.width,
                        rect.extent.height,
                    ),
                    input_type: 0,
                    font_size: 25.0,
                    multiline: false,
                    text_wrapped: false,
                    text_color: -1,
                    x_alignment: 0,
                    y_alignment: 1,
                }),
                Some((
                    rect.offset.x,
                    rect.offset.y,
                    rect.extent.width,
                    rect.extent.height,
                )),
            )
        } else {
            (None, crate::framework::textbox_geometry())
        };

        let run = image_raw != 0 && (draw_text.is_some() || probe_enabled());
        if let Some(rect) = run
            .then(|| {
                select_text_probe_rect(
                    geometry,
                    extent,
                    draw_text.is_some(),
                    probe_enabled(),
                    screenshot_enabled(),
                )
            })
            .flatten()
        {
            ensure_probe(rect);

            let field_waits: &[vk::Semaphore] = if waits_consumed { &[] } else { engine_waits };
            let consumed = match PROBE.lock() {
                Ok(guard) => match guard.as_ref() {
                    Some(p) => unsafe {
                        p.capture(
                            queue,
                            image_raw,
                            field_waits,
                            draw_text.as_ref(),
                            probe_enabled(),
                        )
                    },
                    None => false,
                },
                Err(_) => false,
            };
            if consumed {
                waits_consumed = true;
            }
        }
    }

    if waits_consumed {
        let mut info = *pi;
        info.wait_semaphore_count = 0;
        info.p_wait_semaphores = std::ptr::null();

        return unsafe { host(queue, &info) };
    }

    unsafe { host(queue, p_present_info) }
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

    note_engine_present();
    let _swapchain = swapchain_lock();
    unsafe { present_with_overlay(host, queue, p_present_info) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_present_queue_is_reserved_only_when_the_family_has_a_spare_queue() {
        assert_eq!(reserve_web_present_queue(16, 1), Some(1));
        assert_eq!(reserve_web_present_queue(2, 1), Some(1));
        assert_eq!(reserve_web_present_queue(3, 2), Some(2));
        assert_eq!(reserve_web_present_queue(1, 1), None);
        assert_eq!(reserve_web_present_queue(4, 0), None);
    }

    #[test]
    fn bgra_rows_into_copies_and_swizzles_rows() {
        #[rustfmt::skip]
        let src: Vec<u8> = vec![
             1, 2, 3, 4,  5, 6, 7, 8,  0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
             9, 10, 11, 12,  13, 14, 15, 16,  0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB,
        ];

        let mut dst = vec![0u8; 16];
        assert!(bgra_rows_into(&mut dst, 8, &src, 16, 2, 8, false));
        assert_eq!(
            dst,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );

        let mut dst = vec![0u8; 16];
        assert!(bgra_rows_into(&mut dst, 8, &src, 16, 2, 8, true));
        assert_eq!(
            dst,
            vec![3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]
        );

        let mut dst = vec![0u8; 16];
        assert!(!bgra_rows_into(&mut dst, 8, &src[..8], 16, 2, 8, false));
        let mut dst = vec![0u8; 8];
        assert!(!bgra_rows_into(&mut dst, 8, &src, 16, 2, 8, false));
    }

    #[test]
    fn classify_swapchain_format_detects_bgra_rgba_and_rejects_the_rest() {
        assert_eq!(
            classify_swapchain_format(vk::Format::B8G8R8A8_UNORM.as_raw()),
            CompositeFormat::Bgra
        );
        assert_eq!(
            classify_swapchain_format(vk::Format::B8G8R8A8_SRGB.as_raw()),
            CompositeFormat::Bgra
        );
        assert_eq!(
            classify_swapchain_format(vk::Format::R8G8B8A8_UNORM.as_raw()),
            CompositeFormat::RgbaSwizzle
        );
        assert_eq!(
            classify_swapchain_format(vk::Format::R8G8B8A8_SRGB.as_raw()),
            CompositeFormat::RgbaSwizzle
        );

        assert_eq!(
            classify_swapchain_format(vk::Format::R5G6B5_UNORM_PACK16.as_raw()),
            CompositeFormat::Unsupported
        );
        assert_eq!(classify_swapchain_format(0), CompositeFormat::Unsupported);
    }

    #[test]
    fn clamp_webview_rect_crops_top_left_to_surface_and_stage() {
        assert_eq!(
            clamp_webview_rect(10, 20, 300, 200, 800, 600, 1024, 768),
            Some((10, 20, 300, 200))
        );

        assert_eq!(
            clamp_webview_rect(10, 20, 300, 200, 800, 600, 128, 64),
            Some((10, 20, 128, 64))
        );

        assert_eq!(
            clamp_webview_rect(700, 500, 300, 200, 800, 600, 1024, 768),
            Some((700, 500, 100, 100))
        );

        assert_eq!(
            clamp_webview_rect(-5, -7, 300, 200, 800, 600, 1024, 768),
            Some((0, 0, 300, 200))
        );

        assert_eq!(clamp_webview_rect(900, 0, 10, 10, 800, 600, 64, 64), None);
        assert_eq!(clamp_webview_rect(0, 0, 0, 10, 800, 600, 64, 64), None);
        assert_eq!(clamp_webview_rect(0, 0, 10, 10, 800, 600, 0, 64), None);
        assert_eq!(clamp_webview_rect(0, 0, 10, 10, 0, 0, 64, 64), None);
    }

    #[test]
    fn resolve_webview_rect_falls_back_to_the_centered_stage_rect_and_always_clamps() {
        assert_eq!(
            resolve_webview_rect(None, 800, 600, 800, 600),
            Some((0, 0, 800, 600))
        );

        assert_eq!(
            resolve_webview_rect(None, 800, 600, 400, 300),
            Some((200, 150, 400, 300))
        );

        assert_eq!(
            resolve_webview_rect(None, 800, 600, 1024, 768),
            Some((0, 0, 800, 600))
        );

        assert_eq!(
            resolve_webview_rect(Some((10, 20, 300, 200)), 800, 600, 1024, 768),
            Some((10, 20, 300, 200))
        );

        assert_eq!(
            resolve_webview_rect(Some((700, 500, 300, 200)), 800, 600, 1024, 768),
            Some((700, 500, 100, 100))
        );

        assert_eq!(
            resolve_webview_rect(Some((-5, -7, 300, 200)), 800, 600, 1024, 768),
            Some((0, 0, 300, 200))
        );

        assert_eq!(resolve_webview_rect(None, 0, 0, 800, 600), None);
        assert_eq!(resolve_webview_rect(None, 800, 600, 0, 0), None);
        assert_eq!(
            resolve_webview_rect(Some((900, 0, 10, 10)), 800, 600, 64, 64),
            None
        );
    }

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
            Some((0, 0, 300, 40))
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

    #[test]
    fn overlay_text_masks_secure_and_unknown_input_types() {
        for plain in [0, 1, 2, 3, 4, 7, 8] {
            assert_eq!(mask_overlay_text("Ab1!".to_string(), plain), "Ab1!");
        }
        for secure in [5, 6, 9, 10] {
            assert_eq!(mask_overlay_text("Ab1!".to_string(), secure), "••••");
        }
        assert_eq!(mask_overlay_text("Ab1!".to_string(), 11), "••••");
        assert_eq!(mask_overlay_text("Ab1!".to_string(), i32::MIN), "••••");
        assert_eq!(mask_overlay_text(String::new(), i32::MIN), "");
    }

    #[test]
    fn focused_text_uses_native_font_size_instead_of_field_height() {
        let source = include_str!("vk_overlay.rs");

        assert!(!source.contains(concat!("(h as f32", " * 0.55).max(8.0)")));
        assert!(source.contains("overlay_font_size(overlay.font_size)"));
        assert_eq!(overlay_font_size(14.0), 14.0);
        assert_eq!(overlay_font_size(155.0), MAX_OVERLAY_FONT_SIZE);
        assert_eq!(overlay_font_size(f32::NAN), 14.0);
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
