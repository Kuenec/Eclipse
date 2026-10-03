use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eclipse_config::temp_file;
use serde::{Deserialize, Serialize};
use winit::dpi::LogicalSize;
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowAttributes};

use crate::framework::lifecycle::{self, UnsavedFile};

const WINDOW_STATE_FILE: &str = "window.json";
const FIRST_LAUNCH_SIZE: WindowSize = WindowSize {
    width: 1280,
    height: 720,
};
const MIN_SIZE: WindowSize = WindowSize {
    width: 320,
    height: 180,
};
const MAX_EDGE: u32 = 16_384;
const SAVE_DELAY: Duration = Duration::from_millis(500);
const GAMESCOPE_DISPLAY_VARIABLE: &str = "GAMESCOPE_WAYLAND_DISPLAY";
const HYPRLAND_INSTANCE_VARIABLE: &str = "HYPRLAND_INSTANCE_SIGNATURE";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SizeFields", into = "SizeFields")]
struct WindowSize {
    width: u32,
    height: u32,
}

#[derive(Serialize, Deserialize)]
struct SizeFields {
    width: u32,
    height: u32,
}

impl WindowSize {
    fn clamped(width: f64, height: f64) -> Self {
        Self {
            width: (width.round() as u32).clamp(MIN_SIZE.width, MAX_EDGE),
            height: (height.round() as u32).clamp(MIN_SIZE.height, MAX_EDGE),
        }
    }

    fn logical(self) -> LogicalSize<u32> {
        LogicalSize::new(self.width, self.height)
    }
}

impl TryFrom<SizeFields> for WindowSize {
    type Error = String;

    fn try_from(SizeFields { width, height }: SizeFields) -> Result<Self, Self::Error> {
        if (MIN_SIZE.width..=MAX_EDGE).contains(&width)
            && (MIN_SIZE.height..=MAX_EDGE).contains(&height)
        {
            Ok(Self { width, height })
        } else {
            Err(format!(
                "the window size {width}x{height} is outside {}x{} to {MAX_EDGE}x{MAX_EDGE}",
                MIN_SIZE.width, MIN_SIZE.height
            ))
        }
    }
}

impl From<WindowSize> for SizeFields {
    fn from(WindowSize { width, height }: WindowSize) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FullscreenState {
    Off,
    On { monitor: Option<String> },
}

impl FullscreenState {
    fn restored(&self, monitors: impl IntoIterator<Item = MonitorHandle>) -> Option<Fullscreen> {
        let FullscreenState::On { monitor } = self else {
            return None;
        };
        let Some(wanted) = monitor.as_deref() else {
            return Some(Fullscreen::Borderless(None));
        };
        let found = named_monitor(wanted, monitors, MonitorHandle::name);
        if found.is_none() {
            tracing::info!(
                monitor = wanted,
                "the monitor Roblox was last fullscreen on is not connected; it opens fullscreen \
                 on the current one"
            );
        }
        Some(Fullscreen::Borderless(found))
    }
}

fn named_monitor<M>(
    wanted: &str,
    monitors: impl IntoIterator<Item = M>,
    name: impl Fn(&M) -> Option<String>,
) -> Option<M> {
    monitors
        .into_iter()
        .find(|monitor| name(monitor).as_deref() == Some(wanted))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaximizedHint {
    Trusted,
    Ignored,
}

impl MaximizedHint {
    fn of_desktop(variable: impl FnOnce(&str) -> Option<OsString>) -> Self {
        match variable(HYPRLAND_INSTANCE_VARIABLE) {
            Some(signature) if !signature.is_empty() => Self::Ignored,
            Some(_) | None => Self::Trusted,
        }
    }
}

struct Observed {
    size: WindowSize,
    maximized: bool,
    fullscreen: FullscreenState,
}

impl Observed {
    fn of(window: &Window) -> Self {
        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        let fullscreen = match window.fullscreen() {
            Some(_) => FullscreenState::On {
                monitor: window.current_monitor().and_then(|monitor| monitor.name()),
            },
            None => FullscreenState::Off,
        };
        Self {
            size: WindowSize::clamped(size.width, size.height),
            maximized: window.is_maximized(),
            fullscreen,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WindowState {
    size: WindowSize,
    fullscreen: FullscreenState,
}

impl WindowState {
    fn first_launch(variable: impl FnOnce(&str) -> Option<OsString>) -> Self {
        let fullscreen = match variable(GAMESCOPE_DISPLAY_VARIABLE) {
            Some(display) if !display.is_empty() => FullscreenState::On { monitor: None },
            Some(_) | None => FullscreenState::Off,
        };
        Self {
            size: FIRST_LAUNCH_SIZE,
            fullscreen,
        }
    }

    fn after(&self, observed: Observed, maximized_hint: MaximizedHint) -> Self {
        let maximized = observed.maximized && maximized_hint == MaximizedHint::Trusted;
        let windowed = !maximized && observed.fullscreen == FullscreenState::Off;
        Self {
            size: if windowed { observed.size } else { self.size },
            fullscreen: observed.fullscreen,
        }
    }
}

#[derive(Debug)]
enum UnusableState {
    Unreadable(io::Error),
    Invalid(serde_json::Error),
}

impl fmt::Display for UnusableState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(error) => write!(f, "cannot be read: {error}"),
            Self::Invalid(error) => write!(f, "is not a valid window state: {error}"),
        }
    }
}

fn read(path: &Path) -> Result<Option<WindowState>, UnusableState> {
    let json = match std::fs::read(path) {
        Ok(json) => json,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(UnusableState::Unreadable(error)),
    };
    serde_json::from_slice(&json)
        .map(Some)
        .map_err(UnusableState::Invalid)
}

fn json(state: &WindowState) -> serde_json::Result<Vec<u8>> {
    let mut json = serde_json::to_vec_pretty(state)?;
    json.push(b'\n');
    Ok(json)
}

fn write(dir: &Path, state: &WindowState) -> io::Result<()> {
    temp_file::replace(dir, WINDOW_STATE_FILE, &json(state)?)
}

pub struct WindowStateFile {
    dir: PathBuf,
    state: WindowState,
    maximized_hint: MaximizedHint,
    changed_at: Option<Instant>,
}

impl WindowStateFile {
    pub fn load(runtime_dir: &Path) -> Self {
        let variable = |name: &str| std::env::var_os(name);
        Self {
            dir: runtime_dir.to_path_buf(),
            state: restored(runtime_dir, variable),
            maximized_hint: MaximizedHint::of_desktop(variable),
            changed_at: None,
        }
    }

    pub(crate) fn window_attributes(
        &self,
        attributes: WindowAttributes,
        monitors: impl IntoIterator<Item = MonitorHandle>,
    ) -> WindowAttributes {
        attributes
            .with_inner_size(self.state.size.logical())
            .with_min_inner_size(MIN_SIZE.logical())
            .with_fullscreen(self.state.fullscreen.restored(monitors))
    }

    pub(crate) fn observe(&mut self, window: &Window, now: Instant) {
        self.record(Observed::of(window), now);
    }

    fn record(&mut self, observed: Observed, now: Instant) {
        let next = self.state.after(observed, self.maximized_hint);
        if next == self.state {
            return;
        }
        self.state = next;
        self.changed_at = Some(now);
        match json(&self.state) {
            Ok(contents) => lifecycle::save_at_client_exit(Some(UnsavedFile {
                dir: self.dir.clone(),
                name: WINDOW_STATE_FILE,
                contents,
            })),
            Err(error) => tracing::warn!(
                %error,
                "the window state cannot be written as JSON, so Roblox's System.exit does not \
                 save it"
            ),
        }
    }

    pub(crate) fn save_if_due(&mut self, now: Instant) {
        if self
            .changed_at
            .is_some_and(|changed| now.duration_since(changed) >= SAVE_DELAY)
        {
            self.save();
        }
    }

    pub(crate) fn save(&mut self) {
        if self.changed_at.take().is_none() {
            return;
        }
        lifecycle::save_at_client_exit(None);
        let state = &self.state;
        match write(&self.dir, state) {
            Ok(()) => tracing::debug!(
                width = state.size.width,
                height = state.size.height,
                fullscreen = ?state.fullscreen,
                "window state saved"
            ),
            Err(error) => tracing::warn!(
                path = %self.dir.join(WINDOW_STATE_FILE).display(),
                %error,
                "could not save the window state; the next launch opens the window as it was \
                 saved before"
            ),
        }
    }
}

fn restored(dir: &Path, variable: impl FnOnce(&str) -> Option<OsString>) -> WindowState {
    let path = dir.join(WINDOW_STATE_FILE);
    match read(&path) {
        Ok(Some(state)) => {
            tracing::info!(
                width = state.size.width,
                height = state.size.height,
                fullscreen = ?state.fullscreen,
                "restoring window state"
            );
            state
        }
        Ok(None) => {
            let state = WindowState::first_launch(variable);
            tracing::info!(
                fullscreen = ?state.fullscreen,
                "no saved window state; opening the first-launch window"
            );
            state
        }
        Err(error) => {
            tracing::warn!(
                "{} {error}; opening the first-launch window instead",
                path.display()
            );
            WindowState::first_launch(variable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::Size;

    const DESKTOP: fn(&str) -> Option<OsString> = |_| None;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-window-state-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn size(width: u32, height: u32) -> WindowSize {
        WindowSize { width, height }
    }

    fn windowed(width: u32, height: u32) -> WindowState {
        WindowState {
            size: size(width, height),
            fullscreen: FullscreenState::Off,
        }
    }

    fn file_with(dir: &Path, state: WindowState) -> WindowStateFile {
        WindowStateFile {
            dir: dir.to_path_buf(),
            state,
            maximized_hint: MaximizedHint::Trusted,
            changed_at: None,
        }
    }

    fn seen(width: u32, height: u32) -> Observed {
        Observed {
            size: size(width, height),
            maximized: false,
            fullscreen: FullscreenState::Off,
        }
    }

    fn seen_maximized(width: u32, height: u32) -> Observed {
        Observed {
            maximized: true,
            ..seen(width, height)
        }
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn every_window_state_survives_a_save_and_a_restore() {
        let dir = scratch("round-trip");
        for state in [
            windowed(1600, 900),
            WindowState {
                size: size(1280, 720),
                fullscreen: FullscreenState::On {
                    monitor: Some("DP-1".to_owned()),
                },
            },
            WindowState {
                size: size(1100, 700),
                fullscreen: FullscreenState::On { monitor: None },
            },
        ] {
            write(&dir, &state).unwrap();
            assert_eq!(restored(&dir, DESKTOP), state);
        }
    }

    #[test]
    fn a_first_launch_opens_a_1280x720_window_and_fullscreen_under_gamescope() {
        let dir = scratch("first-launch");
        assert!(matches!(read(&dir.join(WINDOW_STATE_FILE)), Ok(None)));
        assert_eq!(restored(&dir, DESKTOP), windowed(1280, 720));
        assert_eq!(
            restored(&dir, |name| (name == GAMESCOPE_DISPLAY_VARIABLE)
                .then(|| OsString::from(""))),
            windowed(1280, 720),
            "an empty display name is not a gamescope session"
        );
        assert_eq!(
            restored(&dir, |name| (name == GAMESCOPE_DISPLAY_VARIABLE)
                .then(|| OsString::from("gamescope-0"))),
            WindowState {
                size: size(1280, 720),
                fullscreen: FullscreenState::On { monitor: None },
            }
        );
    }

    #[test]
    fn an_unusable_file_is_reported_and_the_first_launch_window_opens() {
        let dir = scratch("unusable");
        let path = dir.join(WINDOW_STATE_FILE);
        for json in [
            "{\"size\":",
            "[]",
            "{\"size\":{\"width\":0,\"height\":0},\"fullscreen\":\"off\"}",
            "{\"size\":{\"width\":20000,\"height\":720},\"fullscreen\":\"off\"}",
            "{\"size\":{\"width\":1280,\"height\":720},\"fullscreen\":\"half\"}",
            "{\"size\":{\"width\":1280,\"height\":720}}",
        ] {
            std::fs::write(&path, json).unwrap();
            assert!(
                matches!(read(&path), Err(UnusableState::Invalid(_))),
                "{json}"
            );
            assert_eq!(restored(&dir, DESKTOP), windowed(1280, 720), "{json}");
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(read(&path), Err(UnusableState::Unreadable(_))));
        assert_eq!(restored(&dir, DESKTOP), windowed(1280, 720));
    }

    #[test]
    fn fullscreen_returns_to_the_monitor_with_the_saved_name() {
        let monitors = ["HDMI-A-1", "DP-1", "DP-2"];
        let name = |monitor: &&str| Some((*monitor).to_owned());
        assert_eq!(named_monitor("DP-1", monitors, name), Some("DP-1"));
        assert_eq!(named_monitor("DP-3", monitors, name), None);
        assert_eq!(named_monitor("DP-1", [], name), None);
        assert_eq!(
            named_monitor("DP-1", monitors, |_: &&str| None::<String>),
            None,
            "a monitor without a name matches nothing"
        );
        assert_eq!(FullscreenState::Off.restored([]), None);
        assert_eq!(
            FullscreenState::On {
                monitor: Some("DP-3".to_owned())
            }
            .restored([]),
            Some(Fullscreen::Borderless(None))
        );
    }

    #[test]
    fn the_saved_size_is_not_overwritten_while_the_window_is_maximized_or_fullscreen() {
        let dir = scratch("windowed-size");
        let mut file = file_with(&dir, windowed(1280, 720));
        let now = Instant::now();

        file.record(
            Observed {
                size: size(3840, 2160),
                maximized: false,
                fullscreen: FullscreenState::On {
                    monitor: Some("DP-2".to_owned()),
                },
            },
            now,
        );
        assert_eq!(
            file.state,
            WindowState {
                size: size(1280, 720),
                fullscreen: FullscreenState::On {
                    monitor: Some("DP-2".to_owned())
                },
            }
        );

        file.record(seen_maximized(2560, 1413), now);
        assert_eq!(file.state, windowed(1280, 720));

        file.record(seen(1000, 600), now);
        assert_eq!(file.state, windowed(1000, 600));

        assert_eq!(WindowSize::clamped(1.0, 1.0), MIN_SIZE);
        assert_eq!(WindowSize::clamped(f64::NAN, 1e9), size(320, MAX_EDGE));
        assert_eq!(WindowSize::clamped(1279.6, 719.4), size(1280, 719));
    }

    #[test]
    fn under_hyprland_every_resize_is_saved_although_it_marks_every_window_maximized() {
        let hyprland = |name: &str| {
            (name == HYPRLAND_INSTANCE_VARIABLE).then(|| OsString::from("efb50993_1791004920"))
        };
        assert_eq!(MaximizedHint::of_desktop(hyprland), MaximizedHint::Ignored);
        assert_eq!(MaximizedHint::of_desktop(DESKTOP), MaximizedHint::Trusted);
        assert_eq!(
            MaximizedHint::of_desktop(
                |name: &str| (name == HYPRLAND_INSTANCE_VARIABLE).then(OsString::new)
            ),
            MaximizedHint::Trusted
        );

        let dir = scratch("hyprland");
        let mut file = WindowStateFile {
            maximized_hint: MaximizedHint::Ignored,
            ..file_with(&dir, windowed(1280, 720))
        };
        file.record(seen_maximized(900, 500), Instant::now());
        assert_eq!(file.state, windowed(900, 500));
    }

    #[test]
    fn system_exit_saves_a_window_change_that_has_not_settled() {
        const CHILD: &str = "ECLIPSE_TEST_WINDOW_STATE_EXIT_CHILD";
        const TEST: &str = "system_exit_saves_a_window_change_that_has_not_settled";
        if let Some(dir) = std::env::var_os(CHILD) {
            let mut file = file_with(Path::new(&dir), windowed(1280, 720));
            file.record(seen(1000, 600), Instant::now());
            let _event_loop = lifecycle::EventLoopThread::enter();
            lifecycle::client_exit_hook(0);
        }

        let dir = scratch("system-exit");
        let output = crate::bounded_child::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("graphics::window_state::tests::{TEST}"),
                    "--test-threads=1",
                ])
                .env(CHILD, &dir),
            std::time::Duration::from_secs(60),
        );

        assert_eq!(
            output.status.code(),
            Some(0),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(file_names(&dir), [WINDOW_STATE_FILE]);
        assert_eq!(restored(&dir, DESKTOP), windowed(1000, 600));
    }

    #[test]
    fn a_change_is_saved_once_it_has_settled_and_leaves_no_temporary_file() {
        let dir = scratch("settled");
        let mut file = file_with(&dir, windowed(1280, 720));
        let start = Instant::now();

        file.save_if_due(start + SAVE_DELAY * 4);
        assert!(
            file_names(&dir).is_empty(),
            "an unchanged state is not written"
        );

        file.record(seen(1000, 600), start);
        file.record(seen(1000, 650), start + SAVE_DELAY / 2);
        file.save_if_due(start + SAVE_DELAY);
        assert!(
            file_names(&dir).is_empty(),
            "a resize still in progress is not written"
        );

        file.save_if_due(start + SAVE_DELAY / 2 + SAVE_DELAY);
        assert_eq!(file_names(&dir), [WINDOW_STATE_FILE]);
        assert_eq!(restored(&dir, DESKTOP), windowed(1000, 650));

        file.record(seen(900, 500), start + SAVE_DELAY * 2);
        file.save();
        assert_eq!(file_names(&dir), [WINDOW_STATE_FILE]);
        assert_eq!(restored(&dir, DESKTOP), windowed(900, 500));
        assert_eq!(file.changed_at, None);
    }

    #[test]
    fn the_window_opens_with_the_saved_state_and_a_usable_minimum_size() {
        let dir = scratch("attributes");
        let attributes = file_with(
            &dir,
            WindowState {
                size: size(1100, 700),
                fullscreen: FullscreenState::On { monitor: None },
            },
        )
        .window_attributes(Window::default_attributes(), []);
        assert_eq!(
            attributes.inner_size,
            Some(Size::Logical(LogicalSize::new(1100.0, 700.0)))
        );
        assert_eq!(
            attributes.min_inner_size,
            Some(Size::Logical(LogicalSize::new(320.0, 180.0)))
        );
        assert_eq!(attributes.fullscreen, Some(Fullscreen::Borderless(None)));

        let windowed = file_with(&dir, windowed(1280, 720))
            .window_attributes(Window::default_attributes(), []);
        assert_eq!(windowed.fullscreen, None);
        assert!(
            !attributes.maximized && !windowed.maximized,
            "Hyprland reports every window as maximized, so Eclipse never restores a maximize"
        );
    }
}
