use std::ffi::CStr;
use std::fmt;

use ash::vk;
use raw_window_handle::HasDisplayHandle as _;

use super::{
    api_version_text, choose, device_name, device_warnings, driver_version_text,
    flatpak_gl_extensions, graphics_families, has_swapchain, nvidia_gl_extension_gap,
    nvidia_kernel_module, start_drawing, usable, Choice, DeviceClass, DeviceFit, DeviceList,
    DeviceSummary, Plan, Requested, Selection, Warning, FLATPAK_DEFAULT_GL, NVIDIA,
};
use crate::egl_engine::GlDriver;

pub struct Diagnosis {
    requested: Requested,
    vulkan: Result<VulkanSurvey, VulkanError>,
    choice: Option<Choice>,
    gl: Option<Result<GlDriver, String>>,
    plan: Plan,
    nvidia: NvidiaFacts,
}

#[must_use]
pub fn diagnose(config: &eclipse_config::Config) -> Diagnosis {
    Diagnosis::of(
        Requested::of(config),
        &Selection::from_environment(config.vulkan_device),
        survey_vulkan(),
        NvidiaFacts::read(),
        probe_gl,
    )
}

impl Diagnosis {
    fn of(
        requested: Requested,
        selection: &Selection,
        vulkan: Result<VulkanSurvey, VulkanError>,
        nvidia: NvidiaFacts,
        probe_gl: impl FnOnce() -> Result<GlDriver, String>,
    ) -> Self {
        let summaries = vulkan.as_ref().map(VulkanSurvey::summaries);
        let choice = summaries
            .as_ref()
            .ok()
            .and_then(|devices| choose(devices, selection));
        let fit = match (&summaries, choice) {
            (Err(error), _) => Err(error.to_string()),
            (Ok(devices), None) if devices.is_empty() => {
                Err("no Vulkan physical devices found".to_owned())
            }
            (Ok(devices), None) => Err(format!(
                "no Vulkan device with a graphics queue and swapchain support; found {}",
                DeviceList(devices)
            )),
            (Ok(devices), Some(choice)) => Ok(((), DeviceFit::of(&devices[choice.index]))),
        };
        let mut gl = None;
        let started = start_drawing(
            requested,
            || fit.clone(),
            || {
                let probed = probe_gl();
                let renderer = match &probed {
                    Ok(driver) => Ok(((), driver.renderer.clone())),
                    Err(error) => Err(error.clone()),
                };
                gl = Some(probed);
                renderer
            },
        );
        Self {
            requested,
            vulkan,
            choice,
            gl,
            plan: started.plan,
            nvidia,
        }
    }

    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let devices = match (&self.vulkan, self.requested, self.choice) {
            (Ok(survey), Requested::Automatic, Some(choice)) => {
                device_warnings(&survey.summaries(), choice)
            }
            _ => Vec::new(),
        };
        self.nvidia_warning()
            .into_iter()
            .chain(devices)
            .chain(self.plan.warning.clone())
            .filter(Warning::needs_fixing)
            .map(|warning| warning.to_string())
            .collect()
    }

    fn nvidia_warning(&self) -> Option<Warning> {
        let Ok(Some(module)) = &self.nvidia.module else {
            return None;
        };
        if let Ok(Some(gl_extensions)) = &self.nvidia.gl_extensions {
            if gl_extensions.iter().any(|name| name == FLATPAK_DEFAULT_GL) {
                return nvidia_gl_extension_gap(module, gl_extensions);
            }
        }
        let nvidia = self.vulkan.as_ref().ok().and_then(|survey| {
            survey
                .devices
                .iter()
                .find(|device| device.summary.vendor_id == NVIDIA)
        });
        let Some(nvidia) = nvidia else {
            return Some(Warning::NvidiaVulkanMissing {
                module: module.clone(),
            });
        };
        let vulkan = nvidia.driver_version();
        (vulkan != *module).then(|| Warning::NvidiaVersionMismatch {
            module: module.clone(),
            vulkan,
        })
    }

    fn write_choice(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Ok(survey) = &self.vulkan else {
            return Ok(());
        };
        let Some(choice) = self.choice else {
            return writeln!(f, "  Vulkan device: none can present");
        };
        let chosen = &survey.devices[choice.index].summary;
        write!(f, "  Vulkan device: {chosen} ({})", choice.reason)?;
        if let Err(unusable) = usable(chosen) {
            write!(f, "; Roblox may not run on it, as {unusable}")?;
        }
        writeln!(f)
    }
}

impl fmt::Display for Diagnosis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.vulkan {
            Ok(survey) => {
                writeln!(
                    f,
                    "  Vulkan instance: {}",
                    api_version_text(survey.instance_version)
                )?;
                if survey.devices.is_empty() {
                    writeln!(f, "  Vulkan devices: none")?;
                }
                for (index, device) in survey.devices.iter().enumerate() {
                    writeln!(f, "  Device {index}: {device}")?;
                }
            }
            Err(error) => writeln!(f, "  Vulkan instance: {error}")?,
        }
        match &self.nvidia.module {
            Ok(Some(module)) => writeln!(f, "  NVIDIA kernel module: {module}")?,
            Ok(None) => {}
            Err(error) => writeln!(f, "  NVIDIA kernel module: {error}")?,
        }
        writeln!(f, "  Roblox draws with: {}", self.plan.graphics)?;
        if self.requested == Requested::Automatic {
            self.write_choice(f)?;
        }
        match &self.gl {
            Some(Ok(driver)) => writeln!(f, "  OpenGL ES: {}, {}", driver.renderer, driver.version),
            Some(Err(error)) => writeln!(f, "  OpenGL ES: {error}"),
            None => Ok(()),
        }
    }
}

struct NvidiaFacts {
    module: Result<Option<String>, String>,
    gl_extensions: Result<Option<Vec<String>>, String>,
}

impl NvidiaFacts {
    fn read() -> Self {
        Self {
            module: nvidia_kernel_module(),
            gl_extensions: flatpak_gl_extensions(),
        }
    }
}

enum VulkanError {
    NoLoader(String),
    Failed(String),
}

impl fmt::Display for VulkanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLoader(error) => write!(f, "no Vulkan loader: {error}"),
            Self::Failed(error) => f.write_str(error),
        }
    }
}

struct VulkanSurvey {
    instance_version: u32,
    devices: Vec<SurveyedDevice>,
}

impl VulkanSurvey {
    fn summaries(&self) -> Vec<DeviceSummary> {
        self.devices
            .iter()
            .map(|device| device.summary.clone())
            .collect()
    }
}

struct SurveyedDevice {
    summary: DeviceSummary,
    driver: Option<Driver>,
}

impl SurveyedDevice {
    fn driver_version(&self) -> String {
        match &self.driver {
            Some(driver) if self.summary.vendor_id == NVIDIA && !driver.info.is_empty() => {
                driver.info.clone()
            }
            _ => driver_version_text(self.summary.vendor_id, self.summary.driver_version),
        }
    }
}

impl fmt::Display for SurveyedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let summary = &self.summary;
        write!(
            f,
            "{summary}, {}, Vulkan {}, driver ",
            summary.class,
            api_version_text(summary.api_version)
        )?;
        match &self.driver {
            Some(driver) => write!(f, "{} {}", driver.name, driver.info)?,
            None => f.write_str(&self.driver_version())?,
        }
        if !summary.presents() {
            f.write_str(", cannot present")?;
        }
        Ok(())
    }
}

struct Driver {
    name: String,
    info: String,
}

fn survey_vulkan() -> Result<VulkanSurvey, VulkanError> {
    let entry =
        unsafe { ash::Entry::load() }.map_err(|error| VulkanError::NoLoader(error.to_string()))?;
    let instance_version = unsafe { entry.try_enumerate_instance_version() }
        .map_err(|error| {
            VulkanError::Failed(format!("vkEnumerateInstanceVersion failed: {error}"))
        })?
        .unwrap_or(vk::API_VERSION_1_0);
    let api_version = if instance_version >= vk::API_VERSION_1_1 {
        vk::API_VERSION_1_3
    } else {
        vk::API_VERSION_1_0
    };
    let application = vk::ApplicationInfo::default()
        .application_name(c"Eclipse doctor")
        .api_version(api_version);
    let create = vk::InstanceCreateInfo::default().application_info(&application);
    let instance = unsafe { entry.create_instance(&create, None) }
        .map_err(|error| VulkanError::Failed(format!("vkCreateInstance failed: {error}")))?;
    let devices = surveyed_devices(&instance, api_version);
    unsafe { instance.destroy_instance(None) };
    Ok(VulkanSurvey {
        instance_version,
        devices: devices?,
    })
}

fn surveyed_devices(
    instance: &ash::Instance,
    api_version: u32,
) -> Result<Vec<SurveyedDevice>, VulkanError> {
    let handles = unsafe { instance.enumerate_physical_devices() }.map_err(|error| {
        VulkanError::Failed(format!("vkEnumeratePhysicalDevices failed: {error}"))
    })?;
    Ok(handles
        .into_iter()
        .map(|handle| {
            let properties = unsafe { instance.get_physical_device_properties(handle) };
            let name = device_name(&properties).into_owned();
            let present_family = has_swapchain(instance, handle, &name)
                .then(|| graphics_families(instance, handle).next())
                .flatten();
            let driver = (api_version >= vk::API_VERSION_1_2
                && properties.api_version >= vk::API_VERSION_1_2)
                .then(|| driver_properties(instance, handle));
            SurveyedDevice {
                summary: DeviceSummary {
                    vendor_id: properties.vendor_id,
                    device_id: properties.device_id,
                    name,
                    class: DeviceClass::of(properties.device_type),
                    api_version: properties.api_version,
                    driver_version: properties.driver_version,
                    present_family,
                },
                driver,
            }
        })
        .collect())
}

fn driver_properties(instance: &ash::Instance, handle: vk::PhysicalDevice) -> Driver {
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
    unsafe { instance.get_physical_device_properties2(handle, &mut properties) };
    Driver {
        name: c_text(driver.driver_name_as_c_str()),
        info: c_text(driver.driver_info_as_c_str()),
    }
}

fn c_text<E>(text: Result<&CStr, E>) -> String {
    text.map(|text| text.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn probe_gl() -> Result<GlDriver, String> {
    let set = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if !(set("WAYLAND_DISPLAY") || set("WAYLAND_SOCKET") || set("DISPLAY")) {
        return Err("no display: neither WAYLAND_DISPLAY nor DISPLAY is set".to_owned());
    }
    let event_loop = winit::event_loop::EventLoop::new()
        .map_err(|error| format!("cannot connect to the display: {error}"))?;
    let display = event_loop
        .display_handle()
        .map_err(|error| format!("the display has no handle: {error}"))?;
    crate::egl_engine::probe_gl(display.as_raw()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eclipse_config::PciId;

    const RTX_5070_GL: &str = "NVIDIA GeForce RTX 5070/PCIe/SSE2";
    const LLVMPIPE_GL: &str = "llvmpipe (LLVM 22.1.0, 256 bits)";
    const GLES_VERSION: &str = "OpenGL ES 3.2 NVIDIA 615.71.09";
    const NO_LOADER: &str = "libvulkan.so.1: cannot open shared object file";
    const ON_LLVMPIPE: &str = "Roblox is rendering on the CPU (llvmpipe), which is slow.";
    const FALLBACK: &str = "  Roblox draws with: OpenGL ES (no usable Vulkan GPU)\n";
    const RTX_5070_LINE: &str =
        "  Device 0: NVIDIA GeForce RTX 5070 [10de:2f04], discrete, Vulkan 1.4.312, driver NVIDIA \
         615.71.09\n";

    fn surveyed(
        name: &str,
        class: DeviceClass,
        (vendor_id, device_id): (u32, u32),
        driver: Option<(&str, &str)>,
    ) -> SurveyedDevice {
        SurveyedDevice {
            summary: DeviceSummary {
                vendor_id,
                device_id,
                name: name.to_owned(),
                class,
                api_version: vk::make_api_version(0, 1, 4, 312),
                driver_version: vk::make_api_version(0, 25, 2, 4),
                present_family: Some(0),
            },
            driver: driver.map(|(name, info)| Driver {
                name: name.to_owned(),
                info: info.to_owned(),
            }),
        }
    }

    fn rtx_5070(driver_info: &str) -> SurveyedDevice {
        surveyed(
            "NVIDIA GeForce RTX 5070",
            DeviceClass::Discrete,
            (NVIDIA, 0x2f04),
            Some(("NVIDIA", driver_info)),
        )
    }

    fn lavapipe() -> SurveyedDevice {
        surveyed(
            LLVMPIPE_GL,
            DeviceClass::Cpu,
            (0x10005, 0),
            Some(("llvmpipe", "Mesa 25.2.4 (LLVM 22.1.0)")),
        )
    }

    fn survey(devices: Vec<SurveyedDevice>) -> Result<VulkanSurvey, VulkanError> {
        Ok(VulkanSurvey {
            instance_version: vk::make_api_version(0, 1, 4, 321),
            devices,
        })
    }

    fn no_loader() -> Result<VulkanSurvey, VulkanError> {
        Err(VulkanError::NoLoader(NO_LOADER.to_owned()))
    }

    fn without_nvidia() -> NvidiaFacts {
        NvidiaFacts {
            module: Ok(None),
            gl_extensions: Ok(None),
        }
    }

    fn nvidia(gl_extensions: Option<&[&str]>) -> NvidiaFacts {
        NvidiaFacts {
            module: Ok(Some("615.71.09".to_owned())),
            gl_extensions: Ok(
                gl_extensions.map(|names| names.iter().map(|&name| name.to_owned()).collect())
            ),
        }
    }

    fn by_default() -> Selection {
        Selection {
            configured: None,
            loader_override: None,
        }
    }

    fn diagnosis(
        requested: Requested,
        selection: &Selection,
        vulkan: Result<VulkanSurvey, VulkanError>,
        nvidia: NvidiaFacts,
        gl: Result<&str, &str>,
    ) -> Diagnosis {
        Diagnosis::of(requested, selection, vulkan, nvidia, || {
            gl.map(|renderer| GlDriver {
                renderer: renderer.to_owned(),
                version: GLES_VERSION.to_owned(),
            })
            .map_err(str::to_owned)
        })
    }

    fn automatic(vulkan: Result<VulkanSurvey, VulkanError>, gl: Result<&str, &str>) -> Diagnosis {
        diagnosis(
            Requested::Automatic,
            &by_default(),
            vulkan,
            without_nvidia(),
            gl,
        )
    }

    #[test]
    fn a_usable_gpu_means_vulkan_without_probing_opengl_es() {
        let diagnosis = Diagnosis::of(
            Requested::Automatic,
            &by_default(),
            survey(vec![lavapipe(), rtx_5070("615.71.09")]),
            without_nvidia(),
            || panic!("OpenGL ES is probed only when Roblox would use it"),
        );
        assert_eq!(
            diagnosis.to_string(),
            "  Vulkan instance: 1.4.321\n  Device 0: llvmpipe (LLVM 22.1.0, 256 bits) \
             [10005:0000], CPU, Vulkan 1.4.312, driver llvmpipe Mesa 25.2.4 (LLVM 22.1.0)\n  \
             Device 1: NVIDIA GeForce RTX 5070 [10de:2f04], discrete, Vulkan 1.4.312, driver \
             NVIDIA 615.71.09\n  Roblox draws with: Vulkan\n  Vulkan device: NVIDIA GeForce RTX \
             5070 [10de:2f04] (usable GPUs come first, then discrete ones)\n"
        );
        assert_eq!(diagnosis.problems(), Vec::<String>::new());
    }

    #[test]
    fn no_usable_vulkan_gpu_with_hardware_gl_is_opengl_es_and_no_problem() {
        let gl_line = format!("  OpenGL ES: {RTX_5070_GL}, {GLES_VERSION}\n");
        let cases = [
            (
                survey(vec![lavapipe()]),
                format!(
                    "  Vulkan device: {LLVMPIPE_GL} [10005:0000] (usable GPUs come first, then \
                     discrete ones); Roblox may not run on it, as it renders on the CPU\n"
                ),
            ),
            (
                no_loader(),
                format!("  Vulkan instance: no Vulkan loader: {NO_LOADER}\n"),
            ),
            (survey(Vec::new()), "  Vulkan devices: none\n".to_owned()),
        ];
        for (vulkan, explained) in cases {
            let diagnosis = automatic(vulkan, Ok(RTX_5070_GL));
            let text = diagnosis.to_string();
            for line in [FALLBACK, &explained, &gl_line] {
                assert!(text.contains(line), "{line:?} in {text}");
            }
            assert_eq!(diagnosis.problems(), Vec::<String>::new(), "{text}");
        }
    }

    #[test]
    fn cpu_rendering_and_missing_drivers_are_problems_only_without_hardware_gl() {
        let lavapipe_only = automatic(survey(vec![lavapipe()]), Ok(LLVMPIPE_GL));
        assert!(
            lavapipe_only
                .to_string()
                .contains("  Roblox draws with: Vulkan\n"),
            "{lavapipe_only}"
        );
        assert_eq!(lavapipe_only.problems(), [ON_LLVMPIPE]);

        let software_gl = automatic(no_loader(), Ok(LLVMPIPE_GL));
        assert!(software_gl.to_string().contains(FALLBACK), "{software_gl}");
        assert_eq!(software_gl.problems(), [ON_LLVMPIPE]);

        let no_display = "no display: neither WAYLAND_DISPLAY nor DISPLAY is set";
        let nothing = automatic(no_loader(), Err(no_display));
        assert!(
            nothing
                .to_string()
                .contains(&format!("  OpenGL ES: {no_display}\n")),
            "{nothing}"
        );
        assert_eq!(
            nothing.problems(),
            [format!(
                "Neither Vulkan nor OpenGL ES works here, so Roblox may not start. no Vulkan \
                 loader: {NO_LOADER}; OpenGL ES error: {no_display}"
            )]
        );
    }

    #[test]
    fn use_opengl_names_the_gl_driver_and_leaves_the_vulkan_choice_out() {
        let missing = Selection {
            configured: Some(PciId {
                vendor: 0xdead,
                device: 0xbeef,
            }),
            loader_override: None,
        };
        let opengl = diagnosis(
            Requested::OpenGlEs,
            &missing,
            survey(vec![rtx_5070("615.71.09")]),
            without_nvidia(),
            Ok(RTX_5070_GL),
        );
        assert_eq!(
            opengl.to_string(),
            format!(
                "  Vulkan instance: 1.4.321\n{RTX_5070_LINE}  Roblox draws with: OpenGL ES \
                 (use_opengl)\n  OpenGL ES: {RTX_5070_GL}, {GLES_VERSION}\n"
            )
        );
        assert_eq!(opengl.problems(), Vec::<String>::new());

        let software = diagnosis(
            Requested::OpenGlEs,
            &missing,
            survey(vec![rtx_5070("615.71.09")]),
            without_nvidia(),
            Ok(LLVMPIPE_GL),
        );
        assert_eq!(software.problems(), [ON_LLVMPIPE]);
    }

    #[test]
    fn a_configured_gpu_that_is_missing_is_named_with_the_gpu_used_instead() {
        let selection = Selection {
            configured: Some(PciId {
                vendor: 0xdead,
                device: 0xbeef,
            }),
            loader_override: None,
        };
        let diagnosis = diagnosis(
            Requested::Automatic,
            &selection,
            survey(vec![rtx_5070("615.71.09")]),
            without_nvidia(),
            Err("OpenGL ES is not probed"),
        );
        assert!(
            diagnosis.to_string().contains(
                "  Vulkan device: NVIDIA GeForce RTX 5070 [10de:2f04] (vulkan_device dead:beef was \
                 not found or cannot present to this window)\n"
            ),
            "{diagnosis}"
        );
        assert_eq!(
            diagnosis.problems(),
            [
                "The GPU dead:beef set in vulkan_device was not found or cannot show this window, \
                 so Eclipse uses NVIDIA GeForce RTX 5070."
            ]
        );
    }

    fn nvidia_problems(
        facts: NvidiaFacts,
        devices: Vec<SurveyedDevice>,
        gl: &str,
    ) -> (String, Vec<String>) {
        let diagnosis = diagnosis(
            Requested::Automatic,
            &by_default(),
            survey(devices),
            facts,
            Ok(gl),
        );
        (diagnosis.to_string(), diagnosis.problems())
    }

    #[test]
    fn a_flatpak_without_the_nvidia_driver_names_the_one_install_command() {
        let (text, problems) = nvidia_problems(
            nvidia(Some(&["default", "glvnd"])),
            vec![lavapipe()],
            LLVMPIPE_GL,
        );
        assert!(
            text.contains("  NVIDIA kernel module: 615.71.09\n"),
            "{text}"
        );
        assert_eq!(
            problems,
            [
                "NVIDIA driver 615.71.09 has no matching Flatpak driver, so Roblox cannot use the \
                 NVIDIA GPU. Run: flatpak install flathub org.freedesktop.Platform.GL.nvidia-615-71-09",
                ON_LLVMPIPE
            ]
        );
        let (_, installed) = nvidia_problems(
            nvidia(Some(&["default", "nvidia-615-71-09"])),
            vec![rtx_5070("615.71.09")],
            RTX_5070_GL,
        );
        assert_eq!(installed, Vec::<String>::new());
    }

    #[test]
    fn a_host_nvidia_driver_needs_its_vulkan_part_at_the_kernel_module_version() {
        let vulkan_missing = "The NVIDIA kernel module 615.71.09 is loaded, but Vulkan finds no \
                              NVIDIA GPU. Install the Vulkan part of NVIDIA's driver 615.71.09.";
        let (_, missing) = nvidia_problems(nvidia(None), vec![lavapipe()], RTX_5070_GL);
        assert_eq!(missing, [vulkan_missing]);
        let no_driver = diagnosis(
            Requested::Automatic,
            &by_default(),
            Err(VulkanError::Failed(
                "vkCreateInstance failed: ERROR_INCOMPATIBLE_DRIVER".to_owned(),
            )),
            nvidia(None),
            Ok(RTX_5070_GL),
        );
        assert_eq!(no_driver.problems(), [vulkan_missing]);

        let (_, mismatch) = nvidia_problems(nvidia(None), vec![rtx_5070("610.43.02")], RTX_5070_GL);
        assert_eq!(
            mismatch,
            [
                "The NVIDIA kernel module is 615.71.09, but the NVIDIA Vulkan driver is 610.43.02. \
                 Restart after a driver update so both match."
            ]
        );

        let mut without_driver_properties = rtx_5070("");
        without_driver_properties.driver = None;
        without_driver_properties.summary.driver_version = (615 << 22) | (71 << 14) | (9 << 6);
        let (text, matching) =
            nvidia_problems(nvidia(None), vec![without_driver_properties], RTX_5070_GL);
        assert_eq!(matching, Vec::<String>::new());
        assert!(text.contains("driver 615.71.09\n"), "{text}");

        let unreadable = NvidiaFacts {
            module: Err("/proc/driver/nvidia/version names no NVIDIA driver version".to_owned()),
            gl_extensions: Ok(None),
        };
        let (text, unknown) = nvidia_problems(unreadable, vec![lavapipe()], RTX_5070_GL);
        assert_eq!(unknown, Vec::<String>::new());
        assert!(
            text.contains(
                "  NVIDIA kernel module: /proc/driver/nvidia/version names no NVIDIA driver \
                 version\n"
            ),
            "{text}"
        );
    }
}
