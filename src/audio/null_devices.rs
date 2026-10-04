use std::process::Command;
use std::time::{Duration, Instant};

use eclipse_config::audio::{DeviceList, DeviceName, Direction};
use serde::Deserialize;

use super::{pactl, parse_json, query};

const PACTL_LIMIT: Duration = Duration::from_secs(10);

const APPEAR_LIMIT: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(50);

pub(crate) struct NullDevices {
    pub(crate) sink: DeviceName,
    pub(crate) source: DeviceName,
    pub(crate) sink_description: String,
    pub(crate) source_description: String,
    modules: Vec<String>,
}

#[derive(Deserialize)]
struct Stream {
    sink: Option<u32>,
    source: Option<u32>,
    properties: StreamProperties,
}

#[derive(Deserialize)]
struct StreamProperties {
    #[serde(rename = "application.process.id")]
    process_id: Option<String>,
}

#[derive(Deserialize)]
struct Node {
    index: u32,
    name: String,
}

impl NullDevices {
    pub(crate) fn create(tag: &str) -> Option<Self> {
        if let Err(error) = pactl(&["info"]) {
            eprintln!("no PulseAudio or PipeWire server to test against: {error}");
            return None;
        }
        let id = format!("eclipse_test_{tag}_{}", std::process::id());
        let sink = format!("{id}_sink");
        let source = format!("{id}_source");
        let mut devices = Self {
            sink: DeviceName::parse(&sink).expect("a device name"),
            source: DeviceName::parse(&source).expect("a device name"),
            sink_description: format!("{id}_output"),
            source_description: format!("{id}_input"),
            modules: Vec::new(),
        };
        let sink_properties = format!("sink_properties=device.description={id}_output");
        assert!(
            devices.load(&[
                "module-null-sink",
                &format!("sink_name={sink}"),
                &sink_properties,
            ]),
            "cannot create the null sink {sink}"
        );
        let pulseaudio_source = [
            "module-null-source",
            &format!("source_name={source}"),
            &format!("source_properties=device.description={id}_input"),
        ];
        let pipewire_source = [
            "module-null-sink",
            &format!("sink_name={source}"),
            "media.class=Audio/Source/Virtual",
            &format!("sink_properties=device.description={id}_input"),
        ];
        assert!(
            devices.load(&pulseaudio_source) || devices.load(&pipewire_source),
            "cannot create the null source {source}"
        );
        let deadline = Instant::now() + APPEAR_LIMIT;
        while !devices.listed() {
            assert!(
                Instant::now() < deadline,
                "{sink} and {source} did not appear within {APPEAR_LIMIT:?}"
            );
            std::thread::sleep(POLL);
        }
        Some(devices)
    }

    fn load(&mut self, arguments: &[&str]) -> bool {
        let output = crate::bounded_child::output(
            Command::new("pactl").arg("load-module").args(arguments),
            PACTL_LIMIT,
        );
        if !output.status.success() {
            return false;
        }
        let module = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        self.modules.push(module);
        true
    }

    fn listed(&self) -> bool {
        query().is_ok_and(|server| {
            let described = |list: &DeviceList, name, description: &str| {
                list.find(name)
                    .is_some_and(|device| device.description == description)
            };
            described(&server.devices.outputs, &self.sink, &self.sink_description)
                && described(
                    &server.devices.inputs,
                    &self.source,
                    &self.source_description,
                )
        })
    }

    pub(crate) fn make_system_default(&self) {
        for (command, name) in [
            ("set-default-sink", &self.sink),
            ("set-default-source", &self.source),
        ] {
            let output = crate::bounded_child::output(
                Command::new("pactl").args([command, name.as_str()]),
                PACTL_LIMIT,
            );
            assert!(
                output.status.success(),
                "pactl {command} {name}: {output:?}"
            );
        }
        let deadline = Instant::now() + APPEAR_LIMIT;
        while !query().is_ok_and(|server| {
            server.devices.outputs.system_default.as_ref() == Some(&self.sink)
                && server.devices.inputs.system_default.as_ref() == Some(&self.source)
        }) {
            assert!(
                Instant::now() < deadline,
                "{} and {} did not become the defaults within {APPEAR_LIMIT:?}",
                self.sink,
                self.source
            );
            std::thread::sleep(POLL);
        }
    }
}

pub(crate) fn stream_device(direction: Direction, process: u32) -> Option<String> {
    let (streams, nodes) = match direction {
        Direction::Output => (["list", "sink-inputs"], ["list", "sinks"]),
        Direction::Input => (["list", "source-outputs"], ["list", "sources"]),
    };
    let streams: Vec<Stream> = parse_json(&pactl(&streams).ok()?, "list streams").ok()?;
    let nodes: Vec<Node> = parse_json(&pactl(&nodes).ok()?, "list nodes").ok()?;
    let process = process.to_string();
    let index = streams
        .into_iter()
        .find(|stream| stream.properties.process_id.as_deref() == Some(process.as_str()))
        .and_then(|stream| match direction {
            Direction::Output => stream.sink,
            Direction::Input => stream.source,
        })?;
    nodes
        .into_iter()
        .find(|node| node.index == index)
        .map(|node| node.name)
}

impl Drop for NullDevices {
    fn drop(&mut self) {
        for module in self.modules.iter().rev() {
            let unloaded = Command::new("pactl")
                .args(["unload-module", module])
                .status();
            if !unloaded.as_ref().is_ok_and(|status| status.success()) {
                eprintln!("cannot unload the test module {module}: {unloaded:?}");
            }
        }
    }
}
