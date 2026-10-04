use std::borrow::Cow;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::{fmt, fs, io};

use ash::{khr, vk};
use eclipse_config::PciId;

use crate::portal::{Notice, NoticeId, PortalError, PortalRequest};
use crate::status::StatusSink;

pub mod diagnosis;

const NVIDIA: u32 = 0x10de;

const INTEL: u32 = 0x8086;

const OLDEST_USABLE_VULKAN: (u32, u32) = (1, 1);

const NVIDIA_KERNEL_DRIVER: &str = "/proc/driver/nvidia/version";

const NVIDIA_VERSION_LINE: &str = "NVRM version:";

const FLATPAK_GL_EXTENSIONS: &str = "/usr/lib/x86_64-linux-gnu/GL";

const FLATPAK_DEFAULT_GL: &str = "default";

const SOFTWARE_GL: [&str; 2] = ["llvmpipe", "softpipe"];

const NOTICE_TITLE: &str = "Roblox graphics problem";

const SELECTION_OVERRIDES: [&str; 5] = [
    "DRI_PRIME",
    "MESA_VK_DEVICE_SELECT",
    "VK_LOADER_DEVICE_SELECT",
    "__NV_PRIME_RENDER_OFFLOAD",
    "__VK_LAYER_NV_optimus",
];

const INTEL_GEN7: [u32; 71] = [
    0x0152, 0x0162, 0x0156, 0x0166, 0x015a, 0x016a, 0x0402, 0x0412, 0x0422, 0x0406, 0x0416, 0x0426,
    0x040a, 0x041a, 0x042a, 0x040b, 0x041b, 0x042b, 0x040e, 0x041e, 0x042e, 0x0c02, 0x0c12, 0x0c22,
    0x0c06, 0x0c16, 0x0c26, 0x0c0a, 0x0c1a, 0x0c2a, 0x0c0b, 0x0c1b, 0x0c2b, 0x0c0e, 0x0c1e, 0x0c2e,
    0x0a02, 0x0a12, 0x0a22, 0x0a06, 0x0a16, 0x0a26, 0x0a0a, 0x0a1a, 0x0a2a, 0x0a0b, 0x0a1b, 0x0a2b,
    0x0a0e, 0x0a1e, 0x0a2e, 0x0d02, 0x0d12, 0x0d22, 0x0d06, 0x0d16, 0x0d26, 0x0d0a, 0x0d1a, 0x0d2a,
    0x0d0b, 0x0d1b, 0x0d2b, 0x0d0e, 0x0d1e, 0x0d2e, 0x0f31, 0x0f32, 0x0f33, 0x0157, 0x0155,
];

static SELECTION: OnceLock<Selection> = OnceLock::new();

static SURVEY: Mutex<Option<Survey>> = Mutex::new(None);

static PLAN: OnceLock<Plan> = OnceLock::new();

static CLIENT_GRAPHICS: OnceLock<Graphics> = OnceLock::new();

pub fn give_client(graphics: Graphics) {
    CLIENT_GRAPHICS
        .set(graphics)
        .expect("Roblox's graphics are chosen once per process");
}

pub(crate) fn client_graphics() -> Graphics {
    CLIENT_GRAPHICS.get().copied().unwrap_or(Graphics::Vulkan)
}

pub fn configure(vulkan_device: Option<PciId>) {
    SELECTION
        .set(Selection::from_environment(vulkan_device))
        .expect("the GPU selection is configured once per process");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requested {
    Automatic,
    OpenGlEs,
}

impl Requested {
    #[must_use]
    pub const fn of(config: &eclipse_config::Config) -> Self {
        if config.use_opengl {
            Self::OpenGlEs
        } else {
            Self::Automatic
        }
    }

    const fn unsurveyed(self) -> Graphics {
        match self {
            Self::Automatic => Graphics::Vulkan,
            Self::OpenGlEs => Graphics::Gles(GlesReason::Configured),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Graphics {
    Vulkan,
    Gles(GlesReason),
}

impl Graphics {
    pub(crate) const fn hides_vulkan(self) -> bool {
        matches!(self, Self::Gles(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlesReason {
    Configured,
    NoUsableVulkan,
}

impl fmt::Display for Graphics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Vulkan => "Vulkan",
            Self::Gles(GlesReason::Configured) => "OpenGL ES (use_opengl)",
            Self::Gles(GlesReason::NoUsableVulkan) => "OpenGL ES (no usable Vulkan GPU)",
        })
    }
}

#[must_use]
pub fn graphics(requested: Requested) -> Graphics {
    planned_graphics().unwrap_or(requested.unsurveyed())
}

pub(crate) fn planned_graphics() -> Option<Graphics> {
    PLAN.get().map(|plan| plan.graphics)
}

pub(crate) fn record_plan(plan: Plan) {
    PLAN.get_or_init(|| {
        match &plan.warning {
            None => tracing::info!(graphics = %plan.graphics, "Roblox's graphics plan"),
            Some(warning) => {
                tracing::warn!(graphics = %plan.graphics, %warning, "Roblox's graphics plan");
            }
        }
        plan
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    graphics: Graphics,
    warning: Option<Warning>,
}

impl Plan {
    const fn new(graphics: Graphics, warning: Option<Warning>) -> Self {
        Self { graphics, warning }
    }

    #[cfg(test)]
    pub(crate) fn graphics(&self) -> Graphics {
        self.graphics
    }

    #[cfg(test)]
    pub(crate) fn warning_texts(&self) -> Vec<String> {
        self.warning.iter().map(ToString::to_string).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeviceFit {
    Usable,
    Unusable { device: String, reason: Unusable },
}

impl DeviceFit {
    fn of(device: &DeviceSummary) -> Self {
        match usable(device) {
            Ok(()) => Self::Usable,
            Err(reason) => Self::Unusable {
                device: device.name.clone(),
                reason,
            },
        }
    }
}

pub(crate) struct PickedDevice {
    pub(crate) handle: vk::PhysicalDevice,
    pub(crate) queue_family: u32,
    pub(crate) fit: DeviceFit,
}

pub(crate) enum Drawing<V, G> {
    Vulkan(V),
    Gles(G),
}

pub(crate) enum Undrawn<VE, GE> {
    Gles(GE),
    Neither { vulkan: VE, gles: GE },
}

pub(crate) struct Started<V, G, VE, GE> {
    pub(crate) drawing: Result<Drawing<V, G>, Undrawn<VE, GE>>,
    pub(crate) plan: Plan,
}

pub(crate) fn start_drawing<V, G, VE: fmt::Display, GE: fmt::Display>(
    requested: Requested,
    mut vulkan: impl FnMut() -> Result<(V, DeviceFit), VE>,
    gles: impl FnOnce() -> Result<(G, String), GE>,
) -> Started<V, G, VE, GE> {
    if requested == Requested::OpenGlEs {
        let graphics = Graphics::Gles(GlesReason::Configured);
        return match gles() {
            Ok((renderer, gl)) => Started {
                drawing: Ok(Drawing::Gles(renderer)),
                plan: Plan::new(graphics, software_gl_warning(&gl)),
            },
            Err(error) => Started {
                plan: Plan::new(
                    graphics,
                    Some(Warning::GlesFailed {
                        error: error.to_string(),
                    }),
                ),
                drawing: Err(Undrawn::Gles(error)),
            },
        };
    }
    let unusable = match vulkan() {
        Ok((renderer, DeviceFit::Usable)) => {
            return Started {
                drawing: Ok(Drawing::Vulkan(renderer)),
                plan: Plan::new(Graphics::Vulkan, None),
            };
        }
        Ok((renderer, DeviceFit::Unusable { device, reason })) => {
            drop(renderer);
            Ok(unusable_device_warning(device, reason))
        }
        Err(error) => Err(error),
    };
    let fallback = Graphics::Gles(GlesReason::NoUsableVulkan);
    match (gles(), unusable) {
        (Ok((renderer, gl)), unusable) => {
            let plan = match (software_gl_warning(&gl), unusable) {
                (None, _) => Plan::new(fallback, Some(Warning::NoUsableVulkan { gl })),
                (Some(_), Ok(device_warning)) => Plan::new(Graphics::Vulkan, Some(device_warning)),
                (Some(software), Err(_)) => Plan::new(fallback, Some(software)),
            };
            Started {
                drawing: Ok(Drawing::Gles(renderer)),
                plan,
            }
        }
        (Err(gles), Ok(device_warning)) => Started {
            drawing: vulkan()
                .map(|(renderer, _)| Drawing::Vulkan(renderer))
                .map_err(|vulkan| Undrawn::Neither { vulkan, gles }),
            plan: Plan::new(Graphics::Vulkan, Some(device_warning)),
        },
        (Err(gles), Err(vulkan)) => Started {
            plan: Plan::new(
                Graphics::Vulkan,
                Some(Warning::NothingDraws {
                    vulkan: vulkan.to_string(),
                    gles: gles.to_string(),
                }),
            ),
            drawing: Err(Undrawn::Neither { vulkan, gles }),
        },
    }
}

fn unusable_device_warning(device: String, reason: Unusable) -> Warning {
    match reason {
        Unusable::Cpu => Warning::Software {
            renderer: renderer_name(&device).to_owned(),
        },
        reason => Warning::Unusable { device, reason },
    }
}

fn software_gl_warning(gl_renderer: &str) -> Option<Warning> {
    software_gl(gl_renderer).map(|renderer| Warning::Software {
        renderer: renderer.to_owned(),
    })
}

fn software_gl(gl_renderer: &str) -> Option<&'static str> {
    let gl_renderer = gl_renderer.to_ascii_lowercase();
    SOFTWARE_GL
        .into_iter()
        .find(|software| gl_renderer.contains(software))
}

pub(crate) fn pick_device(
    instance: &ash::Instance,
    surface_loader: &khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<PickedDevice, String> {
    let handles = unsafe { instance.enumerate_physical_devices() }
        .map_err(|error| format!("vkEnumeratePhysicalDevices: {error}"))?;
    if handles.is_empty() {
        return Err("no Vulkan physical devices found".to_owned());
    }
    let devices: Vec<DeviceSummary> = handles
        .iter()
        .map(|&handle| survey_device(instance, surface_loader, surface, handle))
        .collect();
    let selection = SELECTION
        .get()
        .copied()
        .unwrap_or_else(|| Selection::from_environment(None));
    let Some(choice) = choose(&devices, &selection) else {
        return Err(format!(
            "no Vulkan device with a graphics+present queue for this surface; found {}",
            DeviceList(&devices)
        ));
    };
    let chosen = &devices[choice.index];
    let queue_family = chosen
        .present_family
        .expect("only a device that can present is chosen");
    let survey = Survey {
        chosen: ChosenDevice::of(chosen),
        warnings: device_warnings(&devices, choice),
    };
    if record_survey(&SURVEY, &survey) {
        report_choice(&devices, choice, &survey.warnings);
    }
    Ok(PickedDevice {
        handle: handles[choice.index],
        queue_family,
        fit: DeviceFit::of(chosen),
    })
}

pub fn report(status: &StatusSink) {
    report_to(status, &warnings(), crate::portal::submit);
}

fn warnings() -> Vec<Warning> {
    let nvidia = missing_nvidia_driver();
    let survey = SURVEY.lock().unwrap_or_else(PoisonError::into_inner);
    nvidia
        .into_iter()
        .chain(survey.iter().flat_map(|survey| survey.warnings.clone()))
        .chain(PLAN.get().and_then(|plan| plan.warning.clone()))
        .collect()
}

fn report_to(
    status: &StatusSink,
    warnings: &[Warning],
    submit: impl FnOnce(PortalRequest) -> Result<(), PortalError>,
) {
    for warning in warnings {
        status.warning(warning.to_string());
    }
    let Some(notice) = lasting_notice(warnings) else {
        return;
    };
    if let Err(error) = submit(PortalRequest::Notify(notice)) {
        tracing::warn!(%error, "could not post a desktop notification");
    }
}

fn lasting_notice(warnings: &[Warning]) -> Option<Notice> {
    let lasting: Vec<String> = warnings
        .iter()
        .filter(|warning| warning.lasts_into_play())
        .map(ToString::to_string)
        .collect();
    (!lasting.is_empty()).then(|| Notice {
        id: NoticeId::Graphics,
        title: NOTICE_TITLE.to_owned(),
        body: lasting.join("\n"),
    })
}

fn survey_device(
    instance: &ash::Instance,
    surface_loader: &khr::surface::Instance,
    surface: vk::SurfaceKHR,
    handle: vk::PhysicalDevice,
) -> DeviceSummary {
    let properties = unsafe { instance.get_physical_device_properties(handle) };
    let name = device_name(&properties).into_owned();
    let present_family = present_family(instance, surface_loader, surface, handle, &name);
    DeviceSummary {
        vendor_id: properties.vendor_id,
        device_id: properties.device_id,
        name,
        class: DeviceClass::of(properties.device_type),
        api_version: properties.api_version,
        driver_version: properties.driver_version,
        present_family,
    }
}

fn present_family(
    instance: &ash::Instance,
    surface_loader: &khr::surface::Instance,
    surface: vk::SurfaceKHR,
    handle: vk::PhysicalDevice,
    name: &str,
) -> Option<u32> {
    if !has_swapchain(instance, handle, name) {
        return None;
    }
    graphics_families(instance, handle).find(|&index| {
        match unsafe { surface_loader.get_physical_device_surface_support(handle, index, surface) }
        {
            Ok(supported) => supported,
            Err(error) => {
                tracing::warn!(
                    device = name,
                    queue_family = index,
                    %error,
                    "cannot ask whether the Vulkan device can present to the window"
                );
                false
            }
        }
    })
}

fn has_swapchain(instance: &ash::Instance, handle: vk::PhysicalDevice, name: &str) -> bool {
    match unsafe { instance.enumerate_device_extension_properties(handle) } {
        Ok(extensions) => extensions
            .iter()
            .any(|extension| extension.extension_name_as_c_str() == Ok(khr::swapchain::NAME)),
        Err(error) => {
            tracing::warn!(device = name, %error, "cannot list the Vulkan device's extensions");
            false
        }
    }
}

fn graphics_families(
    instance: &ash::Instance,
    handle: vk::PhysicalDevice,
) -> impl Iterator<Item = u32> {
    let families = unsafe { instance.get_physical_device_queue_family_properties(handle) };
    (0u32..)
        .zip(families)
        .filter(|(_, family)| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .map(|(index, _)| index)
}

fn report_choice(devices: &[DeviceSummary], choice: Choice, warnings: &[Warning]) {
    let report = ChoiceReport { devices, choice };
    if warnings.is_empty() && usable(&devices[choice.index]).is_ok() {
        tracing::info!("{report}");
    } else {
        tracing::warn!("{report}");
    }
}

fn device_warnings(devices: &[DeviceSummary], choice: Choice) -> Vec<Warning> {
    let ChoiceReason::ConfiguredMissing(configured) = choice.reason else {
        return Vec::new();
    };
    vec![Warning::ConfiguredMissing {
        configured,
        used: devices[choice.index].name.clone(),
    }]
}

fn missing_nvidia_driver() -> Option<Warning> {
    nvidia_flatpak_gap().unwrap_or_else(|error| {
        tracing::warn!(%error, "the Flatpak NVIDIA driver is not checked");
        None
    })
}

fn nvidia_flatpak_gap() -> Result<Option<Warning>, String> {
    let Some(module) = nvidia_kernel_module()? else {
        return Ok(None);
    };
    let Some(gl_extensions) = flatpak_gl_extensions()? else {
        return Ok(None);
    };
    Ok(nvidia_gl_extension_gap(&module, &gl_extensions))
}

fn nvidia_kernel_module() -> Result<Option<String>, String> {
    let Some(kernel_driver) =
        read_if_exists(NVIDIA_KERNEL_DRIVER, |path| fs::read_to_string(path))?
    else {
        return Ok(None);
    };
    nvidia_driver_version(&kernel_driver)
        .map(|version| Some(version.to_owned()))
        .ok_or_else(|| format!("{NVIDIA_KERNEL_DRIVER} names no NVIDIA driver version"))
}

fn flatpak_gl_extensions() -> Result<Option<Vec<String>>, String> {
    read_if_exists(FLATPAK_GL_EXTENSIONS, directory_names)
}

fn read_if_exists<T>(
    path: &'static str,
    read: impl FnOnce(&Path) -> io::Result<T>,
) -> Result<Option<T>, String> {
    match read(Path::new(path)) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {path}: {error}")),
    }
}

fn directory_names(path: &Path) -> io::Result<Vec<String>> {
    fs::read_dir(path)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect()
}

fn nvidia_driver_version(kernel_driver: &str) -> Option<&str> {
    kernel_driver
        .lines()
        .find_map(|line| line.strip_prefix(NVIDIA_VERSION_LINE))?
        .split_whitespace()
        .find(|token| {
            token.contains('.')
                && token
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

fn nvidia_gl_extension(version: &str) -> String {
    format!("nvidia-{}", version.replace('.', "-"))
}

fn nvidia_gl_extension_gap(version: &str, gl_extensions: &[String]) -> Option<Warning> {
    let flatpak_layout = gl_extensions.iter().any(|name| name == FLATPAK_DEFAULT_GL);
    let extension = nvidia_gl_extension(version);
    (flatpak_layout && !gl_extensions.contains(&extension)).then(|| Warning::MissingNvidiaDriver {
        version: version.to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Warning {
    MissingNvidiaDriver { version: String },
    NvidiaVulkanMissing { module: String },
    NvidiaVersionMismatch { module: String, vulkan: String },
    ConfiguredMissing { configured: PciId, used: String },
    Unusable { device: String, reason: Unusable },
    Software { renderer: String },
    NoUsableVulkan { gl: String },
    GlesFailed { error: String },
    NothingDraws { vulkan: String, gles: String },
}

impl Warning {
    const fn lasts_into_play(&self) -> bool {
        match self {
            Self::MissingNvidiaDriver { .. }
            | Self::NvidiaVulkanMissing { .. }
            | Self::NvidiaVersionMismatch { .. }
            | Self::ConfiguredMissing { .. }
            | Self::Unusable { .. }
            | Self::Software { .. } => true,
            Self::NoUsableVulkan { .. } | Self::GlesFailed { .. } | Self::NothingDraws { .. } => {
                false
            }
        }
    }

    const fn needs_fixing(&self) -> bool {
        match self {
            Self::NoUsableVulkan { .. } => false,
            Self::MissingNvidiaDriver { .. }
            | Self::NvidiaVulkanMissing { .. }
            | Self::NvidiaVersionMismatch { .. }
            | Self::ConfiguredMissing { .. }
            | Self::Unusable { .. }
            | Self::Software { .. }
            | Self::GlesFailed { .. }
            | Self::NothingDraws { .. } => true,
        }
    }
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNvidiaDriver { version } => write!(
                f,
                "NVIDIA driver {version} has no matching Flatpak driver, so Roblox cannot use the \
                 NVIDIA GPU. Run: flatpak install flathub org.freedesktop.Platform.GL.{}",
                nvidia_gl_extension(version)
            ),
            Self::NvidiaVulkanMissing { module } => write!(
                f,
                "The NVIDIA kernel module {module} is loaded, but Vulkan finds no NVIDIA GPU. \
                 Install the Vulkan part of NVIDIA's driver {module}."
            ),
            Self::NvidiaVersionMismatch { module, vulkan } => write!(
                f,
                "The NVIDIA kernel module is {module}, but the NVIDIA Vulkan driver is {vulkan}. \
                 Restart after a driver update so both match."
            ),
            Self::ConfiguredMissing { configured, used } => write!(
                f,
                "The GPU {configured} set in vulkan_device was not found or cannot show this \
                 window, so Eclipse uses {used}."
            ),
            Self::Unusable { device, reason } => {
                write!(f, "Roblox may not run on {device}, as {reason}.")
            }
            Self::Software { renderer } => write!(
                f,
                "Roblox is rendering on the CPU ({renderer}), which is slow."
            ),
            Self::NoUsableVulkan { gl } => write!(
                f,
                "No usable Vulkan GPU was found, so Roblox uses OpenGL ES on {gl}."
            ),
            Self::GlesFailed { error } => write!(
                f,
                "OpenGL ES does not work here, so Roblox may not start; set use_opengl to false \
                 to use Vulkan. {error}"
            ),
            Self::NothingDraws { vulkan, gles } => write!(
                f,
                "Neither Vulkan nor OpenGL ES works here, so Roblox may not start. {vulkan}; \
                 OpenGL ES error: {gles}"
            ),
        }
    }
}

fn renderer_name(device: &str) -> &str {
    device.split_once(" (").map_or(device, |(name, _)| name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Survey {
    chosen: ChosenDevice,
    warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    configured: Option<PciId>,
    loader_override: Option<&'static str>,
}

impl Selection {
    fn from_environment(configured: Option<PciId>) -> Self {
        Self::with_environment(configured, |name| std::env::var_os(name))
    }

    fn with_environment(
        configured: Option<PciId>,
        variable: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        let loader_override = SELECTION_OVERRIDES
            .into_iter()
            .find(|name| variable(name).is_some_and(|value| !value.is_empty()));
        Self {
            configured,
            loader_override,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceClass {
    Discrete,
    Integrated,
    Virtual,
    Cpu,
    Other,
}

impl DeviceClass {
    fn of(device_type: vk::PhysicalDeviceType) -> Self {
        match device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => Self::Discrete,
            vk::PhysicalDeviceType::INTEGRATED_GPU => Self::Integrated,
            vk::PhysicalDeviceType::VIRTUAL_GPU => Self::Virtual,
            vk::PhysicalDeviceType::CPU => Self::Cpu,
            _ => Self::Other,
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Discrete => 0,
            Self::Integrated => 1,
            Self::Virtual => 2,
            Self::Other => 3,
            Self::Cpu => 4,
        }
    }
}

impl fmt::Display for DeviceClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Discrete => "discrete",
            Self::Integrated => "integrated",
            Self::Virtual => "virtual",
            Self::Cpu => "CPU",
            Self::Other => "other",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceSummary {
    vendor_id: u32,
    device_id: u32,
    name: String,
    class: DeviceClass,
    api_version: u32,
    driver_version: u32,
    present_family: Option<u32>,
}

impl DeviceSummary {
    fn is(&self, id: PciId) -> bool {
        self.vendor_id == u32::from(id.vendor) && self.device_id == u32::from(id.device)
    }

    fn presents(&self) -> bool {
        self.present_family.is_some()
    }
}

impl fmt::Display for DeviceSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_device(f, &self.name, self.vendor_id, self.device_id)
    }
}

fn write_device(
    f: &mut fmt::Formatter<'_>,
    name: &str,
    vendor_id: u32,
    device_id: u32,
) -> fmt::Result {
    write!(f, "{name} [{vendor_id:04x}:{device_id:04x}]")
}

struct DeviceList<'a>(&'a [DeviceSummary]);

impl fmt::Display for DeviceList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_devices(f, self.0.iter())
    }
}

fn write_devices<'a>(
    f: &mut fmt::Formatter<'_>,
    devices: impl Iterator<Item = &'a DeviceSummary>,
) -> fmt::Result {
    for (position, device) in devices.enumerate() {
        let separator = if position == 0 { "" } else { ", " };
        write!(f, "{separator}{device} {}", device.class)?;
        if !device.presents() {
            f.write_str(" that cannot present")?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unusable {
    CannotPresent,
    Cpu,
    OldVulkan,
    IntelGen7,
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CannotPresent => "it cannot present to Eclipse's window",
            Self::Cpu => "it renders on the CPU",
            Self::OldVulkan => "it supports only Vulkan 1.0",
            Self::IntelGen7 => "Mesa's Vulkan driver for this Intel GPU is incomplete",
        })
    }
}

fn usable(device: &DeviceSummary) -> Result<(), Unusable> {
    if !device.presents() {
        return Err(Unusable::CannotPresent);
    }
    if device.class == DeviceClass::Cpu {
        return Err(Unusable::Cpu);
    }
    let version = (
        vk::api_version_major(device.api_version),
        vk::api_version_minor(device.api_version),
    );
    if version < OLDEST_USABLE_VULKAN {
        return Err(Unusable::OldVulkan);
    }
    if device.vendor_id == INTEL && INTEL_GEN7.contains(&device.device_id) {
        return Err(Unusable::IntelGen7);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChoiceReason {
    Configured,
    ConfiguredMissing(PciId),
    LoaderOrder(&'static str),
    Ranked,
}

impl fmt::Display for ChoiceReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configured => f.write_str("chosen by vulkan_device"),
            Self::ConfiguredMissing(id) => write!(
                f,
                "vulkan_device {id} was not found or cannot present to this window"
            ),
            Self::LoaderOrder(variable) => {
                write!(
                    f,
                    "first in the Vulkan loader's order with usable GPUs ahead, as {variable} is \
                     set"
                )
            }
            Self::Ranked => f.write_str("usable GPUs come first, then discrete ones"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Choice {
    index: usize,
    reason: ChoiceReason,
}

fn choose(devices: &[DeviceSummary], selection: &Selection) -> Option<Choice> {
    let presenting = || {
        devices
            .iter()
            .enumerate()
            .filter(|(_, device)| device.presents())
    };
    if let Some(id) = selection.configured {
        if let Some((index, _)) = presenting().find(|(_, device)| device.is(id)) {
            return Some(Choice {
                index,
                reason: ChoiceReason::Configured,
            });
        }
    }
    let (index, reason) = match selection.loader_override {
        Some(variable) => {
            let (index, _) = presenting().min_by_key(|(_, device)| {
                (usable(device).is_err(), device.class == DeviceClass::Cpu)
            })?;
            (index, ChoiceReason::LoaderOrder(variable))
        }
        None => {
            let (index, _) = presenting()
                .min_by_key(|(_, device)| (usable(device).is_err(), device.class.rank()))?;
            (index, ChoiceReason::Ranked)
        }
    };
    Some(Choice {
        index,
        reason: selection
            .configured
            .map_or(reason, ChoiceReason::ConfiguredMissing),
    })
}

struct ChoiceReport<'a> {
    devices: &'a [DeviceSummary],
    choice: Choice,
}

impl fmt::Display for ChoiceReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let chosen = &self.devices[self.choice.index];
        write!(
            f,
            "Vulkan device {chosen}, {}, Vulkan {}, driver {}; {}",
            chosen.class,
            api_version_text(chosen.api_version),
            driver_version_text(chosen.vendor_id, chosen.driver_version),
            self.choice.reason
        )?;
        if let Err(unusable) = usable(chosen) {
            write!(f, "; Roblox may not run on it, as {unusable}")?;
        }
        let mut others = self
            .devices
            .iter()
            .enumerate()
            .filter(|&(index, _)| index != self.choice.index)
            .map(|(_, device)| device)
            .peekable();
        if others.peek().is_none() {
            return Ok(());
        }
        f.write_str("; also found ")?;
        write_devices(f, others)
    }
}

fn device_name(properties: &vk::PhysicalDeviceProperties) -> Cow<'_, str> {
    properties
        .device_name_as_c_str()
        .unwrap_or_default()
        .to_string_lossy()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChosenDevice {
    vendor_id: u32,
    device_id: u32,
    name: String,
}

impl ChosenDevice {
    fn of(device: &DeviceSummary) -> Self {
        Self {
            vendor_id: device.vendor_id,
            device_id: device.device_id,
            name: device.name.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn of_properties(properties: &vk::PhysicalDeviceProperties) -> Self {
        Self {
            vendor_id: properties.vendor_id,
            device_id: properties.device_id,
            name: device_name(properties).into_owned(),
        }
    }

    pub(crate) fn is(&self, properties: &vk::PhysicalDeviceProperties) -> bool {
        properties.vendor_id == self.vendor_id
            && properties.device_id == self.device_id
            && device_name(properties) == self.name
    }
}

impl fmt::Display for ChosenDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_device(f, &self.name, self.vendor_id, self.device_id)
    }
}

pub(crate) fn recorded_choice() -> Option<ChosenDevice> {
    SURVEY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|survey| survey.chosen.clone())
}

fn record_survey(record: &Mutex<Option<Survey>>, survey: &Survey) -> bool {
    let mut recorded = record.lock().unwrap_or_else(PoisonError::into_inner);
    if recorded.as_ref() == Some(survey) {
        return false;
    }
    *recorded = Some(survey.clone());
    true
}

fn api_version_text(version: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(version),
        vk::api_version_minor(version),
        vk::api_version_patch(version)
    )
}

fn driver_version_text(vendor_id: u32, raw: u32) -> String {
    if vendor_id == NVIDIA {
        return format!(
            "{}.{}.{:02}",
            raw >> 22,
            (raw >> 14) & 0xff,
            (raw >> 6) & 0xff
        );
    }
    format!("{}.{}.{}", raw >> 22, (raw >> 12) & 0x3ff, raw & 0xfff)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ffi::CStr;
    use std::rc::Rc;

    use super::*;

    const RTX_5070: (u32, u32) = (0x10de, 0x2f04);
    const RAPHAEL: (u32, u32) = (0x1002, 0x164e);
    const LAVAPIPE: (u32, u32) = (0x10005, 0);
    const HASWELL_GT2: (u32, u32) = (0x8086, 0x0416);

    fn device(class: DeviceClass, (vendor_id, device_id): (u32, u32)) -> DeviceSummary {
        DeviceSummary {
            vendor_id,
            device_id,
            name: format!("{class} test GPU"),
            class,
            api_version: vk::API_VERSION_1_3,
            driver_version: 0,
            present_family: Some(0),
        }
    }

    fn without_presentation(device: DeviceSummary) -> DeviceSummary {
        DeviceSummary {
            present_family: None,
            ..device
        }
    }

    fn by_default() -> Selection {
        Selection {
            configured: None,
            loader_override: None,
        }
    }

    fn configured(vendor: u16, device: u16) -> Selection {
        Selection {
            configured: Some(PciId { vendor, device }),
            loader_override: None,
        }
    }

    fn choice(index: usize, reason: ChoiceReason) -> Option<Choice> {
        Some(Choice { index, reason })
    }

    #[test]
    fn a_gpu_is_chosen_over_a_cpu_device_listed_first() {
        let devices = [
            device(DeviceClass::Cpu, LAVAPIPE),
            device(DeviceClass::Integrated, RAPHAEL),
        ];
        assert_eq!(
            choose(&devices, &by_default()),
            choice(1, ChoiceReason::Ranked)
        );
    }

    #[test]
    fn a_discrete_gpu_that_cannot_present_is_passed_over() {
        let devices = [
            without_presentation(device(DeviceClass::Discrete, RTX_5070)),
            device(DeviceClass::Integrated, RAPHAEL),
        ];
        assert_eq!(
            choose(&devices, &by_default()),
            choice(1, ChoiceReason::Ranked)
        );
    }

    #[test]
    fn a_discrete_gpu_is_chosen_over_an_integrated_one_listed_first() {
        let devices = [
            device(DeviceClass::Integrated, RAPHAEL),
            device(DeviceClass::Discrete, RTX_5070),
        ];
        assert_eq!(
            choose(&devices, &by_default()),
            choice(1, ChoiceReason::Ranked)
        );
    }

    #[test]
    fn devices_of_one_class_keep_the_loader_order() {
        let devices = [
            device(DeviceClass::Virtual, (0x1af4, 1)),
            device(DeviceClass::Integrated, RAPHAEL),
            device(DeviceClass::Integrated, HASWELL_GT2),
            device(DeviceClass::Other, (0x1234, 1)),
        ];
        assert_eq!(
            choose(&devices, &by_default()),
            choice(1, ChoiceReason::Ranked)
        );
    }

    fn vulkan_1_0(device: DeviceSummary) -> DeviceSummary {
        DeviceSummary {
            api_version: vk::API_VERSION_1_0,
            ..device
        }
    }

    #[test]
    fn a_usable_gpu_is_chosen_over_an_unusable_one_of_a_higher_class() {
        let devices = [
            vulkan_1_0(device(DeviceClass::Discrete, RTX_5070)),
            device(DeviceClass::Cpu, LAVAPIPE),
            device(DeviceClass::Integrated, RAPHAEL),
        ];
        assert_eq!(
            choose(&devices, &by_default()),
            choice(2, ChoiceReason::Ranked)
        );
        let selection = Selection {
            configured: None,
            loader_override: Some("DRI_PRIME"),
        };
        assert_eq!(
            choose(&devices, &selection),
            choice(2, ChoiceReason::LoaderOrder("DRI_PRIME"))
        );
    }

    #[test]
    fn a_selection_variable_keeps_the_loader_order() {
        let devices = [
            device(DeviceClass::Integrated, RAPHAEL),
            device(DeviceClass::Discrete, RTX_5070),
        ];
        let selection = Selection {
            configured: None,
            loader_override: Some("DRI_PRIME"),
        };
        assert_eq!(
            choose(&devices, &selection),
            choice(0, ChoiceReason::LoaderOrder("DRI_PRIME"))
        );
    }

    #[test]
    fn a_selection_variable_still_passes_over_cpu_devices() {
        let devices = [
            device(DeviceClass::Cpu, LAVAPIPE),
            without_presentation(device(DeviceClass::Discrete, RTX_5070)),
            device(DeviceClass::Integrated, RAPHAEL),
        ];
        let selection = Selection {
            configured: None,
            loader_override: Some("MESA_VK_DEVICE_SELECT"),
        };
        assert_eq!(
            choose(&devices, &selection),
            choice(2, ChoiceReason::LoaderOrder("MESA_VK_DEVICE_SELECT"))
        );
        assert_eq!(
            choose(&devices[..2], &selection),
            choice(0, ChoiceReason::LoaderOrder("MESA_VK_DEVICE_SELECT"))
        );
    }

    #[test]
    fn a_configured_device_wins_when_it_can_present() {
        let devices = [
            device(DeviceClass::Discrete, RTX_5070),
            device(DeviceClass::Integrated, RAPHAEL),
        ];
        assert_eq!(
            choose(&devices, &configured(0x1002, 0x164e)),
            choice(1, ChoiceReason::Configured)
        );
        let selection = Selection {
            loader_override: Some("DRI_PRIME"),
            ..configured(0x1002, 0x164e)
        };
        assert_eq!(
            choose(&devices, &selection),
            choice(1, ChoiceReason::Configured)
        );
    }

    #[test]
    fn a_configured_device_that_is_missing_or_cannot_present_falls_back_to_the_default() {
        let missing = ChoiceReason::ConfiguredMissing(PciId {
            vendor: 0x1002,
            device: 0x164e,
        });
        let without_it = [
            device(DeviceClass::Cpu, LAVAPIPE),
            device(DeviceClass::Discrete, RTX_5070),
        ];
        assert_eq!(
            choose(&without_it, &configured(0x1002, 0x164e)),
            choice(1, missing)
        );
        let unable = [
            device(DeviceClass::Discrete, RTX_5070),
            without_presentation(device(DeviceClass::Integrated, RAPHAEL)),
        ];
        assert_eq!(
            choose(&unable, &configured(0x1002, 0x164e)),
            choice(0, missing)
        );
    }

    #[test]
    fn a_cpu_device_is_chosen_only_when_nothing_else_presents_and_is_not_usable() {
        let devices = [device(DeviceClass::Cpu, LAVAPIPE)];
        assert_eq!(
            choose(&devices, &by_default()),
            choice(0, ChoiceReason::Ranked)
        );
        assert_eq!(usable(&devices[0]), Err(Unusable::Cpu));
    }

    #[test]
    fn nothing_is_chosen_when_no_device_can_present() {
        let devices = [
            without_presentation(device(DeviceClass::Discrete, RTX_5070)),
            without_presentation(device(DeviceClass::Cpu, LAVAPIPE)),
        ];
        assert_eq!(choose(&devices, &by_default()), None);
        assert_eq!(choose(&devices, &configured(0x10de, 0x2f04)), None);
        assert_eq!(choose(&[], &by_default()), None);
    }

    #[test]
    fn usable_needs_presentation_a_gpu_vulkan_1_1_and_no_gen7_intel() {
        assert_eq!(usable(&device(DeviceClass::Discrete, RTX_5070)), Ok(()));
        assert_eq!(
            usable(&device(DeviceClass::Integrated, HASWELL_GT2)),
            Err(Unusable::IntelGen7)
        );
        assert_eq!(
            usable(&device(DeviceClass::Integrated, (0x8086, 0x1616))),
            Ok(())
        );
        let vulkan_1_0 = DeviceSummary {
            api_version: vk::API_VERSION_1_0,
            ..device(DeviceClass::Discrete, RTX_5070)
        };
        assert_eq!(usable(&vulkan_1_0), Err(Unusable::OldVulkan));
        let vulkan_1_1 = DeviceSummary {
            api_version: vk::make_api_version(0, 1, 1, 0),
            ..vulkan_1_0
        };
        assert_eq!(usable(&vulkan_1_1), Ok(()));
        assert_eq!(
            usable(&without_presentation(device(
                DeviceClass::Discrete,
                RTX_5070
            ))),
            Err(Unusable::CannotPresent)
        );
    }

    #[test]
    fn the_gen7_table_holds_only_distinct_ivy_bridge_bay_trail_and_haswell_ids() {
        let mut ids = INTEL_GEN7.to_vec();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), INTEL_GEN7.len());
        for id in [0x0162, 0x0f31, 0x0416, 0x0d22] {
            assert!(INTEL_GEN7.contains(&id), "{id:04x}");
        }
        for broadwell_or_later in [0x1616, 0x1916, 0x3e92] {
            assert!(!INTEL_GEN7.contains(&broadwell_or_later));
        }
    }

    #[test]
    fn the_first_non_empty_selection_variable_is_named() {
        let environment = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            Selection::with_environment(None, environment(&[("HOME", "/home/user")])),
            by_default()
        );
        assert_eq!(
            Selection::with_environment(
                None,
                environment(&[("DRI_PRIME", ""), ("__NV_PRIME_RENDER_OFFLOAD", "1")])
            )
            .loader_override,
            Some("__NV_PRIME_RENDER_OFFLOAD")
        );
        let id = PciId {
            vendor: 0x1002,
            device: 0x164e,
        };
        assert_eq!(
            Selection::with_environment(
                Some(id),
                environment(&[("VK_LOADER_DEVICE_SELECT", "1002:164e"), ("DRI_PRIME", "1")])
            ),
            Selection {
                configured: Some(id),
                loader_override: Some("DRI_PRIME"),
            }
        );
    }

    #[test]
    fn nvidia_driver_versions_use_nvidias_bit_layout() {
        assert_eq!(
            driver_version_text(0x10de, (615 << 22) | (71 << 14) | (9 << 6)),
            "615.71.09"
        );
        assert_eq!(
            driver_version_text(0x1002, vk::make_api_version(0, 26, 2, 2)),
            "26.2.2"
        );
        assert_eq!(
            driver_version_text(0x8086, vk::make_api_version(0, 25, 3, 14)),
            "25.3.14"
        );
        assert_eq!(
            api_version_text(vk::make_api_version(0, 1, 4, 312)),
            "1.4.312"
        );
    }

    #[test]
    fn the_choice_report_names_the_device_its_versions_the_reason_and_the_others() {
        let devices = [
            DeviceSummary {
                name: "NVIDIA GeForce RTX 5070".to_owned(),
                api_version: vk::make_api_version(0, 1, 4, 312),
                driver_version: (615 << 22) | (71 << 14) | (9 << 6),
                ..device(DeviceClass::Discrete, RTX_5070)
            },
            DeviceSummary {
                name: "AMD Radeon Graphics (RADV RAPHAEL_MENDOCINO)".to_owned(),
                ..without_presentation(device(DeviceClass::Integrated, RAPHAEL))
            },
            DeviceSummary {
                name: "llvmpipe (LLVM 22.1.0, 256 bits)".to_owned(),
                api_version: vk::make_api_version(0, 1, 4, 335),
                driver_version: vk::make_api_version(0, 0, 0, 1),
                ..device(DeviceClass::Cpu, LAVAPIPE)
            },
        ];
        let discrete = ChoiceReport {
            devices: &devices,
            choice: Choice {
                index: 0,
                reason: ChoiceReason::Ranked,
            },
        };
        assert_eq!(
            discrete.to_string(),
            "Vulkan device NVIDIA GeForce RTX 5070 [10de:2f04], discrete, Vulkan 1.4.312, driver \
             615.71.09; usable GPUs come first, then discrete ones; also found AMD Radeon \
             Graphics (RADV RAPHAEL_MENDOCINO) [1002:164e] integrated that cannot present, \
             llvmpipe (LLVM 22.1.0, 256 bits) [10005:0000] CPU"
        );
        let cpu = ChoiceReport {
            devices: &devices[2..],
            choice: Choice {
                index: 0,
                reason: ChoiceReason::LoaderOrder("DRI_PRIME"),
            },
        };
        assert_eq!(
            cpu.to_string(),
            "Vulkan device llvmpipe (LLVM 22.1.0, 256 bits) [10005:0000], CPU, Vulkan 1.4.335, \
             driver 0.0.1; first in the Vulkan loader's order with usable GPUs ahead, as \
             DRI_PRIME is set; Roblox may not run on it, as it renders on the CPU"
        );
    }

    #[test]
    fn the_chosen_device_is_the_one_with_its_ids_and_name() {
        let chosen = ChosenDevice::of(&DeviceSummary {
            name: "NVIDIA GeForce RTX 5070".to_owned(),
            ..device(DeviceClass::Discrete, RTX_5070)
        });
        let properties = |vendor_id, device_id, name: &CStr| {
            vk::PhysicalDeviceProperties::default()
                .vendor_id(vendor_id)
                .device_id(device_id)
                .device_name(name)
                .expect("the name fits")
        };
        assert!(chosen.is(&properties(0x10de, 0x2f04, c"NVIDIA GeForce RTX 5070")));
        assert!(!chosen.is(&properties(0x10de, 0x2f04, c"NVIDIA GeForce RTX 5070 Ti")));
        assert!(!chosen.is(&properties(0x10de, 0x2f05, c"NVIDIA GeForce RTX 5070")));
        assert!(!chosen.is(&properties(0x1002, 0x2f04, c"NVIDIA GeForce RTX 5070")));
        assert_eq!(
            ChosenDevice::of_properties(&properties(0x10de, 0x2f04, c"NVIDIA GeForce RTX 5070")),
            chosen
        );
        assert_eq!(chosen.to_string(), "NVIDIA GeForce RTX 5070 [10de:2f04]");
    }

    #[test]
    fn a_survey_is_recorded_once_until_the_device_or_its_warnings_change() {
        let record = Mutex::new(None);
        let survey = |chosen: &DeviceSummary, warnings| Survey {
            chosen: ChosenDevice::of(chosen),
            warnings,
        };
        let nvidia = survey(&device(DeviceClass::Discrete, RTX_5070), Vec::new());
        let amd = survey(&device(DeviceClass::Integrated, RAPHAEL), Vec::new());
        let amd_warned = Survey {
            warnings: vec![Warning::ConfiguredMissing {
                configured: PciId {
                    vendor: 0xdead,
                    device: 0xbeef,
                },
                used: "integrated test GPU".to_owned(),
            }],
            ..amd.clone()
        };
        assert!(record_survey(&record, &nvidia));
        assert!(!record_survey(&record, &nvidia));
        assert!(record_survey(&record, &amd));
        assert!(record_survey(&record, &amd_warned));
        assert!(!record_survey(&record, &amd_warned));
        assert_eq!(*record.lock().expect("unpoisoned"), Some(amd_warned));
    }

    fn warning_texts(devices: &[DeviceSummary], selection: &Selection) -> Vec<String> {
        let choice = choose(devices, selection).expect("a device can present");
        device_warnings(devices, choice)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_usable_device_chosen_by_default_or_by_config_gives_no_warning() {
        let devices = [
            device(DeviceClass::Discrete, RTX_5070),
            device(DeviceClass::Integrated, RAPHAEL),
        ];
        assert!(warning_texts(&devices, &by_default()).is_empty());
        assert!(warning_texts(&devices, &configured(0x1002, 0x164e)).is_empty());
    }

    #[test]
    fn a_configured_gpu_that_is_missing_names_the_gpu_used_instead() {
        let devices = [DeviceSummary {
            name: "NVIDIA GeForce RTX 5070".to_owned(),
            ..device(DeviceClass::Discrete, RTX_5070)
        }];
        assert_eq!(
            warning_texts(&devices, &configured(0xdead, 0xbeef)),
            [
                "The GPU dead:beef set in vulkan_device was not found or cannot show this window, \
                 so Eclipse uses NVIDIA GeForce RTX 5070."
            ]
        );
    }

    const RTX_5070_GL: &str = "NVIDIA GeForce RTX 5070/PCIe/SSE2";
    const LLVMPIPE_GL: &str = "llvmpipe (LLVM 22.1.0, 256 bits)";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fake {
        Vulkan(u8),
        Gles,
    }

    struct Renderer {
        fake: Fake,
        events: Rc<RefCell<Vec<Event>>>,
    }

    impl Drop for Renderer {
        fn drop(&mut self) {
            self.events.borrow_mut().push(Event::Dropped(self.fake));
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Started(Fake),
        Dropped(Fake),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Drawn {
        Vulkan(u8),
        Gles,
        GlesFailed,
        Neither,
    }

    struct Outcome {
        drawn: Drawn,
        graphics: Graphics,
        warnings: Vec<String>,
        events: Vec<Event>,
    }

    fn start_with(
        requested: Requested,
        vulkan: Result<DeviceSummary, &'static str>,
        gl: Result<&'static str, &'static str>,
    ) -> Outcome {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut vulkan_starts = 0;
        let started = start_drawing(
            requested,
            || {
                vulkan_starts += 1;
                let device = vulkan.clone()?;
                let fake = Fake::Vulkan(vulkan_starts);
                events.borrow_mut().push(Event::Started(fake));
                let renderer = Renderer {
                    fake,
                    events: Rc::clone(&events),
                };
                Ok::<_, &str>((renderer, DeviceFit::of(&device)))
            },
            || {
                let gl = gl?;
                events.borrow_mut().push(Event::Started(Fake::Gles));
                let renderer = Renderer {
                    fake: Fake::Gles,
                    events: Rc::clone(&events),
                };
                Ok::<_, &str>((renderer, gl.to_owned()))
            },
        );
        let drawn = match &started.drawing {
            Ok(Drawing::Vulkan(renderer)) => match renderer.fake {
                Fake::Vulkan(start) => Drawn::Vulkan(start),
                Fake::Gles => panic!("a GLES renderer came back as Vulkan"),
            },
            Ok(Drawing::Gles(renderer)) => {
                assert_eq!(renderer.fake, Fake::Gles);
                Drawn::Gles
            }
            Err(Undrawn::Gles(_)) => Drawn::GlesFailed,
            Err(Undrawn::Neither { .. }) => Drawn::Neither,
        };
        let Started { drawing, plan } = started;
        drop(drawing);
        let events = events.borrow().clone();
        Outcome {
            drawn,
            graphics: plan.graphics(),
            warnings: plan.warning_texts(),
            events,
        }
    }

    fn lavapipe() -> DeviceSummary {
        DeviceSummary {
            name: "llvmpipe (LLVM 22.1.0, 256 bits)".to_owned(),
            ..device(DeviceClass::Cpu, LAVAPIPE)
        }
    }

    fn haswell() -> DeviceSummary {
        DeviceSummary {
            name: "Intel(R) HD Graphics 4600 (HSW GT2)".to_owned(),
            ..device(DeviceClass::Integrated, HASWELL_GT2)
        }
    }

    const FALLBACK: Graphics = Graphics::Gles(GlesReason::NoUsableVulkan);

    const FALLBACK_TO_RTX_5070: &str =
        "No usable Vulkan GPU was found, so Roblox uses OpenGL ES on NVIDIA GeForce RTX \
         5070/PCIe/SSE2.";

    const ON_LLVMPIPE: &str = "Roblox is rendering on the CPU (llvmpipe), which is slow.";

    #[test]
    fn use_opengl_starts_only_opengl_es_and_warns_only_about_software_gl() {
        for (gl, warnings) in [(RTX_5070_GL, Vec::new()), (LLVMPIPE_GL, vec![ON_LLVMPIPE])] {
            let outcome = start_with(
                Requested::OpenGlEs,
                Ok(device(DeviceClass::Discrete, RTX_5070)),
                Ok(gl),
            );
            assert_eq!(outcome.graphics, Graphics::Gles(GlesReason::Configured));
            assert_eq!(outcome.drawn, Drawn::Gles);
            assert_eq!(outcome.warnings, warnings);
            assert_eq!(
                outcome.events,
                [Event::Started(Fake::Gles), Event::Dropped(Fake::Gles)]
            );
        }
        let failed = start_with(
            Requested::OpenGlEs,
            Ok(device(DeviceClass::Discrete, RTX_5070)),
            Err("eglInitialize failed"),
        );
        assert_eq!(failed.graphics, Graphics::Gles(GlesReason::Configured));
        assert_eq!(failed.drawn, Drawn::GlesFailed);
        assert_eq!(
            failed.warnings,
            [
                "OpenGL ES does not work here, so Roblox may not start; set use_opengl to false to \
                 use Vulkan. eglInitialize failed"
            ]
        );
        assert_eq!(failed.events, []);
    }

    #[test]
    fn a_usable_vulkan_gpu_keeps_vulkan_and_never_starts_opengl_es() {
        let outcome = start_with(
            Requested::Automatic,
            Ok(device(DeviceClass::Discrete, RTX_5070)),
            Err("OpenGL ES must not start"),
        );
        assert_eq!(outcome.graphics, Graphics::Vulkan);
        assert_eq!(outcome.drawn, Drawn::Vulkan(1));
        assert_eq!(outcome.warnings, Vec::<String>::new());
        assert_eq!(
            outcome.events,
            [
                Event::Started(Fake::Vulkan(1)),
                Event::Dropped(Fake::Vulkan(1))
            ]
        );
    }

    #[test]
    fn an_unusable_vulkan_gpu_with_hardware_gl_falls_back_to_opengl_es() {
        let old_vulkan = vulkan_1_0(device(DeviceClass::Discrete, RTX_5070));
        for unusable in [lavapipe(), haswell(), old_vulkan] {
            let outcome = start_with(Requested::Automatic, Ok(unusable.clone()), Ok(RTX_5070_GL));
            assert_eq!(outcome.graphics, FALLBACK, "{unusable:?}");
            assert_eq!(outcome.drawn, Drawn::Gles);
            assert_eq!(outcome.warnings, [FALLBACK_TO_RTX_5070]);
            assert_eq!(
                outcome.events,
                [
                    Event::Started(Fake::Vulkan(1)),
                    Event::Dropped(Fake::Vulkan(1)),
                    Event::Started(Fake::Gles),
                    Event::Dropped(Fake::Gles)
                ],
                "the Vulkan renderer is gone before OpenGL ES starts on the same window"
            );
        }
        let failed = start_with(
            Requested::Automatic,
            Err("no Vulkan driver"),
            Ok(RTX_5070_GL),
        );
        assert_eq!(failed.graphics, FALLBACK);
        assert_eq!(failed.drawn, Drawn::Gles);
        assert_eq!(failed.warnings, [FALLBACK_TO_RTX_5070]);
    }

    #[test]
    fn a_usable_gpu_after_an_unusable_one_keeps_vulkan_without_a_warning() {
        let devices = [
            vulkan_1_0(device(DeviceClass::Discrete, RTX_5070)),
            device(DeviceClass::Integrated, RAPHAEL),
        ];
        let chosen = choose(&devices, &by_default()).expect("both GPUs present");
        let outcome = start_with(
            Requested::Automatic,
            Ok(devices[chosen.index].clone()),
            Ok(RTX_5070_GL),
        );
        assert_eq!(outcome.graphics, Graphics::Vulkan);
        assert_eq!(outcome.drawn, Drawn::Vulkan(1));
        assert_eq!(outcome.warnings, Vec::<String>::new());
    }

    #[test]
    fn lavapipe_with_software_gl_keeps_vulkan_for_roblox_and_warns_about_the_cpu() {
        let outcome = start_with(Requested::Automatic, Ok(lavapipe()), Ok(LLVMPIPE_GL));
        assert_eq!(outcome.graphics, Graphics::Vulkan);
        assert_eq!(outcome.drawn, Drawn::Gles);
        assert_eq!(outcome.warnings, [ON_LLVMPIPE]);

        let gl_failed = start_with(Requested::Automatic, Ok(lavapipe()), Err("no EGL"));
        assert_eq!(gl_failed.graphics, Graphics::Vulkan);
        assert_eq!(gl_failed.drawn, Drawn::Vulkan(2));
        assert_eq!(gl_failed.warnings, [ON_LLVMPIPE]);
        assert_eq!(
            gl_failed.events,
            [
                Event::Started(Fake::Vulkan(1)),
                Event::Dropped(Fake::Vulkan(1)),
                Event::Started(Fake::Vulkan(2)),
                Event::Dropped(Fake::Vulkan(2))
            ]
        );
    }

    #[test]
    fn an_unusable_gpu_with_software_gl_keeps_vulkan_and_names_why_it_may_fail() {
        let outcome = start_with(Requested::Automatic, Ok(haswell()), Ok(LLVMPIPE_GL));
        assert_eq!(outcome.graphics, Graphics::Vulkan);
        assert_eq!(outcome.drawn, Drawn::Gles);
        assert_eq!(
            outcome.warnings,
            [
                "Roblox may not run on Intel(R) HD Graphics 4600 (HSW GT2), as Mesa's Vulkan \
                 driver for this Intel GPU is incomplete."
            ]
        );
    }

    #[test]
    fn failed_vulkan_uses_software_gl_and_warns_about_the_cpu() {
        let outcome = start_with(
            Requested::Automatic,
            Err("no Vulkan driver"),
            Ok(LLVMPIPE_GL),
        );
        assert_eq!(outcome.graphics, FALLBACK);
        assert_eq!(outcome.drawn, Drawn::Gles);
        assert_eq!(outcome.warnings, [ON_LLVMPIPE]);
    }

    #[test]
    fn neither_vulkan_nor_opengl_es_leaves_roblox_on_its_own_path_with_both_errors() {
        let outcome = start_with(
            Requested::Automatic,
            Err("Vulkan error: vkCreateInstance failed"),
            Err("eglInitialize failed"),
        );
        assert_eq!(outcome.graphics, Graphics::Vulkan);
        assert_eq!(outcome.drawn, Drawn::Neither);
        assert_eq!(
            outcome.warnings,
            [
                "Neither Vulkan nor OpenGL ES works here, so Roblox may not start. Vulkan error: \
                 vkCreateInstance failed; OpenGL ES error: eglInitialize failed"
            ]
        );
        assert_eq!(outcome.events, []);
    }

    #[test]
    fn only_llvmpipe_and_softpipe_are_software_gl() {
        for software in [
            "llvmpipe (LLVM 22.1.0, 256 bits)",
            "zink Vulkan 1.4(llvmpipe (LLVM 22.1.0, 256 bits))",
            "Gallium 0.4 on SoftPipe",
        ] {
            assert!(software_gl(software).is_some(), "{software}");
        }
        assert_eq!(
            software_gl("zink Vulkan 1.4(llvmpipe (LLVM 22.1.0, 256 bits))"),
            Some("llvmpipe")
        );
        for hardware in [
            "NVIDIA GeForce RTX 5070/PCIe/SSE2",
            "AMD Radeon Graphics (radeonsi, raphael_mendocino, LLVM 22.1.0, DRM 3.64, 7.2.8)",
            "Mesa Intel(R) Graphics (RPL-S)",
            "",
        ] {
            assert_eq!(software_gl(hardware), None, "{hardware}");
        }
    }

    #[test]
    fn use_opengl_asks_for_opengl_es_and_names_itself_in_the_plan() {
        let config = eclipse_config::Config::default();
        assert_eq!(Requested::of(&config), Requested::Automatic);
        let config = eclipse_config::Config {
            use_opengl: true,
            ..config
        };
        assert_eq!(Requested::of(&config), Requested::OpenGlEs);
        assert_eq!(Requested::Automatic.unsurveyed(), Graphics::Vulkan);
        assert_eq!(
            Requested::OpenGlEs.unsurveyed(),
            Graphics::Gles(GlesReason::Configured)
        );
        assert_eq!(Graphics::Vulkan.to_string(), "Vulkan");
        assert_eq!(
            Graphics::Gles(GlesReason::Configured).to_string(),
            "OpenGL ES (use_opengl)"
        );
        assert_eq!(FALLBACK.to_string(), "OpenGL ES (no usable Vulkan GPU)");
    }

    #[test]
    fn the_chosen_device_fits_roblox_only_when_usable() {
        assert_eq!(
            DeviceFit::of(&device(DeviceClass::Discrete, RTX_5070)),
            DeviceFit::Usable
        );
        assert_eq!(
            DeviceFit::of(&lavapipe()),
            DeviceFit::Unusable {
                device: "llvmpipe (LLVM 22.1.0, 256 bits)".to_owned(),
                reason: Unusable::Cpu,
            }
        );
    }

    #[test]
    fn the_nvidia_driver_version_is_the_first_dotted_number_on_the_nvrm_line() {
        let open_module = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  615.71.09  \
                           Release Build  (notroot@)  Thu Oct  1 21:43:58 UTC 2026\n\
                           GCC version:  Selected multilib: .;@m64\n";
        assert_eq!(nvidia_driver_version(open_module), Some("615.71.09"));
        let proprietary_module = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.54.14  Thu \
                                  Feb 22 01:44:30 UTC 2024\n";
        assert_eq!(nvidia_driver_version(proprietary_module), Some("550.54.14"));
        assert_eq!(
            nvidia_driver_version("NVRM version: NVIDIA UNIX x86_64 Kernel Module  390.157  Wed\n"),
            Some("390.157")
        );
        for (fixture, version) in [
            (
                include_str!("../tests/fixtures/doctor/nvidia-version"),
                "580.95.05",
            ),
            (
                include_str!("../tests/fixtures/doctor/nvidia-version-470"),
                "470.256.02",
            ),
        ] {
            assert_eq!(nvidia_driver_version(fixture), Some(version));
        }
        for garbage in [
            "",
            "NVRM version:",
            "GCC version:  gcc version 14.2.1 20250207\n",
            "NVRM version: NVIDIA 615 .71. 1..2 x86_64 \u{fffd}\0\n",
            "\u{fffd}\u{fffd}\0NVRM",
        ] {
            assert_eq!(nvidia_driver_version(garbage), None, "{garbage:?}");
        }
    }

    fn report_of(warnings: &[Warning]) -> (Vec<String>, Vec<Notice>) {
        let (sender, updates) = std::sync::mpsc::channel();
        let posted = RefCell::new(Vec::new());
        report_to(&StatusSink::with_window(sender), warnings, |request| {
            posted.borrow_mut().push(request);
            Ok(())
        });
        let shown = updates
            .try_iter()
            .map(|update| match update {
                crate::status::StatusUpdate::Warning(text) => text,
                other => panic!("only warnings are shown, not {other:?}"),
            })
            .collect();
        let notices = posted
            .into_inner()
            .into_iter()
            .map(|request| match request {
                PortalRequest::Notify(notice) => notice,
                _ => panic!("only a notification is posted"),
            })
            .collect();
        (shown, notices)
    }

    #[test]
    fn warnings_that_last_into_play_are_also_one_desktop_notification() {
        let warnings = [
            Warning::MissingNvidiaDriver {
                version: "615.71.09".to_owned(),
            },
            Warning::ConfiguredMissing {
                configured: PciId {
                    vendor: 0xdead,
                    device: 0xbeef,
                },
                used: "llvmpipe (LLVM 22.1.0, 256 bits)".to_owned(),
            },
            Warning::NoUsableVulkan {
                gl: RTX_5070_GL.to_owned(),
            },
            Warning::Unusable {
                device: "Intel(R) HD Graphics 4600 (HSW GT2)".to_owned(),
                reason: Unusable::IntelGen7,
            },
            Warning::Software {
                renderer: "llvmpipe".to_owned(),
            },
        ];
        let texts: Vec<String> = warnings.iter().map(ToString::to_string).collect();

        let (shown, notices) = report_of(&warnings);

        assert_eq!(shown, texts);
        assert_eq!(
            notices,
            [Notice {
                id: NoticeId::Graphics,
                title: "Roblox graphics problem".to_owned(),
                body: [&texts[0], &texts[1], &texts[3], &texts[4]]
                    .map(String::as_str)
                    .join("\n"),
            }]
        );
    }

    #[test]
    fn warnings_that_end_with_the_status_window_post_no_notification() {
        let warnings = [
            Warning::NoUsableVulkan {
                gl: RTX_5070_GL.to_owned(),
            },
            Warning::GlesFailed {
                error: "eglInitialize failed".to_owned(),
            },
            Warning::NothingDraws {
                vulkan: "no Vulkan loader".to_owned(),
                gles: "eglInitialize failed".to_owned(),
            },
        ];
        let (shown, notices) = report_of(&warnings);
        assert_eq!(shown.len(), 3);
        assert_eq!(notices, []);
        assert_eq!(report_of(&[]), (Vec::new(), Vec::new()));
    }

    #[test]
    fn a_flatpak_without_the_matching_nvidia_gl_extension_names_the_install_command() {
        let names = |list: &[&str]| list.iter().map(|&name| name.to_owned()).collect::<Vec<_>>();
        let gap = nvidia_gl_extension_gap("615.71.09", &names(&["default", "glvnd", "vulkan"]))
            .expect("the matching extension is missing");
        assert_eq!(
            gap.to_string(),
            "NVIDIA driver 615.71.09 has no matching Flatpak driver, so Roblox cannot use the \
             NVIDIA GPU. Run: flatpak install flathub org.freedesktop.Platform.GL.nvidia-615-71-09"
        );
        assert_eq!(
            nvidia_gl_extension_gap("615.71.09", &names(&["default", "nvidia-610-43-02"])),
            Some(gap)
        );
        assert_eq!(
            nvidia_gl_extension_gap("615.71.09", &names(&["default", "nvidia-615-71-09"])),
            None
        );
        assert_eq!(nvidia_gl_extension_gap("615.71.09", &names(&[])), None);
        assert_eq!(
            nvidia_gl_extension_gap("615.71.09", &names(&["nvidia", "mesa"])),
            None
        );
    }
}
