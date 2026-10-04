use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use ash::vk;
use ash::vk::Handle;

use super::ndk_registry;
use crate::gpu::ChosenDevice;

static HOST_PDSC: AtomicU64 = AtomicU64::new(0);
static HOST_PDSC2: AtomicU64 = AtomicU64::new(0);
static HOST_PDSPM: AtomicU64 = AtomicU64::new(0);
static REPORTED_PHYSICAL_DEVICE: AtomicU64 = AtomicU64::new(0);

fn fit_engine_extent(caps: &mut vk::SurfaceCapabilitiesKHR) {
    const UNDEF: u32 = u32::MAX;
    if caps.current_extent.width == UNDEF || caps.current_extent.height == UNDEF {
        let win = ndk_registry::engine_window_geometry().unwrap_or((800, 600));
        caps.current_extent =
            clamp_window_extent(win, caps.min_image_extent, caps.max_image_extent);
    }
    for extent in [
        &mut caps.current_extent,
        &mut caps.min_image_extent,
        &mut caps.max_image_extent,
    ] {
        extent.width = extent.width.max(crate::egl_engine::MIN_ENGINE_EDGE);
        extent.height = extent.height.max(crate::egl_engine::MIN_ENGINE_EDGE);
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
        if r != vk::Result::SUCCESS {
            return r;
        }
        fit_engine_extent(&mut *p_caps);
    }
    if super::vk_overlay::take_pending_rebuild(surface) {
        return vk::Result::ERROR_SURFACE_LOST_KHR;
    }
    vk::Result::SUCCESS
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
            fit_engine_extent(&mut (*p_caps).surface_capabilities);
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

fn read_counted<T: Copy>(
    blank: T,
    mut query: impl FnMut(*mut u32, *mut T) -> vk::Result,
) -> Result<Vec<T>, vk::Result> {
    loop {
        let mut count = 0;
        let r = query(&mut count, std::ptr::null_mut());
        if r != vk::Result::SUCCESS {
            return Err(r);
        }
        let mut items = vec![blank; count as usize];
        match query(&mut count, items.as_mut_ptr()) {
            vk::Result::SUCCESS => {
                items.truncate(count as usize);
                return Ok(items);
            }
            vk::Result::INCOMPLETE => {}
            r => return Err(r),
        }
    }
}

unsafe fn fill_counted<T, Slot>(
    items: &[T],
    count: &mut u32,
    out: *mut Slot,
    mut write: impl FnMut(*mut Slot, &T),
) -> vk::Result {
    if out.is_null() {
        *count = items.len() as u32;
        return vk::Result::SUCCESS;
    }
    let written = items.len().min(*count as usize);
    for (index, item) in items[..written].iter().enumerate() {
        write(unsafe { out.add(index) }, item);
    }
    *count = written as u32;
    if written < items.len() {
        vk::Result::INCOMPLETE
    } else {
        vk::Result::SUCCESS
    }
}

unsafe fn copy_counted<T: Copy>(items: &[T], count: &mut u32, out: *mut T) -> vk::Result {
    let copy = |slot: *mut T, item: &T| unsafe { slot.write(*item) };
    unsafe { fill_counted(items, count, out, copy) }
}

unsafe fn host_present_modes(
    query: vk::PFN_vkGetPhysicalDeviceSurfacePresentModesKHR,
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
) -> Result<Vec<vk::PresentModeKHR>, vk::Result> {
    read_counted(vk::PresentModeKHR::FIFO, |count, modes| unsafe {
        query(physical_device, surface, count, modes)
    })
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
    unsafe { copy_counted(&modes, count, p_present_modes) }
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

pub(crate) unsafe fn swapchain_present_mode(
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

pub(crate) unsafe fn swapchain_usage_with(
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    requested: vk::ImageUsageFlags,
    wanted: vk::ImageUsageFlags,
) -> Result<vk::ImageUsageFlags, vk::Result> {
    let host = HOST_PDSC.load(Ordering::Relaxed);
    if host == 0 || physical_device == vk::PhysicalDevice::null() {
        return Ok(requested);
    }
    let query = unsafe {
        std::mem::transmute::<usize, vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR>(
            host as usize,
        )
    };
    let mut caps = vk::SurfaceCapabilitiesKHR::default();
    match unsafe { query(physical_device, surface, &mut caps) } {
        vk::Result::SUCCESS if caps.supported_usage_flags.contains(wanted) => {
            Ok(requested | wanted)
        }
        vk::Result::SUCCESS => Ok(requested),
        r => Err(r),
    }
}

#[cfg(test)]
pub(super) fn use_host_surface_capabilities_for_test(
    query: Option<vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR>,
) {
    HOST_PDSC.store(query.map_or(0, |f| f as usize as u64), Ordering::SeqCst);
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

fn host_surface_extension(target: ndk_registry::WsiTarget) -> &'static CStr {
    match target {
        ndk_registry::WsiTarget::Wayland { .. } => vk::KHR_WAYLAND_SURFACE_NAME,
        ndk_registry::WsiTarget::Xlib { .. } => vk::KHR_XLIB_SURFACE_NAME,
    }
}

unsafe fn requests_android_surface(names: &[*const c_char]) -> bool {
    names
        .iter()
        .any(|&p| !p.is_null() && unsafe { CStr::from_ptr(p) } == vk::KHR_ANDROID_SURFACE_NAME)
}

unsafe fn swap_android_surface(
    names: &[*const c_char],
    host_surface: &'static CStr,
) -> Vec<*const c_char> {
    let android = vk::KHR_ANDROID_SURFACE_NAME;
    names
        .iter()
        .map(|&p| {
            if p.is_null() {
                return p;
            }

            let s = unsafe { CStr::from_ptr(p) };
            if s == android {
                host_surface.as_ptr()
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
    if p_name.is_null() || crate::gpu::client_graphics().hides_vulkan() {
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
    if let Some(shim) = device_enumeration_shim(name) {
        return unsafe { host_gipa(instance, p_name) }.and(Some(shim));
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
    if crate::gpu::client_graphics().hides_vulkan() {
        return vk::Result::ERROR_INCOMPATIBLE_DRIVER;
    }
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

    let rewritten = match ndk_registry::wsi_target() {
        Some(target) => unsafe { swap_android_surface(names, host_surface_extension(target)) },
        None if unsafe { requests_android_surface(names) } => {
            tracing::error!(
                "vkCreateInstance asked for VK_KHR_android_surface before the host window \
                 recorded a Wayland or X11 surface target; Eclipse needs a Wayland or X11 \
                 session (WAYLAND_DISPLAY or DISPLAY)"
            );
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        None => names.to_vec(),
    };

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
    if crate::gpu::client_graphics().hides_vulkan() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let Some(target) = ndk_registry::wsi_target() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let Some(host_gipa) = host_get_instance_proc_addr() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    match target {
        ndk_registry::WsiTarget::Wayland { display, surface } => {
            let pfn = unsafe { host_gipa(instance, c"vkCreateWaylandSurfaceKHR".as_ptr()) };
            let Some(pfn) = pfn else {
                return vk::Result::ERROR_INITIALIZATION_FAILED;
            };
            let create_wayland: vk::PFN_vkCreateWaylandSurfaceKHR =
                unsafe { std::mem::transmute(pfn) };
            let create_info = vk::WaylandSurfaceCreateInfoKHR::default()
                .display(display as *mut vk::wl_display)
                .surface(surface as *mut vk::wl_surface);
            unsafe { create_wayland(instance, &create_info, p_allocator, p_surface) }
        }
        ndk_registry::WsiTarget::Xlib { display, window } => {
            let pfn = unsafe { host_gipa(instance, c"vkCreateXlibSurfaceKHR".as_ptr()) };
            let Some(pfn) = pfn else {
                return vk::Result::ERROR_INITIALIZATION_FAILED;
            };
            let create_xlib: vk::PFN_vkCreateXlibSurfaceKHR = unsafe { std::mem::transmute(pfn) };
            let create_info = vk::XlibSurfaceCreateInfoKHR::default()
                .dpy(display as *mut vk::Display)
                .window(window);
            unsafe { create_xlib(instance, &create_info, p_allocator, p_surface) }
        }
    }
}

type VoidFunction = unsafe extern "system" fn();

const ENUMERATE_PHYSICAL_DEVICES: &CStr = c"vkEnumeratePhysicalDevices";
const ENUMERATE_PHYSICAL_DEVICE_GROUPS: &CStr = c"vkEnumeratePhysicalDeviceGroups";
const ENUMERATE_PHYSICAL_DEVICE_GROUPS_KHR: &CStr = c"vkEnumeratePhysicalDeviceGroupsKHR";

static SHOWED_EVERY_DEVICE: AtomicBool = AtomicBool::new(false);

fn device_enumeration_shim(name: &CStr) -> Option<VoidFunction> {
    let shim = if name == ENUMERATE_PHYSICAL_DEVICES {
        unsafe {
            std::mem::transmute::<vk::PFN_vkEnumeratePhysicalDevices, VoidFunction>(
                eclipse_vk_enumerate_physical_devices,
            )
        }
    } else if name == ENUMERATE_PHYSICAL_DEVICE_GROUPS {
        unsafe {
            std::mem::transmute::<vk::PFN_vkEnumeratePhysicalDeviceGroups, VoidFunction>(
                eclipse_vk_enumerate_physical_device_groups,
            )
        }
    } else if name == ENUMERATE_PHYSICAL_DEVICE_GROUPS_KHR {
        unsafe {
            std::mem::transmute::<vk::PFN_vkEnumeratePhysicalDeviceGroups, VoidFunction>(
                eclipse_vk_enumerate_physical_device_groups_khr,
            )
        }
    } else {
        return None;
    };
    Some(shim)
}

unsafe fn host_device_enumeration(
    instance: vk::Instance,
    name: &CStr,
) -> Option<(VoidFunction, vk::PFN_vkGetPhysicalDeviceProperties)> {
    let host_gipa = host_get_instance_proc_addr()?;
    let enumerate = unsafe { host_gipa(instance, name.as_ptr()) }?;
    let properties = unsafe { host_gipa(instance, c"vkGetPhysicalDeviceProperties".as_ptr()) }?;
    Some((enumerate, unsafe {
        std::mem::transmute::<VoidFunction, vk::PFN_vkGetPhysicalDeviceProperties>(properties)
    }))
}

pub(crate) unsafe extern "system" fn eclipse_vk_enumerate_physical_devices(
    instance: vk::Instance,
    p_physical_device_count: *mut u32,
    p_physical_devices: *mut vk::PhysicalDevice,
) -> vk::Result {
    let Some((enumerate, properties)) =
        (unsafe { host_device_enumeration(instance, ENUMERATE_PHYSICAL_DEVICES) })
    else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    unsafe {
        enumerate_chosen_physical_devices(
            std::mem::transmute::<VoidFunction, vk::PFN_vkEnumeratePhysicalDevices>(enumerate),
            properties,
            &ChosenDeviceFilter::recorded(),
            instance,
            p_physical_device_count,
            p_physical_devices,
        )
    }
}

pub(crate) unsafe extern "system" fn eclipse_vk_enumerate_physical_device_groups(
    instance: vk::Instance,
    p_physical_device_group_count: *mut u32,
    p_physical_device_group_properties: *mut vk::PhysicalDeviceGroupProperties<'_>,
) -> vk::Result {
    unsafe {
        enumerate_host_physical_device_groups(
            ENUMERATE_PHYSICAL_DEVICE_GROUPS,
            instance,
            p_physical_device_group_count,
            p_physical_device_group_properties,
        )
    }
}

unsafe extern "system" fn eclipse_vk_enumerate_physical_device_groups_khr(
    instance: vk::Instance,
    p_physical_device_group_count: *mut u32,
    p_physical_device_group_properties: *mut vk::PhysicalDeviceGroupProperties<'_>,
) -> vk::Result {
    unsafe {
        enumerate_host_physical_device_groups(
            ENUMERATE_PHYSICAL_DEVICE_GROUPS_KHR,
            instance,
            p_physical_device_group_count,
            p_physical_device_group_properties,
        )
    }
}

unsafe fn enumerate_host_physical_device_groups(
    name: &CStr,
    instance: vk::Instance,
    p_physical_device_group_count: *mut u32,
    p_physical_device_group_properties: *mut vk::PhysicalDeviceGroupProperties<'_>,
) -> vk::Result {
    let Some((enumerate, properties)) = (unsafe { host_device_enumeration(instance, name) }) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    unsafe {
        enumerate_chosen_physical_device_groups(
            std::mem::transmute::<VoidFunction, vk::PFN_vkEnumeratePhysicalDeviceGroups>(enumerate),
            properties,
            &ChosenDeviceFilter::recorded(),
            instance,
            p_physical_device_group_count,
            p_physical_device_group_properties,
        )
    }
}

struct ChosenDeviceFilter<'a> {
    chosen: Option<ChosenDevice>,
    showed_every_device: &'a AtomicBool,
}

impl ChosenDeviceFilter<'static> {
    fn recorded() -> Self {
        Self {
            chosen: crate::gpu::recorded_choice(),
            showed_every_device: &SHOWED_EVERY_DEVICE,
        }
    }
}

impl ChosenDeviceFilter<'_> {
    fn keep<T>(
        &self,
        host: Vec<T>,
        mut chosen_part: impl FnMut(&ChosenDevice, &T) -> Option<T>,
    ) -> Vec<T> {
        if let Some(chosen) = &self.chosen {
            let kept: Vec<T> = host
                .iter()
                .filter_map(|item| chosen_part(chosen, item))
                .collect();
            if !kept.is_empty() {
                return kept;
            }
        }
        self.warn_once_about_every_device();
        host
    }

    fn warn_once_about_every_device(&self) {
        if self.showed_every_device.swap(true, Ordering::Relaxed) {
            return;
        }
        match &self.chosen {
            Some(chosen) => tracing::warn!(
                "no Vulkan device Roblox enumerates is {chosen}, the GPU Eclipse chose, so \
                 Roblox sees every device"
            ),
            None => tracing::warn!(
                "Eclipse chose no Vulkan device before Roblox enumerated them, so Roblox sees \
                 every device"
            ),
        }
    }
}

unsafe fn is_chosen(
    chosen: &ChosenDevice,
    properties: vk::PFN_vkGetPhysicalDeviceProperties,
    physical_device: vk::PhysicalDevice,
) -> bool {
    let mut found = vk::PhysicalDeviceProperties::default();
    unsafe { properties(physical_device, &mut found) };
    chosen.is(&found)
}

unsafe fn enumerate_chosen_physical_devices(
    enumerate: vk::PFN_vkEnumeratePhysicalDevices,
    properties: vk::PFN_vkGetPhysicalDeviceProperties,
    filter: &ChosenDeviceFilter<'_>,
    instance: vk::Instance,
    p_physical_device_count: *mut u32,
    p_physical_devices: *mut vk::PhysicalDevice,
) -> vk::Result {
    let Some(count) = (unsafe { p_physical_device_count.as_mut() }) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let host = match read_counted(vk::PhysicalDevice::null(), |count, devices| unsafe {
        enumerate(instance, count, devices)
    }) {
        Ok(host) => host,
        Err(r) => return r,
    };
    let devices = filter.keep(host, |chosen, &device| {
        unsafe { is_chosen(chosen, properties, device) }.then_some(device)
    });
    unsafe { copy_counted(&devices, count, p_physical_devices) }
}

unsafe fn chosen_group_members(
    chosen: &ChosenDevice,
    properties: vk::PFN_vkGetPhysicalDeviceProperties,
    group: &vk::PhysicalDeviceGroupProperties<'static>,
) -> Option<vk::PhysicalDeviceGroupProperties<'static>> {
    let mut kept = vk::PhysicalDeviceGroupProperties {
        physical_device_count: 0,
        physical_devices: [vk::PhysicalDevice::null(); vk::MAX_DEVICE_GROUP_SIZE],
        ..*group
    };
    let members = group
        .physical_devices
        .iter()
        .take(group.physical_device_count as usize)
        .copied();
    for device in members.filter(|&device| unsafe { is_chosen(chosen, properties, device) }) {
        kept.physical_devices[kept.physical_device_count as usize] = device;
        kept.physical_device_count += 1;
    }
    match kept.physical_device_count {
        0 => None,
        1 => Some(kept.subset_allocation(false)),
        _ => Some(kept),
    }
}

unsafe fn write_group_members(
    slot: *mut vk::PhysicalDeviceGroupProperties<'_>,
    group: &vk::PhysicalDeviceGroupProperties<'_>,
) {
    unsafe {
        (&raw mut (*slot).physical_device_count).write(group.physical_device_count);
        (&raw mut (*slot).physical_devices).write(group.physical_devices);
        (&raw mut (*slot).subset_allocation).write(group.subset_allocation);
    }
}

unsafe fn enumerate_chosen_physical_device_groups(
    enumerate: vk::PFN_vkEnumeratePhysicalDeviceGroups,
    properties: vk::PFN_vkGetPhysicalDeviceProperties,
    filter: &ChosenDeviceFilter<'_>,
    instance: vk::Instance,
    p_physical_device_group_count: *mut u32,
    p_physical_device_group_properties: *mut vk::PhysicalDeviceGroupProperties<'_>,
) -> vk::Result {
    let Some(count) = (unsafe { p_physical_device_group_count.as_mut() }) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let host = match read_counted(
        vk::PhysicalDeviceGroupProperties::default(),
        |count, groups| unsafe { enumerate(instance, count, groups) },
    ) {
        Ok(host) => host,
        Err(r) => return r,
    };
    let groups = filter.keep(host, |chosen, group| unsafe {
        chosen_group_members(chosen, properties, group)
    });
    let write = |slot, group: &_| unsafe { write_group_members(slot, group) };
    unsafe { fill_counted(&groups, count, p_physical_device_group_properties, write) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::unmapped_xlib_window::UnmappedXlibWindow;

    #[test]
    fn engine_extent_replaces_wayland_undefined_and_keeps_concrete() {
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
        fit_engine_extent(&mut caps);
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
        fit_engine_extent(&mut concrete);
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
    fn a_window_one_pixel_high_or_wide_reaches_roblox_as_two_pixels() {
        for (window, offered) in [((1, 1), (2, 2)), ((300, 1), (300, 2)), ((1, 300), (2, 300))] {
            let window = vk::Extent2D {
                width: window.0,
                height: window.1,
            };
            let mut caps = vk::SurfaceCapabilitiesKHR {
                current_extent: window,
                min_image_extent: window,
                max_image_extent: window,
                ..Default::default()
            };
            fit_engine_extent(&mut caps);
            let offered = vk::Extent2D {
                width: offered.0,
                height: offered.1,
            };
            assert_eq!(
                (
                    caps.current_extent,
                    caps.min_image_extent,
                    caps.max_image_extent
                ),
                (offered, offered, offered),
                "Roblox asserts on a 0-pixel texture when it halves a 1-pixel edge"
            );
        }
    }

    #[test]
    fn swap_android_surface_replaces_only_android_and_preserves_order() {
        let surface = vk::KHR_SURFACE_NAME;
        let pdp2 = c"VK_KHR_get_physical_device_properties2";
        let android = vk::KHR_ANDROID_SURFACE_NAME;
        let wayland = vk::KHR_WAYLAND_SURFACE_NAME;

        let input: [*const c_char; 3] = [surface.as_ptr(), pdp2.as_ptr(), android.as_ptr()];

        let out = unsafe { swap_android_surface(&input, vk::KHR_WAYLAND_SURFACE_NAME) };
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
    fn swap_android_surface_is_identity_without_android() {
        let surface = vk::KHR_SURFACE_NAME;
        let wayland = vk::KHR_WAYLAND_SURFACE_NAME;
        let input: [*const c_char; 2] = [surface.as_ptr(), wayland.as_ptr()];

        let out = unsafe { swap_android_surface(&input, vk::KHR_WAYLAND_SURFACE_NAME) };
        assert_eq!(out.len(), 2);

        assert_eq!(unsafe { CStr::from_ptr(out[0]) }, surface);
        assert_eq!(unsafe { CStr::from_ptr(out[1]) }, wayland);

        let empty = unsafe { swap_android_surface(&[], vk::KHR_WAYLAND_SURFACE_NAME) };
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

    #[test]
    fn android_surface_becomes_the_host_window_system_surface() {
        let xlib = ndk_registry::WsiTarget::Xlib {
            display: 0x1000,
            window: 0x2a0_0002,
        };
        let wayland = ndk_registry::WsiTarget::Wayland {
            display: 0x1000,
            surface: 0x2000,
        };
        assert_eq!(host_surface_extension(xlib), vk::KHR_XLIB_SURFACE_NAME);
        assert_eq!(
            host_surface_extension(wayland),
            vk::KHR_WAYLAND_SURFACE_NAME
        );

        let input = [
            vk::KHR_SURFACE_NAME.as_ptr(),
            vk::KHR_ANDROID_SURFACE_NAME.as_ptr(),
        ];
        let out = unsafe { swap_android_surface(&input, host_surface_extension(xlib)) };
        assert_eq!(unsafe { CStr::from_ptr(out[0]) }, vk::KHR_SURFACE_NAME);
        assert_eq!(unsafe { CStr::from_ptr(out[1]) }, vk::KHR_XLIB_SURFACE_NAME);
        assert!(unsafe { requests_android_surface(&input) });
        assert!(!unsafe { requests_android_surface(&out) });
    }

    const RTX_5070: u64 = 1;
    const RADEON: u64 = 2;
    const LLVMPIPE: u64 = 3;
    const RTX_5070_TWIN: u64 = 4;
    const ARC: u64 = 5;

    fn stub_device_properties(device: u64) -> vk::PhysicalDeviceProperties {
        let (vendor_id, device_id, name) = match device {
            RTX_5070 | RTX_5070_TWIN => (0x10de, 0x2f04, c"NVIDIA GeForce RTX 5070"),
            RADEON => (
                0x1002,
                0x164e,
                c"AMD Radeon Graphics (RADV RAPHAEL_MENDOCINO)",
            ),
            LLVMPIPE => (0x10005, 0, c"llvmpipe (LLVM 22.1.0, 256 bits)"),
            ARC => (0x8086, 0xe20b, c"Intel(R) Arc(tm) B580 Graphics (BMG G21)"),
            other => panic!("no stub device {other}"),
        };
        vk::PhysicalDeviceProperties::default()
            .vendor_id(vendor_id)
            .device_id(device_id)
            .device_name(name)
            .expect("the device name fits")
    }

    unsafe extern "system" fn stub_properties(
        device: vk::PhysicalDevice,
        out: *mut vk::PhysicalDeviceProperties,
    ) {
        unsafe { out.write(stub_device_properties(device.as_raw())) };
    }

    unsafe fn report_host_devices(
        devices: &[u64],
        count: *mut u32,
        out: *mut vk::PhysicalDevice,
    ) -> vk::Result {
        let devices: Vec<_> = devices
            .iter()
            .map(|&raw| vk::PhysicalDevice::from_raw(raw))
            .collect();
        unsafe { copy_counted(&devices, &mut *count, out) }
    }

    unsafe extern "system" fn host_with_three_devices(
        _: vk::Instance,
        count: *mut u32,
        out: *mut vk::PhysicalDevice,
    ) -> vk::Result {
        unsafe { report_host_devices(&[RADEON, RTX_5070, LLVMPIPE], count, out) }
    }

    unsafe extern "system" fn host_with_twins(
        _: vk::Instance,
        count: *mut u32,
        out: *mut vk::PhysicalDevice,
    ) -> vk::Result {
        unsafe { report_host_devices(&[RTX_5070, RADEON, RTX_5070_TWIN], count, out) }
    }

    unsafe extern "system" fn host_without_devices(
        _: vk::Instance,
        _: *mut u32,
        _: *mut vk::PhysicalDevice,
    ) -> vk::Result {
        vk::Result::ERROR_INITIALIZATION_FAILED
    }

    fn group(
        members: &[u64],
        subset_allocation: bool,
    ) -> vk::PhysicalDeviceGroupProperties<'static> {
        let mut group =
            vk::PhysicalDeviceGroupProperties::default().subset_allocation(subset_allocation);
        for (slot, &raw) in group.physical_devices.iter_mut().zip(members) {
            *slot = vk::PhysicalDevice::from_raw(raw);
        }
        group.physical_device_count = members.len() as u32;
        group
    }

    unsafe extern "system" fn host_with_two_groups(
        _: vk::Instance,
        count: *mut u32,
        out: *mut vk::PhysicalDeviceGroupProperties<'_>,
    ) -> vk::Result {
        let groups = [group(&[RADEON, RTX_5070], true), group(&[LLVMPIPE], false)];
        unsafe { copy_counted(&groups, &mut *count, out.cast()) }
    }

    unsafe extern "system" fn host_with_a_twin_group(
        _: vk::Instance,
        count: *mut u32,
        out: *mut vk::PhysicalDeviceGroupProperties<'_>,
    ) -> vk::Result {
        let groups = [
            group(&[RADEON], false),
            group(&[RTX_5070, RTX_5070_TWIN], true),
        ];
        unsafe { copy_counted(&groups, &mut *count, out.cast()) }
    }

    unsafe extern "system" fn host_without_groups(
        _: vk::Instance,
        _: *mut u32,
        _: *mut vk::PhysicalDeviceGroupProperties<'_>,
    ) -> vk::Result {
        vk::Result::ERROR_INITIALIZATION_FAILED
    }

    fn filter(chosen: Option<u64>, showed_every_device: &AtomicBool) -> ChosenDeviceFilter<'_> {
        ChosenDeviceFilter {
            chosen: chosen
                .map(|device| ChosenDevice::of_properties(&stub_device_properties(device))),
            showed_every_device,
        }
    }

    fn client_devices(
        enumerate: vk::PFN_vkEnumeratePhysicalDevices,
        filter: &ChosenDeviceFilter<'_>,
    ) -> Result<Vec<u64>, vk::Result> {
        let call = |count: &mut u32, out: *mut vk::PhysicalDevice| unsafe {
            enumerate_chosen_physical_devices(
                enumerate,
                stub_properties,
                filter,
                vk::Instance::null(),
                count,
                out,
            )
        };
        let mut count = 0;
        let r = call(&mut count, std::ptr::null_mut());
        if r != vk::Result::SUCCESS {
            return Err(r);
        }
        let mut devices = vec![vk::PhysicalDevice::null(); count as usize];
        assert_eq!(call(&mut count, devices.as_mut_ptr()), vk::Result::SUCCESS);
        assert_eq!(count as usize, devices.len());
        Ok(devices.iter().map(|device| device.as_raw()).collect())
    }

    fn client_groups(
        enumerate: vk::PFN_vkEnumeratePhysicalDeviceGroups,
        filter: &ChosenDeviceFilter<'_>,
    ) -> Result<Vec<(Vec<u64>, bool)>, vk::Result> {
        let call = |count: &mut u32, out: *mut vk::PhysicalDeviceGroupProperties<'_>| unsafe {
            enumerate_chosen_physical_device_groups(
                enumerate,
                stub_properties,
                filter,
                vk::Instance::null(),
                count,
                out,
            )
        };
        let mut count = 0;
        let r = call(&mut count, std::ptr::null_mut());
        if r != vk::Result::SUCCESS {
            return Err(r);
        }
        let mut chained = 0u8;
        let chain = (&raw mut chained).cast::<std::ffi::c_void>();
        let mut groups = vec![
            vk::PhysicalDeviceGroupProperties {
                p_next: chain,
                physical_device_count: u32::MAX,
                ..Default::default()
            };
            count as usize
        ];
        assert_eq!(call(&mut count, groups.as_mut_ptr()), vk::Result::SUCCESS);
        assert_eq!(count as usize, groups.len());
        Ok(groups
            .iter()
            .map(|group| {
                assert_eq!(
                    group.s_type,
                    vk::StructureType::PHYSICAL_DEVICE_GROUP_PROPERTIES
                );
                assert_eq!(group.p_next, chain, "the caller's pNext chain stays");
                let members = group.physical_devices[..group.physical_device_count as usize]
                    .iter()
                    .map(|device| device.as_raw())
                    .collect();
                (members, group.subset_allocation == vk::TRUE)
            })
            .collect())
    }

    #[test]
    fn the_client_sees_only_the_chosen_device() {
        let showed_every_device = AtomicBool::new(false);
        assert_eq!(
            client_devices(
                host_with_three_devices,
                &filter(Some(RTX_5070), &showed_every_device)
            ),
            Ok(vec![RTX_5070])
        );
        assert_eq!(
            client_devices(
                host_with_three_devices,
                &filter(Some(LLVMPIPE), &showed_every_device)
            ),
            Ok(vec![LLVMPIPE])
        );
        assert!(!showed_every_device.load(Ordering::SeqCst));
    }

    #[test]
    fn identical_twins_of_the_chosen_device_are_all_kept() {
        let showed_every_device = AtomicBool::new(false);
        assert_eq!(
            client_devices(
                host_with_twins,
                &filter(Some(RTX_5070), &showed_every_device)
            ),
            Ok(vec![RTX_5070, RTX_5070_TWIN])
        );
    }

    #[test]
    fn device_enumeration_follows_the_vulkan_two_call_contract() {
        let showed_every_device = AtomicBool::new(false);
        let filter = filter(Some(RTX_5070), &showed_every_device);
        let call = |count: *mut u32, out: *mut vk::PhysicalDevice| unsafe {
            enumerate_chosen_physical_devices(
                host_with_twins,
                stub_properties,
                &filter,
                vk::Instance::null(),
                count,
                out,
            )
        };
        let untouched = vk::PhysicalDevice::from_raw(0xdead);

        let mut count = 0;
        assert_eq!(call(&mut count, std::ptr::null_mut()), vk::Result::SUCCESS);
        assert_eq!(count, 2);

        let mut short = [untouched; 2];
        let mut count = 1;
        assert_eq!(call(&mut count, short.as_mut_ptr()), vk::Result::INCOMPLETE);
        assert_eq!(
            (count, short),
            (1, [vk::PhysicalDevice::from_raw(RTX_5070), untouched])
        );

        let mut count = 0;
        assert_eq!(call(&mut count, short.as_mut_ptr()), vk::Result::INCOMPLETE);
        assert_eq!(count, 0);

        let mut roomy = [untouched; 3];
        let mut count = 3;
        assert_eq!(call(&mut count, roomy.as_mut_ptr()), vk::Result::SUCCESS);
        assert_eq!(count, 2);
        assert_eq!(roomy[2], untouched);

        assert_eq!(
            call(std::ptr::null_mut(), std::ptr::null_mut()),
            vk::Result::ERROR_INITIALIZATION_FAILED
        );
    }

    #[test]
    fn without_a_recorded_or_matching_choice_the_client_sees_every_device_and_one_warning() {
        let showed_every_device = AtomicBool::new(false);
        let unchosen = filter(None, &showed_every_device);
        let log = crate::loader::log_capture::formatted_log("warn", || {
            for _ in 0..2 {
                assert_eq!(
                    client_devices(host_with_three_devices, &unchosen),
                    Ok(vec![RADEON, RTX_5070, LLVMPIPE])
                );
            }
        });
        assert_eq!(
            log.matches("Eclipse chose no Vulkan device before Roblox enumerated them")
                .count(),
            1,
            "{log}"
        );

        let showed_every_device = AtomicBool::new(false);
        let absent = filter(Some(ARC), &showed_every_device);
        let log = crate::loader::log_capture::formatted_log("warn", || {
            assert_eq!(
                client_groups(host_with_two_groups, &absent),
                Ok(vec![
                    (vec![RADEON, RTX_5070], true),
                    (vec![LLVMPIPE], false)
                ])
            );
        });
        assert!(
            log.contains(
                "no Vulkan device Roblox enumerates is Intel(R) Arc(tm) B580 Graphics (BMG G21) \
                 [8086:e20b], the GPU Eclipse chose"
            ),
            "{log}"
        );
    }

    #[test]
    fn a_host_enumeration_error_reaches_the_client() {
        let showed_every_device = AtomicBool::new(false);
        let filter = filter(Some(RTX_5070), &showed_every_device);
        assert_eq!(
            client_devices(host_without_devices, &filter),
            Err(vk::Result::ERROR_INITIALIZATION_FAILED)
        );
        assert_eq!(
            client_groups(host_without_groups, &filter),
            Err(vk::Result::ERROR_INITIALIZATION_FAILED)
        );
    }

    #[test]
    fn device_groups_keep_only_the_chosen_members() {
        let showed_every_device = AtomicBool::new(false);
        let filter = filter(Some(RTX_5070), &showed_every_device);
        assert_eq!(
            client_groups(host_with_two_groups, &filter),
            Ok(vec![(vec![RTX_5070], false)]),
            "a group left with one device cannot allocate memory on a subset"
        );
        assert_eq!(
            client_groups(host_with_a_twin_group, &filter),
            Ok(vec![(vec![RTX_5070, RTX_5070_TWIN], true)])
        );
    }

    #[test]
    fn device_group_enumeration_follows_the_vulkan_two_call_contract() {
        let showed_every_device = AtomicBool::new(false);
        let filter = filter(Some(RTX_5070), &showed_every_device);
        let call = |count: *mut u32, out: *mut vk::PhysicalDeviceGroupProperties<'_>| unsafe {
            enumerate_chosen_physical_device_groups(
                host_with_two_groups,
                stub_properties,
                &filter,
                vk::Instance::null(),
                count,
                out,
            )
        };
        let mut count = 0;
        assert_eq!(call(&mut count, std::ptr::null_mut()), vk::Result::SUCCESS);
        assert_eq!(count, 1);

        let mut untouched = [vk::PhysicalDeviceGroupProperties {
            physical_device_count: u32::MAX,
            ..Default::default()
        }];
        let mut count = 0;
        assert_eq!(
            call(&mut count, untouched.as_mut_ptr()),
            vk::Result::INCOMPLETE
        );
        assert_eq!((count, untouched[0].physical_device_count), (0, u32::MAX));

        assert_eq!(
            call(std::ptr::null_mut(), std::ptr::null_mut()),
            vk::Result::ERROR_INITIALIZATION_FAILED
        );
    }

    #[test]
    fn the_client_resolves_device_enumeration_to_the_chosen_gpu_filter() {
        let Some(entry) = host_entry() else {
            eprintln!("SKIP: no Vulkan loader");
            return;
        };
        let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let create_info = vk::InstanceCreateInfo::default().application_info(&application);
        let instance = match unsafe { entry.create_instance(&create_info, None) } {
            Ok(instance) => instance,
            Err(error) => {
                eprintln!("SKIP: no Vulkan 1.1 instance ({error})");
                return;
            }
        };
        let resolve = |name: &CStr| {
            unsafe { eclipse_vk_get_instance_proc_addr(instance.handle(), name.as_ptr()) }
                .map(|function| function as *const ())
        };
        let host_has = |name: &CStr| {
            unsafe { (entry.static_fn().get_instance_proc_addr)(instance.handle(), name.as_ptr()) }
                .is_some()
        };
        let devices = resolve(ENUMERATE_PHYSICAL_DEVICES);
        let groups = resolve(ENUMERATE_PHYSICAL_DEVICE_GROUPS);
        let groups_khr = resolve(ENUMERATE_PHYSICAL_DEVICE_GROUPS_KHR);
        let host_has_groups_khr = host_has(ENUMERATE_PHYSICAL_DEVICE_GROUPS_KHR);
        let global = unsafe {
            eclipse_vk_get_instance_proc_addr(
                vk::Instance::null(),
                ENUMERATE_PHYSICAL_DEVICES.as_ptr(),
            )
        };
        let host = unsafe { instance.enumerate_physical_devices() };
        let client = devices.map(|enumerate| {
            let enumerate = unsafe {
                std::mem::transmute::<*const (), vk::PFN_vkEnumeratePhysicalDevices>(enumerate)
            };
            read_counted(vk::PhysicalDevice::null(), |count, out| unsafe {
                enumerate(instance.handle(), count, out)
            })
        });
        unsafe { instance.destroy_instance(None) };

        assert_eq!(
            devices,
            Some(eclipse_vk_enumerate_physical_devices as *const ())
        );
        assert_eq!(
            groups,
            Some(eclipse_vk_enumerate_physical_device_groups as *const ())
        );
        assert_eq!(
            groups_khr,
            host_has_groups_khr
                .then_some(eclipse_vk_enumerate_physical_device_groups_khr as *const ()),
            "the KHR alias exists for the client exactly when the host offers it"
        );
        assert!(global.is_none(), "device enumeration needs an instance");
        let host = host.expect("the host lists its devices");
        let client = client
            .expect("the shim was resolved")
            .expect("the client lists its devices");
        assert!(client.iter().all(|device| host.contains(device)));
        assert_eq!(client.is_empty(), host.is_empty());
    }

    const X11_SURFACE_CHILD: &str = "ECLIPSE_TEST_X11_SURFACE_CHILD";

    const X11_SURFACE_CHILD_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

    fn bionic_native(name: &str) -> u64 {
        use super::super::resolve::SymbolProvider;
        super::super::native_provider::EclipseNativeProvider::with_bionic_natives()
            .resolve(name)
            .unwrap_or_else(|| panic!("{name} is an Eclipse native"))
            .addr
    }

    fn engine_vulkan_surface_on(window: &UnmappedXlibWindow) {
        let create_instance: vk::PFN_vkCreateInstance =
            unsafe { std::mem::transmute(bionic_native("vkCreateInstance") as usize) };
        let create_android_surface: vk::PFN_vkCreateAndroidSurfaceKHR =
            unsafe { std::mem::transmute(bionic_native("vkCreateAndroidSurfaceKHR") as usize) };
        let extensions = [
            vk::KHR_SURFACE_NAME.as_ptr(),
            vk::KHR_ANDROID_SURFACE_NAME.as_ptr(),
        ];
        let create_info = vk::InstanceCreateInfo::default().enabled_extension_names(&extensions);
        let mut instance = vk::Instance::null();
        let created = unsafe { create_instance(&create_info, std::ptr::null(), &mut instance) };
        if created != vk::Result::SUCCESS {
            eprintln!("SKIP: no Vulkan driver offers VK_KHR_xlib_surface ({created:?})");
            return;
        }
        let entry = host_entry().expect("vkCreateInstance succeeded through the host loader");
        let instance = unsafe { ash::Instance::load(entry.static_fn(), instance) };
        let mut surface = vk::SurfaceKHR::null();
        let result = unsafe {
            create_android_surface(
                instance.handle(),
                &vk::AndroidSurfaceCreateInfoKHR::default(),
                std::ptr::null(),
                &mut surface,
            )
        };
        let surfaces = ash::khr::surface::Instance::new(entry, &instance);
        unsafe {
            if result == vk::Result::SUCCESS {
                surfaces.destroy_surface(surface, None);
            }
            instance.destroy_instance(None);
        }
        assert_eq!(result, vk::Result::SUCCESS, "window {:#x}", window.window);
        assert_ne!(surface, vk::SurfaceKHR::null());
    }

    fn engine_egl_surface_on(window: &UnmappedXlibWindow) {
        use khronos_egl as egl;
        let get_display: unsafe extern "C" fn(*mut std::ffi::c_void) -> *mut std::ffi::c_void =
            unsafe { std::mem::transmute(bionic_native("eglGetDisplay") as usize) };
        let lib =
            unsafe { libloading::Library::new(super::super::native_provider::HOST_EGL_SONAME) }
                .expect("host libEGL");
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required_from(lib) }
            .expect("EGL 1.4 entry points");
        let raw = unsafe { get_display(std::ptr::null_mut()) };
        assert!(!raw.is_null(), "eglGetDisplay(EGL_DEFAULT_DISPLAY) on X11");
        let display = unsafe { egl::Display::from_ptr(raw) };
        egl.initialize(display).expect("eglInitialize");
        let attribs = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES2_BIT,
            egl::NONE,
        ];
        let mut configs = Vec::with_capacity(256);
        egl.choose_config(display, &attribs, &mut configs)
            .expect("eglChooseConfig");
        let config = configs
            .into_iter()
            .find(|&config| {
                egl.get_config_attrib(display, config, egl::NATIVE_VISUAL_ID)
                    .is_ok_and(|id| id as std::ffi::c_ulong == window.visual_id)
            })
            .expect("a GLES2 window config for the window's visual");
        let surface = unsafe {
            egl.create_window_surface(
                display,
                config,
                window.window as egl::NativeWindowType,
                None,
            )
        }
        .expect("eglCreateWindowSurface on the X11 window");
        egl.destroy_surface(display, surface)
            .expect("eglDestroySurface");
        egl.terminate(display).expect("eglTerminate");
    }

    #[test]
    fn x11_window_backs_the_engine_vulkan_and_egl_surfaces() {
        if std::env::var_os(X11_SURFACE_CHILD).is_none() {
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
                    "loader::vulkan_wsi::tests::\
                     x11_window_backs_the_engine_vulkan_and_egl_surfaces",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(X11_SURFACE_CHILD, "1")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("WAYLAND_SOCKET"),
                X11_SURFACE_CHILD_LIMIT,
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
        let window = match UnmappedXlibWindow::open(64, 64) {
            Ok(window) => window,
            Err(e) => {
                eprintln!("SKIP: no usable X11 display ({e})");
                return;
            }
        };
        ndk_registry::set_wsi_target(Some(ndk_registry::WsiTarget::Xlib {
            display: window.display as usize,
            window: window.window,
        }));
        engine_vulkan_surface_on(&window);
        engine_egl_surface_on(&window);
    }
}
