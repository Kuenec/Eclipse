use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use ash::vk;

use super::ndk_registry;

static HOST_PDSC: AtomicU64 = AtomicU64::new(0);
static HOST_PDSC2: AtomicU64 = AtomicU64::new(0);

fn fix_undefined_extent(caps: &mut vk::SurfaceCapabilitiesKHR) {
    const UNDEF: u32 = u32::MAX;
    if caps.current_extent.width == UNDEF || caps.current_extent.height == UNDEF {
        let win = ndk_registry::engine_window_geometry().unwrap_or((800, 600));
        caps.current_extent =
            clamp_window_extent(win, caps.min_image_extent, caps.max_image_extent);
    }
}

fn clamp_window_extent(win: (i32, i32), min: vk::Extent2D, max: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: (win.0.max(1) as u32).clamp(min.width, max.width.max(min.width)),
        height: (win.1.max(1) as u32).clamp(min.height, max.height.max(min.height)),
    }
}

pub unsafe extern "system" fn eclipse_vk_get_physical_device_surface_capabilities_khr(
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_caps: *mut vk::SurfaceCapabilitiesKHR,
) -> vk::Result {
    let host = HOST_PDSC.load(Ordering::Relaxed);
    if host == 0 || p_caps.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }

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

pub unsafe extern "system" fn eclipse_vk_get_physical_device_surface_capabilities2_khr(
    physical_device: vk::PhysicalDevice,
    p_surface_info: *const vk::PhysicalDeviceSurfaceInfo2KHR<'_>,
    p_caps: *mut vk::SurfaceCapabilities2KHR<'_>,
) -> vk::Result {
    let host = HOST_PDSC2.load(Ordering::Relaxed);
    if host == 0 || p_caps.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }

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

pub unsafe extern "C" fn eclipse_dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void {
    if !symbol.is_null() {
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

    unsafe { libc::dlsym(handle, symbol) }
}

pub(crate) fn host_entry() -> Option<&'static ash::Entry> {
    static HOST_ENTRY: OnceLock<Option<ash::Entry>> = OnceLock::new();

    HOST_ENTRY
        .get_or_init(|| unsafe { ash::Entry::load() }.ok())
        .as_ref()
}

fn host_get_instance_proc_addr() -> Option<vk::PFN_vkGetInstanceProcAddr> {
    host_entry().map(|e| e.static_fn().get_instance_proc_addr)
}

unsafe fn swap_android_for_wayland_surface(names: &[*const c_char]) -> Vec<*const c_char> {
    let android = vk::KHR_ANDROID_SURFACE_NAME;
    let wayland = vk::KHR_WAYLAND_SURFACE_NAME;
    names
        .iter()
        .map(|&p| {
            if p.is_null() {
                return p;
            }

            let s = unsafe { CStr::from_ptr(p) };
            if s == android {
                wayland.as_ptr()
            } else {
                p
            }
        })
        .collect()
}

pub unsafe extern "system" fn eclipse_vk_get_instance_proc_addr(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }

    let name = unsafe { CStr::from_ptr(p_name) };
    if name == c"vkCreateInstance" {
        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkCreateInstance, unsafe extern "system" fn()>(
                eclipse_vk_create_instance,
            )
        });
    }
    if name == c"vkCreateAndroidSurfaceKHR" {
        return Some(unsafe {
            std::mem::transmute::<vk::PFN_vkCreateAndroidSurfaceKHR, unsafe extern "system" fn()>(
                eclipse_vk_create_android_surface_khr,
            )
        });
    }
    let host_gipa = host_get_instance_proc_addr()?;

    if let Some(shim) =
        unsafe { super::vk_overlay::intercept_instance_proc(instance, name, host_gipa) }
    {
        return shim;
    }

    if name == c"vkGetPhysicalDeviceSurfaceCapabilitiesKHR" {
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
        let host = unsafe { host_gipa(instance, p_name) };
        HOST_PDSC2.store(host.map_or(0, |f| f as usize as u64), Ordering::Relaxed);
        return Some(unsafe {
            std::mem::transmute::<
                vk::PFN_vkGetPhysicalDeviceSurfaceCapabilities2KHR,
                unsafe extern "system" fn(),
            >(eclipse_vk_get_physical_device_surface_capabilities2_khr)
        });
    }

    unsafe { host_gipa(instance, p_name) }
}

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

    let ci = unsafe { *p_create_info };

    let names: &[*const c_char] =
        if ci.enabled_extension_count == 0 || ci.pp_enabled_extension_names.is_null() {
            &[]
        } else {
            unsafe {
                std::slice::from_raw_parts(
                    ci.pp_enabled_extension_names,
                    ci.enabled_extension_count as usize,
                )
            }
        };

    let rewritten = unsafe { swap_android_for_wayland_surface(names) };

    let mut patched = ci;
    patched.enabled_extension_count = rewritten.len() as u32;
    patched.pp_enabled_extension_names = rewritten.as_ptr();

    let create_instance = entry.fp_v1_0().create_instance;

    let r = unsafe { create_instance(&patched, p_allocator, p_instance) };

    if r == vk::Result::SUCCESS && !p_instance.is_null() {
        super::vk_overlay::set_instance(unsafe { *p_instance });
    }
    r
}

pub unsafe extern "system" fn eclipse_vk_create_android_surface_khr(
    instance: vk::Instance,
    _p_create_info: *const vk::AndroidSurfaceCreateInfoKHR<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_surface: *mut vk::SurfaceKHR,
) -> vk::Result {
    let (Some(display), Some(surface)) =
        (ndk_registry::wsi_display(), ndk_registry::wsi_wl_surface())
    else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let Some(host_gipa) = host_get_instance_proc_addr() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    let pfn = unsafe { host_gipa(instance, c"vkCreateWaylandSurfaceKHR".as_ptr()) };
    let Some(pfn) = pfn else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    let create_wayland: vk::PFN_vkCreateWaylandSurfaceKHR = unsafe { std::mem::transmute(pfn) };

    let create_info = vk::WaylandSurfaceCreateInfoKHR::default()
        .display(display as *mut vk::wl_display)
        .surface(surface as *mut vk::wl_surface);

    unsafe { create_wayland(instance, &create_info, p_allocator, p_surface) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_undefined_extent_replaces_wayland_undefined_and_keeps_concrete() {
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

        assert_eq!(
            clamp_window_extent((800, 600), min, max),
            vk::Extent2D {
                width: 800,
                height: 600
            }
        );

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
        let surface = vk::KHR_SURFACE_NAME;
        let pdp2 = c"VK_KHR_get_physical_device_properties2";
        let android = vk::KHR_ANDROID_SURFACE_NAME;
        let wayland = vk::KHR_WAYLAND_SURFACE_NAME;

        let input: [*const c_char; 3] = [surface.as_ptr(), pdp2.as_ptr(), android.as_ptr()];

        let out = unsafe { swap_android_for_wayland_surface(&input) };
        assert_eq!(out.len(), 3, "length is preserved");

        let as_cstr = |p: *const c_char| {
            assert!(!p.is_null());

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

        assert!(
            !out.iter().any(|&p| as_cstr(p) == android),
            "no VK_KHR_android_surface remains after the rewrite"
        );
    }

    #[test]
    fn swap_android_for_wayland_surface_is_identity_without_android() {
        let surface = vk::KHR_SURFACE_NAME;
        let wayland = vk::KHR_WAYLAND_SURFACE_NAME;
        let input: [*const c_char; 2] = [surface.as_ptr(), wayland.as_ptr()];

        let out = unsafe { swap_android_for_wayland_surface(&input) };
        assert_eq!(out.len(), 2);

        assert_eq!(unsafe { CStr::from_ptr(out[0]) }, surface);
        assert_eq!(unsafe { CStr::from_ptr(out[1]) }, wayland);

        let empty = unsafe { swap_android_for_wayland_surface(&[]) };
        assert!(empty.is_empty(), "empty extension list stays empty");
    }
}
