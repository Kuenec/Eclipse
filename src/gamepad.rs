use std::path::Path;

use crate::flatpak;

mod sdl;

pub use sdl::{sdl_version, GamepadService, SdlVersion};

const MAX_PADS: usize = 8;

const KEYCODE_DPAD_UP: i32 = 19;
const KEYCODE_DPAD_DOWN: i32 = 20;
const KEYCODE_DPAD_LEFT: i32 = 21;
const KEYCODE_DPAD_RIGHT: i32 = 22;
const KEYCODE_BUTTON_A: i32 = 96;
const KEYCODE_BUTTON_B: i32 = 97;
const KEYCODE_BUTTON_X: i32 = 99;
const KEYCODE_BUTTON_Y: i32 = 100;
const KEYCODE_BUTTON_L1: i32 = 102;
const KEYCODE_BUTTON_R1: i32 = 103;
const KEYCODE_BUTTON_THUMBL: i32 = 106;
const KEYCODE_BUTTON_THUMBR: i32 = 107;
const KEYCODE_BUTTON_START: i32 = 108;
const KEYCODE_BUTTON_SELECT: i32 = 109;

const AXIS_X: i32 = 0;
const AXIS_Y: i32 = 1;
const AXIS_Z: i32 = 11;
const AXIS_RZ: i32 = 14;
const AXIS_HAT_X: i32 = 15;
const AXIS_HAT_Y: i32 = 16;
const AXIS_LTRIGGER: i32 = 17;
const AXIS_RTRIGGER: i32 = 18;
const AXIS_GAS: i32 = 22;
const AXIS_BRAKE: i32 = 23;

const MOTION_NEGATIVE: i32 = -1;
const MOTION_POSITIVE: i32 = 1;

const SUPPORTED_KEYS_IN_ANDROID_ORDER: [i32; 14] = [
    KEYCODE_BUTTON_A,
    KEYCODE_BUTTON_B,
    KEYCODE_BUTTON_X,
    KEYCODE_BUTTON_Y,
    KEYCODE_BUTTON_L1,
    KEYCODE_BUTTON_R1,
    KEYCODE_BUTTON_THUMBL,
    KEYCODE_BUTTON_THUMBR,
    KEYCODE_BUTTON_START,
    KEYCODE_BUTTON_SELECT,
    KEYCODE_DPAD_UP,
    KEYCODE_DPAD_DOWN,
    KEYCODE_DPAD_LEFT,
    KEYCODE_DPAD_RIGHT,
];

const SUPPORTED_MOTIONS_IN_ANDROID_ORDER: [i32; 10] = [
    AXIS_X,
    AXIS_HAT_Y,
    AXIS_Y,
    AXIS_LTRIGGER,
    AXIS_RTRIGGER,
    AXIS_GAS,
    AXIS_BRAKE,
    AXIS_Z,
    AXIS_RZ,
    AXIS_HAT_X,
];

const STICK_RANGE: f32 = 32767.0;

const SDL_GAMEPAD_TYPE_XBOX360: i32 = 2;
const SDL_GAMEPAD_TYPE_XBOXONE: i32 = 3;
const SDL_GAMEPAD_TYPE_PS3: i32 = 4;
const SDL_GAMEPAD_TYPE_PS4: i32 = 5;
const SDL_GAMEPAD_TYPE_PS5: i32 = 6;

const INPUT_DEVICE_GRANT: flatpak::Version = flatpak::Version::new(1, 16, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceAccess {
    Visible,
    MissingInFlatpak(Option<flatpak::Version>),
    MissingOnHost,
}

pub fn device_access() -> DeviceAccess {
    device_access_at(Path::new("/dev/input"), Path::new(flatpak::INFO_PATH))
}

fn device_access_at(input_dir: &Path, flatpak_info: &Path) -> DeviceAccess {
    let event_node_visible = std::fs::read_dir(input_dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().as_encoded_bytes().starts_with(b"event"))
    });
    if event_node_visible {
        DeviceAccess::Visible
    } else if flatpak_info.exists() {
        let version = std::fs::read_to_string(flatpak_info)
            .ok()
            .and_then(|info| flatpak::Version::of_instance(&info));
        DeviceAccess::MissingInFlatpak(version)
    } else {
        DeviceAccess::MissingOnHost
    }
}

impl std::fmt::Display for DeviceAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let app_id = crate::APP_ID;
        match self {
            Self::Visible => f.write_str("input devices are visible"),
            Self::MissingInFlatpak(Some(version)) if *version >= INPUT_DEVICE_GRANT => write!(
                f,
                "/dev/input is not visible in the sandbox because an override removed it; to \
                 allow controllers, run `flatpak override --user --device=input {app_id}`"
            ),
            Self::MissingInFlatpak(Some(version)) => write!(
                f,
                "/dev/input is not visible in the sandbox because Flatpak {version} is older \
                 than {INPUT_DEVICE_GRANT}, which cannot grant input devices alone; to allow \
                 controllers, run `flatpak override --user --device=all {app_id}`"
            ),
            Self::MissingInFlatpak(None) => write!(
                f,
                "/dev/input is not visible in the sandbox (Flatpak older than \
                 {INPUT_DEVICE_GRANT}, or an override removed it); to allow controllers, run \
                 `flatpak override --user --device=all {app_id}`"
            ),
            Self::MissingOnHost => f.write_str(
                "no /dev/input/event* devices are visible; check that your user can read input \
                 devices",
            ),
        }
    }
}

pub(crate) fn start_service() -> Option<GamepadService> {
    match device_access() {
        DeviceAccess::Visible => match GamepadService::spawn(crate::framework::wake_main_looper) {
            Ok(service) => Some(service),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "controllers unavailable: the controller thread could not start"
                );
                None
            }
        },
        access @ (DeviceAccess::MissingInFlatpak(_) | DeviceAccess::MissingOnHost) => {
            tracing::info!("controllers unavailable: {access}");
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceId(i32);

impl DeviceId {
    pub(crate) fn from_sdl(which: u32) -> Option<Self> {
        i32::try_from(which).ok().map(Self)
    }

    pub(crate) const fn get(self) -> i32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadKind {
    Generic,
    PlayStation,
    DualSense,
    Xbox,
}

impl GamepadKind {
    pub(crate) fn from_sdl_type(sdl_type: i32) -> Self {
        match sdl_type {
            SDL_GAMEPAD_TYPE_XBOX360 | SDL_GAMEPAD_TYPE_XBOXONE => Self::Xbox,
            SDL_GAMEPAD_TYPE_PS3 | SDL_GAMEPAD_TYPE_PS4 => Self::PlayStation,
            SDL_GAMEPAD_TYPE_PS5 => Self::DualSense,
            _ => Self::Generic,
        }
    }

    pub(crate) const fn engine_type(self) -> i32 {
        match self {
            Self::Generic => 0,
            Self::PlayStation => 1,
            Self::DualSense => 2,
            Self::Xbox => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PadButton {
    South = 0,
    East = 1,
    West = 2,
    North = 3,
    Back = 4,
    Start = 6,
    LeftStick = 7,
    RightStick = 8,
    LeftShoulder = 9,
    RightShoulder = 10,
    DpadUp = 11,
    DpadDown = 12,
    DpadLeft = 13,
    DpadRight = 14,
}

impl PadButton {
    const ALL: [Self; 14] = [
        Self::South,
        Self::East,
        Self::West,
        Self::North,
        Self::Back,
        Self::Start,
        Self::LeftStick,
        Self::RightStick,
        Self::LeftShoulder,
        Self::RightShoulder,
        Self::DpadUp,
        Self::DpadDown,
        Self::DpadLeft,
        Self::DpadRight,
    ];

    pub(crate) fn from_sdl(button: u8) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.sdl_index() == button)
    }

    pub(crate) const fn sdl_index(self) -> u8 {
        self as u8
    }

    const fn android_key_code(self) -> i32 {
        match self {
            Self::South => KEYCODE_BUTTON_A,
            Self::East => KEYCODE_BUTTON_B,
            Self::West => KEYCODE_BUTTON_X,
            Self::North => KEYCODE_BUTTON_Y,
            Self::Back => KEYCODE_BUTTON_SELECT,
            Self::Start => KEYCODE_BUTTON_START,
            Self::LeftStick => KEYCODE_BUTTON_THUMBL,
            Self::RightStick => KEYCODE_BUTTON_THUMBR,
            Self::LeftShoulder => KEYCODE_BUTTON_L1,
            Self::RightShoulder => KEYCODE_BUTTON_R1,
            Self::DpadUp => KEYCODE_DPAD_UP,
            Self::DpadDown => KEYCODE_DPAD_DOWN,
            Self::DpadLeft => KEYCODE_DPAD_LEFT,
            Self::DpadRight => KEYCODE_DPAD_RIGHT,
        }
    }

    fn for_android_key_code(key_code: i32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|button| button.android_key_code() == key_code)
    }

    const fn bit(self) -> u16 {
        1 << self as u8
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ButtonSet(u16);

impl ButtonSet {
    pub(crate) fn from_fn(mut has: impl FnMut(PadButton) -> bool) -> Self {
        Self(
            PadButton::ALL
                .into_iter()
                .filter(|button| has(*button))
                .fold(0, |bits, button| bits | button.bit()),
        )
    }

    const fn contains(self, button: PadButton) -> bool {
        self.0 & button.bit() != 0
    }

    fn insert(&mut self, button: PadButton) {
        self.0 |= button.bit();
    }

    fn remove(&mut self, button: PadButton) {
        self.0 &= !button.bit();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PadAxis {
    LeftX,
    LeftY,
    RightX,
    RightY,
    LeftTrigger,
    RightTrigger,
}

impl PadAxis {
    const ALL: [Self; 6] = [
        Self::LeftX,
        Self::LeftY,
        Self::RightX,
        Self::RightY,
        Self::LeftTrigger,
        Self::RightTrigger,
    ];

    pub(crate) fn from_sdl(axis: u8) -> Option<Self> {
        Self::ALL.get(usize::from(axis)).copied()
    }

    pub(crate) const fn sdl_index(self) -> u8 {
        self as u8
    }

    const fn control(self) -> PadControl {
        match self {
            Self::LeftX | Self::LeftY => PadControl::LeftStick,
            Self::RightX | Self::RightY => PadControl::RightStick,
            Self::LeftTrigger => PadControl::LeftTrigger,
            Self::RightTrigger => PadControl::RightTrigger,
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AxisSet(u8);

impl AxisSet {
    pub(crate) fn from_fn(mut has: impl FnMut(PadAxis) -> bool) -> Self {
        Self(
            PadAxis::ALL
                .into_iter()
                .filter(|axis| has(*axis))
                .fold(0, |bits, axis| bits | axis.bit()),
        )
    }

    const fn contains(self, axis: PadAxis) -> bool {
        self.0 & axis.bit() != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PadControl {
    LeftStick,
    RightStick,
    LeftTrigger,
    RightTrigger,
}

impl PadControl {
    const fn bit(self) -> u8 {
        match self {
            Self::LeftStick => 1,
            Self::RightStick => 2,
            Self::LeftTrigger => 4,
            Self::RightTrigger => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ControlSet(u8);

impl ControlSet {
    const fn contains(self, control: PadControl) -> bool {
        self.0 & control.bit() != 0
    }

    fn insert(&mut self, control: PadControl) {
        self.0 |= control.bit();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PadInfo {
    pub(crate) device: DeviceId,
    pub(crate) kind: GamepadKind,
    pub(crate) buttons: ButtonSet,
    pub(crate) axes: AxisSet,
}

impl PadInfo {
    const fn removed(device: DeviceId) -> Self {
        Self {
            device,
            kind: GamepadKind::Generic,
            buttons: ButtonSet(0),
            axes: AxisSet(0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PadUpdate {
    Removed(DeviceId),
    Button {
        device: DeviceId,
        button: PadButton,
        down: bool,
    },
    Axis {
        device: DeviceId,
        axis: PadAxis,
        value: i16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PadLimitReached;

impl std::fmt::Display for PadLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Roblox takes at most {MAX_PADS} controllers")
    }
}

#[derive(Clone, Copy, Debug)]
struct InboxPad {
    info: PadInfo,
    held: ButtonSet,
    pressed: ButtonSet,
    released: ButtonSet,
    axes: [i16; 6],
    moved: ControlSet,
    removed: bool,
    reported: bool,
}

impl InboxPad {
    fn new(info: PadInfo) -> Self {
        Self {
            info,
            held: ButtonSet::default(),
            pressed: ButtonSet::default(),
            released: ButtonSet::default(),
            axes: [0; 6],
            moved: ControlSet::default(),
            removed: false,
            reported: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PadReport {
    info: PadInfo,
    held: ButtonSet,
    pressed: ButtonSet,
    released: ButtonSet,
    axes: [i16; 6],
    moved: ControlSet,
    removed: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Inbox {
    pads: Vec<InboxPad>,
}

impl Inbox {
    pub(crate) fn apply(&mut self, update: PadUpdate) -> bool {
        match update {
            PadUpdate::Removed(device) => self.remove(device),
            PadUpdate::Button {
                device,
                button,
                down,
            } => self.pad(device).is_some_and(|pad| {
                if down {
                    pad.held.insert(button);
                    pad.pressed.insert(button);
                } else {
                    pad.held.remove(button);
                    pad.released.insert(button);
                }
                true
            }),
            PadUpdate::Axis {
                device,
                axis,
                value,
            } => self.pad(device).is_some_and(|pad| {
                pad.axes[usize::from(axis.sdl_index())] = value;
                pad.moved.insert(axis.control());
                true
            }),
        }
    }

    pub(crate) fn add(&mut self, info: PadInfo) -> Result<(), PadLimitReached> {
        if self.pad(info.device).is_some() {
            return Ok(());
        }
        if self.pads.iter().filter(|pad| !pad.removed).count() >= MAX_PADS {
            return Err(PadLimitReached);
        }
        self.pads.push(InboxPad::new(info));
        Ok(())
    }

    fn remove(&mut self, device: DeviceId) -> bool {
        let Some(index) = self
            .pads
            .iter()
            .position(|pad| pad.info.device == device && !pad.removed)
        else {
            return false;
        };
        if self.pads[index].reported {
            self.pads[index].removed = true;
        } else {
            self.pads.swap_remove(index);
        }
        true
    }

    fn pad(&mut self, device: DeviceId) -> Option<&mut InboxPad> {
        self.pads
            .iter_mut()
            .find(|pad| pad.info.device == device && !pad.removed)
    }

    fn drain(&mut self, mut each: impl FnMut(PadReport)) {
        for pad in &mut self.pads {
            each(PadReport {
                info: pad.info,
                held: pad.held,
                pressed: pad.pressed,
                released: pad.released,
                axes: pad.axes,
                moved: pad.moved,
                removed: pad.removed,
            });
            pad.pressed = ButtonSet::default();
            pad.released = ButtonSet::default();
            pad.moved = ControlSet::default();
            pad.reported = true;
        }
        self.pads.retain(|pad| !pad.removed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GamepadCall {
    SupportedKey {
        device: DeviceId,
        key_code: i32,
        supported: bool,
        kind: GamepadKind,
    },
    SupportedMotion {
        device: DeviceId,
        axis: i32,
        direction: i32,
        supported: bool,
        kind: GamepadKind,
    },
    Connect {
        device: DeviceId,
        kind: GamepadKind,
    },
    Disconnect {
        device: DeviceId,
    },
    Button {
        device: DeviceId,
        key_code: i32,
        down: bool,
    },
    Axis {
        device: DeviceId,
        axis: i32,
        x: f32,
        y: f32,
        value: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadGate {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SentAxes {
    left: (f32, f32),
    right: (f32, f32),
    left_trigger: f32,
    right_trigger: f32,
}

#[derive(Clone, Copy, Debug)]
struct EnginePad {
    info: PadInfo,
    connected: bool,
    down: ButtonSet,
    sent: SentAxes,
}

#[derive(Debug, Default)]
pub struct EngineGamepads {
    pads: Vec<EnginePad>,
    calls: Vec<GamepadCall>,
}

impl EngineGamepads {
    pub(crate) fn drain(&mut self, inbox: &mut Inbox, gate: GamepadGate) -> &[GamepadCall] {
        self.calls.clear();
        let Self { pads, calls } = self;
        inbox.drain(|report| {
            let index = match pads
                .iter()
                .position(|pad| pad.info.device == report.info.device)
            {
                Some(index) => index,
                None => {
                    pads.push(EnginePad {
                        info: report.info,
                        connected: false,
                        down: ButtonSet::default(),
                        sent: SentAxes::default(),
                    });
                    pads.len() - 1
                }
            };
            let pad = &mut pads[index];
            if gate == GamepadGate::Open {
                deliver_buttons(calls, pad, &report);
                deliver_axes(calls, pad, &report);
            }
            if report.removed {
                if pad.connected {
                    push_disconnect(calls, pad.info.device);
                }
                pads.swap_remove(index);
            }
        });
        &self.calls
    }

    pub(crate) fn release_all(&mut self) -> &[GamepadCall] {
        self.calls.clear();
        for pad in self.pads.iter_mut().filter(|pad| pad.connected) {
            let device = pad.info.device;
            for button in PadButton::ALL {
                if pad.down.contains(button) {
                    self.calls.push(GamepadCall::Button {
                        device,
                        key_code: button.android_key_code(),
                        down: false,
                    });
                }
            }
            pad.down = ButtonSet::default();
            let sent = std::mem::take(&mut pad.sent);
            if sent.left != (0.0, 0.0) {
                push_stick(&mut self.calls, device, [AXIS_X, AXIS_Y], (0.0, 0.0));
            }
            if sent.right != (0.0, 0.0) {
                push_stick(&mut self.calls, device, [AXIS_Z, AXIS_RZ], (0.0, 0.0));
            }
            if sent.left_trigger != 0.0 {
                push_trigger(&mut self.calls, device, AXIS_LTRIGGER, 0.0);
            }
            if sent.right_trigger != 0.0 {
                push_trigger(&mut self.calls, device, AXIS_RTRIGGER, 0.0);
            }
        }
        &self.calls
    }
}

fn deliver_buttons(calls: &mut Vec<GamepadCall>, pad: &mut EnginePad, report: &PadReport) {
    for button in PadButton::ALL {
        let held = report.held.contains(button);
        let edges: &[bool] = match (
            pad.down.contains(button),
            report.pressed.contains(button),
            report.released.contains(button),
        ) {
            (false, true, true) if !held => &[true, false],
            (false, true, _) => &[true],
            (true, true, true) if held => &[false, true],
            (true, _, true) => &[false],
            _ => &[],
        };
        for &down in edges {
            push_input(
                calls,
                pad,
                GamepadCall::Button {
                    device: report.info.device,
                    key_code: button.android_key_code(),
                    down,
                },
            );
            if down {
                pad.down.insert(button);
            } else {
                pad.down.remove(button);
            }
        }
    }
}

fn deliver_axes(calls: &mut Vec<GamepadCall>, pad: &mut EnginePad, report: &PadReport) {
    let device = report.info.device;
    let raw = |axis: PadAxis| report.axes[usize::from(axis.sdl_index())];
    if report.moved.contains(PadControl::LeftStick) {
        let stick = stick_position(raw(PadAxis::LeftX), raw(PadAxis::LeftY));
        if stick != pad.sent.left {
            pad.sent.left = stick;
            connect_before_input(calls, pad);
            push_stick(calls, device, [AXIS_X, AXIS_Y], stick);
        }
    }
    if report.moved.contains(PadControl::RightStick) {
        let stick = stick_position(raw(PadAxis::RightX), raw(PadAxis::RightY));
        if stick != pad.sent.right {
            pad.sent.right = stick;
            connect_before_input(calls, pad);
            push_stick(calls, device, [AXIS_Z, AXIS_RZ], stick);
        }
    }
    if report.moved.contains(PadControl::LeftTrigger) {
        let value = normalized(raw(PadAxis::LeftTrigger));
        if value != pad.sent.left_trigger {
            pad.sent.left_trigger = value;
            connect_before_input(calls, pad);
            push_trigger(calls, device, AXIS_LTRIGGER, value);
        }
    }
    if report.moved.contains(PadControl::RightTrigger) {
        let value = normalized(raw(PadAxis::RightTrigger));
        if value != pad.sent.right_trigger {
            pad.sent.right_trigger = value;
            connect_before_input(calls, pad);
            push_trigger(calls, device, AXIS_RTRIGGER, value);
        }
    }
}

fn normalized(raw: i16) -> f32 {
    (f32::from(raw) / STICK_RANGE).clamp(-1.0, 1.0)
}

fn stick_position(raw_x: i16, raw_y: i16) -> (f32, f32) {
    (normalized(raw_x), -normalized(raw_y))
}

fn push_stick(calls: &mut Vec<GamepadCall>, device: DeviceId, axes: [i32; 2], (x, y): (f32, f32)) {
    for axis in axes {
        calls.push(GamepadCall::Axis {
            device,
            axis,
            x,
            y,
            value: 0.0,
        });
    }
}

fn push_trigger(calls: &mut Vec<GamepadCall>, device: DeviceId, axis: i32, value: f32) {
    calls.push(GamepadCall::Axis {
        device,
        axis,
        x: 0.0,
        y: 0.0,
        value,
    });
}

fn push_input(calls: &mut Vec<GamepadCall>, pad: &mut EnginePad, call: GamepadCall) {
    connect_before_input(calls, pad);
    calls.push(call);
}

fn connect_before_input(calls: &mut Vec<GamepadCall>, pad: &mut EnginePad) {
    if pad.connected {
        return;
    }
    pad.connected = true;
    push_supported_lists(calls, &pad.info);
    calls.push(GamepadCall::Connect {
        device: pad.info.device,
        kind: pad.info.kind,
    });
}

fn push_disconnect(calls: &mut Vec<GamepadCall>, device: DeviceId) {
    push_supported_lists(calls, &PadInfo::removed(device));
    calls.push(GamepadCall::Disconnect { device });
}

fn motion_supported(info: &PadInfo, axis: i32) -> bool {
    let has = |axis: PadAxis| info.axes.contains(axis);
    match axis {
        AXIS_X => has(PadAxis::LeftX),
        AXIS_Y => has(PadAxis::LeftY),
        AXIS_Z => has(PadAxis::RightX),
        AXIS_RZ => has(PadAxis::RightY),
        AXIS_LTRIGGER | AXIS_BRAKE => has(PadAxis::LeftTrigger),
        AXIS_RTRIGGER | AXIS_GAS => has(PadAxis::RightTrigger),
        _ => false,
    }
}

fn push_supported_lists(calls: &mut Vec<GamepadCall>, info: &PadInfo) {
    let PadInfo { device, kind, .. } = *info;
    for key_code in SUPPORTED_KEYS_IN_ANDROID_ORDER {
        calls.push(GamepadCall::SupportedKey {
            device,
            key_code,
            supported: PadButton::for_android_key_code(key_code)
                .is_some_and(|button| info.buttons.contains(button)),
            kind,
        });
    }
    for axis in SUPPORTED_MOTIONS_IN_ANDROID_ORDER {
        let supported = motion_supported(info, axis);
        calls.push(GamepadCall::SupportedMotion {
            device,
            axis,
            direction: MOTION_NEGATIVE,
            supported,
            kind,
        });
        if matches!(axis, AXIS_HAT_X | AXIS_HAT_Y) {
            calls.push(GamepadCall::SupportedMotion {
                device,
                axis,
                direction: MOTION_POSITIVE,
                supported,
                kind,
            });
        }
    }
}

const SYNTHETIC_PAD: DeviceId = DeviceId(i32::MAX);

const SYNTHETIC_PAD_STEPS: [&[PadUpdate]; 5] = [
    &[
        PadUpdate::Button {
            device: SYNTHETIC_PAD,
            button: PadButton::South,
            down: true,
        },
        PadUpdate::Button {
            device: SYNTHETIC_PAD,
            button: PadButton::South,
            down: false,
        },
    ],
    &[PadUpdate::Axis {
        device: SYNTHETIC_PAD,
        axis: PadAxis::LeftX,
        value: i16::MAX,
    }],
    &[PadUpdate::Axis {
        device: SYNTHETIC_PAD,
        axis: PadAxis::LeftTrigger,
        value: i16::MAX,
    }],
    &[
        PadUpdate::Button {
            device: SYNTHETIC_PAD,
            button: PadButton::DpadLeft,
            down: true,
        },
        PadUpdate::Button {
            device: SYNTHETIC_PAD,
            button: PadButton::DpadLeft,
            down: false,
        },
    ],
    &[PadUpdate::Removed(SYNTHETIC_PAD)],
];

pub(crate) fn synthetic_pad() -> (Inbox, [&'static [PadUpdate]; 5]) {
    let info = PadInfo {
        device: SYNTHETIC_PAD,
        kind: GamepadKind::Xbox,
        buttons: ButtonSet::from_fn(|_| true),
        axes: AxisSet::from_fn(|_| true),
    };
    let inbox = Inbox {
        pads: vec![InboxPad::new(info)],
    };
    (inbox, SYNTHETIC_PAD_STEPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAD: DeviceId = DeviceId(7);

    fn xbox_pad() -> PadInfo {
        PadInfo {
            device: PAD,
            kind: GamepadKind::Xbox,
            buttons: ButtonSet::from_fn(|_| true),
            axes: AxisSet::from_fn(|_| true),
        }
    }

    fn inbox_with_pad() -> Inbox {
        let mut inbox = Inbox::default();
        inbox.add(xbox_pad()).expect("an empty inbox takes a pad");
        inbox
    }

    fn button(button: PadButton, down: bool) -> PadUpdate {
        PadUpdate::Button {
            device: PAD,
            button,
            down,
        }
    }

    fn axis(axis: PadAxis, value: i16) -> PadUpdate {
        PadUpdate::Axis {
            device: PAD,
            axis,
            value,
        }
    }

    fn drain_after(
        engine: &mut EngineGamepads,
        inbox: &mut Inbox,
        updates: &[PadUpdate],
        gate: GamepadGate,
    ) -> Vec<GamepadCall> {
        for &update in updates {
            assert!(inbox.apply(update), "{update:?}");
        }
        engine.drain(inbox, gate).to_vec()
    }

    fn connected_engine(inbox: &mut Inbox) -> EngineGamepads {
        let mut engine = EngineGamepads::default();
        let calls = drain_after(
            &mut engine,
            inbox,
            &[
                button(PadButton::North, true),
                button(PadButton::North, false),
            ],
            GamepadGate::Open,
        );
        assert!(calls.contains(&GamepadCall::Connect {
            device: PAD,
            kind: GamepadKind::Xbox
        }));
        engine
    }

    fn press(key_code: i32, down: bool) -> GamepadCall {
        GamepadCall::Button {
            device: PAD,
            key_code,
            down,
        }
    }

    fn stick_call(axis: i32, x: f32, y: f32) -> GamepadCall {
        GamepadCall::Axis {
            device: PAD,
            axis,
            x,
            y,
            value: 0.0,
        }
    }

    fn trigger_call(axis: i32, value: f32) -> GamepadCall {
        GamepadCall::Axis {
            device: PAD,
            axis,
            x: 0.0,
            y: 0.0,
            value,
        }
    }

    #[test]
    fn sdl_buttons_map_to_android_gamepad_key_codes() {
        let expected = [
            (0, 96),
            (1, 97),
            (2, 99),
            (3, 100),
            (4, 109),
            (6, 108),
            (7, 106),
            (8, 107),
            (9, 102),
            (10, 103),
            (11, 19),
            (12, 20),
            (13, 21),
            (14, 22),
        ];
        for (sdl_button, key_code) in expected {
            let button = PadButton::from_sdl(sdl_button).expect("a forwarded button");
            assert_eq!(button.sdl_index(), sdl_button);
            assert_eq!(
                button.android_key_code(),
                key_code,
                "SDL button {sdl_button}"
            );
        }
        for unforwarded in [5, 15, 16, 17, 18, 19, 20, 21, 25, 26] {
            assert_eq!(
                PadButton::from_sdl(unforwarded),
                None,
                "SDL button {unforwarded}"
            );
        }
    }

    #[test]
    fn sdl_types_map_to_the_client_gamepad_classes() {
        let engine_type = |sdl_type| GamepadKind::from_sdl_type(sdl_type).engine_type();
        assert_eq!(engine_type(2), 3);
        assert_eq!(engine_type(3), 3);
        assert_eq!(engine_type(4), 1);
        assert_eq!(engine_type(5), 1);
        assert_eq!(engine_type(6), 2);
        for other in [0, 1, 7, 8, 9, 10, 11, 12, -1] {
            assert_eq!(engine_type(other), 0, "SDL type {other}");
        }
    }

    #[test]
    fn the_first_press_connects_the_pad_before_the_press() {
        let mut inbox = inbox_with_pad();
        let mut engine = EngineGamepads::default();
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[button(PadButton::South, true)],
            GamepadGate::Open,
        );
        assert_eq!(calls.len(), 14 + 12 + 1 + 1, "{calls:#?}");
        let keys: Vec<i32> = calls[..14]
            .iter()
            .map(|call| match *call {
                GamepadCall::SupportedKey {
                    device: PAD,
                    key_code,
                    supported: true,
                    kind: GamepadKind::Xbox,
                } => key_code,
                other => panic!("expected a supported key, got {other:?}"),
            })
            .collect();
        assert_eq!(
            keys,
            [96, 97, 99, 100, 102, 103, 106, 107, 108, 109, 19, 20, 21, 22]
        );
        let motions: Vec<(i32, i32, bool)> = calls[14..26]
            .iter()
            .map(|call| match *call {
                GamepadCall::SupportedMotion {
                    device: PAD,
                    axis,
                    direction,
                    supported,
                    kind: GamepadKind::Xbox,
                } => (axis, direction, supported),
                other => panic!("expected a supported motion, got {other:?}"),
            })
            .collect();
        assert_eq!(
            motions,
            [
                (0, -1, true),
                (16, -1, false),
                (16, 1, false),
                (1, -1, true),
                (17, -1, true),
                (18, -1, true),
                (22, -1, true),
                (23, -1, true),
                (11, -1, true),
                (14, -1, true),
                (15, -1, false),
                (15, 1, false),
            ]
        );
        assert_eq!(
            calls[26..],
            [
                GamepadCall::Connect {
                    device: PAD,
                    kind: GamepadKind::Xbox
                },
                press(96, true)
            ]
        );
    }

    #[test]
    fn a_pad_added_and_removed_without_input_makes_no_calls() {
        let mut inbox = inbox_with_pad();
        let mut engine = EngineGamepads::default();
        assert_eq!(engine.drain(&mut inbox, GamepadGate::Open), []);
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[PadUpdate::Removed(PAD)],
            GamepadGate::Open,
        );
        assert_eq!(calls, []);
        assert!(inbox.pads.is_empty());
        assert!(engine.pads.is_empty());
    }

    #[test]
    fn removing_a_connected_pad_clears_its_lists_then_disconnects() {
        let mut inbox = inbox_with_pad();
        let mut engine = connected_engine(&mut inbox);
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[PadUpdate::Removed(PAD)],
            GamepadGate::Closed,
        );
        assert_eq!(calls.len(), 14 + 12 + 1, "{calls:#?}");
        for call in &calls[..26] {
            match *call {
                GamepadCall::SupportedKey {
                    device: PAD,
                    supported: false,
                    kind: GamepadKind::Generic,
                    ..
                }
                | GamepadCall::SupportedMotion {
                    device: PAD,
                    supported: false,
                    kind: GamepadKind::Generic,
                    ..
                } => {}
                other => panic!("expected an unsupported entry with type 0, got {other:?}"),
            }
        }
        assert_eq!(calls[26], GamepadCall::Disconnect { device: PAD });
        assert!(engine.pads.is_empty());
    }

    #[test]
    fn a_press_and_release_in_one_drain_arrive_in_order() {
        let mut inbox = inbox_with_pad();
        let mut engine = connected_engine(&mut inbox);
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[
                button(PadButton::South, true),
                button(PadButton::South, false),
            ],
            GamepadGate::Open,
        );
        assert_eq!(calls, [press(96, true), press(96, false)]);
        let held = drain_after(
            &mut engine,
            &mut inbox,
            &[button(PadButton::East, true)],
            GamepadGate::Open,
        );
        assert_eq!(held, [press(97, true)]);
        assert_eq!(engine.drain(&mut inbox, GamepadGate::Open), []);
        let tapped = drain_after(
            &mut engine,
            &mut inbox,
            &[
                button(PadButton::East, false),
                button(PadButton::East, true),
            ],
            GamepadGate::Open,
        );
        assert_eq!(tapped, [press(97, false), press(97, true)]);
    }

    #[test]
    fn many_stick_updates_in_one_drain_send_the_last_position_once() {
        let mut inbox = inbox_with_pad();
        let mut engine = connected_engine(&mut inbox);
        let updates: Vec<PadUpdate> = (1..=100)
            .map(|step| axis(PadAxis::LeftX, step * 300))
            .chain([axis(PadAxis::LeftY, 16384)])
            .collect();
        let calls = drain_after(&mut engine, &mut inbox, &updates, GamepadGate::Open);
        let x = 30000.0 / 32767.0;
        let y = -16384.0 / 32767.0;
        assert_eq!(calls, [stick_call(0, x, y), stick_call(1, x, y)]);
    }

    #[test]
    fn raw_stick_extremes_map_to_unit_values_with_up_positive() {
        let mut inbox = inbox_with_pad();
        let mut engine = connected_engine(&mut inbox);
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[
                axis(PadAxis::RightX, i16::MIN),
                axis(PadAxis::RightY, i16::MIN),
            ],
            GamepadGate::Open,
        );
        assert_eq!(
            calls,
            [stick_call(11, -1.0, 1.0), stick_call(14, -1.0, 1.0)]
        );
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[
                axis(PadAxis::RightX, i16::MAX),
                axis(PadAxis::RightY, i16::MAX),
            ],
            GamepadGate::Open,
        );
        assert_eq!(
            calls,
            [stick_call(11, 1.0, -1.0), stick_call(14, 1.0, -1.0)]
        );
    }

    #[test]
    fn a_full_trigger_sends_one_on_its_trigger_axis() {
        let mut inbox = inbox_with_pad();
        let mut engine = connected_engine(&mut inbox);
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[axis(PadAxis::LeftTrigger, i16::MAX)],
            GamepadGate::Open,
        );
        assert_eq!(calls, [trigger_call(17, 1.0)]);
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[axis(PadAxis::RightTrigger, i16::MAX)],
            GamepadGate::Open,
        );
        assert_eq!(calls, [trigger_call(18, 1.0)]);
    }

    #[test]
    fn a_value_equal_to_the_last_one_sent_is_not_resent() {
        let mut inbox = inbox_with_pad();
        let mut engine = connected_engine(&mut inbox);
        let first = drain_after(
            &mut engine,
            &mut inbox,
            &[axis(PadAxis::LeftTrigger, 1000)],
            GamepadGate::Open,
        );
        assert_eq!(first.len(), 1);
        let repeated = drain_after(
            &mut engine,
            &mut inbox,
            &[axis(PadAxis::LeftTrigger, 1000), axis(PadAxis::LeftX, 0)],
            GamepadGate::Open,
        );
        assert_eq!(repeated, []);
    }

    #[test]
    fn input_drained_while_the_gate_is_closed_makes_no_calls() {
        let mut inbox = inbox_with_pad();
        let mut engine = EngineGamepads::default();
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[
                button(PadButton::South, true),
                axis(PadAxis::LeftX, 20000),
                axis(PadAxis::RightTrigger, 20000),
            ],
            GamepadGate::Closed,
        );
        assert_eq!(calls, []);
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[button(PadButton::South, false)],
            GamepadGate::Open,
        );
        assert_eq!(calls, []);
    }

    #[test]
    fn focus_loss_releases_held_buttons_and_centers_moved_axes() {
        let mut inbox = inbox_with_pad();
        let mut engine = connected_engine(&mut inbox);
        drain_after(
            &mut engine,
            &mut inbox,
            &[button(PadButton::South, true), axis(PadAxis::LeftX, 16384)],
            GamepadGate::Open,
        );
        assert_eq!(
            engine.release_all(),
            [
                press(96, false),
                stick_call(0, 0.0, 0.0),
                stick_call(1, 0.0, 0.0)
            ]
        );
        assert_eq!(engine.release_all(), []);
        let calls = drain_after(
            &mut engine,
            &mut inbox,
            &[button(PadButton::South, false)],
            GamepadGate::Open,
        );
        assert_eq!(calls, []);
    }

    #[test]
    fn a_ninth_pad_is_refused() {
        let mut inbox = Inbox::default();
        for id in 1..=8 {
            let info = PadInfo {
                device: DeviceId(id),
                ..xbox_pad()
            };
            assert_eq!(inbox.add(info), Ok(()));
        }
        let ninth = PadInfo {
            device: DeviceId(9),
            ..xbox_pad()
        };
        assert_eq!(inbox.add(ninth), Err(PadLimitReached));
        assert!(!inbox.apply(PadUpdate::Button {
            device: DeviceId(9),
            button: PadButton::South,
            down: true,
        }));
        assert!(inbox.apply(PadUpdate::Removed(DeviceId(1))));
        assert_eq!(inbox.add(ninth), Ok(()));
    }

    #[test]
    fn sdl_ids_beyond_android_device_ids_are_refused() {
        assert_eq!(DeviceId::from_sdl(1), Some(DeviceId(1)));
        assert_eq!(
            DeviceId::from_sdl(i32::MAX as u32),
            Some(DeviceId(i32::MAX))
        );
        assert_eq!(DeviceId::from_sdl(i32::MAX as u32 + 1), None);
    }

    #[test]
    fn the_synthetic_pad_script_connects_moves_and_disconnects_in_order() {
        let (mut inbox, steps) = synthetic_pad();
        let mut engine = EngineGamepads::default();
        let mut calls = Vec::new();
        for step in steps {
            for &update in step {
                assert!(inbox.apply(update), "{update:?}");
            }
            calls.extend_from_slice(engine.drain(&mut inbox, GamepadGate::Open));
        }
        let device = SYNTHETIC_PAD;
        let inputs = [
            GamepadCall::Connect {
                device,
                kind: GamepadKind::Xbox,
            },
            GamepadCall::Button {
                device,
                key_code: 96,
                down: true,
            },
            GamepadCall::Button {
                device,
                key_code: 96,
                down: false,
            },
            GamepadCall::Axis {
                device,
                axis: 0,
                x: 1.0,
                y: 0.0,
                value: 0.0,
            },
            GamepadCall::Axis {
                device,
                axis: 1,
                x: 1.0,
                y: 0.0,
                value: 0.0,
            },
            GamepadCall::Axis {
                device,
                axis: 17,
                x: 0.0,
                y: 0.0,
                value: 1.0,
            },
            GamepadCall::Button {
                device,
                key_code: 21,
                down: true,
            },
            GamepadCall::Button {
                device,
                key_code: 21,
                down: false,
            },
        ];
        assert_eq!(calls.len(), 26 + inputs.len() + 27, "{calls:#?}");
        assert_eq!(calls[26..26 + inputs.len()], inputs);
        assert_eq!(calls.last(), Some(&GamepadCall::Disconnect { device }));
        assert!(inbox.pads.is_empty());
    }

    fn fixture(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-gamepad-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        dir
    }

    #[test]
    fn device_access_reads_event_nodes_and_the_flatpak_marker() {
        let root = fixture("access");
        let input = root.join("input");
        let flatpak_info = root.join("flatpak-info");
        assert_eq!(
            device_access_at(&input, &flatpak_info),
            DeviceAccess::MissingOnHost
        );
        std::fs::write(&flatpak_info, "[Application]\n").expect("write the Flatpak marker");
        assert_eq!(
            device_access_at(&input, &flatpak_info),
            DeviceAccess::MissingInFlatpak(None)
        );
        std::fs::write(&flatpak_info, "[Instance]\nflatpak-version=1.16.1\n")
            .expect("write the Flatpak marker");
        std::fs::create_dir_all(input.join("by-id")).expect("create /dev/input");
        assert_eq!(
            device_access_at(&input, &flatpak_info),
            DeviceAccess::MissingInFlatpak(Some(flatpak::Version::new(1, 16, 1)))
        );
        std::fs::write(input.join("event3"), "").expect("create an event node");
        assert_eq!(
            device_access_at(&input, &flatpak_info),
            DeviceAccess::Visible
        );
        std::fs::remove_dir_all(&root).expect("remove the fixture");
    }

    #[test]
    fn missing_input_devices_ask_for_the_narrowest_grant_the_flatpak_supports() {
        let grant = |access: DeviceAccess| {
            let text = access.to_string();
            text.split_once("run `")
                .and_then(|(_, command)| command.strip_suffix('`'))
                .unwrap_or_else(|| panic!("no command in {text}"))
                .to_owned()
        };
        for (version, command) in [
            (
                Some(flatpak::Version::new(1, 16, 0)),
                "flatpak override --user --device=input io.github.kuenec.Eclipse",
            ),
            (
                Some(flatpak::Version::new(1, 18, 2)),
                "flatpak override --user --device=input io.github.kuenec.Eclipse",
            ),
            (
                Some(flatpak::Version::new(1, 14, 10)),
                "flatpak override --user --device=all io.github.kuenec.Eclipse",
            ),
            (
                None,
                "flatpak override --user --device=all io.github.kuenec.Eclipse",
            ),
        ] {
            assert_eq!(
                grant(DeviceAccess::MissingInFlatpak(version)),
                command,
                "{version:?}"
            );
        }
        assert!(
            DeviceAccess::MissingInFlatpak(Some(flatpak::Version::new(1, 14, 10)))
                .to_string()
                .contains("Flatpak 1.14.10 is older than 1.16.0"),
        );
    }
}
