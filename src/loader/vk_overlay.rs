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
//!
//! 2026-07-03 (web-engine plan M3) — WEBVIEW COMPOSITE: while a challenge WebView is live
//! (`crate::webview::client::active_view() != 0`), each present also composites the helper's
//! staged BGRA frame into the presented image at the cached view rect
//! ([`composite_webview_frame`], reusing the [`Probe`] upload machinery). This seam is
//! VULKAN-ONLY at M3 (honest): the engine present is `vkQueuePresentKHR` (Mode 6 — the live-boot
//! fact above). The GL-mode composite is DEFERRED with its seam recorded: a tier-0
//! `eglSwapBuffers` interposer registered in `native_provider.rs` beside
//! `eclipse_egl_get_display` (first-strong-match), needed only if a GL-mode engine boot ever
//! meets the challenge.

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

/// A hardcoded login-field rect (x=181, y=149, w=438, h=46 in the 800×600 surface space — the value
/// `NativeTextBoxInfo` reported for the login field on 2026-06-14), clamped to the surface.
///
/// 2026-07-17: this is now reachable ONLY from the env-gated `ECLIPSE_VK_PROBE` diagnostic, which must
/// still be able to read a rect back with no field focused. It is NOT a fallback for the draw path: it
/// used to be, and that turned the engine's own "no field is focused" answer into "draw at a guess" —
/// see [`resolve_field_rect`]. (The retired 2026-06-14 note promised B3 would "use the live
/// focused-field geometry + gate on focus"; B3 delivered the geometry and neither the removal nor the
/// gate, which is precisely where the stale credential overlay lived.)
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

/// Resolve the focused field's on-screen rect from the engine's cached `NativeTextBoxInfo` geometry,
/// clamped to the surface — or `None` when there is NO live textbox session, in which case the overlay
/// must draw NOTHING.
///
/// 2026-07-17: `None` in ⇒ `None` out is the entire point. This used to fall back to a hardcoded login
/// rect, which discarded the one honest clear signal Eclipse already observes
/// (`nativeGetTextBoxInfo() == null`, recorded by `framework::record_textbox_session`): 20 s after the
/// owner reached the home screen the overlay was still compositing the credential `EditText`'s
/// still-registered text onto that hardcoded rect. No timer, no heuristic — the engine's own protocol
/// already answers "is a field live?", it was simply being overruled here.
///
/// Pure (clamp math byte-identical to the geometry arm it replaces) and unit-pinned, no GPU — the
/// [`resolve_webview_rect`] precedent (2026-07-17).
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

/// Whether the probe should capture the whole surface instead of its normal field-sized diagnostic
/// rect. This must affect readback only; it must never become the text overlay's draw rectangle.
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

/// Select the ONE rectangle for this text/probe pass.
///
/// A text draw is authorized only by a coherent live textbox geometry and always uses that geometry.
/// Probe-only readback may instead use the full surface or the historical calibration rect. Keeping
/// these branches disjoint fixes the 2026-07-17 privacy failure where `ECLIPSE_VK_SCREENSHOT=1`
/// expanded a password overlay to 800×600 and rendered the credential hundreds of pixels tall.
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

/// Convert live field text to what may be composited. Roblox's Android keyboard adapter maps engine
/// type `0` (its default/fallthrough, measured on in-game chat), types `1..=4`, `7`, and `8` to
/// non-password Android `InputType` values; types `5`, `6`, `9`, and `10` are password variants.
/// Unknown metadata still fails closed to bullets. The explicit allow-list keeps real password fields
/// masked while letting the measured Home search (`1`) and in-game chat (`0`) render normally.
fn mask_overlay_text(text: &str, input_type: i32) -> String {
    if matches!(input_type, 0..=4 | 7 | 8) {
        text.to_owned()
    } else {
        "\u{2022}".repeat(text.chars().count())
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

/// Present-rate diagnostics are process-start configuration just like the overlay/probe gates.
/// Cache the environment lookup so even a diagnostic run does not lock the process environment on
/// every game frame—the measurement must not become part of what it measures.
fn fps_probe_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("ECLIPSE_VK_FPS").is_some())
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

/// 2026-07-03 (web-engine M3): the webview composite's own reused [`Probe`] (separate from the
/// field probe so the two rects/buffers never fight; both are built via [`ensure_probe_in`]).
static WEB_COMPOSITE: Mutex<Option<Probe>> = Mutex::new(None);

/// The last `(generation << 32 | seq)` whose staged bytes were CPU-copied into the webview
/// composite buffer. Unchanged staging skips the ~3 MB row copy (the buffer already holds the
/// pixels); the GPU copy-to-image still runs EVERY present, because the engine repaints the
/// whole frame each present (the same reason the text overlay draws every present — see the
/// dated note in `present_with_overlay`).
static WEB_COMPOSITE_LAST: AtomicU64 = AtomicU64::new(0);

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
        write_probe: bool,
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
            if write_probe {
                let w = self.rect.extent.width as usize;
                let h = self.rect.extent.height as usize;
                let size = w * h * 4;
                // Explicit diagnostic only: normal overlay drawing must never persist a credential
                // field image to `/tmp`. Force alpha opaque for the compositor-independent PNG.
                {
                    let data = std::slice::from_raw_parts(self.mapped, size);
                    let mut png_rgba = data.to_vec();
                    for px in png_rgba.chunks_exact_mut(4) {
                        px[3] = 255;
                    }
                    let png = encode_png_rgba(&png_rgba, w as u32, h as u32);
                    let _ = std::fs::write("/tmp/eclipse_field_probe.png", png);
                }
                // Analyze only under the same explicit probe gate. Even a masked field's ink profile
                // can disclose credential length; a normal boot emits no such diagnostic.
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

    /// 2026-07-03 (web-engine M3): write the staged webview BGRA frame into this probe's
    /// host-visible buffer (CPU row copy, skipped when `refresh` is false — the buffer already
    /// holds the frame), then copy the buffer INTO the presented swapchain image at `self.rect` —
    /// exactly the writeback half of [`Self::capture`], without the readback. Returns `true` iff
    /// the submit consumed `engine_waits` (the caller must then present with a cleared wait
    /// list); `false` on any error (caller presents unmodified).
    ///
    /// # Safety
    /// As [`Self::capture`]: `queue` is the engine's present queue; `image_raw` the presented
    /// swapchain image; `engine_waits` the present's wait semaphores (valid for the call).
    #[allow(clippy::too_many_arguments)] // 2026-07-03: capture-shaped GPU args + the staged-source triple.
    unsafe fn upload_bgra(
        &self,
        queue: vk::Queue,
        image_raw: u64,
        engine_waits: &[vk::Semaphore],
        src: &[u8],
        src_stride: usize,
        swizzle: bool,
        refresh: bool,
    ) -> bool {
        let image = vk::Image::from_raw(image_raw);
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        // SAFETY: all handles are this probe's, on `self.device`; the standard fence-wait →
        // (CPU write) → record → submit → wait sequence. Each fallible step bails (→ caller
        // presents unmodified). The CPU write happens AFTER the fence wait, so no in-flight
        // submit reads the buffer while it is rewritten (HOST_VISIBLE|HOST_COHERENT memory).
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

/// Build (or rebuild on a rect change) the probe object held in `slot` — the shared core of the
/// field probe and (2026-07-03, M3) the webview composite; both buffers + copy regions are sized
/// to their rect. Returns `true` iff a NEW probe was built (its mapped buffer starts undefined,
/// so a webview caller must refresh its CPU contents).
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
        return false; // already current
    }
    let device = STATE.lock().map(|s| s.device).unwrap_or(0);
    if device == 0 {
        return false;
    }
    let Some(entry) = super::vulkan_wsi::host_entry() else {
        return false;
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
            "vk-overlay: overlay/composite objects built for rect"
        );
        *guard = Some(p);
        return true;
    }
    false
}

/// Build the field probe (best-effort), or rebuild it if the field `rect` changed (the login
/// layout drifts, so the focused field moves/resizes).
fn ensure_probe(rect: vk::Rect2D) {
    let _ = ensure_probe_in(&PROBE, rect);
}

// === Webview composite (web-engine plan M3, 2026-07-03) ==========================================

/// How the staged BGRA webview frame maps onto the captured swapchain format
/// (detect-don't-assume: never write garbage for an unknown format).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompositeFormat {
    /// `B8G8R8A8_{UNORM,SRGB}` — the helper's BGRA bytes copy verbatim.
    Bgra,
    /// `R8G8B8A8_{UNORM,SRGB}` — swizzle R↔B during the row copy.
    RgbaSwizzle,
    /// Anything else: one-shot WARN + skip (honest degradation, no garbage on screen).
    Unsupported,
}

/// Classify the RAW `VkFormat` value captured from the engine's swapchain create-info.
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

/// Row-copy `rows` rows of `row_bytes` from the staged frame (`src`, stride `src_stride`) into
/// the composite buffer (`dst`, stride `dst_stride`), optionally swizzling BGRA→RGBA. Pure +
/// total (`false` on any out-of-bounds geometry — never a panic on the present thread).
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
            for (d, s) in drow.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
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

/// Clamp the requested webview rect to the surface AND the staged frame's own dimensions
/// (top-left crop, no scaling at M3 — documented cut). `None` = no visible overlap (skip the
/// composite this present). Pure — unit-pinned.
#[allow(clippy::too_many_arguments)] // 2026-07-03: 8 scalars beat an ad-hoc struct for a pure clamp.
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

/// 2026-07-17: THE single answer to "where is the live WebView on screen" — the cached absolute
/// frame when the registry has one, else the centered fallback of the staged frame's own
/// dimensions, in both cases CLAMPED to the surface + stage. `None` = nothing visible (skip).
///
/// It exists because the compositor and the input hit-test used to resolve this INDEPENDENTLY and
/// disagreed: the composite fell back to a centered rect when `composited_rect()` was `None`, while
/// `graphics::webview_relative_point` bailed on `None` — so the challenge WebView (whose recorded
/// frame is degenerate: it is never measured headless, so `absolute_frame` returns `None`) DREW
/// full-window but captured NO input, and 90/90 measured presses fell through to the engine
/// beneath it. Both callers now consume this one function, so they cannot diverge again.
///
/// The CLAMPED rect is the honest one for both: `upload_bgra` blits stage pixel (0,0) to screen
/// `(cx, cy)`, so `(cx, cy)` is where the view's origin actually lands (a negative requested origin
/// SHIFTS the drawn content rather than cropping it — the M3 "top-left crop, no scaling" cut).
/// Pure — unit-pinned, no GPU. `pub(crate)` so the input path's regression guard
/// (`graphics::tests`) can compose it with the hit-test mapping it feeds.
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
            // `min` keeps both subtractions non-negative; a zero extent is refused by the clamp.
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

/// Composite the active WebView's staged helper frame into the presented swapchain image.
/// Returns `true` iff the submit consumed `engine_waits` (the caller then presents with a
/// cleared wait list — the probe's `consumed` plumbing).
///
/// Runs on the engine present thread: the ONLY client state it touches is the try-locked staging
/// buffer ([`with_latest_frame`](crate::webview::client::with_latest_frame) — contention skips
/// this present), the cached rect, and (2026-07-17) the screen rect it PUBLISHES for the input
/// hit-test; it never walks the registry or touches the socket.
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
    // 2026-07-17: `resolve_webview_rect` is the ONE resolver the input hit-test consumes too (via
    // the publish below) — the rect the user SEES is the rect the user CLICKS, by construction.
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
        let consumed = match WEB_COMPOSITE.lock() {
            Ok(guard) => match guard.as_ref() {
                // SAFETY: the probe's objects live on the engine's captured device; `queue` /
                // `engine_waits` are the engine's own present arguments (the `capture` contract).
                Some(p) => unsafe {
                    p.upload_bgra(
                        queue,
                        image_raw,
                        engine_waits,
                        &stage.bytes,
                        stage.stride as usize,
                        swizzle,
                        refresh,
                    )
                },
                None => false,
            },
            Err(_) => false,
        };
        if consumed && refresh {
            WEB_COMPOSITE_LAST.store(key, Ordering::Relaxed);
        }
        // `upload_bgra` is `true` only once the blit SUBMITTED and its fence signalled, so it is
        // an exact "these pixels are in the presented image" signal — publish the rect only then,
        // never on a skipped/failed composite (input would then aim at pixels nobody drew).
        (consumed, consumed.then_some((cx as i32, cy as i32, cw, ch)))
    })
    .unwrap_or((false, None));
    // Published OUTSIDE the `views` try-lock the closure holds: the screen-rect lock stays a leaf.
    if let Some(rect) = drawn {
        crate::webview::client::publish_composited_screen_rect(view, rect);
    }
    consumed
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
    if p_present_info.is_null() {
        // SAFETY: forwarding `(queue, p_present_info)` is the plain host present path.
        return unsafe { host(queue, p_present_info) };
    }
    // 2026-07-03 (web-engine M3): a live challenge WebView is the third composite trigger beside
    // the text overlay and the probe. The idle gate cost stays ONE atomic load per trigger (the
    // active_text_field §2.4 precedent) — zero per-present work during gameplay.
    let webview_live = crate::webview::client::active_view() != 0;
    if !overlay_enabled() && !probe_enabled() && !webview_live {
        // Overlay opted out + probe off + no webview → forward the engine's present unchanged.
        // SAFETY: forwarding `(queue, p_present_info)` is the plain host present path.
        return unsafe { host(queue, p_present_info) };
    }
    // Fast path: the overlay is on by default, but it only has work when a text field is focused
    // (or a webview is live). Otherwise skip ALL the per-present work (locks, readback) — zero
    // gameplay cost. (Cheap atomic checks before any lock.)
    if !probe_enabled() && crate::framework::active_text_field() == 0 && !webview_live {
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
            // SAFETY: per the contract `p_wait_semaphores` holds `wait_semaphore_count` entries.
            unsafe {
                std::slice::from_raw_parts(pi.p_wait_semaphores, pi.wait_semaphore_count as usize)
            }
        } else {
            &[]
        };
    // Once ANY composite submit consumes + fence-waits the engine waits, later submits and the
    // final present must run with an empty wait list (the probe's `consumed` plumbing).
    let mut waits_consumed = false;

    // --- Webview composite (M3): every present while a staged frame exists — the engine repaints
    // the whole image each present, so a skipped composite would flicker (the exact reason the
    // text overlay below also runs on every present). Unchanged staging skips only the CPU copy.
    if webview_live && image_raw != 0 {
        let view = crate::webview::client::active_view();
        if view != 0
            && composite_webview_frame(queue, view, image_raw, extent, format_raw, engine_waits)
        {
            waits_consumed = true;
        }
    }

    // Text overlay / explicit field-readback probe. Text is authorized only by one coherent snapshot
    // that binds the active widget, geometry, input type, and text. Probe-only capture may use a wider
    // diagnostic rect, but that rect can never authorize or position a text draw.
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
        let (draw_text, geometry) = if let Some(overlay) = live {
            (
                Some(mask_overlay_text(&overlay.text, overlay.input_type)),
                Some(overlay.geometry),
            )
        } else if let Some(text) = text_test {
            // Explicit developer test: no credential source is involved. Keep its historical
            // field-sized placement even when the full-frame screenshot diagnostic is also enabled.
            let rect = login_field_rect(extent);
            (
                Some(text),
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
        // Run on EVERY present when overlay-drawing (else the text flickers — the engine repaints the
        // field each frame) or when probing (so the PNG/sparkline reflect the CURRENT frame, not a stale
        // one). The readback fence-wait stalls the queue, but this only runs under the env-gated overlay/
        // probe (never in a normal/gameplay boot). No active field + not probing → plain present.
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
            // 2026-07-03 (M3): if the webview composite above already consumed + fence-waited the
            // engine waits, the probe submit must not wait on them a second time.
            let field_waits: &[vk::Semaphore] = if waits_consumed { &[] } else { engine_waits };
            let consumed = match PROBE.lock() {
                Ok(guard) => match guard.as_ref() {
                    // SAFETY: probe objects are on the engine's device; `queue`/the waits are the engine's.
                    Some(p) => unsafe {
                        p.capture(
                            queue,
                            image_raw,
                            field_waits,
                            draw_text.as_deref(),
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
        // A composite submit consumed + fence-waited the engine waits; present with them cleared so
        // the host present does not wait on already-consumed semaphores.
        let mut info = *pi;
        info.wait_semaphore_count = 0;
        info.p_wait_semaphores = std::ptr::null();
        // SAFETY: `info` copies the present info with no wait semaphores; swapchain/image arrays unchanged.
        return unsafe { host(queue, &info) };
    }
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
    // SAFETY: `HOST_QUEUE_PRESENT` holds the host `vkQueuePresentKHR` address; ABI `PFN_vkQueuePresentKHR`.
    let host: vk::PFN_vkQueuePresentKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkQueuePresentKHR>(addr) };
    // SAFETY: forwards (queue, present-info) to the host, compositing the overlay first when applicable.
    unsafe { present_with_overlay(host, queue, p_present_info) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-07-03 (web-engine M3): the webview composite's PURE parts — the row copy/swizzle and
    // the rect clamp — pinned in plain `cargo test` (no GPU pretense; the GPU half mirrors the
    // live-proven Probe writeback and is exercised at the M6 live boot).

    #[test]
    fn bgra_rows_into_copies_and_swizzles_rows() {
        // 2x2 BGRA source with a wider stride (16) than the row payload (8).
        #[rustfmt::skip]
        let src: Vec<u8> = vec![
            /* row 0 */ 1, 2, 3, 4,  5, 6, 7, 8,  0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
            /* row 1 */ 9, 10, 11, 12,  13, 14, 15, 16,  0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB,
        ];
        // Verbatim copy (B8G8R8A8 swapchain): the stride padding must never leak into dst.
        let mut dst = vec![0u8; 16];
        assert!(bgra_rows_into(&mut dst, 8, &src, 16, 2, 8, false));
        assert_eq!(
            dst,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        // Swizzled copy (R8G8B8A8 swapchain): BGRA → RGBA swaps bytes 0 and 2 of each pixel.
        let mut dst = vec![0u8; 16];
        assert!(bgra_rows_into(&mut dst, 8, &src, 16, 2, 8, true));
        assert_eq!(
            dst,
            vec![3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]
        );
        // Total on bad geometry: a source too short for the requested rows is `false`, no panic
        // (this runs on the engine present thread under panic = "abort").
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
        // Detect-don't-assume: anything else must be refused (skip + one-shot WARN), never
        // written as garbage.
        assert_eq!(
            classify_swapchain_format(vk::Format::R5G6B5_UNORM_PACK16.as_raw()),
            CompositeFormat::Unsupported
        );
        assert_eq!(classify_swapchain_format(0), CompositeFormat::Unsupported);
    }

    #[test]
    fn clamp_webview_rect_crops_top_left_to_surface_and_stage() {
        // Fully inside, stage large enough → unchanged.
        assert_eq!(
            clamp_webview_rect(10, 20, 300, 200, 800, 600, 1024, 768),
            Some((10, 20, 300, 200))
        );
        // Rect wider than the staged frame → top-left crop to the stage (no scaling at M3).
        assert_eq!(
            clamp_webview_rect(10, 20, 300, 200, 800, 600, 128, 64),
            Some((10, 20, 128, 64))
        );
        // Rect running off the surface → cropped to the surface.
        assert_eq!(
            clamp_webview_rect(700, 500, 300, 200, 800, 600, 1024, 768),
            Some((700, 500, 100, 100))
        );
        // Negative origin clamps to 0 (the visible top-left crop).
        assert_eq!(
            clamp_webview_rect(-5, -7, 300, 200, 800, 600, 1024, 768),
            Some((0, 0, 300, 200))
        );
        // No overlap / degenerate inputs → None (skip the composite, never a zero-size copy).
        assert_eq!(clamp_webview_rect(900, 0, 10, 10, 800, 600, 64, 64), None);
        assert_eq!(clamp_webview_rect(0, 0, 0, 10, 800, 600, 64, 64), None);
        assert_eq!(clamp_webview_rect(0, 0, 10, 10, 800, 600, 0, 64), None);
        assert_eq!(clamp_webview_rect(0, 0, 10, 10, 0, 0, 64, 64), None);
    }

    /// 2026-07-17: the ONE rect the composite draws AND the input hit-test maps against. The
    /// fallback arm is byte-for-byte the M3 composite's own (verified against the replaced code):
    /// this fuses the fallback + clamp that were already there, it does not change what is drawn.
    #[test]
    fn resolve_webview_rect_falls_back_to_the_centered_stage_rect_and_always_clamps() {
        // No cached frame — the MEASURED challenge-WebView case (`absolute_frame` returns None:
        // the content is never measured headless). The page still draws, so it must still click.
        assert_eq!(
            resolve_webview_rect(None, 800, 600, 800, 600),
            Some((0, 0, 800, 600))
        );
        // A stage smaller than the surface centers.
        assert_eq!(
            resolve_webview_rect(None, 800, 600, 400, 300),
            Some((200, 150, 400, 300))
        );
        // A stage larger than the surface fills it (the fallback's own `min`).
        assert_eq!(
            resolve_webview_rect(None, 800, 600, 1024, 768),
            Some((0, 0, 800, 600))
        );
        // A cached frame wins over the fallback...
        assert_eq!(
            resolve_webview_rect(Some((10, 20, 300, 200)), 800, 600, 1024, 768),
            Some((10, 20, 300, 200))
        );
        // ...and is CLAMPED — the hit-test must agree with what is on screen, not the request.
        assert_eq!(
            resolve_webview_rect(Some((700, 500, 300, 200)), 800, 600, 1024, 768),
            Some((700, 500, 100, 100))
        );
        // A negative origin lands the stage's (0,0) at the clamped origin (the M3 no-scaling cut),
        // so the clamped rect is also the honest one to subtract in the hit-test.
        assert_eq!(
            resolve_webview_rect(Some((-5, -7, 300, 200)), 800, 600, 1024, 768),
            Some((0, 0, 300, 200))
        );
        // Nothing visible → no composite → (via the publish) no input capture either.
        assert_eq!(resolve_webview_rect(None, 0, 0, 800, 600), None);
        assert_eq!(resolve_webview_rect(None, 800, 600, 0, 0), None);
        assert_eq!(
            resolve_webview_rect(Some((900, 0, 10, 10)), 800, 600, 64, 64),
            None
        );
    }

    /// 2026-07-17: no live textbox session ⇒ NO rect ⇒ the overlay draws nothing.
    ///
    /// This pins the fix for the stale masked-password overlay. MEASURED
    /// (`/tmp/eclipse-owner-manual2.log`): `onAppReady: Home` at 06:11:20.461, and at 06:11:40.333 —
    /// 20 s later, with the login long finished — `vk-overlay field-probe ink |
    /// =-#*==#-=@=-@=#= | total_ink=408`, byte-identical to the 06:10:57.778 line, i.e. the
    /// credential `EditText`'s still-registered text still being composited. The rect it was drawn at
    /// came from `field_rect`'s hardcoded `login_field_rect` fallback, which fired precisely BECAUSE
    /// the engine had answered "no field is focused" (`nativeGetTextBoxInfo() == null` ⇒ geometry
    /// `None`). The clear signal was already there; the fallback threw it away.
    ///
    /// The `login_field_rect` constant must never be reachable from this resolver: that is the
    /// difference between drawing where the engine says the field IS and drawing at a guess.
    #[test]
    fn resolve_field_rect_draws_nothing_without_a_live_textbox_session() {
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let as_tuple = |r: Option<vk::Rect2D>| {
            r.map(|r| (r.offset.x, r.offset.y, r.extent.width, r.extent.height))
        };
        // THE BUG: no live session (the engine reported a null textbox info) ⇒ nothing is drawn.
        // Pre-fix this returned the hardcoded login rect (181, 149, 438, 46) and the overlay drew.
        assert_eq!(as_tuple(resolve_field_rect(None, extent)), None);
        // A degenerate rect is not a drawable session either (and pre-fix it too fell through to the
        // hardcoded rect, via the `gw > 0 && gh > 0` match guard).
        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 0, 46)), extent)),
            None
        );
        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 438, 0)), extent)),
            None
        );
        // A live session draws at the geometry the ENGINE reported — the login layout drifts, which is
        // why the hardcoded rect was wrong even while a field WAS focused. Both values are MEASURED
        // (the boot's two `overlay/composite objects built for rect` lines, 06:09:30 and 06:09:34).
        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 300, 390, 46)), extent)),
            Some((181, 300, 390, 46))
        );
        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 438, 46)), extent)),
            Some((181, 149, 438, 46))
        );
        // ...and is CLAMPED to the surface (unchanged math — the field must never index outside it).
        assert_eq!(
            as_tuple(resolve_field_rect(Some((700, 560, 438, 46)), extent)),
            Some((700, 560, 100, 40))
        );
        // A negative origin clamps to the surface origin rather than wrapping the `as u32` cast.
        assert_eq!(
            as_tuple(resolve_field_rect(Some((-5, -7, 300, 40)), extent)),
            Some((0, 0, 300, 40))
        );
    }

    /// 2026-07-17: a full-frame probe is a READBACK choice, never a text-placement choice.
    /// The measured failure expanded a 390×46 password field to 800×600, which made the credential
    /// enormous and caused the capture path to write those altered pixels back into the live window.
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

        // Text always draws in the engine-reported live field, even with both diagnostics enabled.
        assert_eq!(
            as_tuple(select_text_probe_rect(live, extent, true, true, true)),
            Some((181, 300, 390, 46))
        );
        // With no text to draw, the same diagnostic may read the full frame.
        assert_eq!(
            as_tuple(select_text_probe_rect(live, extent, false, true, true)),
            Some((0, 0, 800, 600))
        );
        // Stale text without a coherent live session authorizes NO draw, even under the probe.
        assert_eq!(
            as_tuple(select_text_probe_rect(None, extent, true, true, true)),
            None
        );
    }

    /// Every non-password type in Roblox's engine→Android mapping renders as entered; all password
    /// variants and unknown metadata fail closed. Chat is measured as type 0, search as type 1, and
    /// username as type 7.
    #[test]
    fn overlay_text_masks_secure_and_unknown_input_types() {
        for plain in [0, 1, 2, 3, 4, 7, 8] {
            assert_eq!(mask_overlay_text("Ab1!", plain), "Ab1!");
        }
        for secure in [5, 6, 9, 10] {
            assert_eq!(mask_overlay_text("Ab1!", secure), "••••");
        }
        assert_eq!(mask_overlay_text("Ab1!", 11), "••••");
        assert_eq!(mask_overlay_text("Ab1!", i32::MIN), "••••");
        assert_eq!(mask_overlay_text("", i32::MIN), "");
    }
}
