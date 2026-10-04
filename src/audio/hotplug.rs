use std::io::{self, BufRead as _, BufReader, Read as _};
use std::process::{Child, ChildStdout, Stdio};

use eclipse_config::audio::{DeviceList, DeviceName, Direction, SoundDevices};
use eclipse_config::SettingKey;
use serde::Deserialize;

use super::{noun, pactl_command, parse_json, pulse_variable, query, LOG_TARGET, PACTL};

const FOLLOWER_THREAD: &str = "eclipse-sound-devices";

struct DeviceChanges {
    pactl: Child,
    events: io::Lines<BufReader<ChildStdout>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EventKind {
    New,
    Change,
    Remove,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Facility {
    Sink,
    Source,
    Server,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct ServerEvent {
    event: EventKind,
    on: Facility,
}

impl ServerEvent {
    fn moves_devices(&self) -> bool {
        match (self.event, self.on) {
            (EventKind::New | EventKind::Remove, Facility::Sink | Facility::Source)
            | (EventKind::Change, Facility::Server) => true,
            (EventKind::Change, Facility::Sink | Facility::Source)
            | (EventKind::New | EventKind::Remove, Facility::Server)
            | (_, Facility::Other) => false,
        }
    }
}

impl DeviceChanges {
    fn subscribe() -> Result<Self, String> {
        let mut command = pactl_command(&["subscribe"]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut pactl = command
            .spawn()
            .map_err(|error| format!("cannot run `{PACTL} subscribe`: {error}"))?;
        let stdout = pactl
            .stdout
            .take()
            .expect("pactl's stdout is piped at spawn");
        Ok(Self {
            pactl,
            events: BufReader::new(stdout).lines(),
        })
    }

    fn next(&mut self) -> Result<(), String> {
        loop {
            let line = match self.events.next() {
                Some(Ok(line)) => line,
                Some(Err(error)) => {
                    return Err(format!("cannot read `{PACTL} subscribe`: {error}"))
                }
                None => return Err(self.ended()),
            };
            let event: ServerEvent = parse_json(line.as_bytes(), "subscribe")?;
            if event.moves_devices() {
                return Ok(());
            }
        }
    }

    fn ended(&mut self) -> String {
        let status = match self.pactl.wait() {
            Ok(status) => status.to_string(),
            Err(error) => format!("cannot wait for it: {error}"),
        };
        let mut said = String::new();
        if let Some(stderr) = self.pactl.stderr.as_mut() {
            if let Err(error) = stderr.read_to_string(&mut said) {
                said = format!("cannot read its error output: {error}");
            }
        }
        match said.trim() {
            "" => format!("`{PACTL} subscribe` ended ({status})"),
            said => format!("`{PACTL} subscribe` ended ({status}): {said}"),
        }
    }
}

impl Drop for DeviceChanges {
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.pactl.try_wait() {
            return;
        }
        if let Err(error) = self.pactl.kill().and_then(|()| self.pactl.wait()) {
            tracing::debug!(target: LOG_TARGET, %error, "cannot stop `{PACTL} subscribe`");
        }
    }
}

pub fn watch_devices(
    mut listed: impl FnMut(&SoundDevices) -> io::Result<()>,
) -> Result<(), String> {
    let mut changes = DeviceChanges::subscribe()?;
    let mut devices = query()?.devices;
    let mut shown = |devices: &SoundDevices| {
        listed(devices).map_err(|error| format!("cannot print the sound devices: {error}"))
    };
    shown(&devices)?;
    loop {
        changes.next()?;
        let now = query()?.devices;
        if now != devices {
            shown(&now)?;
            devices = now;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    Missing,
    Listed,
    SystemDefault,
}

impl Standing {
    fn of(list: &DeviceList, name: &DeviceName) -> Self {
        match (list.find(name), &list.system_default) {
            (None, _) => Self::Missing,
            (Some(_), Some(default)) if default == name => Self::SystemDefault,
            (Some(_), _) => Self::Listed,
        }
    }

    fn reopens_streams(self, now: Self) -> bool {
        now == Self::Listed && self != Self::Listed
    }
}

fn standings(chosen: &[(Direction, DeviceName)], devices: &SoundDevices) -> Vec<Standing> {
    chosen
        .iter()
        .map(|(direction, name)| Standing::of(devices.of(*direction), name))
        .collect()
}

fn chosen_targets() -> Vec<(Direction, DeviceName)> {
    Direction::ALL
        .into_iter()
        .filter_map(|direction| {
            let name = std::env::var(pulse_variable(direction)).ok()?;
            Some((direction, DeviceName::parse(&name)?))
        })
        .collect()
}

pub fn follow_chosen_devices(reopen: fn(Direction)) {
    let chosen = chosen_targets();
    if chosen.is_empty() {
        return;
    }
    let follower = std::thread::Builder::new()
        .name(FOLLOWER_THREAD.to_owned())
        .spawn(move || {
            if let Err(error) = follow(&chosen, reopen) {
                tracing::warn!(
                    target: LOG_TARGET,
                    "Roblox stops following the chosen sound devices: {error}"
                );
            }
        });
    if let Err(error) = follower {
        tracing::warn!(
            target: LOG_TARGET,
            "Roblox cannot follow the chosen sound devices: {error}"
        );
    }
}

fn follow(chosen: &[(Direction, DeviceName)], reopen: fn(Direction)) -> Result<(), String> {
    let mut changes = DeviceChanges::subscribe()?;
    let mut before = standings(chosen, &query()?.devices);
    loop {
        changes.next()?;
        let devices = match query() {
            Ok(server) => server.devices,
            Err(error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    "Eclipse cannot list the sound devices after a change: {error}"
                );
                continue;
            }
        };
        let now = standings(chosen, &devices);
        for ((direction, name), (was, is)) in chosen.iter().zip(before.iter().zip(&now)) {
            if was.reopens_streams(*is) {
                tracing::info!(
                    target: LOG_TARGET,
                    "Roblox reopens its {} on {} \"{name}\"",
                    noun(*direction),
                    SettingKey::audio_device(*direction).name()
                );
                reopen(*direction);
            }
        }
        before = now;
    }
}

#[cfg(test)]
mod tests {
    use eclipse_config::audio::SoundDevice;

    use super::*;

    fn name(name: &str) -> DeviceName {
        DeviceName::parse(name).expect("a device name")
    }

    fn list(names: &[&str], system_default: &str) -> DeviceList {
        DeviceList {
            devices: names
                .iter()
                .map(|device| SoundDevice {
                    name: name(device),
                    description: (*device).to_owned(),
                    monitor: false,
                })
                .collect(),
            system_default: Some(name(system_default)),
        }
    }

    #[test]
    fn only_devices_coming_or_going_and_default_changes_move_devices() {
        let moving: Vec<&str> = [
            r#"{"index":57,"event":"new","on":"sink"}"#,
            r#"{"index":57,"event":"change","on":"sink"}"#,
            r#"{"index":58,"event":"remove","on":"source"}"#,
            r#"{"index":4294967295,"event":"change","on":"server"}"#,
            r#"{"index":12,"event":"new","on":"sink-input"}"#,
            r#"{"index":3,"event":"remove","on":"card"}"#,
        ]
        .into_iter()
        .filter(|line| {
            serde_json::from_str::<ServerEvent>(line)
                .expect("a pactl event")
                .moves_devices()
        })
        .collect();
        assert_eq!(
            moving,
            [
                r#"{"index":57,"event":"new","on":"sink"}"#,
                r#"{"index":58,"event":"remove","on":"source"}"#,
                r#"{"index":4294967295,"event":"change","on":"server"}"#,
            ]
        );
    }

    #[test]
    fn a_chosen_device_is_missing_listed_or_the_system_default() {
        let headset = name("headset");
        assert_eq!(
            Standing::of(&list(&["speakers"], "speakers"), &headset),
            Standing::Missing
        );
        assert_eq!(
            Standing::of(&list(&["speakers", "headset"], "speakers"), &headset),
            Standing::Listed
        );
        assert_eq!(
            Standing::of(&list(&["speakers", "headset"], "headset"), &headset),
            Standing::SystemDefault
        );
    }

    #[test]
    fn streams_reopen_when_the_chosen_device_returns_or_stops_being_the_default() {
        let all = [Standing::Missing, Standing::Listed, Standing::SystemDefault];
        let reopening: Vec<(Standing, Standing)> = all
            .iter()
            .flat_map(|was| all.iter().map(move |is| (*was, *is)))
            .filter(|(was, is)| was.reopens_streams(*is))
            .collect();
        assert_eq!(
            reopening,
            [
                (Standing::Missing, Standing::Listed),
                (Standing::SystemDefault, Standing::Listed),
            ]
        );
    }
}
