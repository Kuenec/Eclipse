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

use ash::vk;
use ash::vk::Handle;
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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

/// `PFN_vkQueuePresentKHR` — the overlay seam. STEP A: rate-limited-logs the captured engine state plus
/// the present's swapchain/image index (proving everything needed for the draw is in hand), then
/// forwards to the host present. The synchronized overlay draw into `images[imageIndex]` is layered in
/// once Step A is live-confirmed.
///
/// # Safety
/// `queue`/`p_present_info` are the engine's, per the `vkQueuePresentKHR` contract; forwarded unchanged.
/// `p_present_info` is read (its `imageIndex`) only when non-null.
unsafe extern "system" fn eclipse_vk_queue_present_khr(
    queue: vk::Queue,
    p_present_info: *const vk::PresentInfoKHR<'_>,
) -> vk::Result {
    let n = PRESENT_COUNT.fetch_add(1, Ordering::Relaxed);
    // Log ONCE (first present) that the overlay seam is armed + the captured state is complete — per-frame
    // logging would spam at the engine's frame rate. The actual overlay draw (next increment) is silent.
    if n == 0 {
        let mut image_index = u32::MAX;
        let mut sc_count = 0u32;
        if !p_present_info.is_null() {
            // SAFETY: non-null per the check; `p_image_indices` is a `swapchain_count`-length array per
            // the `VkPresentInfoKHR` contract — we read only its first element for the log.
            let pi = unsafe { &*p_present_info };
            sc_count = pi.swapchain_count;
            if pi.swapchain_count > 0 && !pi.p_image_indices.is_null() {
                image_index = unsafe { *pi.p_image_indices };
            }
        }
        if let Ok(st) = STATE.lock() {
            tracing::info!(
                presents = n + 1,
                device_set = st.device != 0,
                swapchain_set = st.swapchain != 0,
                format = st.format,
                width = st.width,
                height = st.height,
                images = st.images.len(),
                present_swapchains = sc_count,
                image_index,
                "vk-overlay: eclipse_vk_queue_present_khr interposed (engine present)"
            );
        }
    }
    let Some(addr) = cached(&HOST_QUEUE_PRESENT) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    // SAFETY: `HOST_QUEUE_PRESENT` holds the host `vkQueuePresentKHR` address; ABI `PFN_vkQueuePresentKHR`.
    let host: vk::PFN_vkQueuePresentKHR =
        unsafe { std::mem::transmute::<usize, vk::PFN_vkQueuePresentKHR>(addr) };
    // SAFETY: forwarding the engine's `(queue, p_present_info)` unchanged is the present path.
    unsafe { host(queue, p_present_info) }
}
