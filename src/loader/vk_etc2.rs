use super::etc2::{self, Etc2Format};
use ash::vk;
use ash::vk::Handle;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let on = std::env::var_os("ECLIPSE_ETC2_EMULATION").is_none_or(|v| v != "0");
        tracing::info!(on, "vk-etc2: ETC2 texture emulation");
        on
    })
}

static HOST_GET_FEATURES: AtomicU64 = AtomicU64::new(0);
static HOST_GET_FORMAT_PROPERTIES: AtomicU64 = AtomicU64::new(0);
static HOST_GET_IMAGE_FORMAT_PROPERTIES: AtomicU64 = AtomicU64::new(0);
static HOST_CREATE_IMAGE: AtomicU64 = AtomicU64::new(0);
static HOST_DESTROY_IMAGE: AtomicU64 = AtomicU64::new(0);
static HOST_CREATE_IMAGE_VIEW: AtomicU64 = AtomicU64::new(0);
static HOST_BIND_BUFFER_MEMORY: AtomicU64 = AtomicU64::new(0);
static HOST_DESTROY_BUFFER: AtomicU64 = AtomicU64::new(0);
static HOST_MAP_MEMORY: AtomicU64 = AtomicU64::new(0);
static HOST_UNMAP_MEMORY: AtomicU64 = AtomicU64::new(0);
static HOST_FREE_MEMORY: AtomicU64 = AtomicU64::new(0);
static HOST_CMD_COPY_BUFFER_TO_IMAGE: AtomicU64 = AtomicU64::new(0);

struct Decoded {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: u64,
    mapped: usize,
}

const MIN_STAGING_BYTES: u64 = 64 * 1024;
const POOL_KEEP_BYTES: u64 = 256 * 1024 * 1024;

static POOL: Mutex<Vec<Decoded>> = Mutex::new(Vec::new());

#[derive(Default)]
struct Tracking {
    images: HashMap<u64, Etc2Format>,
    buffer_memory: HashMap<u64, (u64, u64)>,
    mappings: HashMap<u64, (usize, u64)>,
    decoded: HashMap<u64, Vec<Decoded>>,
}

struct Host {
    device: ash::Device,
    memory: vk::PhysicalDeviceMemoryProperties,
}

static TRACKING: Mutex<Option<Tracking>> = Mutex::new(None);
static HOST: Mutex<Option<Host>> = Mutex::new(None);

fn with_tracking<R>(f: impl FnOnce(&mut Tracking) -> R) -> Option<R> {
    let mut guard = TRACKING.lock().ok()?;
    Some(f(guard.get_or_insert_with(Tracking::default)))
}

fn store(slot: &AtomicU64, host: vk::PFN_vkVoidFunction) -> bool {
    let addr = host.map_or(0, |f| f as usize as u64);
    slot.store(addr, Ordering::Relaxed);
    addr != 0
}

fn load<F: Copy>(slot: &AtomicU64) -> Option<F> {
    match slot.load(Ordering::Relaxed) {
        0 => None,
        addr => Some(unsafe { std::mem::transmute_copy::<usize, F>(&(addr as usize)) }),
    }
}

macro_rules! shim {
    ($pfn:ty, $f:expr) => {
        Some(unsafe { std::mem::transmute::<$pfn, unsafe extern "system" fn()>($f) })
    };
}

pub(crate) fn intercept(
    name: &CStr,
    host: impl FnOnce() -> vk::PFN_vkVoidFunction,
) -> Option<vk::PFN_vkVoidFunction> {
    if !enabled() {
        return None;
    }
    let (slot, wrapper): (&AtomicU64, vk::PFN_vkVoidFunction) = match name.to_bytes() {
        b"vkGetPhysicalDeviceFeatures" => (
            &HOST_GET_FEATURES,
            shim!(
                vk::PFN_vkGetPhysicalDeviceFeatures,
                get_physical_device_features
            ),
        ),
        b"vkGetPhysicalDeviceFormatProperties" => (
            &HOST_GET_FORMAT_PROPERTIES,
            shim!(
                vk::PFN_vkGetPhysicalDeviceFormatProperties,
                get_format_properties
            ),
        ),
        b"vkGetPhysicalDeviceImageFormatProperties" => (
            &HOST_GET_IMAGE_FORMAT_PROPERTIES,
            shim!(
                vk::PFN_vkGetPhysicalDeviceImageFormatProperties,
                get_image_format_properties
            ),
        ),
        b"vkCreateImage" => (
            &HOST_CREATE_IMAGE,
            shim!(vk::PFN_vkCreateImage, create_image),
        ),
        b"vkDestroyImage" => (
            &HOST_DESTROY_IMAGE,
            shim!(vk::PFN_vkDestroyImage, destroy_image),
        ),
        b"vkCreateImageView" => (
            &HOST_CREATE_IMAGE_VIEW,
            shim!(vk::PFN_vkCreateImageView, create_image_view),
        ),
        b"vkBindBufferMemory" => (
            &HOST_BIND_BUFFER_MEMORY,
            shim!(vk::PFN_vkBindBufferMemory, bind_buffer_memory),
        ),
        b"vkDestroyBuffer" => (
            &HOST_DESTROY_BUFFER,
            shim!(vk::PFN_vkDestroyBuffer, destroy_buffer),
        ),
        b"vkMapMemory" => (&HOST_MAP_MEMORY, shim!(vk::PFN_vkMapMemory, map_memory)),
        b"vkUnmapMemory" => (
            &HOST_UNMAP_MEMORY,
            shim!(vk::PFN_vkUnmapMemory, unmap_memory),
        ),
        b"vkFreeMemory" => (&HOST_FREE_MEMORY, shim!(vk::PFN_vkFreeMemory, free_memory)),
        b"vkCmdCopyBufferToImage" => (
            &HOST_CMD_COPY_BUFFER_TO_IMAGE,
            shim!(vk::PFN_vkCmdCopyBufferToImage, cmd_copy_buffer_to_image),
        ),
        _ => return None,
    };
    if store(slot, host()) {
        Some(wrapper)
    } else {
        Some(None)
    }
}

pub(crate) fn strip_from_enabled_features(features: &mut vk::PhysicalDeviceFeatures) {
    if enabled() {
        features.texture_compression_etc2 = vk::FALSE;
    }
}

pub(crate) fn device_created(
    instance: vk::Instance,
    physical_device: vk::PhysicalDevice,
    device: vk::Device,
) {
    if !enabled() {
        return;
    }
    let Some(entry) = super::vulkan_wsi::host_entry() else {
        tracing::warn!("vk-etc2: no host Vulkan entry; ETC2 uploads will be skipped");
        return;
    };
    let gipa = entry.static_fn().get_instance_proc_addr;
    let Some(gdpa) = (unsafe { gipa(instance, c"vkGetDeviceProcAddr".as_ptr()) }) else {
        tracing::warn!("vk-etc2: host has no vkGetDeviceProcAddr; ETC2 uploads will be skipped");
        return;
    };
    let gdpa: vk::PFN_vkGetDeviceProcAddr = unsafe { std::mem::transmute(gdpa) };
    let missing = [c"vkGetPhysicalDeviceMemoryProperties"]
        .into_iter()
        .find(|name| unsafe { gipa(instance, name.as_ptr()) }.is_none())
        .or_else(|| {
            [
                c"vkCreateBuffer",
                c"vkDestroyBuffer",
                c"vkGetBufferMemoryRequirements",
                c"vkAllocateMemory",
                c"vkFreeMemory",
                c"vkBindBufferMemory",
                c"vkMapMemory",
            ]
            .into_iter()
            .find(|name| unsafe { gdpa(device, name.as_ptr()) }.is_none())
        });
    if let Some(name) = missing {
        tracing::warn!(
            ?name,
            "vk-etc2: host lacks a staging command; ETC2 uploads will be skipped"
        );
        return;
    }
    let instance = unsafe { ash::Instance::load(entry.static_fn(), instance) };
    let memory = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    let device = unsafe { ash::Device::load(instance.fp_v1_0(), device) };
    if let Ok(mut host) = HOST.lock() {
        *host = Some(Host { device, memory });
    }
}

pub(crate) fn device_destroyed() {
    let tracking = with_tracking(std::mem::take).unwrap_or_default();
    let pooled = POOL
        .lock()
        .map(|mut pool| std::mem::take(&mut *pool))
        .unwrap_or_default();
    let Ok(mut host) = HOST.lock() else { return };
    if let Some(host) = host.take() {
        for decoded in tracking.decoded.into_values().flatten().chain(pooled) {
            unsafe { free_decoded(&host.device, &decoded) };
        }
    }
}

fn recycle(host: &Host, finished: Vec<Decoded>) {
    let Ok(mut pool) = POOL.lock() else { return };
    pool.extend(finished);
    let mut kept: u64 = pool.iter().map(|d| d.capacity).sum();
    while kept > POOL_KEEP_BYTES {
        let Some(largest) = (0..pool.len()).max_by_key(|&i| pool[i].capacity) else {
            break;
        };
        let decoded = pool.swap_remove(largest);
        kept -= decoded.capacity;
        unsafe { free_decoded(&host.device, &decoded) };
    }
}

unsafe fn free_decoded(device: &ash::Device, decoded: &Decoded) {
    unsafe {
        device.destroy_buffer(decoded.buffer, None);
        device.free_memory(decoded.memory, None);
    }
}

fn substitute(format: vk::Format) -> Option<(Etc2Format, vk::Format)> {
    let etc2 = Etc2Format::from_vk(format.as_raw())?;
    let srgb = matches!(
        format,
        vk::Format::ETC2_R8G8B8_SRGB_BLOCK | vk::Format::ETC2_R8G8B8A8_SRGB_BLOCK
    );
    Some((
        etc2,
        if srgb {
            vk::Format::R8G8B8A8_SRGB
        } else {
            vk::Format::R8G8B8A8_UNORM
        },
    ))
}

unsafe extern "system" fn get_physical_device_features(
    physical_device: vk::PhysicalDevice,
    p_features: *mut vk::PhysicalDeviceFeatures,
) {
    let Some(host) = load::<vk::PFN_vkGetPhysicalDeviceFeatures>(&HOST_GET_FEATURES) else {
        return;
    };
    unsafe { host(physical_device, p_features) };
    if let Some(features) = unsafe { p_features.as_mut() } {
        if features.texture_compression_etc2 == vk::FALSE {
            features.texture_compression_etc2 = vk::TRUE;
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                tracing::info!("vk-etc2: advertising textureCompressionETC2 (decoded on the CPU)");
            }
        }
    }
}

unsafe extern "system" fn get_format_properties(
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
    p_properties: *mut vk::FormatProperties,
) {
    let Some(host) =
        load::<vk::PFN_vkGetPhysicalDeviceFormatProperties>(&HOST_GET_FORMAT_PROPERTIES)
    else {
        return;
    };
    let format = substitute(format).map_or(format, |(_, rgba)| rgba);
    unsafe { host(physical_device, format, p_properties) };
}

unsafe extern "system" fn get_image_format_properties(
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
    image_type: vk::ImageType,
    tiling: vk::ImageTiling,
    usage: vk::ImageUsageFlags,
    flags: vk::ImageCreateFlags,
    p_properties: *mut vk::ImageFormatProperties,
) -> vk::Result {
    let Some(host) =
        load::<vk::PFN_vkGetPhysicalDeviceImageFormatProperties>(&HOST_GET_IMAGE_FORMAT_PROPERTIES)
    else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let (format, flags) = match substitute(format) {
        Some((_, rgba)) => (
            rgba,
            flags & !vk::ImageCreateFlags::BLOCK_TEXEL_VIEW_COMPATIBLE,
        ),
        None => (format, flags),
    };
    unsafe {
        host(
            physical_device,
            format,
            image_type,
            tiling,
            usage,
            flags,
            p_properties,
        )
    }
}

unsafe extern "system" fn create_image(
    device: vk::Device,
    p_create_info: *const vk::ImageCreateInfo<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_image: *mut vk::Image,
) -> vk::Result {
    let Some(host) = load::<vk::PFN_vkCreateImage>(&HOST_CREATE_IMAGE) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let Some((etc2, rgba)) =
        (unsafe { p_create_info.as_ref() }).and_then(|info| substitute(info.format))
    else {
        return unsafe { host(device, p_create_info, p_allocator, p_image) };
    };
    let mut info = unsafe { *p_create_info };
    info.format = rgba;
    info.flags &= !vk::ImageCreateFlags::BLOCK_TEXEL_VIEW_COMPATIBLE;
    let r = unsafe { host(device, &info, p_allocator, p_image) };
    if r == vk::Result::SUCCESS {
        if let Some(image) = unsafe { p_image.as_ref() } {
            with_tracking(|t| t.images.insert(image.as_raw(), etc2));
        }
    }
    r
}

unsafe extern "system" fn destroy_image(
    device: vk::Device,
    image: vk::Image,
    p_allocator: *const vk::AllocationCallbacks<'_>,
) {
    with_tracking(|t| t.images.remove(&image.as_raw()));
    if let Some(host) = load::<vk::PFN_vkDestroyImage>(&HOST_DESTROY_IMAGE) {
        unsafe { host(device, image, p_allocator) };
    }
}

unsafe extern "system" fn create_image_view(
    device: vk::Device,
    p_create_info: *const vk::ImageViewCreateInfo<'_>,
    p_allocator: *const vk::AllocationCallbacks<'_>,
    p_view: *mut vk::ImageView,
) -> vk::Result {
    let Some(host) = load::<vk::PFN_vkCreateImageView>(&HOST_CREATE_IMAGE_VIEW) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    match (unsafe { p_create_info.as_ref() }).and_then(|info| substitute(info.format)) {
        Some((_, rgba)) => {
            let mut info = unsafe { *p_create_info };
            info.format = rgba;
            unsafe { host(device, &info, p_allocator, p_view) }
        }
        None => unsafe { host(device, p_create_info, p_allocator, p_view) },
    }
}

unsafe extern "system" fn bind_buffer_memory(
    device: vk::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    offset: vk::DeviceSize,
) -> vk::Result {
    let Some(host) = load::<vk::PFN_vkBindBufferMemory>(&HOST_BIND_BUFFER_MEMORY) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let r = unsafe { host(device, buffer, memory, offset) };
    if r == vk::Result::SUCCESS {
        with_tracking(|t| {
            t.buffer_memory
                .insert(buffer.as_raw(), (memory.as_raw(), offset))
        });
    }
    r
}

unsafe extern "system" fn destroy_buffer(
    device: vk::Device,
    buffer: vk::Buffer,
    p_allocator: *const vk::AllocationCallbacks<'_>,
) {
    if let Some(host) = load::<vk::PFN_vkDestroyBuffer>(&HOST_DESTROY_BUFFER) {
        unsafe { host(device, buffer, p_allocator) };
    }
    let decoded = with_tracking(|t| {
        t.buffer_memory.remove(&buffer.as_raw());
        t.decoded.remove(&buffer.as_raw())
    })
    .flatten();
    if let Some(decoded) = decoded {
        if let Ok(host) = HOST.lock() {
            if let Some(host) = host.as_ref() {
                recycle(host, decoded);
            }
        }
    }
}

unsafe extern "system" fn map_memory(
    device: vk::Device,
    memory: vk::DeviceMemory,
    offset: vk::DeviceSize,
    size: vk::DeviceSize,
    flags: vk::MemoryMapFlags,
    pp_data: *mut *mut c_void,
) -> vk::Result {
    let Some(host) = load::<vk::PFN_vkMapMemory>(&HOST_MAP_MEMORY) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let r = unsafe { host(device, memory, offset, size, flags, pp_data) };
    if r == vk::Result::SUCCESS {
        if let Some(&ptr) = unsafe { pp_data.as_ref() } {
            with_tracking(|t| t.mappings.insert(memory.as_raw(), (ptr as usize, offset)));
        }
    }
    r
}

unsafe extern "system" fn unmap_memory(device: vk::Device, memory: vk::DeviceMemory) {
    with_tracking(|t| t.mappings.remove(&memory.as_raw()));
    if let Some(host) = load::<vk::PFN_vkUnmapMemory>(&HOST_UNMAP_MEMORY) {
        unsafe { host(device, memory) };
    }
}

unsafe extern "system" fn free_memory(
    device: vk::Device,
    memory: vk::DeviceMemory,
    p_allocator: *const vk::AllocationCallbacks<'_>,
) {
    with_tracking(|t| t.mappings.remove(&memory.as_raw()));
    if let Some(host) = load::<vk::PFN_vkFreeMemory>(&HOST_FREE_MEMORY) {
        unsafe { host(device, memory, p_allocator) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RegionLayout {
    blocks_per_row: usize,
    block_rows: usize,
    layer_bytes: usize,
    layers: usize,
}

fn region_layout(format: Etc2Format, region: &vk::BufferImageCopy) -> RegionLayout {
    let row_texels = match region.buffer_row_length {
        0 => region.image_extent.width,
        n => n,
    };
    let height_texels = match region.buffer_image_height {
        0 => region.image_extent.height,
        n => n,
    };
    let blocks_per_row = row_texels.div_ceil(4) as usize;
    let block_rows = height_texels.div_ceil(4) as usize;
    let layers = (region.image_subresource.layer_count as usize)
        .max(region.image_extent.depth as usize)
        .max(1);
    RegionLayout {
        blocks_per_row,
        block_rows,
        layer_bytes: blocks_per_row * block_rows * format.block_bytes(),
        layers,
    }
}

fn decode_region(format: Etc2Format, src: &[u8], region: &vk::BufferImageCopy) -> Vec<u8> {
    let layout = region_layout(format, region);
    let (width, height) = (region.image_extent.width, region.image_extent.height);
    let texel_bytes = width as usize * height as usize * 4;
    let mut out = vec![0u8; texel_bytes * layout.layers];
    for layer in 0..layout.layers {
        let start = layer * layout.layer_bytes;
        let Some(blocks) = src.get(start..start + layout.layer_bytes) else {
            break;
        };
        etc2::decode(
            format,
            blocks,
            layout.blocks_per_row,
            width,
            height,
            &mut out[layer * texel_bytes..(layer + 1) * texel_bytes],
        );
    }
    out
}

unsafe fn source_bytes<'a>(
    tracking: &Tracking,
    buffer: vk::Buffer,
    format: Etc2Format,
    region: &vk::BufferImageCopy,
) -> Option<&'a [u8]> {
    let &(memory, bind_offset) = tracking.buffer_memory.get(&buffer.as_raw())?;
    let &(base, map_offset) = tracking.mappings.get(&memory)?;
    let at = (bind_offset + region.buffer_offset).checked_sub(map_offset)?;
    let layout = region_layout(format, region);
    let len = layout.layer_bytes * layout.layers;
    Some(unsafe { std::slice::from_raw_parts((base + at as usize) as *const u8, len) })
}

unsafe fn upload_decoded(host: &Host, texels: &[u8]) -> Option<Decoded> {
    let len = texels.len() as u64;
    let reused = POOL.lock().ok().and_then(|mut pool| {
        let best = (0..pool.len())
            .filter(|&i| pool[i].capacity >= len)
            .min_by_key(|&i| pool[i].capacity)?;
        Some(pool.swap_remove(best))
    });
    let decoded = match reused {
        Some(decoded) => decoded,
        None => unsafe { allocate_staging(host, len.next_power_of_two().max(MIN_STAGING_BYTES))? },
    };
    unsafe {
        std::ptr::copy_nonoverlapping(texels.as_ptr(), decoded.mapped as *mut u8, texels.len());
    }
    Some(decoded)
}

unsafe fn allocate_staging(host: &Host, capacity: u64) -> Option<Decoded> {
    let device = &host.device;
    let info = vk::BufferCreateInfo::default()
        .size(capacity)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { device.create_buffer(&info, None) }.ok()?;
    let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
    let coherent = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let Some(memory_type) = (0..host.memory.memory_type_count).find(|&i| {
        requirements.memory_type_bits & (1 << i) != 0
            && host.memory.memory_types[i as usize]
                .property_flags
                .contains(coherent)
    }) else {
        unsafe { device.destroy_buffer(buffer, None) };
        return None;
    };
    let allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_type);
    let Ok(memory) = (unsafe { device.allocate_memory(&allocate, None) }) else {
        unsafe { device.destroy_buffer(buffer, None) };
        return None;
    };
    let mapped = unsafe {
        device
            .bind_buffer_memory(buffer, memory, 0)
            .ok()
            .and_then(|()| {
                device
                    .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                    .ok()
            })
    };
    let decoded = Decoded {
        buffer,
        memory,
        capacity,
        mapped: mapped.map_or(0, |ptr| ptr as usize),
    };
    if decoded.mapped == 0 {
        unsafe { free_decoded(device, &decoded) };
        return None;
    }
    Some(decoded)
}

fn read_mapped(src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.1") {
        unsafe { stream_copy(src, &mut out) };
        return out;
    }
    out.copy_from_slice(src);
    out
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn stream_copy(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::{__m128i, _mm_storeu_si128, _mm_stream_load_si128};

    let head = src.as_ptr().align_offset(16).min(src.len());
    dst[..head].copy_from_slice(&src[..head]);
    let mut at = head;
    while at + 16 <= src.len() {
        unsafe {
            let line = _mm_stream_load_si128(src.as_ptr().add(at).cast::<__m128i>().cast_mut());
            _mm_storeu_si128(dst.as_mut_ptr().add(at).cast::<__m128i>(), line);
        }
        at += 16;
    }
    dst[at..].copy_from_slice(&src[at..]);
}

fn warn_once(flag: &AtomicBool, message: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        tracing::warn!("vk-etc2: {message}");
    }
}

unsafe extern "system" fn cmd_copy_buffer_to_image(
    command_buffer: vk::CommandBuffer,
    src_buffer: vk::Buffer,
    dst_image: vk::Image,
    dst_image_layout: vk::ImageLayout,
    region_count: u32,
    p_regions: *const vk::BufferImageCopy,
) {
    let Some(host_copy) = load::<vk::PFN_vkCmdCopyBufferToImage>(&HOST_CMD_COPY_BUFFER_TO_IMAGE)
    else {
        return;
    };
    let format = with_tracking(|t| t.images.get(&dst_image.as_raw()).copied()).flatten();
    let Some(format) = format else {
        return unsafe {
            host_copy(
                command_buffer,
                src_buffer,
                dst_image,
                dst_image_layout,
                region_count,
                p_regions,
            )
        };
    };
    if region_count == 0 || p_regions.is_null() {
        return;
    }
    let regions = unsafe { std::slice::from_raw_parts(p_regions, region_count as usize) };
    static UNMAPPED: AtomicBool = AtomicBool::new(false);
    static NO_STAGING: AtomicBool = AtomicBool::new(false);
    static UPLOADED: AtomicBool = AtomicBool::new(false);

    for region in regions {
        let src =
            with_tracking(|t| unsafe { source_bytes(t, src_buffer, format, region) }).flatten();
        let texels = src.map(|src| decode_region(format, &read_mapped(src), region));
        let Some(texels) = texels else {
            warn_once(
                &UNMAPPED,
                "an ETC2 upload's staging buffer is not mapped; that texture stays blank",
            );
            continue;
        };
        let decoded = HOST
            .lock()
            .ok()
            .and_then(|host| unsafe { upload_decoded(host.as_ref()?, &texels) });
        let Some(decoded) = decoded else {
            warn_once(
                &NO_STAGING,
                "no staging buffer for decoded ETC2 texels; that texture stays blank",
            );
            continue;
        };
        let copy = vk::BufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,
            buffer_image_height: 0,
            ..*region
        };
        unsafe {
            host_copy(
                command_buffer,
                decoded.buffer,
                dst_image,
                dst_image_layout,
                1,
                &copy,
            )
        };
        with_tracking(|t| {
            t.decoded
                .entry(src_buffer.as_raw())
                .or_default()
                .push(decoded)
        });
        if !UPLOADED.swap(true, Ordering::Relaxed) {
            tracing::info!(?format, "vk-etc2: first ETC2 texture decoded and uploaded");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(
        width: u32,
        height: u32,
        row: u32,
        image_height: u32,
        layers: u32,
    ) -> vk::BufferImageCopy {
        vk::BufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: row,
            buffer_image_height: image_height,
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: layers,
            },
            image_offset: vk::Offset3D::default(),
            image_extent: vk::Extent3D {
                width,
                height,
                depth: 1,
            },
        }
    }

    #[test]
    fn mapped_reads_copy_every_byte_at_any_alignment_and_length() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for start in 0..17 {
            for len in [0, 1, 15, 16, 17, 63, 64, 500, 1000 - start] {
                let src = &data[start..start + len];
                assert_eq!(read_mapped(src), src, "start {start} len {len}");
            }
        }
    }

    #[test]
    fn etc2_formats_are_backed_by_rgba8_in_the_same_colour_space() {
        assert_eq!(
            substitute(vk::Format::ETC2_R8G8B8_UNORM_BLOCK),
            Some((Etc2Format::Rgb8, vk::Format::R8G8B8A8_UNORM))
        );
        assert_eq!(
            substitute(vk::Format::ETC2_R8G8B8A8_SRGB_BLOCK),
            Some((Etc2Format::Rgba8, vk::Format::R8G8B8A8_SRGB))
        );
        assert_eq!(substitute(vk::Format::BC1_RGB_UNORM_BLOCK), None);
    }

    #[test]
    fn tightly_packed_regions_round_partial_blocks_up() {
        let layout = region_layout(Etc2Format::Rgb8, &region(6, 5, 0, 0, 1));
        assert_eq!(
            layout,
            RegionLayout {
                blocks_per_row: 2,
                block_rows: 2,
                layer_bytes: 32,
                layers: 1
            }
        );
    }

    #[test]
    fn buffer_row_length_and_image_height_set_the_block_grid() {
        let layout = region_layout(Etc2Format::Rgba8, &region(8, 8, 16, 12, 6));
        assert_eq!(layout.blocks_per_row, 4);
        assert_eq!(layout.block_rows, 3);
        assert_eq!(layout.layer_bytes, 4 * 3 * 16);
        assert_eq!(layout.layers, 6);
    }

    #[test]
    fn tiny_mips_still_take_one_whole_block() {
        let layout = region_layout(Etc2Format::Rgb8, &region(1, 1, 0, 0, 1));
        assert_eq!(layout.layer_bytes, 8);
        let src = (0xF0u64 << 56).to_be_bytes();
        assert_eq!(
            decode_region(Etc2Format::Rgb8, &src, &region(1, 1, 0, 0, 1)),
            vec![255, 2, 2, 255]
        );
    }

    #[test]
    fn every_array_layer_is_decoded_into_its_own_slice() {
        let red = (0xF0u64 << 56).to_be_bytes();
        let blue = (0xFFu64 << 40).to_be_bytes();
        let src = [red, blue].concat();
        let texels = decode_region(Etc2Format::Rgb8, &src, &region(4, 4, 0, 0, 2));
        assert_eq!(texels.len(), 2 * 4 * 4 * 4);
        assert_eq!(&texels[..4], &[255, 2, 2, 255]);
        assert_eq!(&texels[64..68], &[2, 2, 255, 255]);
    }
}
