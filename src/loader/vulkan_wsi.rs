use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use ash::vk;
use ash::vk::Handle;

use super::ndk_registry;

static HOST_PDSC: AtomicU64 = AtomicU64::new(0);
static HOST_PDSC2: AtomicU64 = AtomicU64::new(0);
static HOST_PDSPM: AtomicU64 = AtomicU64::new(0);
static REPORTED_PHYSICAL_DEVICE: AtomicU64 = AtomicU64::new(0);

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

pub(crate) unsafe extern "system" fn eclipse_vk_get_physical_device_surface_capabilities_khr(
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

pub(crate) unsafe extern "system" fn eclipse_vk_get_physical_device_surface_capabilities2_khr(
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

fn host_surface_present_modes_fn() -> Option<vk::PFN_vkGetPhysicalDeviceSurfacePresentModesKHR> {
    match HOST_PDSPM.load(Ordering::Relaxed) {
        0 => None,
        host => Some(unsafe {
            std::mem::transmute::<usize, vk::PFN_vkGetPhysicalDeviceSurfacePresentModesKHR>(
                host as usize,
            )
        }),
    }
}

unsafe fn host_present_modes(
    query: vk::PFN_vkGetPhysicalDeviceSurfacePresentModesKHR,
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
) -> Result<Vec<vk::PresentModeKHR>, vk::Result> {
    loop {
        let mut count = 0;
        let r = unsafe { query(physical_device, surface, &mut count, std::ptr::null_mut()) };
        if r != vk::Result::SUCCESS {
            return Err(r);
        }
        let mut modes = vec![vk::PresentModeKHR::FIFO; count as usize];
        match unsafe { query(physical_device, surface, &mut count, modes.as_mut_ptr()) } {
            vk::Result::SUCCESS => {
                modes.truncate(count as usize);
                return Ok(modes);
            }
            vk::Result::INCOMPLETE => {}
            r => return Err(r),
        }
    }
}

fn presents_immediate_through_mailbox(host: &[vk::PresentModeKHR]) -> bool {
    !host.contains(&vk::PresentModeKHR::IMMEDIATE) && host.contains(&vk::PresentModeKHR::MAILBOX)
}

fn android_surface_present_modes(mut host: Vec<vk::PresentModeKHR>) -> Vec<vk::PresentModeKHR> {
    if presents_immediate_through_mailbox(&host) {
        host.push(vk::PresentModeKHR::IMMEDIATE);
    }
    host
}

fn host_swapchain_present_mode(
    requested: vk::PresentModeKHR,
    host: &[vk::PresentModeKHR],
) -> vk::PresentModeKHR {
    if requested == vk::PresentModeKHR::IMMEDIATE && presents_immediate_through_mailbox(host) {
        vk::PresentModeKHR::MAILBOX
    } else {
        requested
    }
}

unsafe fn enumerate_android_present_modes(
    query: vk::PFN_vkGetPhysicalDeviceSurfacePresentModesKHR,
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_present_mode_count: *mut u32,
    p_present_modes: *mut vk::PresentModeKHR,
) -> vk::Result {
    let Some(count) = (unsafe { p_present_mode_count.as_mut() }) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let modes = match unsafe { host_present_modes(query, physical_device, surface) } {
        Ok(host) => android_surface_present_modes(host),
        Err(r) => return r,
    };
    if p_present_modes.is_null() {
        *count = modes.len() as u32;
        return vk::Result::SUCCESS;
    }
    let written = modes.len().min(*count as usize);
    unsafe { std::ptr::copy_nonoverlapping(modes.as_ptr(), p_present_modes, written) };
    *count = written as u32;
    if written < modes.len() {
        vk::Result::INCOMPLETE
    } else {
        vk::Result::SUCCESS
    }
}

pub(crate) unsafe extern "system" fn eclipse_vk_get_physical_device_surface_present_modes_khr(
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_present_mode_count: *mut u32,
    p_present_modes: *mut vk::PresentModeKHR,
) -> vk::Result {
    let Some(query) = host_surface_present_modes_fn() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    REPORTED_PHYSICAL_DEVICE.store(physical_device.as_raw(), Ordering::Relaxed);
    unsafe {
        enumerate_android_present_modes(
            query,
            physical_device,
            surface,
            p_present_mode_count,
            p_present_modes,
        )
    }
}

pub(crate) fn swapchain_present_mode(
    surface: vk::SurfaceKHR,
    requested: vk::PresentModeKHR,
) -> Result<vk::PresentModeKHR, vk::Result> {
    if requested != vk::PresentModeKHR::IMMEDIATE {
        return Ok(requested);
    }
    let reported = REPORTED_PHYSICAL_DEVICE.load(Ordering::Relaxed);
    match host_surface_present_modes_fn() {
        Some(query) if reported != 0 => {
            let physical_device = vk::PhysicalDevice::from_raw(reported);
            let host = unsafe { host_present_modes(query, physical_device, surface) }?;
            Ok(host_swapchain_present_mode(requested, &host))
        }
        _ => Ok(requested),
    }
}

#[cfg(test)]
pub(super) fn use_host_present_modes_for_test(
    query: Option<vk::PFN_vkGetPhysicalDeviceSurfacePresentModesKHR>,
) {
    HOST_PDSPM.store(query.map_or(0, |f| f as usize as u64), Ordering::SeqCst);
    REPORTED_PHYSICAL_DEVICE.store(0, Ordering::SeqCst);
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

pub(crate) unsafe extern "system" fn eclipse_vk_get_instance_proc_addr(
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
    if name == c"vkGetPhysicalDeviceSurfacePresentModesKHR" {
        let host = unsafe { host_gipa(instance, p_name) };
        HOST_PDSPM.store(host.map_or(0, |f| f as usize as u64), Ordering::Relaxed);
        return Some(unsafe {
            std::mem::transmute::<
                vk::PFN_vkGetPhysicalDeviceSurfacePresentModesKHR,
                unsafe extern "system" fn(),
            >(eclipse_vk_get_physical_device_surface_present_modes_khr)
        });
    }

    unsafe { host_gipa(instance, p_name) }
}

pub(crate) unsafe extern "system" fn eclipse_vk_create_instance(
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

pub(crate) unsafe extern "system" fn eclipse_vk_create_android_surface_khr(
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

    const HOST_WITHOUT_TEARING: [vk::PresentModeKHR; 2] =
        [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::FIFO];
    const HOST_WITH_TEARING: [vk::PresentModeKHR; 3] = [
        vk::PresentModeKHR::MAILBOX,
        vk::PresentModeKHR::FIFO,
        vk::PresentModeKHR::IMMEDIATE,
    ];

    unsafe fn report_host_modes(
        modes: &[vk::PresentModeKHR],
        count: *mut u32,
        out: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        let count = unsafe { &mut *count };
        if out.is_null() {
            *count = modes.len() as u32;
            return vk::Result::SUCCESS;
        }
        let written = modes.len().min(*count as usize);
        unsafe { std::ptr::copy_nonoverlapping(modes.as_ptr(), out, written) };
        *count = written as u32;
        if written < modes.len() {
            vk::Result::INCOMPLETE
        } else {
            vk::Result::SUCCESS
        }
    }

    unsafe extern "system" fn host_without_tearing(
        _: vk::PhysicalDevice,
        _: vk::SurfaceKHR,
        count: *mut u32,
        out: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        unsafe { report_host_modes(&HOST_WITHOUT_TEARING, count, out) }
    }

    unsafe extern "system" fn host_with_tearing(
        _: vk::PhysicalDevice,
        _: vk::SurfaceKHR,
        count: *mut u32,
        out: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        unsafe { report_host_modes(&HOST_WITH_TEARING, count, out) }
    }

    unsafe extern "system" fn host_with_fifo_only(
        _: vk::PhysicalDevice,
        _: vk::SurfaceKHR,
        count: *mut u32,
        out: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        unsafe { report_host_modes(&[vk::PresentModeKHR::FIFO], count, out) }
    }

    static HOST_GAINED_A_MODE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    unsafe extern "system" fn host_that_gains_a_mode_after_counting(
        _: vk::PhysicalDevice,
        _: vk::SurfaceKHR,
        count: *mut u32,
        out: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        if out.is_null() && !HOST_GAINED_A_MODE.load(Ordering::SeqCst) {
            unsafe { *count = HOST_WITHOUT_TEARING.len() as u32 };
            return vk::Result::SUCCESS;
        }
        HOST_GAINED_A_MODE.store(true, Ordering::SeqCst);
        unsafe { report_host_modes(&HOST_WITH_TEARING, count, out) }
    }

    unsafe extern "system" fn host_with_lost_surface(
        _: vk::PhysicalDevice,
        _: vk::SurfaceKHR,
        _: *mut u32,
        _: *mut vk::PresentModeKHR,
    ) -> vk::Result {
        vk::Result::ERROR_SURFACE_LOST_KHR
    }

    fn android_modes(
        query: vk::PFN_vkGetPhysicalDeviceSurfacePresentModesKHR,
    ) -> Vec<vk::PresentModeKHR> {
        let (physical_device, surface) = (vk::PhysicalDevice::null(), vk::SurfaceKHR::null());
        let mut count = 0;
        let r = unsafe {
            enumerate_android_present_modes(
                query,
                physical_device,
                surface,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(r, vk::Result::SUCCESS);
        let mut modes = vec![vk::PresentModeKHR::FIFO_RELAXED; count as usize];
        let r = unsafe {
            enumerate_android_present_modes(
                query,
                physical_device,
                surface,
                &mut count,
                modes.as_mut_ptr(),
            )
        };
        assert_eq!(r, vk::Result::SUCCESS);
        modes.truncate(count as usize);
        modes
    }

    #[test]
    fn a_host_that_cannot_tear_presents_immediate_swapchains_through_mailbox() {
        assert_eq!(
            android_modes(host_without_tearing),
            vec![
                vk::PresentModeKHR::MAILBOX,
                vk::PresentModeKHR::FIFO,
                vk::PresentModeKHR::IMMEDIATE,
            ]
        );
        assert_eq!(
            host_swapchain_present_mode(vk::PresentModeKHR::IMMEDIATE, &HOST_WITHOUT_TEARING),
            vk::PresentModeKHR::MAILBOX
        );
        assert_eq!(
            host_swapchain_present_mode(vk::PresentModeKHR::FIFO, &HOST_WITHOUT_TEARING),
            vk::PresentModeKHR::FIFO
        );
        assert_eq!(
            host_swapchain_present_mode(vk::PresentModeKHR::MAILBOX, &HOST_WITHOUT_TEARING),
            vk::PresentModeKHR::MAILBOX
        );
    }

    #[test]
    fn a_host_that_can_tear_keeps_its_own_present_modes() {
        assert_eq!(android_modes(host_with_tearing), HOST_WITH_TEARING.to_vec());
        assert_eq!(
            host_swapchain_present_mode(vk::PresentModeKHR::IMMEDIATE, &HOST_WITH_TEARING),
            vk::PresentModeKHR::IMMEDIATE
        );
        assert_eq!(
            android_modes(host_with_fifo_only),
            vec![vk::PresentModeKHR::FIFO]
        );
    }

    #[test]
    fn present_mode_enumeration_follows_the_vulkan_two_call_contract() {
        let (physical_device, surface) = (vk::PhysicalDevice::null(), vk::SurfaceKHR::null());
        let mut count = 2;
        let mut modes = [vk::PresentModeKHR::FIFO_RELAXED; 2];
        let r = unsafe {
            enumerate_android_present_modes(
                host_without_tearing,
                physical_device,
                surface,
                &mut count,
                modes.as_mut_ptr(),
            )
        };
        assert_eq!(r, vk::Result::INCOMPLETE);
        assert_eq!(count, 2);
        assert_eq!(modes, HOST_WITHOUT_TEARING);

        let mut count = 0;
        let lost = unsafe {
            enumerate_android_present_modes(
                host_with_lost_surface,
                physical_device,
                surface,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(lost, vk::Result::ERROR_SURFACE_LOST_KHR);

        let missing_count = unsafe {
            enumerate_android_present_modes(
                host_with_tearing,
                physical_device,
                surface,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(missing_count, vk::Result::ERROR_INITIALIZATION_FAILED);
    }

    #[test]
    fn a_host_mode_list_that_grows_while_being_read_is_read_again() {
        assert_eq!(
            android_modes(host_that_gains_a_mode_after_counting),
            HOST_WITH_TEARING.to_vec()
        );
    }
}
