use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt as _;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use cpal::traits::HostTrait as _;
use eclipse_config::audio::{
    AudioDevice, DeviceList, DeviceName, Direction, SoundDevice, SoundDevices,
};
use eclipse_config::{Config, SettingKey};
use rustix::io::Errno;
use rustix::process::{Pid, Signal};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tracing::Level;

use crate::bounded_child::{self, Bounded};
use crate::diagnostics::RunLog;
use crate::inheritance_cutoff::InheritanceCutoff;

pub mod hotplug;
#[cfg(test)]
pub(crate) mod null_devices;
#[cfg(test)]
pub(crate) mod private_server;
#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "eclipse::audio";

const PACTL: &str = "pactl";

const PACTL_LIMIT: Duration = Duration::from_secs(2);

const FIRST_NON_STDIO_FD: RawFd = 3;

const PIPEWIRE_SERVER: &str = "PipeWire";

const MONITOR_CLASS: &str = "monitor";

pub(crate) const PULSE_PCM: &str = "pulse";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerKind {
    PipeWire,
    PulseAudio,
}

#[derive(Debug)]
pub struct SoundServer {
    kind: ServerKind,
    pub devices: SoundDevices,
}

pub fn query() -> Result<SoundServer, String> {
    let info = pactl(&["info"])?;
    let sinks = pactl(&["list", "sinks"])?;
    let sources = pactl(&["list", "sources"])?;
    parse_server(&info, &sinks, &sources)
}

fn pactl_command(arguments: &[&str]) -> Command {
    let mut command = unforked_pactl_command(arguments);
    end_with_this_process(&mut command);
    command
}

fn unforked_pactl_command(arguments: &[&str]) -> Command {
    let mut command = Command::new(PACTL);
    command
        .arg("--format=json")
        .args(arguments)
        .env_remove("LC_ALL")
        .env("LC_NUMERIC", "C");
    command
}

fn end_with_this_process(command: &mut Command) {
    let parent = rustix::process::getpid();
    let cutoff = InheritanceCutoff::at(FIRST_NON_STDIO_FD);
    unsafe {
        command.pre_exec(move || end_with(parent, cutoff));
    }
}

fn end_with(parent: Pid, cutoff: InheritanceCutoff) -> io::Result<()> {
    cutoff.mark_close_on_exec()?;
    rustix::process::set_parent_process_death_signal(Some(Signal::TERM))?;
    if rustix::process::getppid() != Some(parent) {
        return Err(Errno::SRCH.into());
    }
    Ok(())
}

fn pactl(arguments: &[&str]) -> Result<Vec<u8>, String> {
    bounded_pactl(pactl_command(arguments), arguments)
}

fn unforked_pactl(arguments: &[&str]) -> Result<Vec<u8>, String> {
    bounded_pactl(unforked_pactl_command(arguments), arguments)
}

fn bounded_pactl(mut command: Command, arguments: &[&str]) -> Result<Vec<u8>, String> {
    let shown = format!("{PACTL} {}", arguments.join(" "));
    let (outcome, output) = match bounded_child::run(&mut command, PACTL_LIMIT) {
        Ok(Bounded::Finished(output)) if output.status.success() => return Ok(output.stdout),
        Ok(Bounded::Finished(output)) => (format!("failed ({})", output.status), output),
        Ok(Bounded::TimedOut(output)) => (format!("did not answer within {PACTL_LIMIT:?}"), output),
        Err(error) => return Err(format!("cannot run `{shown}`: {error}")),
    };
    let said = String::from_utf8_lossy(&output.stderr);
    match said.trim() {
        "" => Err(format!("`{shown}` {outcome}")),
        said => Err(format!("`{shown}` {outcome}: {said}")),
    }
}

#[derive(Deserialize)]
struct ServerInfo {
    server_name: String,
    default_sink_name: Option<String>,
    default_source_name: Option<String>,
}

#[derive(Deserialize)]
struct ListedDevice {
    name: String,
    description: Option<String>,
    #[serde(default)]
    properties: DeviceProperties,
}

#[derive(Default, Deserialize)]
struct DeviceProperties {
    #[serde(rename = "device.class")]
    device_class: Option<String>,
}

#[derive(Deserialize)]
struct ListedStream {
    sink: Option<u32>,
    source: Option<u32>,
    properties: StreamProperties,
}

#[derive(Deserialize)]
struct StreamProperties {
    #[serde(rename = "application.process.id")]
    process_id: Option<String>,
    #[serde(rename = "application.process.binary")]
    process_binary: Option<String>,
    #[serde(rename = "pipewire.access.portal.app_id")]
    portal_app_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct StreamOwner {
    process_id: String,
    binary: String,
}

impl StreamOwner {
    pub(crate) fn process(id: u32) -> Result<Self, String> {
        let link = format!("/proc/{id}/exe");
        let executable =
            std::fs::read_link(&link).map_err(|error| format!("cannot read {link}: {error}"))?;
        let binary = executable
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| format!("{link} names no executable: {}", executable.display()))?;
        Ok(Self {
            process_id: id.to_string(),
            binary: binary.to_owned(),
        })
    }

    fn owns(&self, stream: &StreamProperties) -> bool {
        stream.process_id.as_deref() == Some(self.process_id.as_str())
            && stream.process_binary.as_deref() == Some(self.binary.as_str())
            && stream
                .portal_app_id
                .as_deref()
                .is_none_or(|app_id| app_id == crate::APP_ID)
    }
}

#[derive(Deserialize)]
struct ShortDevice {
    index: u32,
    name: String,
}

fn parse_server(info: &[u8], sinks: &[u8], sources: &[u8]) -> Result<SoundServer, String> {
    let info: ServerInfo = parse_json(info, "info")?;
    let kind = if info.server_name.contains(PIPEWIRE_SERVER) {
        ServerKind::PipeWire
    } else {
        ServerKind::PulseAudio
    };
    let devices = SoundDevices {
        outputs: device_list(
            parse_json(sinks, "list sinks")?,
            info.default_sink_name.as_deref(),
        ),
        inputs: device_list(
            parse_json(sources, "list sources")?,
            info.default_source_name.as_deref(),
        ),
    };
    Ok(SoundServer { kind, devices })
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8], command: &str) -> Result<T, String> {
    serde_json::from_slice(bytes)
        .map_err(|error| format!("`{PACTL} {command}` printed unreadable JSON: {error}"))
}

fn device_list(listed: Vec<ListedDevice>, system_default: Option<&str>) -> DeviceList {
    let devices = listed
        .into_iter()
        .filter_map(|device| {
            let name = DeviceName::parse(&device.name)?;
            let monitor = device.properties.device_class.as_deref() == Some(MONITOR_CLASS);
            let description = device
                .description
                .filter(|description| !description.trim().is_empty())
                .unwrap_or(device.name);
            Some(SoundDevice {
                name,
                description,
                monitor,
            })
        })
        .collect();
    DeviceList {
        devices,
        system_default: system_default.and_then(DeviceName::parse),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Routing {
    targets: Vec<(Direction, DeviceName)>,
    notes: Vec<(Level, String)>,
}

impl Routing {
    #[must_use]
    pub fn plan(config: &Config) -> Self {
        let chosen = chosen_devices(config);
        if chosen.is_empty() {
            return Self::default();
        }
        Self::on_server(&chosen, &query())
    }

    fn on_server(
        chosen: &[(Direction, &DeviceName)],
        server: &Result<SoundServer, String>,
    ) -> Self {
        let mut routing = Self::default();
        for &(direction, name) in chosen {
            let key = SettingKey::audio_device(direction).name();
            let noun = noun(direction);
            let server = match server {
                Ok(server) => server,
                Err(error) => {
                    routing.notes.push((
                        Level::WARN,
                        format!(
                            "Roblox uses the system default {noun} instead of {key} \"{name}\" \
                             because Eclipse cannot list the sound devices: {error}"
                        ),
                    ));
                    continue;
                }
            };
            match (server.devices.of(direction).find(name), server.kind) {
                (Some(device), _) => {
                    routing.targets.push((direction, name.clone()));
                    routing.notes.push((
                        Level::INFO,
                        format!("Roblox's {noun} is {} ({key})", device.description),
                    ));
                }
                (None, ServerKind::PipeWire) => {
                    routing.targets.push((direction, name.clone()));
                    routing.notes.push((
                        Level::WARN,
                        format!(
                            "{key} \"{name}\" is not connected; Roblox uses the system default \
                             {noun} until it is"
                        ),
                    ));
                }
                (None, ServerKind::PulseAudio) => routing.notes.push((
                    Level::WARN,
                    format!(
                        "{key} \"{name}\" is not connected; Roblox uses the system default {noun}"
                    ),
                )),
            }
        }
        routing
    }

    pub fn apply(&self, client: &mut Command) {
        for (direction, name) in &self.targets {
            client.env(pulse_variable(*direction), name.as_str());
        }
    }

    pub fn write_notes(&self, log: &mut RunLog) -> io::Result<()> {
        for (level, text) in &self.notes {
            log.record(*level, LOG_TARGET, text)?;
        }
        Ok(())
    }
}

fn chosen_devices(config: &Config) -> Vec<(Direction, &DeviceName)> {
    Direction::ALL
        .into_iter()
        .filter_map(|direction| match config.audio_device(direction) {
            AudioDevice::Named(name) => Some((direction, name)),
            AudioDevice::SystemDefault => None,
        })
        .collect()
}

const fn pulse_variable(direction: Direction) -> &'static str {
    match direction {
        Direction::Output => "PULSE_SINK",
        Direction::Input => "PULSE_SOURCE",
    }
}

const fn noun(direction: Direction) -> &'static str {
    match direction {
        Direction::Output => "output device",
        Direction::Input => "microphone",
    }
}

fn chosen_target(direction: Direction) -> Option<DeviceName> {
    DeviceName::parse(&std::env::var(pulse_variable(direction)).ok()?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputSource {
    Chosen(DeviceName),
    SystemDefault(Option<DeviceName>),
}

impl InputSource {
    pub(crate) fn requested() -> Self {
        chosen_target(Direction::Input).map_or(Self::SystemDefault(None), Self::Chosen)
    }

    pub(crate) fn with_default_named(self) -> Self {
        let Self::SystemDefault(None) = self else {
            return self;
        };
        match unforked_pactl(&["info"]).and_then(|info| parse_json::<ServerInfo>(&info, "info")) {
            Ok(info) => Self::SystemDefault(
                info.default_source_name
                    .as_deref()
                    .and_then(DeviceName::parse),
            ),
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    "Eclipse cannot name the system default microphone: {error}"
                );
                Self::SystemDefault(None)
            }
        }
    }
}

impl fmt::Display for InputSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = SettingKey::audio_device(Direction::Input).name();
        match self {
            Self::Chosen(name) => write!(f, "{name} ({key})"),
            Self::SystemDefault(Some(name)) => write!(f, "{name} (system default)"),
            Self::SystemDefault(None) => f.write_str("system default"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordingSource {
    Listed(Vec<DeviceName>),
    Unknown(String),
}

impl RecordingSource {
    pub(crate) fn of_this_process() -> Self {
        match StreamOwner::process(std::process::id())
            .and_then(|owner| stream_devices(Direction::Input, &owner))
        {
            Ok(sources) => Self::Listed(sources),
            Err(error) => Self::Unknown(error),
        }
    }
}

impl fmt::Display for RecordingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listed(sources) if sources.is_empty() => f.write_str("none listed"),
            Self::Listed(sources) => {
                let names: Vec<&str> = sources.iter().map(DeviceName::as_str).collect();
                f.write_str(&names.join(", "))
            }
            Self::Unknown(error) => write!(f, "unknown ({error})"),
        }
    }
}

pub(crate) fn stream_devices(
    direction: Direction,
    owner: &StreamOwner,
) -> Result<Vec<DeviceName>, String> {
    let (streams, devices) = match direction {
        Direction::Output => ("sink-inputs", "sinks"),
        Direction::Input => ("source-outputs", "sources"),
    };
    let indices = owned_stream_devices(
        &unforked_pactl(&["list", streams])?,
        &format!("list {streams}"),
        direction,
        owner,
    )?;
    if indices.is_empty() {
        return Ok(Vec::new());
    }
    let command = format!("list short {devices}");
    let listed: Vec<ShortDevice> =
        parse_json(&unforked_pactl(&["list", "short", devices])?, &command)?;
    let mut names = Vec::new();
    for index in indices {
        let device = listed
            .iter()
            .find(|device| device.index == index)
            .ok_or_else(|| format!("`{PACTL} {command}` does not list device {index}"))?;
        let name = DeviceName::parse(&device.name).ok_or_else(|| {
            format!(
                "`{PACTL} {command}` names device {index} {:?}, which is no device name",
                device.name
            )
        })?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

fn owned_stream_devices(
    listing: &[u8],
    command: &str,
    direction: Direction,
    owner: &StreamOwner,
) -> Result<Vec<u32>, String> {
    let listed: Vec<ListedStream> = parse_json(listing, command)?;
    Ok(listed
        .into_iter()
        .filter(|stream| owner.owns(&stream.properties))
        .filter_map(|stream| match direction {
            Direction::Output => stream.sink,
            Direction::Input => stream.source,
        })
        .collect())
}

pub(crate) fn host_device(direction: Direction) -> Option<cpal::Device> {
    let targeted = std::env::var_os(pulse_variable(direction)).is_some_and(|name| !name.is_empty());
    if let Some(pulse) = targeted.then(pulse_pcm).flatten() {
        return Some(pulse.clone());
    }
    let host = cpal::default_host();
    match direction {
        Direction::Output => host.default_output_device(),
        Direction::Input => host.default_input_device(),
    }
}

fn pulse_pcm() -> Option<&'static cpal::Device> {
    static PULSE: OnceLock<Option<cpal::Device>> = OnceLock::new();
    PULSE
        .get_or_init(|| {
            let id = cpal::DeviceId::new(cpal::HostId::Alsa, PULSE_PCM);
            let pulse = cpal::default_host().device_by_id(&id);
            if pulse.is_none() {
                tracing::warn!(
                    target: LOG_TARGET,
                    "ALSA has no \"{PULSE_PCM}\" device, so Roblox uses ALSA's default device \
                     instead of the one PULSE_SINK or PULSE_SOURCE names"
                );
            }
            pulse
        })
        .as_ref()
}
