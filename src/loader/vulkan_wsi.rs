//! Eclipse-owned tier-0 Vulkan WSI translation seam (render Phase 6).
//!
//! 2026-06-13: With the guest device API level at 28 (render Phase 5), Roblox's engine ATTEMPTS the
//! Vulkan render path (Mode 6). Its `vkCreateInstance` requests the **Android-only** instance extension
//! `VK_KHR_android_surface`, which the host Linux Vulkan ICD does NOT advertise (it has `VK_KHR_surface`
//! plus `VK_KHR_wayland_surface`) → `VK_ERROR_EXTENSION_NOT_PRESENT` → `Mode 6 failed: Unable to create
//! Vulkan instance` (confirmed live in `/tmp/eclipse-sdk28.log`). The engine then also calls
//! `vkCreateAndroidSurfaceKHR`, which the host ICD likewise lacks.
//!
//! This module is the Vulkan-side parallel of the proven Phase-3 tier-0 `eglGetDisplay` connection-match
//! ([`super::native_provider::eclipse_egl_get_display`]): Eclipse registers its OWN `vk*` natives at
//! loader tier 0 (`EclipseNativeProvider`), which WIN over the host `libvulkan` tier by `resolve`'s
//! first-strong-match, and translate the Android WSI to the host Wayland WSI:
//!   * [`eclipse_vk_create_instance`] copies the requested instance-extension list, swapping any
//!     `VK_KHR_android_surface` entry for `VK_KHR_wayland_surface` (everything else preserved in order),
//!     then forwards to the host `vkCreateInstance`.
//!   * [`eclipse_vk_create_android_surface_khr`] ignores the engine's `ANativeWindow`-bound create-info
//!     and builds a `VkWaylandSurfaceCreateInfoKHR` from Eclipse's own winit `wl_display`
//!     ([`ndk_registry::wsi_display`]) + `wl_surface` ([`ndk_registry::wsi_wl_surface`]), then forwards
//!     to the host `vkCreateWaylandSurfaceKHR`.
//!   * [`eclipse_vk_get_instance_proc_addr`] returns the two shims by name (so the engine reaches them
//!     whether it resolves `vk*` by direct symbol OR by proc-addr) and forwards every other name to the
//!     host `vkGetInstanceProcAddr`.
//!
//! The host entry points come from the SAME `ash::Entry::load()` the renderer uses ([`crate::graphics`]),
//! so this never re-enters the engine's relocated `vk*` symbols (no recursion). `ash` is built without
//! per-extension Cargo features (`Cargo.toml`: `default-features = false`, `features = ["loaded", "std",
//! "debug"]`); the `vk::` struct types + extension-name constants are always compiled regardless. ABI:
//! the Vulkan PFN calling convention is `extern "system"` (matches ash's `PFN_*` typedefs), so every
//! shim and every transmuted host fn pointer uses `extern "system"`.

use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use ash::vk;

use super::ndk_registry;

/// 2026-06-14: cached host `vkGetPhysicalDeviceSurfaceCapabilitiesKHR` / `…2KHR`, captured at
/// `vkGetInstanceProcAddr` time. `0` = unresolved.
static HOST_PDSC: AtomicU64 = AtomicU64::new(0);
static HOST_PDSC2: AtomicU64 = AtomicU64::new(0);

/// Replace an UNDEFINED Wayland surface extent with the concrete window size. On Wayland, the host driver
/// reports `currentExtent = 0xFFFFFFFF×0xFFFFFFFF` (per the Vulkan WSI spec: the surface size is determined
/// by the swapchain's `imageExtent`, not the surface). Roblox's engine — written for Android, where a
/// surface ALWAYS has a concrete size — treats that as invalid and logs `Vulkan: skipping framebuffer
/// creation, invalid currentExtent -1x-1`, leaving no render target → `Mode 6 failed: Error opening shader
/// pack` → `RenderView is NULL`. So Eclipse presents the real window extent like an Android surface would.
fn fix_undefined_extent(caps: &mut vk::SurfaceCapabilitiesKHR) {
    const UNDEF: u32 = u32::MAX;
    if caps.current_extent.width == UNDEF || caps.current_extent.height == UNDEF {
        let win = ndk_registry::engine_window_geometry().unwrap_or((800, 600));
        caps.current_extent =
            clamp_window_extent(win, caps.min_image_extent, caps.max_image_extent);
    }
}

/// The pure core of [`fix_undefined_extent`]: the window size (≥ 1) clamped into the surface's supported
/// `[minImageExtent, maxImageExtent]` range (the `max(min)` guards a driver reporting `max < min`).
fn clamp_window_extent(win: (i32, i32), min: vk::Extent2D, max: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: (win.0.max(1) as u32).clamp(min.width, max.width.max(min.width)),
        height: (win.1.max(1) as u32).clamp(min.height, max.height.max(min.height)),
    }
}

/// `PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR` — forward to the host, then fix the undefined Wayland
/// extent via [`fix_undefined_extent`].
///
/// # Safety
/// `surface`/`physical_device` forwarded unchanged; `p_caps` must be a valid out-pointer (Vulkan contract).
pub unsafe extern "system" fn eclipse_vk_get_physical_device_surface_capabilities_khr(
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_caps: *mut vk::SurfaceCapabilitiesKHR,
) -> vk::Result {
    let host = HOST_PDSC.load(Ordering::Relaxed);
    if host == 0 || p_caps.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: 2026-06-14 — `host` is the host fn captured at gipa time; forward then rewrite the filled struct.
    unsafe {
        let host_fn: vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR =
            std::mem::transmute(host as usize as *const ());
        let r = host_fn(physical_device, surface, p_caps);
        if r == vk::Result::SUCCESS {
            fix_undefined_extent(&mut *p_caps);
        }
        r
    }
}

/// `PFN_vkGetPhysicalDeviceSurfaceCapabilities2KHR` — as above, unwrapping the `…2KHR` outer struct.
///
/// # Safety
/// As [`eclipse_vk_get_physical_device_surface_capabilities_khr`]; `p_surface_info`/`p_caps` are the `…2` args.
pub unsafe extern "system" fn eclipse_vk_get_physical_device_surface_capabilities2_khr(
    physical_device: vk::PhysicalDevice,
    p_surface_info: *const vk::PhysicalDeviceSurfaceInfo2KHR<'_>,
    p_caps: *mut vk::SurfaceCapabilities2KHR<'_>,
) -> vk::Result {
    let host = HOST_PDSC2.load(Ordering::Relaxed);
    if host == 0 || p_caps.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: 2026-06-14 — `host` is the host `…2KHR` fn captured at gipa time; rewrite the inner caps.
    unsafe {
        let host_fn: vk::PFN_vkGetPhysicalDeviceSurfaceCapabilities2KHR =
            std::mem::transmute(host as usize as *const ());
        let r = host_fn(physical_device, p_surface_info, p_caps);
        if r == vk::Result::SUCCESS {
            fix_undefined_extent(&mut (*p_caps).surface_capabilities);
        }
        r
    }
}

/// `void* dlsym(void* handle, const char* symbol)` — Eclipse-owned tier-0 `dlsym` interposer.
///
/// 2026-06-13: the guest engine resolves its Vulkan entry points by `dlopen`-ing `libvulkan.so` at
/// RUNTIME and `dlsym`-ing the loader commands by name — they are NOT `DT_NEEDED`/UND imports, so the
/// tier-0 `vk*` natives registered in `native_provider` are never consulted (proven: zero shim hits).
/// `dlsym`, by contrast, IS a UND import the engine resolves through Eclipse's scope, so intercepting it
/// here lets Eclipse hand back its WSI-translating shims for exactly the three loader entry points the
/// engine looks up by name (`vkGetInstanceProcAddr` is the only one strictly required — the engine reaches
/// `vkCreateInstance` + `vkCreateAndroidSurfaceKHR` THROUGH it — but the other two are intercepted too in
/// case the engine resolves them directly). Every other symbol is forwarded UNCHANGED to the host `dlsym`,
/// so this is a faithful pass-through except for the three Vulkan names.
///
/// # Safety
/// `handle`/`symbol` follow the `dlsym(3)` contract (`symbol` null or a valid NUL-terminated C string).
pub unsafe extern "C" fn eclipse_dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void {
    if !symbol.is_null() {
        // SAFETY: 2026-06-13 — `symbol` is non-null (checked) and a valid NUL-terminated C string per the
        // `dlsym` contract; `CStr::from_ptr` reads up to its NUL.
        let name = unsafe { CStr::from_ptr(symbol) };
        let shim: Option<*mut c_void> = if name == c"vkGetInstanceProcAddr" {
            Some(eclipse_vk_get_instance_proc_addr as *const () as *mut c_void)
        } else if name == c"vkCreateInstance" {
            Some(eclipse_vk_create_instance as *const () as *mut c_void)
        } else if name == c"vkCreateAndroidSurfaceKHR" {
            Some(eclipse_vk_create_android_surface_khr as *const () as *mut c_void)
        } else {
            None
        };
        if let Some(p) = shim {
            return p;
        }
    }
    // SAFETY: 2026-06-13 — forward unchanged to the host `dlsym` (Eclipse links libdl); `handle` is the
    // engine's own (from its `dlopen`, which Eclipse routes to the host) and `symbol` is its C string.
    unsafe { libc::dlsym(handle, symbol) }
}

/// The host Vulkan loader, loaded once via the SAME `ash::Entry::load()` the renderer uses. `None` if the
/// host has no usable `libvulkan` (the shims then return `VK_ERROR_INITIALIZATION_FAILED` — a clean
/// Vulkan failure, never UB). Process-lifetime; `ash::Entry` owns its `libloading` handle.
pub(crate) fn host_entry() -> Option<&'static ash::Entry> {
    static HOST_ENTRY: OnceLock<Option<ash::Entry>> = OnceLock::new();
    // SAFETY: 2026-06-13 — `ash::Entry::load()` dlopen's the platform Vulkan loader (`libvulkan.so.1`)
    // and reads the entry-level command pointers from it; it is `unsafe` only because it loads native
    // code, returns `Err` (handled as `None` below) when the loader is absent, and dereferences nothing
    // of ours. This is the identical call the renderer makes in `graphics.rs`.
    HOST_ENTRY
        .get_or_init(|| unsafe { ash::Entry::load() }.ok())
        .as_ref()
}

/// The host `vkGetInstanceProcAddr`, taken from the loaded [`host_entry`]. `None` when the host loader is
/// unavailable. ash's `Entry::static_fn().get_instance_proc_addr` is exactly this command pointer.
fn host_get_instance_proc_addr() -> Option<vk::PFN_vkGetInstanceProcAddr> {
    host_entry().map(|e| e.static_fn().get_instance_proc_addr)
}

/// Copy an instance-extension-name list, replacing every `VK_KHR_android_surface` entry with the host's
/// `VK_KHR_wayland_surface` and leaving all other entries (incl. `VK_KHR_surface`) untouched and in
/// order. This is the pure core of [`eclipse_vk_create_instance`]'s rewrite, isolated for unit testing.
///
/// The replacement pointer is a `'static` `&CStr` (ash's `vk::KHR_WAYLAND_SURFACE_NAME`), so the returned
/// `Vec` is valid for the duration of the host `vkCreateInstance` call (which copies the names it reads).
///
/// # Safety
/// Each pointer in `names` must be a valid NUL-terminated C string (the Vulkan `ppEnabledExtensionNames`
/// contract); they are only read for the `VK_KHR_android_surface` comparison and otherwise copied through.
unsafe fn swap_android_for_wayland_surface(names: &[*const c_char]) -> Vec<*const c_char> {
    let android = vk::KHR_ANDROID_SURFACE_NAME;
    let wayland = vk::KHR_WAYLAND_SURFACE_NAME;
    names
        .iter()
        .map(|&p| {
            if p.is_null() {
                return p;
            }
            // SAFETY: 2026-06-13 — per the function contract `p` is a valid NUL-terminated C string;
            // `CStr::from_ptr` reads up to and including its NUL. We compare it to the fixed
            // `VK_KHR_android_surface` literal and either substitute the `'static` Wayland name pointer
            // or pass the original through unchanged.
            let s = unsafe { CStr::from_ptr(p) };
            if s == android {
                wayland.as_ptr()
            } else {
                p
            }
        })
        .collect()
}

/// `PFN_vkGetInstanceProcAddr` — Eclipse-owned tier-0 entry point. Returns Eclipse's `vkCreateInstance`
/// / `vkCreateAndroidSurfaceKHR` shims by name (so the engine reaches them via proc-addr just as it does
/// via direct symbol resolution), and forwards every other name to the host `vkGetInstanceProcAddr`.
/// Returns `None` (no function) when the host loader is unavailable or `p_name` is null.
///
/// # Safety
/// `p_name` is the Vulkan-supplied NUL-terminated command name (or null); `instance` is an opaque handle
/// forwarded unchanged. This fn only reads the C string at `p_name` and never dereferences `instance`.
pub unsafe extern "system" fn eclipse_vk_get_instance_proc_addr(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    // SAFETY: 2026-06-13 — `p_name` is non-null (checked) and, per the `vkGetInstanceProcAddr` contract,
    // a valid NUL-terminated C string; `CStr::from_ptr` reads up to its NUL.
    let name = unsafe { CStr::from_ptr(p_name) };
    if name == c"vkCreateInstance" {
        // SAFETY: 2026-06-13 — `eclipse_vk_create_instance` has the exact `PFN_vkCreateInstance` ABI
        // (`unsafe extern "system" fn(*const InstanceCreateInfo, *const AllocationCallbacks, *mut
        // Instance) -> Result`); transmuting it to the type-erased `PFN_vkVoidFunction`
        // (`Option<unsafe extern "system" fn()>`) is the standard `vkGetInstanceProcAddr` return shape.
        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkCreateInstance, unsafe extern "system" fn()>(
                eclipse_vk_create_instance,
            )
        });
    }
    if name == c"vkCreateAndroidSurfaceKHR" {
        // SAFETY: 2026-06-13 — `eclipse_vk_create_android_surface_khr` has the exact
        // `PFN_vkCreateAndroidSurfaceKHR` ABI; transmuting to `PFN_vkVoidFunction` matches the
        // `vkGetInstanceProcAddr` return shape, as above.
        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkCreateAndroidSurfaceKHR, unsafe extern "system" fn()>(
                eclipse_vk_create_android_surface_khr,
            )
        });
    }
    let host_gipa = host_get_instance_proc_addr()?;
    // 2026-06-14: the text-overlay present-path interception (`vkCreateDevice` / `vkGetDeviceProcAddr`,
    // which in turn yields `vkQueuePresentKHR` etc.). Kept in `vk_overlay` so this module stays the WSI
    // surface/instance layer; returns `Some` only for the names it wraps.
    // SAFETY: `name`/`host_gipa`/`instance` are valid per this fn's contract; `vk_overlay` only reads the
    // name and forwards `(instance, name)` to `host_gipa` to capture the host command.
    if let Some(shim) =
        unsafe { super::vk_overlay::intercept_instance_proc(instance, name, host_gipa) }
    {
        return shim;
    }
    // 2026-06-14: intercept the surface-capabilities queries — cache the host version and return our shim
    // that replaces Wayland's UNDEFINED currentExtent (0xFFFFFFFF) with the concrete window extent, so the
    // engine (Android-only, expects a fixed surface size) stops skipping framebuffer creation.
    if name == c"vkGetPhysicalDeviceSurfaceCapabilitiesKHR" {
        // SAFETY: 2026-06-14 — forwarding `(instance, p_name)` to the host gipa is the resolution path.
        let host = unsafe { host_gipa(instance, p_name) };
        HOST_PDSC.store(host.map_or(0, |f| f as usize as u64), Ordering::Relaxed);
        return Some(unsafe {
            std::mem::transmute::<
                vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR,
                unsafe extern "system" fn(),
            >(eclipse_vk_get_physical_device_surface_capabilities_khr)
        });
    }
    if name == c"vkGetPhysicalDeviceSurfaceCapabilities2KHR" {
        // SAFETY: 2026-06-14 — as above; the `…2KHR` entry point shares the inner-caps rewrite.
        let host = unsafe { host_gipa(instance, p_name) };
        HOST_PDSC2.store(host.map_or(0, |f| f as usize as u64), Ordering::Relaxed);
        return Some(unsafe {
            std::mem::transmute::<
                vk::PFN_vkGetPhysicalDeviceSurfaceCapabilities2KHR,
                unsafe extern "system" fn(),
            >(eclipse_vk_get_physical_device_surface_capabilities2_khr)
        });
    }
    // SAFETY: 2026-06-13 — `host_gipa` is the host loader's `vkGetInstanceProcAddr`; forwarding the
    // engine's `(instance, p_name)` to it is exactly what an ICD/loader expects for any name we do not
    // intercept.
    unsafe { host_gipa(instance, p_name) }
}

/// `PFN_vkCreateInstance` — Eclipse-owned tier-0 override. Copies the requested instance-extension list,
/// swapping `VK_KHR_android_surface` (absent from the host Linux ICD) for `VK_KHR_wayland_surface`, while
/// preserving `p_next`, `p_application_info`, layers, flags, and every other extension in order, then
/// forwards to the host `vkCreateInstance`. Returns `VK_ERROR_INITIALIZATION_FAILED` if the host loader
/// is unavailable (a clean Vulkan failure, never UB).
///
/// # Safety
/// `p_create_info` must be a valid `*const VkInstanceCreateInfo` (the engine's, per the Vulkan contract);
/// `p_allocator`/`p_instance` are forwarded unchanged. We read the create-info's extension array
/// (`enabled_extension_count` × `pp_enabled_extension_names`) and otherwise pass the struct through.
pub unsafe extern "system" fn eclipse_vk_create_instance(
    p_create_info: *const vk::InstanceCreateInfo<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_instance: *mut vk::Instance,
) -> vk::Result {
    let Some(entry) = host_entry() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    if p_create_info.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: 2026-06-13 — `p_create_info` is non-null (checked) and a valid `VkInstanceCreateInfo` per
    // the `vkCreateInstance` contract; reading the struct by value is sound.
    let ci = unsafe { *p_create_info };

    // Read the requested extension-name array (may be empty / null-with-zero-count).
    let names: &[*const c_char] =
        if ci.enabled_extension_count == 0 || ci.pp_enabled_extension_names.is_null() {
            &[]
        } else {
            // SAFETY: 2026-06-13 — per the Vulkan contract, when `enabled_extension_count > 0` the
            // `pp_enabled_extension_names` (checked non-null) points to that many `*const c_char` entries.
            unsafe {
                std::slice::from_raw_parts(
                    ci.pp_enabled_extension_names,
                    ci.enabled_extension_count as usize,
                )
            }
        };
    // SAFETY: 2026-06-13 — each entry is a valid NUL-terminated extension name per the Vulkan contract.
    let rewritten = unsafe { swap_android_for_wayland_surface(names) };

    let mut patched = ci;
    patched.enabled_extension_count = rewritten.len() as u32;
    patched.pp_enabled_extension_names = rewritten.as_ptr();

    let create_instance = entry.fp_v1_0().create_instance;
    // SAFETY: 2026-06-13 — `create_instance` is the host loader's `vkCreateInstance`. `patched` is a
    // valid `VkInstanceCreateInfo` borrowing `rewritten` (alive for this call) for the names and the
    // original `ci`'s `p_next`/`p_application_info`/layers (alive in the caller); `p_allocator`/
    // `p_instance` are the engine's own, forwarded unchanged. The host copies the names it reads.
    let r = unsafe { create_instance(&patched, p_allocator, p_instance) };
    // 2026-06-14: capture the created `VkInstance` for the text overlay (`vk_overlay` builds an
    // `ash::Instance` from it to load the engine's device commands). On success only.
    if r == vk::Result::SUCCESS && !p_instance.is_null() {
        // SAFETY: on success the host wrote a valid `VkInstance` to `p_instance`.
        super::vk_overlay::set_instance(unsafe { *p_instance });
    }
    r
}

/// `PFN_vkCreateAndroidSurfaceKHR` — Eclipse-owned tier-0 override. The host Linux ICD has no
/// `vkCreateAndroidSurfaceKHR`; instead Eclipse builds a `VkWaylandSurfaceCreateInfoKHR` from its OWN
/// winit `wl_display` + `wl_surface` (the engine's `ANativeWindow`-bound `p_create_info` is ignored —
/// Eclipse owns the real WSI handles) and forwards to the host `vkCreateWaylandSurfaceKHR`, resolved via
/// the host `vkGetInstanceProcAddr` on this `instance`. Returns `VK_ERROR_INITIALIZATION_FAILED` if the
/// winit handles are not yet published or the host `vkCreateWaylandSurfaceKHR` is unavailable.
///
/// # Safety
/// `instance` is the engine's `VkInstance`; `p_create_info` (the engine's `VkAndroidSurfaceCreateInfoKHR`)
/// is NOT dereferenced; `p_allocator`/`p_surface` are forwarded unchanged. The `display`/`surface`
/// pointers come from [`ndk_registry`] (Eclipse's live winit window for the process lifetime).
pub unsafe extern "system" fn eclipse_vk_create_android_surface_khr(
    instance: vk::Instance,
    _p_create_info: *const vk::AndroidSurfaceCreateInfoKHR<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_surface: *mut vk::SurfaceKHR,
) -> vk::Result {
    let (Some(display), Some(surface)) =
        (ndk_registry::wsi_display(), ndk_registry::wsi_wl_surface())
    else {
        // No winit Wayland handles published (e.g. X11/other, or before `resumed`): cannot translate.
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let Some(host_gipa) = host_get_instance_proc_addr() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    // SAFETY: 2026-06-13 — resolving `vkCreateWaylandSurfaceKHR` on the engine's `instance` via the host
    // `vkGetInstanceProcAddr`; the name is a `'static` C-string literal.
    let pfn = unsafe { host_gipa(instance, c"vkCreateWaylandSurfaceKHR".as_ptr()) };
    let Some(pfn) = pfn else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    // SAFETY: 2026-06-13 — `vkGetInstanceProcAddr` returned a non-null pointer for the
    // `VK_KHR_wayland_surface` command, so it has the `PFN_vkCreateWaylandSurfaceKHR` ABI; transmuting
    // the type-erased `PFN_vkVoidFunction` back to it is the standard proc-addr usage.
    let create_wayland: vk::PFN_vkCreateWaylandSurfaceKHR = unsafe { std::mem::transmute(pfn) };

    // `vk::wl_display`/`vk::wl_surface` are both `c_void` (ash platform_types); the stored `usize`s are
    // the live winit `wl_display*` / `wl_surface*` for the window's lifetime.
    let create_info = vk::WaylandSurfaceCreateInfoKHR::default()
        .display(display as *mut vk::wl_display)
        .surface(surface as *mut vk::wl_surface);
    // SAFETY: 2026-06-13 — `create_wayland` is the host `vkCreateWaylandSurfaceKHR`; `instance` is the
    // engine's, `&create_info` is a valid stack `VkWaylandSurfaceCreateInfoKHR` for the call,
    // `p_allocator`/`p_surface` are the engine's own, forwarded unchanged.
    unsafe { create_wayland(instance, &create_info, p_allocator, p_surface) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_undefined_extent_replaces_wayland_undefined_and_keeps_concrete() {
        // 2026-06-14 regression guard: the engine logs `Vulkan: skipping framebuffer creation, invalid
        // currentExtent -1x-1` and fails (Mode 6 → RenderView NULL) when the Wayland surface reports the
        // SPEC-VALID "undefined" extent (0xFFFFFFFF). `fix_undefined_extent` must replace it with the
        // window size, and MUST leave an already-concrete extent untouched.
        let min = vk::Extent2D {
            width: 1,
            height: 1,
        };
        let max = vk::Extent2D {
            width: 16384,
            height: 16384,
        };
        let undef = vk::Extent2D {
            width: u32::MAX,
            height: u32::MAX,
        };
        // The pure clamp: window size, bounded to [min,max].
        assert_eq!(
            clamp_window_extent((800, 600), min, max),
            vk::Extent2D {
                width: 800,
                height: 600
            }
        );
        // Clamps below min / above max, and treats a zero/negative window as ≥ 1.
        assert_eq!(
            clamp_window_extent(
                (0, -5),
                vk::Extent2D {
                    width: 4,
                    height: 4
                },
                max
            ),
            vk::Extent2D {
                width: 4,
                height: 4
            }
        );
        // A surface with an UNDEFINED extent is rewritten (engine_window_geometry default 800x600 here).
        let mut caps = vk::SurfaceCapabilitiesKHR {
            current_extent: undef,
            min_image_extent: min,
            max_image_extent: max,
            ..Default::default()
        };
        fix_undefined_extent(&mut caps);
        assert_ne!(
            caps.current_extent.width,
            u32::MAX,
            "undefined extent must be replaced"
        );
        assert!(caps.current_extent.width >= 1 && caps.current_extent.height >= 1);
        // A surface with a CONCRETE extent is left exactly as-is.
        let mut concrete = vk::SurfaceCapabilitiesKHR {
            current_extent: vk::Extent2D {
                width: 1280,
                height: 720,
            },
            min_image_extent: min,
            max_image_extent: max,
            ..Default::default()
        };
        fix_undefined_extent(&mut concrete);
        assert_eq!(
            concrete.current_extent,
            vk::Extent2D {
                width: 1280,
                height: 720
            },
            "a concrete extent must not be touched"
        );
    }

    #[test]
    fn swap_android_for_wayland_surface_replaces_only_android_and_preserves_order() {
        // 2026-06-13 — the confirmed-root-cause regression guard for the engine's Vulkan instance failing
        // on `VK_KHR_android_surface` (absent from the host Linux ICD). The rewrite MUST swap exactly that
        // one name for `VK_KHR_wayland_surface`, leave `VK_KHR_surface` and every other entry untouched,
        // and preserve order — otherwise `vkCreateInstance` either still fails (android left in) or drops
        // a needed extension. Pure + deterministic (no Vulkan instance), the smallest check that fails if
        // the rewrite regresses.
        let surface = vk::KHR_SURFACE_NAME;
        let pdp2 = c"VK_KHR_get_physical_device_properties2";
        let android = vk::KHR_ANDROID_SURFACE_NAME;
        let wayland = vk::KHR_WAYLAND_SURFACE_NAME;

        let input: [*const c_char; 3] = [surface.as_ptr(), pdp2.as_ptr(), android.as_ptr()];
        // SAFETY: every pointer above is a valid `'static` NUL-terminated C string.
        let out = unsafe { swap_android_for_wayland_surface(&input) };
        assert_eq!(out.len(), 3, "length is preserved");

        // Each entry compared by C-string content (pointer identity may differ for the swapped slot).
        let as_cstr = |p: *const c_char| {
            assert!(!p.is_null());
            // SAFETY: each output pointer is a valid NUL-terminated C string.
            unsafe { CStr::from_ptr(p) }
        };
        assert_eq!(
            as_cstr(out[0]),
            surface,
            "VK_KHR_surface preserved in place"
        );
        assert_eq!(
            as_cstr(out[1]),
            pdp2,
            "an unrelated extension preserved in place"
        );
        assert_eq!(
            as_cstr(out[2]),
            wayland,
            "VK_KHR_android_surface -> VK_KHR_wayland_surface"
        );
        // No android_surface survives anywhere in the output.
        assert!(
            !out.iter().any(|&p| as_cstr(p) == android),
            "no VK_KHR_android_surface remains after the rewrite"
        );
    }

    #[test]
    fn swap_android_for_wayland_surface_is_identity_without_android() {
        // When the engine does not request VK_KHR_android_surface, the list passes through unchanged
        // (order + content) — Eclipse never perturbs a host-valid extension request.
        let surface = vk::KHR_SURFACE_NAME;
        let wayland = vk::KHR_WAYLAND_SURFACE_NAME;
        let input: [*const c_char; 2] = [surface.as_ptr(), wayland.as_ptr()];
        // SAFETY: both pointers are valid `'static` NUL-terminated C strings.
        let out = unsafe { swap_android_for_wayland_surface(&input) };
        assert_eq!(out.len(), 2);
        // SAFETY: output pointers are valid NUL-terminated C strings.
        assert_eq!(unsafe { CStr::from_ptr(out[0]) }, surface);
        assert_eq!(unsafe { CStr::from_ptr(out[1]) }, wayland);

        // Empty input -> empty output (no panic, no allocation surprises).
        let empty = unsafe { swap_android_for_wayland_surface(&[]) };
        assert!(empty.is_empty(), "empty extension list stays empty");
    }
}
