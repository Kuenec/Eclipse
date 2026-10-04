use std::ffi::OsStr;

use super::*;

const INFO: &[u8] = include_bytes!("../../tests/fixtures/pactl/info.json");
const SINKS: &[u8] = include_bytes!("../../tests/fixtures/pactl/sinks.json");
const SOURCES: &[u8] = include_bytes!("../../tests/fixtures/pactl/sources.json");

const HEADSET: &str = "alsa_output.usb-Example_Headset-00.analog-stereo";
const HEADSET_MIC: &str = "alsa_input.usb-Example_Headset-00.mono-fallback";

fn name(name: &str) -> DeviceName {
    DeviceName::parse(name).expect("a device name")
}

fn device(device: &str, description: &str) -> SoundDevice {
    SoundDevice {
        name: name(device),
        description: description.to_owned(),
        monitor: false,
    }
}

fn monitor(device: &str, description: &str) -> SoundDevice {
    SoundDevice {
        monitor: true,
        ..self::device(device, description)
    }
}

fn server(kind: ServerKind) -> SoundServer {
    SoundServer {
        kind,
        devices: parse_server(INFO, SINKS, SOURCES)
            .expect("the fixtures parse")
            .devices,
    }
}

#[test]
fn the_device_list_names_devices_by_description_and_marks_monitors() {
    let server = parse_server(INFO, SINKS, SOURCES).expect("the fixtures parse");
    assert_eq!(server.kind, ServerKind::PipeWire);
    assert_eq!(
        server.devices,
        SoundDevices {
            outputs: DeviceList {
                devices: vec![
                    device(
                        "alsa_output.pci-0000_00_1f.3.analog-stereo",
                        "Built-in Audio Analog Stereo"
                    ),
                    device(HEADSET, "Example Headset Analog Stereo"),
                ],
                system_default: Some(name(HEADSET)),
            },
            inputs: DeviceList {
                devices: vec![
                    monitor(
                        "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor",
                        "Monitor of Built-in Audio Analog Stereo"
                    ),
                    monitor(
                        "alsa_output.usb-Example_Headset-00.analog-stereo.monitor",
                        "Monitor of Example Headset Analog Stereo"
                    ),
                    device(HEADSET_MIC, "Example Headset Mono"),
                    device("virtual-mic", "virtual-mic"),
                ],
                system_default: Some(name(HEADSET_MIC)),
            },
        }
    );
}

#[test]
fn only_a_pipewire_server_is_taken_for_pipewire() {
    let plain =
        String::from_utf8_lossy(INFO).replace("PulseAudio (on PipeWire 1.6.9)", "pulseaudio");
    let parsed = parse_server(plain.as_bytes(), SINKS, SOURCES).expect("the fixtures parse");
    assert_eq!(parsed.kind, ServerKind::PulseAudio);
}

#[test]
fn unreadable_json_names_the_pactl_command() {
    let error = parse_server(INFO, b"[{\"name\": 0,", SOURCES).expect_err("truncated");
    assert!(
        error.starts_with("`pactl list sinks` printed unreadable JSON: "),
        "{error}"
    );
}

fn chosen(direction: Direction, device: &str) -> Config {
    let mut config = Config::default();
    match direction {
        Direction::Output => config.audio_output_device = AudioDevice::Named(name(device)),
        Direction::Input => config.audio_input_device = AudioDevice::Named(name(device)),
    }
    config
}

fn routed(config: &Config, server: &Result<SoundServer, String>) -> Routing {
    Routing::on_server(&chosen_devices(config), server)
}

#[test]
fn a_connected_device_is_targeted_with_one_line_naming_it() {
    let routing = routed(
        &chosen(Direction::Input, HEADSET_MIC),
        &Ok(server(ServerKind::PulseAudio)),
    );
    assert_eq!(
        routing,
        Routing {
            targets: vec![(Direction::Input, name(HEADSET_MIC))],
            notes: vec![(
                Level::INFO,
                "Roblox's microphone is Example Headset Mono (audio_input_device)".to_owned()
            )],
        }
    );
}

#[test]
fn a_monitor_source_can_be_the_microphone() {
    let monitor = "alsa_output.usb-Example_Headset-00.analog-stereo.monitor";
    let routing = routed(
        &chosen(Direction::Input, monitor),
        &Ok(server(ServerKind::PulseAudio)),
    );
    assert_eq!(
        routing,
        Routing {
            targets: vec![(Direction::Input, name(monitor))],
            notes: vec![(
                Level::INFO,
                "Roblox's microphone is Monitor of Example Headset Analog Stereo \
                 (audio_input_device)"
                    .to_owned()
            )],
        }
    );
}

#[test]
fn a_missing_device_falls_back_to_the_default_with_one_line() {
    let config = chosen(
        Direction::Output,
        "alsa_output.usb-Unplugged-00.analog-stereo",
    );
    let missing = "audio_output_device \"alsa_output.usb-Unplugged-00.analog-stereo\" is not \
                   connected; Roblox uses the system default output device";

    let pulseaudio = routed(&config, &Ok(server(ServerKind::PulseAudio)));
    assert_eq!(
        pulseaudio,
        Routing {
            targets: Vec::new(),
            notes: vec![(Level::WARN, missing.to_owned())],
        }
    );

    let pipewire = routed(&config, &Ok(server(ServerKind::PipeWire)));
    assert_eq!(
        pipewire,
        Routing {
            targets: vec![(
                Direction::Output,
                name("alsa_output.usb-Unplugged-00.analog-stereo")
            )],
            notes: vec![(Level::WARN, format!("{missing} until it is"))],
        }
    );
}

#[test]
fn without_a_device_list_each_choice_falls_back_to_the_default_with_one_line() {
    let mut config = chosen(Direction::Output, HEADSET);
    config.audio_input_device = AudioDevice::Named(name(HEADSET_MIC));
    let refused = "`pactl info` failed (exit status: 1): Connection failure: Connection refused";

    let routing = routed(&config, &Err(refused.to_owned()));

    assert_eq!(routing.targets, []);
    assert_eq!(
        routing.notes,
        [
            (
                Level::WARN,
                format!(
                    "Roblox uses the system default output device instead of \
                     audio_output_device \"{HEADSET}\" because Eclipse cannot list the sound \
                     devices: {refused}"
                )
            ),
            (
                Level::WARN,
                format!(
                    "Roblox uses the system default microphone instead of audio_input_device \
                     \"{HEADSET_MIC}\" because Eclipse cannot list the sound devices: {refused}"
                )
            ),
        ]
    );
}

#[test]
fn the_system_default_needs_no_device_list_and_leaves_the_environment_alone() {
    let routing = Routing::plan(&Config::default());
    assert_eq!(routing, Routing::default());
    let mut client = Command::new("eclipse");
    routing.apply(&mut client);
    assert_eq!(client.get_envs().count(), 0);
}

#[test]
fn targets_reach_the_client_as_pulse_variables() {
    let routing = Routing {
        targets: vec![
            (Direction::Output, name(HEADSET)),
            (Direction::Input, name(HEADSET_MIC)),
        ],
        notes: Vec::new(),
    };
    let mut client = Command::new("eclipse");
    routing.apply(&mut client);
    let environment: Vec<(&OsStr, Option<&OsStr>)> = client.get_envs().collect();
    assert_eq!(
        environment,
        [
            (OsStr::new("PULSE_SINK"), Some(OsStr::new(HEADSET))),
            (OsStr::new("PULSE_SOURCE"), Some(OsStr::new(HEADSET_MIC))),
        ]
    );
}
