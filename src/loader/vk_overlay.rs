//! Vulkan present-path text overlay (component-map F · render): a MangoHud/Steam-overlay-style
//! injection that draws Eclipse's focused login text field INTO the engine's own Vulkan swapchain
//! image, just before it is presented.
//!
//! WHY here. Post-handoff the engine OWNS the `wl_surface` and renders to it with **Vulkan** (Mode 6 —
//! confirmed by the live boot: `Vulkan: swapchain images 3 … size 800x600`), presenting via
//! `vkQueuePresentKHR`. Eclipse dropped its own `VulkanRenderer` at handoff (to release the
//! `wl_surface`), so the engine's per-frame present is the ONLY seam where Eclipse can composite over
//! the engine's picture — there is no second surface to draw on (Wayland cannot subsurface across the
//! winit-owned connection). So Eclipse interposes the engine's device-level present path the same way
//! it already interposes the instance-level WSI ([`super::vulkan_wsi`]): tier-0 shims, host fn captured
//! by name, ABI-exact forwarding.
//!
//! INTERPOSITION CHAIN. `vkGetInstanceProcAddr` (already Eclipse's, see `vulkan_wsi`) hands back our
//! `vkCreateDevice` + `vkGetDeviceProcAddr`; our `vkGetDeviceProcAddr` hands back our
//! `vkQueuePresentKHR` / `vkCreateSwapchainKHR` / `vkGetSwapchainImagesKHR`. Those capture the engine's
//! `VkDevice`, the active swapchain's format/extent + its images, then at present time draw the overlay
//! into `images[imageIndex]` and forward to the host present.
//!
//! 2026-06-14 — STEP A (this revision): the capture + a pass-through `vkQueuePresentKHR` that
//! rate-limited-logs the captured state, proving the seam fires and the swapchain image is reachable
//! BEFORE any GPU drawing is added. The synchronized overlay draw is layered on once Step A is
//! confirmed by a live boot.

use ab_glyph::{Font, FontVec, ScaleFont};
use ash::vk;
use ash::vk::Handle;
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// The system font for the overlay text, discovered once (reuses `graphics`'s portable `fc-match`/
/// font-dir discovery). `None` if no font is found (overlay then draws nothing).
fn overlay_font() -> Option<&'static FontVec> {
    static FONT: OnceLock<Option<FontVec>> = OnceLock::new();
    FONT.get_or_init(|| {
        let path = crate::graphics::discover_font_path()?;
        let bytes = std::fs::read(path).ok()?;
        FontVec::try_from_vec(bytes).ok()
    })
    .as_ref()
}

/// Encode `rgba` (`w`×`h`, row-major `R8G8B8A8`) as a minimal **uncompressed** PNG — a self-contained,
/// dependency-free image encoder (signature + IHDR + one IDAT of a zlib stream using stored/uncompressed
/// DEFLATE blocks + IEND, with CRC32 + Adler32). Used by the field probe to write a compositor-independent
/// screenshot the dev host can view (the buried Eclipse window can't be captured with `grim`). Not for
/// shipping pixels — bytes are bigger than a compressed PNG, but it adds zero deps. 2026-06-14.
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
    let mut out = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]; // PNG signature
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, color type 6 (RGBA), deflate, no filter, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    // Raw scanlines, each prefixed with filter byte 0 (none).
    let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
        raw.extend_from_slice(row);
    }
    // zlib stream: header + stored DEFLATE blocks (<=65535 each) + Adler32 of the raw data.
    let mut zlib = vec![0x78u8, 0x01];
    let mut i = 0;
    while i < raw.len() {
        let block = (raw.len() - i).min(65535);
        let bfinal = u8::from(i + block >= raw.len());
        zlib.push(bfinal); // BFINAL bit, BTYPE=00 (stored)
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

/// Alpha-blend `text` (white) onto the RGBA `buf` (`w`×`h`, row-major `R8G8B8A8`) — left-aligned,
/// vertically centered, at a scale that fits the field height. This is how the overlay draws host-typed
/// text onto the engine's actual field pixels (preserving its background/border). No-op if no font / no
/// text. 2026-06-14.
fn draw_text_onto_rgba(buf: &mut [u8], w: u32, h: u32, text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(font) = overlay_font() else {
        return;
    };
    let scale = (h as f32 * 0.55).max(8.0);
    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let baseline_y = (h as f32 - scale) * 0.5 + ascent;
    let mut pen_x = 10.0f32; // left inset
    for ch in text.chars() {
        // Masked-password dot: draw a vertically-centered filled circle (the `•` GLYPH renders too high
        // in the EM box). Keeps the password masked while looking like a real password field.
        if ch == '\u{2022}' {
            let r = (scale * 0.13).max(2.0);
            let cxi = (pen_x + r) as i32;
            let cyi = (h as f32 * 0.5) as i32;
            let ri = r as i32;
            for dy in -ri..=ri {
                for dx in -ri..=ri {
                    if (dx * dx + dy * dy) as f32 <= r * r {
                        let px = cxi + dx;
                        let py = cyi + dy;
                        if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                            let idx = ((py as u32 * w + px as u32) * 4) as usize;
                            if idx + 2 < buf.len() {
                                buf[idx] = 255;
                                buf[idx + 1] = 255;
                                buf[idx + 2] = 255;
                            }
                        }
                    }
                }
            }
            pen_x += r * 3.0; // dot spacing
            continue;
        }
        let gid = scaled.glyph_id(ch);
        let advance = scaled.h_advance(gid);
        let glyph = gid.with_scale_and_position(scale, ab_glyph::point(pen_x, baseline_y));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, c| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                    let idx = ((py as u32 * w + px as u32) * 4) as usize;
                    let a = c.clamp(0.0, 1.0);
                    // White text, straight alpha-blended over the existing (field) pixel.
                    for k in 0..3 {
                        let bg = f32::from(buf[idx + k]);
                        buf[idx + k] = (255.0 * a + bg * (1.0 - a)) as u8;
                    }
                }
            });
        }
        pen_x += advance;
    }
    // Blinking caret at the cursor (end of the text): a 2px white bar spanning the text height, toggled
    // ~every 0.5 s (per-draw counter; the overlay draws ~60×/s while a field is focused).
    static BLINK: AtomicU64 = AtomicU64::new(0);
    if (BLINK.fetch_add(1, Ordering::Relaxed) / 30).is_multiple_of(2) {
        let cx = pen_x as i32 + 1;
        let y0 = (baseline_y - scale * 0.72).max(0.0) as u32;
        let y1 = ((baseline_y + scale * 0.08) as u32).min(h);
        for cy in y0..y1 {
            for dx in 0..2 {
                let px = cx + dx;
                if px >= 0 && (px as u32) < w {
                    let idx = ((cy * w + px as u32) * 4) as usize;
                    if idx + 2 < buf.len() {
                        buf[idx] = 255;
                        buf[idx + 1] = 255;
                        buf[idx + 2] = 255;
                    }
                }
            }
        }
    }
}

/// Whether the present-path text overlay is enabled — ON by default (it makes host-typed text visible in
/// focused fields, e.g. login), with an opt-out (`ECLIPSE_NO_VK_OVERLAY`) for debugging. When on, the
/// overlay only does GPU work while a text field is focused (`present_with_overlay`'s fast path), so it
/// is zero-cost during gameplay.
fn overlay_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("ECLIPSE_NO_VK_OVERLAY").is_none())
}

// Host device-level command pointers, captured (as their address in a `usize`-in-`u64`) the first time
// the engine resolves each name through our `vkGetInstanceProcAddr`/`vkGetDeviceProcAddr`. `0` =
// unresolved (the matching shim then returns a clean Vulkan failure, never UB). Mirrors the
// `HOST_PDSC`/`HOST_PDSC2` pattern in `super::vulkan_wsi`.
static HOST_GDPA: AtomicU64 = AtomicU64::new(0);
static HOST_CREATE_DEVICE: AtomicU64 = AtomicU64::new(0);
static HOST_QUEUE_PRESENT: AtomicU64 = AtomicU64::new(0);
static HOST_CREATE_SWAPCHAIN: AtomicU64 = AtomicU64::new(0);
static HOST_GET_SWAPCHAIN_IMAGES: AtomicU64 = AtomicU64::new(0);
static PRESENT_COUNT: AtomicU64 = AtomicU64::new(0);

// The engine's `VkInstance` (captured in `vulkan_wsi::eclipse_vk_create_instance`) and `VkPhysicalDevice`
// + the first requested queue-family index (captured in `eclipse_vk_create_device`). Raw `u64`/`u32` so
// they are `Send`. Needed to build an `ash::Instance`/`ash::Device` for the overlay and a command pool.
static INSTANCE: AtomicU64 = AtomicU64::new(0);
static PHYSICAL_DEVICE: AtomicU64 = AtomicU64::new(0);
static QUEUE_FAMILY: AtomicU32 = AtomicU32::new(u32::MAX);

/// Capture the engine's `VkInstance` (called from `vulkan_wsi::eclipse_vk_create_instance` on success).
pub(crate) fn set_instance(instance: vk::Instance) {
    INSTANCE.store(instance.as_raw(), Ordering::Relaxed);
}

/// The engine Vulkan objects the overlay needs, captured by the interposed device/swapchain commands.
/// All handles are stored as their raw `u64` ([`Handle::as_raw`]) so the `static` is `Send` (raw
/// dispatchable handles are pointers, which are not `Send`); they are reconstructed with
/// [`Handle::from_raw`] at use. One swapchain is tracked at a time — the engine recreates it on resize
/// ([`eclipse_vk_create_swapchain_khr`] overwrites), and a present against a different swapchain simply
/// skips the overlay that frame (safe).
#[derive(Default)]
struct OverlayState {
    /// The engine's `VkDevice` raw handle (`0` until `vkCreateDevice` is captured).
    device: u64,
    /// The active `VkSwapchainKHR` raw handle (`0` until captured).
    swapchain: u64,
    /// The swapchain's `VkFormat` (raw `i32`).
    format: i32,
    /// The swapchain image extent.
    width: u32,
    height: u32,
    /// The swapchain's `VkImage` raw handles, indexed by the present `imageIndex`.
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

/// Type-erase a resolved host command pointer to the `usize` address we cache (or `0` for none).
fn pfn_to_addr(p: vk::PFN_vkVoidFunction) -> u64 {
    p.map_or(0, |f| f as usize as u64)
}

/// Load a cached host command address as a `usize`, or `None` if unresolved.
fn cached(a: &AtomicU64) -> Option<usize> {
    match a.load(Ordering::Relaxed) {
        0 => None,
        v => Some(v as usize),
    }
}

/// Instance-level interception, called from [`super::vulkan_wsi::eclipse_vk_get_instance_proc_addr`]
/// for every name it does not itself handle. Returns `Some(shim)` for the two commands we must wrap to
/// reach the device/present path (`vkCreateDevice`, `vkGetDeviceProcAddr`), caching the host version
/// resolved via the engine's instance; `None` to let the caller forward to the host loader.
///
/// # Safety
/// `name` is a valid command name; `host_gipa` is the host `vkGetInstanceProcAddr`; `instance` is the
/// engine's opaque handle, forwarded unchanged. Only `name` is read.
pub unsafe fn intercept_instance_proc(
    instance: vk::Instance,
    name: &CStr,
    host_gipa: vk::PFN_vkGetInstanceProcAddr,
) -> Option<vk::PFN_vkVoidFunction> {
    if name == c"vkCreateDevice" {
        // SAFETY: forwarding `(instance, name)` to the host gipa is the standard resolution path.
        let host = unsafe { host_gipa(instance, name.as_ptr()) };
        HOST_CREATE_DEVICE.store(pfn_to_addr(host), Ordering::Relaxed);
        // SAFETY: `eclipse_vk_create_device` has the exact `PFN_vkCreateDevice` ABI; transmuting to the
        // type-erased `PFN_vkVoidFunction` is the standard proc-addr return shape (as in `vulkan_wsi`).
        return Some(Some(unsafe {
            std::mem::transmute::<vk::PFN_vkCreateDevice, unsafe extern "system" fn()>(
                eclipse_vk_create_device,
            )
        }));
    }
    if name == c"vkGetDeviceProcAddr" {
        // SAFETY: as above.
        let host = unsafe { host_gipa(instance, name.as_ptr()) };
        HOST_GDPA.store(pfn_to_addr(host), Ordering::Relaxed);
        // SAFETY: `eclipse_vk_get_device_proc_addr` has the exact `PFN_vkGetDeviceProcAddr` ABI.
        return Some(Some(unsafe {
            std::mem::transmute::<vk::PFN_vkGetDeviceProcAddr, unsafe extern "system" fn()>(
                eclipse_vk_get_device_proc_addr,
            )
        }));
    }
    None
}

/// `PFN_vkGetDeviceProcAddr` — Eclipse-owned tier-0 override. Hands back our present-path shims
/// (`vkQueuePresentKHR`, `vkCreateSwapchainKHR`, `vkGetSwapchainImagesKHR`) by name, caching the host
/// version for each, and forwards every other name to the host `vkGetDeviceProcAddr`. Returns `None`
/// when the host gdpa is unavailable or `p_name` is null.
///
/// # Safety
/// `p_name` is the Vulkan-supplied NUL-terminated command name (or null); `device` is the engine's
/// opaque handle forwarded unchanged. Only the C string at `p_name` is read.
unsafe extern "system" fn eclipse_vk_get_device_proc_addr(
    device: vk::Device,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    let host_gdpa_addr = cached(&HOST_GDPA)?;
    // SAFETY: `HOST_GDPA` holds the address the host gipa returned for `vkGetDeviceProcAddr`; its ABI is
    // `PFN_vkGetDeviceProcAddr`. Transmuting the cached address to that fn type is exact.
    let host_gdpa: vk::PFN_vkGetDeviceProcAddr =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkGetDeviceProcAddr>(host_gdpa_addr) };
    // SAFETY: `p_name` is non-null (checked) and a valid NUL-terminated command name.
    let name = unsafe { CStr::from_ptr(p_name) };

    if name == c"vkQueuePresentKHR" {
        // SAFETY: forwarding `(device, name)` to the host gdpa resolves the real device command.
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_QUEUE_PRESENT.store(pfn_to_addr(host), Ordering::Relaxed);
        // SAFETY: `eclipse_vk_queue_present_khr` has the exact `PFN_vkQueuePresentKHR` ABI.
        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkQueuePresentKHR, unsafe extern "system" fn()>(
                eclipse_vk_queue_present_khr,
            )
        });
    }
    if name == c"vkCreateSwapchainKHR" {
        // SAFETY: as above.
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_CREATE_SWAPCHAIN.store(pfn_to_addr(host), Ordering::Relaxed);
        // SAFETY: `eclipse_vk_create_swapchain_khr` has the exact `PFN_vkCreateSwapchainKHR` ABI.
        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkCreateSwapchainKHR, unsafe extern "system" fn()>(
                eclipse_vk_create_swapchain_khr,
            )
        });
    }
    if name == c"vkGetSwapchainImagesKHR" {
        // SAFETY: as above.
        let host = unsafe { host_gdpa(device, p_name) };
        HOST_GET_SWAPCHAIN_IMAGES.store(pfn_to_addr(host), Ordering::Relaxed);
        // SAFETY: `eclipse_vk_get_swapchain_images_khr` has the exact `PFN_vkGetSwapchainImagesKHR` ABI.
        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkGetSwapchainImagesKHR, unsafe extern "system" fn()>(
                eclipse_vk_get_swapchain_images_khr,
            )
        });
    }
    // SAFETY: any name we do not intercept is forwarded to the host gdpa exactly as the loader expects.
    unsafe { host_gdpa(device, p_name) }
}

/// `PFN_vkCreateDevice` — forwards to the host, then captures the created `VkDevice` for the overlay.
///
/// # Safety
/// All arguments are the engine's, per the `vkCreateDevice` contract; forwarded unchanged. `p_device`
/// is read only on success.
unsafe extern "system" fn eclipse_vk_create_device(
    physical_device: vk::PhysicalDevice,
    p_create_info: *const vk::DeviceCreateInfo<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_device: *mut vk::Device,
) -> vk::Result {
    let Some(addr) = cached(&HOST_CREATE_DEVICE) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    // SAFETY: `HOST_CREATE_DEVICE` holds the host `vkCreateDevice` address; its ABI is `PFN_vkCreateDevice`.
    let host: vk::PFN_vkCreateDevice =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkCreateDevice>(addr) };
    // SAFETY: forwarding the engine's create args unchanged to the host `vkCreateDevice`.
    let r = unsafe { host(physical_device, p_create_info, p_allocator, p_device) };
    if r == vk::Result::SUCCESS && !p_device.is_null() {
        // SAFETY: on success `p_device` points to a valid `VkDevice` written by the host.
        let device = unsafe { *p_device };
        // Capture the physical device + the first requested queue family (for the overlay's command pool;
        // the present queue is, on Roblox's single-queue setup, from this graphics+present family).
        PHYSICAL_DEVICE.store(physical_device.as_raw(), Ordering::Relaxed);
        if !p_create_info.is_null() {
            // SAFETY: the engine's `VkDeviceCreateInfo` is valid per the contract; its
            // `p_queue_create_infos` holds `queue_create_info_count` entries when count > 0.
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

/// `PFN_vkCreateSwapchainKHR` — forwards to the host, then captures the new swapchain's handle, format,
/// and extent for the overlay (and clears the stale image list).
///
/// # Safety
/// All arguments are the engine's, per the `vkCreateSwapchainKHR` contract; forwarded unchanged.
/// `p_create_info`/`p_swapchain` are read only on success.
unsafe extern "system" fn eclipse_vk_create_swapchain_khr(
    device: vk::Device,
    p_create_info: *const vk::SwapchainCreateInfoKHR<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_swapchain: *mut vk::SwapchainKHR,
) -> vk::Result {
    let Some(addr) = cached(&HOST_CREATE_SWAPCHAIN) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    // SAFETY: `HOST_CREATE_SWAPCHAIN` holds the host fn address; its ABI is `PFN_vkCreateSwapchainKHR`.
    let host: vk::PFN_vkCreateSwapchainKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkCreateSwapchainKHR>(addr) };
    // SAFETY: forwarding the engine's create args unchanged to the host `vkCreateSwapchainKHR`.
    let r = unsafe { host(device, p_create_info, p_allocator, p_swapchain) };
    if r == vk::Result::SUCCESS && !p_create_info.is_null() && !p_swapchain.is_null() {
        // SAFETY: on success both pointers are valid (the engine's create-info; the host-written handle).
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

/// `PFN_vkGetSwapchainImagesKHR` — forwards to the host, then (on the image-fetch call, `p_images`
/// non-null) captures the swapchain's images for index→image lookup at present.
///
/// # Safety
/// All arguments are the engine's, per the contract; forwarded unchanged. The output array is read only
/// when both `p_images` and `p_count` are non-null and the call succeeded.
unsafe extern "system" fn eclipse_vk_get_swapchain_images_khr(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    p_swapchain_image_count: *mut u32,
    p_swapchain_images: *mut vk::Image,
) -> vk::Result {
    let Some(addr) = cached(&HOST_GET_SWAPCHAIN_IMAGES) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    // SAFETY: `HOST_GET_SWAPCHAIN_IMAGES` holds the host fn address; ABI `PFN_vkGetSwapchainImagesKHR`.
    let host: vk::PFN_vkGetSwapchainImagesKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkGetSwapchainImagesKHR>(addr) };
    // SAFETY: forwarding the engine's args unchanged to the host fn.
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
        // SAFETY: on a successful image-fetch the host wrote `*p_count` images into the `p_images` array.
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

/// The login text field's on-screen rect (from `NativeTextBoxInfo`: x=181, y=149, w=438, h=46 in the
/// 800×600 surface space — matches the focused field), clamped to the surface. STEP B1 hardcodes this to
/// prove compositing; B3 will use the live focused-field geometry + gate on focus. 2026-06-14.
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

/// The focused field's on-screen rect: the engine's LIVE `NativeTextBoxInfo` geometry (cached by the main
/// thread via `framework::textbox_geometry`, clamped to the surface) so the overlay text lands where the
/// field actually is (the login layout drifts). Falls back to the hardcoded login rect (test/diagnostic).
fn field_rect(extent: vk::Extent2D) -> vk::Rect2D {
    // Diagnostic: ECLIPSE_VK_SCREENSHOT → capture the WHOLE frame (the probe then writes a full-screen
    // PNG so the dev host can see the current UI + find drifted button/field positions).
    static SHOT: OnceLock<bool> = OnceLock::new();
    if *SHOT.get_or_init(|| std::env::var_os("ECLIPSE_VK_SCREENSHOT").is_some()) {
        return vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
    }
    match crate::framework::textbox_geometry() {
        Some((gx, gy, gw, gh)) if gw > 0 && gh > 0 => {
            let x = (gx.max(0) as u32).min(extent.width.saturating_sub(1));
            let y = (gy.max(0) as u32).min(extent.height.saturating_sub(1));
            let w = gw.min(extent.width - x).max(1);
            let h = gh.min(extent.height - y).max(1);
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
        _ => login_field_rect(extent),
    }
}

/// Find the index of the engine's `our_sc` swapchain in the present info + the image index it presents.
/// `None` if the present does not include our swapchain (e.g. before capture, or a different swapchain).
///
/// # Safety
/// `pi`'s `p_swapchains`/`p_image_indices` are `swapchain_count`-length arrays per the `VkPresentInfoKHR`
/// contract.
unsafe fn locate_image_index(pi: &vk::PresentInfoKHR<'_>, our_sc: u64) -> Option<u32> {
    if our_sc == 0
        || pi.swapchain_count == 0
        || pi.p_swapchains.is_null()
        || pi.p_image_indices.is_null()
    {
        return None;
    }
    let n = pi.swapchain_count as usize;
    // SAFETY: per the contract both arrays hold `swapchain_count` entries.
    let swapchains = unsafe { std::slice::from_raw_parts(pi.p_swapchains, n) };
    let indices = unsafe { std::slice::from_raw_parts(pi.p_image_indices, n) };
    swapchains
        .iter()
        .position(|sc| sc.as_raw() == our_sc)
        .map(|i| indices[i])
}

/// Whether the field-readback probe is enabled (env `ECLIPSE_VK_PROBE`, cached). A diagnostic that
/// writes a PNG of the engine's rendered login-field rect — compositor-independent visual verification.
fn probe_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("ECLIPSE_VK_PROBE").is_some())
}

/// The first host-visible+coherent memory type supporting `type_bits`, or `None`.
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

/// Field-readback diagnostic (env `ECLIPSE_VK_PROBE`): copies the login-field rect out of the engine's
/// presented swapchain image into a host-visible buffer and writes `/tmp/eclipse_field_probe.png` — a
/// compositor-INDEPENDENT way to SEE exactly what the engine renders in the field (the dev host can't
/// screenshot the buried window). Built once on the engine's captured device; capture is rate-limited.
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
// SAFETY: `mapped` is read only on the engine present thread under the `PROBE` mutex; all handles are
// device-owned + process-lifetime. `Send` is required for the `static Mutex<Option<Probe>>`.
unsafe impl Send for Probe {}

impl Probe {
    /// Build the readback objects on the engine's captured device for `rect` (the field region).
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
        // SAFETY: live handles captured from the engine; loaders build Rust dispatch wrappers only.
        let instance =
            unsafe { ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(instance_raw)) };
        let device =
            unsafe { ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw(device_raw)) };
        // SAFETY: `physical_raw` is the engine's physical device; reads memory properties only.
        let mem_props = unsafe {
            instance
                .get_physical_device_memory_properties(vk::PhysicalDevice::from_raw(physical_raw))
        };
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(queue_family);
        // SAFETY: device valid; create-info outlives the call.
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }.ok()?;
        let cleanup_pool = |device: &ash::Device| {
            // SAFETY: `command_pool` is valid; freed exactly once on a failure path.
            unsafe { device.destroy_command_pool(command_pool, None) };
        };
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: pool valid; writes one command buffer.
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
        // SAFETY: device valid; fence SIGNALED so the first capture's wait returns at once.
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
        // SAFETY: device valid; create-info outlives the call.
        let buffer = match unsafe { device.create_buffer(&buf_info, None) } {
            Ok(b) => b,
            Err(_) => {
                // SAFETY: fence + pool valid; free before bailing.
                unsafe { device.destroy_fence(fence, None) };
                cleanup_pool(&device);
                return None;
            }
        };
        // SAFETY: `buffer` is valid.
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let cleanup_buf = |device: &ash::Device| {
            // SAFETY: all three handles valid; freed once on a failure path.
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
        // SAFETY: device valid; allocates the buffer's backing.
        let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(_) => {
                cleanup_buf(&device);
                return None;
            }
        };
        // SAFETY: buffer + memory valid; bind at offset 0.
        if unsafe { device.bind_buffer_memory(buffer, memory, 0) }.is_err() {
            // SAFETY: free the just-allocated memory + the rest.
            unsafe { device.free_memory(memory, None) };
            cleanup_buf(&device);
            return None;
        }
        // SAFETY: memory is host-visible (chosen above); map the whole range for the process lifetime.
        let mapped =
            match unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) } {
                Ok(p) => p.cast::<u8>(),
                Err(_) => {
                    // SAFETY: free memory + buffer + fence + pool.
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

    /// Copy the field rect out of `image_raw` (ordered after the engine render via `engine_waits`), read
    /// it back, and write `/tmp/eclipse_field_probe.png`. Returns `true` if the engine wait semaphores
    /// were consumed (so the caller must present with them cleared); `false` on any error (caller
    /// presents unmodified).
    ///
    /// # Safety
    /// `queue` is the engine's present queue; `image_raw` is the presented swapchain image; `engine_waits`
    /// are the present's wait semaphores (valid for the call).
    unsafe fn capture(
        &self,
        queue: vk::Queue,
        image_raw: u64,
        engine_waits: &[vk::Semaphore],
        draw_text: Option<&str>,
    ) -> bool {
        let image = vk::Image::from_raw(image_raw);
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        // SAFETY: all handles are this probe's, on `self.device`; the standard fence-wait → record →
        // submit → wait sequence. Each fallible step bails (→ caller presents unmodified).
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
            // Read-modify-write: draw the host-typed text onto the read-back field pixels (preserving the
            // engine's background + border), then copy the result BACK into the swapchain image — Eclipse
            // compositing the field's text, which is the Android platform's job. The readback already
            // consumed `engine_waits` + fence-completed, so the image is back in PRESENT_SRC and the
            // writeback needs no semaphore wait.
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
                    // `region` (the field-rect BufferImageCopy) is direction-agnostic — reuse it for the
                    // buffer→image copy.
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
            let w = self.rect.extent.width as usize;
            let h = self.rect.extent.height as usize;
            let size = w * h * 4;
            // Compositor-independent screenshot: write a PNG of the captured rect EVERY capture (so it
            // reflects the CURRENT frame — `grim` can't reach the buried window). Force alpha opaque.
            {
                let data = std::slice::from_raw_parts(self.mapped, size);
                let mut png_rgba = data.to_vec();
                for px in png_rgba.chunks_exact_mut(4) {
                    px[3] = 255;
                }
                let png = encode_png_rgba(&png_rgba, w as u32, h as u32);
                let _ = std::fs::write("/tmp/eclipse_field_probe.png", png);
            }
            // Analyze the readback into an ASCII ink-profile sparkline (light glyphs on the dark field).
            // Rate-limited (~once/sec) so per-frame overlay drawing doesn't spam the log.
            static LOG_TICK: AtomicU64 = AtomicU64::new(0);
            if !LOG_TICK.fetch_add(1, Ordering::Relaxed).is_multiple_of(60) {
                return true;
            }
            let data = std::slice::from_raw_parts(self.mapped, size);
            const BUCKETS: usize = 64;
            let mut col_ink = [0u32; BUCKETS];
            let mut total_ink = 0u32;
            let (y0, y1) = (h * 3 / 10, h * 7 / 10); // middle band → the text row, not the borders
            for y in y0..y1 {
                for x in 0..w {
                    let i = (y * w + x) * 4;
                    let lum =
                        (u32::from(data[i]) + u32::from(data[i + 1]) + u32::from(data[i + 2])) / 3;
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
                    levels[(c as usize * (levels.len() - 1) / max as usize).min(levels.len() - 1)]
                })
                .collect();
            tracing::info!(total_ink, "vk-overlay field-probe ink |{spark}|");
        }
        true
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // SAFETY: all handles are this probe's on `self.device`; idle first, then free our objects only.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
        }
    }
}

/// Build the probe (best-effort), or rebuild it if the field `rect` changed (the login layout drifts, so
/// the focused field moves/resizes — the probe's buffer + copy region are sized to the rect).
fn ensure_probe(rect: vk::Rect2D) {
    let Ok(mut guard) = PROBE.lock() else { return };
    if guard.as_ref().is_some_and(|p| {
        p.rect.offset.x == rect.offset.x
            && p.rect.offset.y == rect.offset.y
            && p.rect.extent.width == rect.extent.width
            && p.rect.extent.height == rect.extent.height
    }) {
        return; // already current
    }
    let device = STATE.lock().map(|s| s.device).unwrap_or(0);
    if device == 0 {
        return;
    }
    let Some(entry) = super::vulkan_wsi::host_entry() else {
        return;
    };
    // Drop any stale probe (its Drop idles + frees) BEFORE building the new one for the changed rect.
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
            "vk-overlay: field overlay/probe built for field rect"
        );
        *guard = Some(p);
    }
}

/// Composite the overlay into the presented image (if our swapchain is being presented + the renderer is
/// ready), then call the host present — rewriting its wait-semaphore list to our overlay-done semaphore
/// so the present waits for our draw. Any failure falls through to an unmodified host present.
///
/// # Safety
/// `host` is the host `vkQueuePresentKHR`; `queue`/`p_present_info` are the engine's, forwarded (possibly
/// with a rewritten wait-semaphore list pointing at our own semaphore, alive for the call).
unsafe fn present_with_overlay(
    host: vk::PFN_vkQueuePresentKHR,
    queue: vk::Queue,
    p_present_info: *const vk::PresentInfoKHR<'_>,
) -> vk::Result {
    if p_present_info.is_null() || (!overlay_enabled() && !probe_enabled()) {
        // Overlay opted out + probe off, or null info → forward the engine's present unchanged.
        // SAFETY: forwarding `(queue, p_present_info)` is the plain host present path.
        return unsafe { host(queue, p_present_info) };
    }
    // Fast path: the overlay is on by default, but it only has work when a text field is focused. With no
    // focused field and no probe, skip ALL the per-present work (locks, readback) — zero gameplay cost.
    // (A cheap atomic check before any lock.)
    if !probe_enabled() && crate::framework::active_text_field() == 0 {
        // SAFETY: plain host present — nothing to composite this frame.
        return unsafe { host(queue, p_present_info) };
    }
    // SAFETY: non-null per the check; a valid `VkPresentInfoKHR` per the contract.
    let pi = unsafe { &*p_present_info };
    let our_sc = match STATE.lock() {
        Ok(s) => s.swapchain,
        Err(_) => 0,
    };
    // SAFETY: `pi`'s arrays are per-contract; `locate_image_index` reads them bounded by `swapchain_count`.
    let Some(image_index) = (unsafe { locate_image_index(pi, our_sc) }) else {
        return unsafe { host(queue, p_present_info) };
    };

    // Field-readback probe takes priority over the draw: copy the field rect out of the presented image,
    // write the PNG, then present with the engine wait semaphores cleared (the probe submit consumed +
    // fence-waited them). Rate-limited — the readback fence-wait stalls the queue, so do it ~twice/sec.
    if probe_enabled() || overlay_enabled() {
        let (extent, image_raw) = match STATE.lock() {
            Ok(st) => (
                vk::Extent2D {
                    width: st.width,
                    height: st.height,
                },
                st.images.get(image_index as usize).copied().unwrap_or(0),
            ),
            Err(_) => (vk::Extent2D::default(), 0),
        };
        // Text to draw onto the field: the focused field's live text (when overlay enabled), or a
        // hardcoded test string (env `ECLIPSE_VK_TEXT_TEST`) so the render path can be verified without a
        // focused field. `None` → readback-only (probe diagnostic).
        let draw_text: Option<String> = if overlay_enabled() {
            crate::framework::active_text_field_text()
                .filter(|s| !s.is_empty())
                .or_else(|| std::env::var("ECLIPSE_VK_TEXT_TEST").ok())
        } else {
            None
        };
        // SECURITY: a password field (engine `textInputType == 5`, confirmed live: the login password
        // step) must NEVER render plaintext — mask it to one `•` per character. (Username is type 7.)
        const SECURE_INPUT_TYPE: i32 = 5;
        let draw_text = draw_text.map(|s| {
            if crate::framework::textbox_input_type() == SECURE_INPUT_TYPE {
                "\u{2022}".repeat(s.chars().count())
            } else {
                s
            }
        });
        // Run on EVERY present when overlay-drawing (else the text flickers — the engine repaints the
        // field each frame) or when probing (so the PNG/sparkline reflect the CURRENT frame, not a stale
        // one). The readback fence-wait stalls the queue, but this only runs under the env-gated overlay/
        // probe (never in a normal/gameplay boot). No active field + not probing → plain present.
        let run = image_raw != 0 && (draw_text.is_some() || probe_enabled());
        if !run {
            return unsafe { host(queue, p_present_info) };
        }
        // Use the focused field's LIVE geometry (cached from the engine by the main thread) so the text
        // lands where the field actually is — the login layout drifts. Fall back to the hardcoded login
        // rect (test/diagnostic). Clamp to the surface.
        let rect = field_rect(extent);
        ensure_probe(rect);
        let engine_waits: &[vk::Semaphore] = if pi.wait_semaphore_count > 0
            && !pi.p_wait_semaphores.is_null()
        {
            // SAFETY: per the contract `p_wait_semaphores` holds `wait_semaphore_count` entries.
            unsafe {
                std::slice::from_raw_parts(pi.p_wait_semaphores, pi.wait_semaphore_count as usize)
            }
        } else {
            &[]
        };
        let consumed = match PROBE.lock() {
            Ok(guard) => match guard.as_ref() {
                // SAFETY: probe objects are on the engine's device; `queue`/`engine_waits` are the engine's.
                Some(p) => unsafe {
                    p.capture(queue, image_raw, engine_waits, draw_text.as_deref())
                },
                None => false,
            },
            Err(_) => false,
        };
        if consumed {
            // The probe consumed + fence-waited the engine waits; present with them cleared so the host
            // present does not wait on already-consumed semaphores.
            let mut info = *pi;
            info.wait_semaphore_count = 0;
            info.p_wait_semaphores = std::ptr::null();
            // SAFETY: `info` copies the present info with no wait semaphores; swapchain/image arrays unchanged.
            return unsafe { host(queue, &info) };
        }
        return unsafe { host(queue, p_present_info) };
    }

    // Neither probe nor overlay matched (e.g. our swapchain not in this present) → unmodified present.
    // SAFETY: forwarding `(queue, p_present_info)` is the plain host present path.
    unsafe { host(queue, p_present_info) }
}

/// `PFN_vkQueuePresentKHR` — the overlay seam. Logs ONCE that it is armed, then composites the overlay
/// into the presented image (via [`present_with_overlay`]) and forwards to the host present.
///
/// # Safety
/// `queue`/`p_present_info` are the engine's, per the `vkQueuePresentKHR` contract; forwarded to the host
/// (possibly with a rewritten wait-semaphore list — see [`present_with_overlay`]).
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
    // Present-rate (fps) every ~120 frames — measures any overlay overhead (it composites only while a
    // field is focused). Env-gated diagnostic to avoid steady-state logging.
    if std::env::var_os("ECLIPSE_VK_FPS").is_some() && n.is_multiple_of(120) {
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
    // SAFETY: `HOST_QUEUE_PRESENT` holds the host `vkQueuePresentKHR` address; ABI `PFN_vkQueuePresentKHR`.
    let host: vk::PFN_vkQueuePresentKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkQueuePresentKHR>(addr) };
    // SAFETY: forwards (queue, present-info) to the host, compositing the overlay first when applicable.
    unsafe { present_with_overlay(host, queue, p_present_info) }
}
