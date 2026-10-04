use std::ffi::{c_char, c_int, CStr};
use std::fmt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use super::{
    AxisSet, ButtonSet, DeviceId, EngineGamepads, GamepadCall, GamepadGate, GamepadKind, Inbox,
    PadAxis, PadButton, PadInfo, PadUpdate,
};

const LIBRARY: &str = "libSDL3.so.0";

const MINIMUM_VERSION: c_int = 3_002_000;

const SDL_INIT_GAMEPAD: u32 = 0x2000;

const SDL_EVENT_GAMEPAD_AXIS_MOTION: u32 = 0x650;
const SDL_EVENT_GAMEPAD_BUTTON_DOWN: u32 = 0x651;
const SDL_EVENT_GAMEPAD_BUTTON_UP: u32 = 0x652;
const SDL_EVENT_GAMEPAD_ADDED: u32 = 0x653;
const SDL_EVENT_GAMEPAD_REMOVED: u32 = 0x654;

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(4);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[repr(C)]
struct SdlGamepad {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GamepadDeviceEvent {
    kind: u32,
    reserved: u32,
    timestamp: u64,
    which: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GamepadButtonEvent {
    kind: u32,
    reserved: u32,
    timestamp: u64,
    which: u32,
    button: u8,
    down: u8,
    padding: [u8; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GamepadAxisEvent {
    kind: u32,
    reserved: u32,
    timestamp: u64,
    which: u32,
    axis: u8,
    padding: [u8; 3],
    value: i16,
    padding4: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
union SdlEvent {
    kind: u32,
    device: GamepadDeviceEvent,
    button: GamepadButtonEvent,
    axis: GamepadAxisEvent,
    padding: [u8; 128],
}

struct SdlApi {
    get_version: unsafe extern "C" fn() -> c_int,
    set_hint: unsafe extern "C" fn(*const c_char, *const c_char) -> bool,
    init: unsafe extern "C" fn(u32) -> bool,
    quit: unsafe extern "C" fn(),
    poll_event: unsafe extern "C" fn(*mut SdlEvent) -> bool,
    open_gamepad: unsafe extern "C" fn(u32) -> *mut SdlGamepad,
    close_gamepad: unsafe extern "C" fn(*mut SdlGamepad),
    get_gamepad_type: unsafe extern "C" fn(*mut SdlGamepad) -> c_int,
    gamepad_has_button: unsafe extern "C" fn(*mut SdlGamepad, c_int) -> bool,
    gamepad_has_axis: unsafe extern "C" fn(*mut SdlGamepad, c_int) -> bool,
    get_error: unsafe extern "C" fn() -> *const c_char,
    _library: libloading::Library,
}

pub struct SdlVersion(c_int);

impl fmt::Display for SdlVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(version) = self;
        write!(
            f,
            "{}.{}.{}",
            version / 1_000_000,
            version / 1000 % 1000,
            version % 1000
        )
    }
}

#[derive(Debug)]
enum SdlUnavailable {
    Library(libloading::Error),
    Symbol {
        name: &'static str,
        error: libloading::Error,
    },
    TooOld {
        version: c_int,
    },
    Init(String),
}

impl fmt::Display for SdlUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Library(error) => write!(f, "{LIBRARY} could not be loaded: {error}"),
            Self::Symbol { name, error } => write!(f, "{LIBRARY} lacks {name}: {error}"),
            Self::TooOld { version } => {
                write!(f, "SDL {} is older than 3.2.0", SdlVersion(*version))
            }
            Self::Init(reason) => write!(f, "SDL could not start its gamepad support: {reason}"),
        }
    }
}

impl SdlApi {
    fn load() -> Result<Self, SdlUnavailable> {
        let library =
            unsafe { libloading::Library::new(LIBRARY) }.map_err(SdlUnavailable::Library)?;
        Ok(Self {
            get_version: symbol(&library, "SDL_GetVersion")?,
            set_hint: symbol(&library, "SDL_SetHint")?,
            init: symbol(&library, "SDL_Init")?,
            quit: symbol(&library, "SDL_Quit")?,
            poll_event: symbol(&library, "SDL_PollEvent")?,
            open_gamepad: symbol(&library, "SDL_OpenGamepad")?,
            close_gamepad: symbol(&library, "SDL_CloseGamepad")?,
            get_gamepad_type: symbol(&library, "SDL_GetGamepadType")?,
            gamepad_has_button: symbol(&library, "SDL_GamepadHasButton")?,
            gamepad_has_axis: symbol(&library, "SDL_GamepadHasAxis")?,
            get_error: symbol(&library, "SDL_GetError")?,
            _library: library,
        })
    }

    fn loaded() -> Result<&'static Self, SdlUnavailable> {
        static API: OnceLock<SdlApi> = OnceLock::new();
        if let Some(api) = API.get() {
            return Ok(api);
        }
        let api = Self::load()?;
        Ok(API.get_or_init(|| api))
    }

    fn supported_version(&self) -> Result<SdlVersion, SdlUnavailable> {
        let version = unsafe { (self.get_version)() };
        if version < MINIMUM_VERSION {
            return Err(SdlUnavailable::TooOld { version });
        }
        Ok(SdlVersion(version))
    }

    fn error(&self) -> String {
        let message = unsafe { (self.get_error)() };
        if message.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

pub fn sdl_version() -> Result<SdlVersion, String> {
    SdlApi::loaded()
        .and_then(SdlApi::supported_version)
        .map_err(|error| error.to_string())
}

fn symbol<T: Copy>(library: &libloading::Library, name: &'static str) -> Result<T, SdlUnavailable> {
    unsafe { library.get::<T>(name.as_bytes()) }
        .map(|symbol| *symbol)
        .map_err(|error| SdlUnavailable::Symbol { name, error })
}

struct SdlSession {
    api: &'static SdlApi,
    pads: Vec<(DeviceId, NonNull<SdlGamepad>)>,
}

impl SdlSession {
    fn start() -> Result<Self, SdlUnavailable> {
        let api = SdlApi::loaded()?;
        let version = api.supported_version()?;
        if !unsafe { (api.set_hint)(c"SDL_NO_SIGNAL_HANDLERS".as_ptr(), c"1".as_ptr()) } {
            return Err(SdlUnavailable::Init(api.error()));
        }
        if !unsafe { (api.init)(SDL_INIT_GAMEPAD) } {
            return Err(SdlUnavailable::Init(api.error()));
        }
        tracing::info!(%version, "controller support started");
        Ok(Self {
            api,
            pads: Vec::new(),
        })
    }

    fn poll(&self, event: &mut SdlEvent) -> bool {
        unsafe { (self.api.poll_event)(event) }
    }

    fn handle(&mut self, event: &SdlEvent, inbox: &Mutex<Inbox>) -> bool {
        let kind = unsafe { event.kind };
        match kind {
            SDL_EVENT_GAMEPAD_ADDED => self.open(unsafe { event.device }.which, inbox),
            SDL_EVENT_GAMEPAD_REMOVED => self.close(unsafe { event.device }.which, inbox),
            SDL_EVENT_GAMEPAD_BUTTON_DOWN | SDL_EVENT_GAMEPAD_BUTTON_UP => {
                let event = unsafe { event.button };
                let (Some(device), Some(button)) = (
                    DeviceId::from_sdl(event.which),
                    PadButton::from_sdl(event.button),
                ) else {
                    return false;
                };
                apply(
                    inbox,
                    PadUpdate::Button {
                        device,
                        button,
                        down: event.down != 0,
                    },
                )
            }
            SDL_EVENT_GAMEPAD_AXIS_MOTION => {
                let event = unsafe { event.axis };
                let (Some(device), Some(axis)) = (
                    DeviceId::from_sdl(event.which),
                    PadAxis::from_sdl(event.axis),
                ) else {
                    return false;
                };
                apply(
                    inbox,
                    PadUpdate::Axis {
                        device,
                        axis,
                        value: event.value,
                    },
                )
            }
            _ => false,
        }
    }

    fn open(&mut self, which: u32, inbox: &Mutex<Inbox>) -> bool {
        let Some(device) = DeviceId::from_sdl(which) else {
            tracing::warn!(
                which,
                "controller skipped: its SDL id does not fit an Android device id"
            );
            return false;
        };
        let Some(gamepad) = NonNull::new(unsafe { (self.api.open_gamepad)(which) }) else {
            tracing::warn!(which, error = %self.api.error(), "controller could not be opened");
            return false;
        };
        let kind =
            GamepadKind::from_sdl_type(unsafe { (self.api.get_gamepad_type)(gamepad.as_ptr()) });
        let info = PadInfo {
            device,
            kind,
            buttons: ButtonSet::from_fn(|button| unsafe {
                (self.api.gamepad_has_button)(gamepad.as_ptr(), c_int::from(button.sdl_index()))
            }),
            axes: AxisSet::from_fn(|axis| unsafe {
                (self.api.gamepad_has_axis)(gamepad.as_ptr(), c_int::from(axis.sdl_index()))
            }),
        };
        match lock(inbox).add(info) {
            Ok(()) => {
                tracing::info!(id = device.get(), ?kind, "controller connected");
                self.pads.push((device, gamepad));
                true
            }
            Err(limit) => {
                tracing::warn!(id = device.get(), "controller ignored: {limit}");
                unsafe { (self.api.close_gamepad)(gamepad.as_ptr()) };
                false
            }
        }
    }

    fn close(&mut self, which: u32, inbox: &Mutex<Inbox>) -> bool {
        let Some(index) = self
            .pads
            .iter()
            .position(|(device, _)| Some(*device) == DeviceId::from_sdl(which))
        else {
            return false;
        };
        let (device, gamepad) = self.pads.swap_remove(index);
        unsafe { (self.api.close_gamepad)(gamepad.as_ptr()) };
        tracing::info!(id = device.get(), "controller disconnected");
        apply(inbox, PadUpdate::Removed(device))
    }
}

impl Drop for SdlSession {
    fn drop(&mut self) {
        for (_, gamepad) in self.pads.drain(..) {
            unsafe { (self.api.close_gamepad)(gamepad.as_ptr()) };
        }
        unsafe { (self.api.quit)() };
    }
}

fn apply(inbox: &Mutex<Inbox>, update: PadUpdate) -> bool {
    lock(inbox).apply(update)
}

fn lock(inbox: &Mutex<Inbox>) -> MutexGuard<'_, Inbox> {
    inbox.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct Shared {
    inbox: Mutex<Inbox>,
    pending: AtomicBool,
    focused: AtomicBool,
    stop: AtomicBool,
}

pub struct GamepadService {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl GamepadService {
    pub fn spawn(notify: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let thread = std::thread::Builder::new()
            .name("eclipse-gamepad".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || run(&shared, &notify)
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub fn set_focused(&self, focused: bool) {
        let was_focused = self.shared.focused.swap(focused, Ordering::AcqRel);
        if focused && !was_focused {
            self.unpark();
        }
    }

    pub fn drain<'engine>(
        &self,
        engine: &'engine mut EngineGamepads,
        gate: GamepadGate,
    ) -> &'engine [GamepadCall] {
        self.shared.pending.store(false, Ordering::SeqCst);
        engine.drain(&mut lock(&self.shared.inbox), gate)
    }

    fn unpark(&self) {
        if let Some(thread) = self.thread.as_ref() {
            thread.thread().unpark();
        }
    }
}

impl Drop for GamepadService {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        thread.thread().unpark();
        if thread.join().is_err() {
            tracing::warn!("the controller thread panicked while stopping");
        }
    }
}

fn run(shared: &Shared, notify: &dyn Fn()) {
    let mut session = match SdlSession::start() {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(%error, "controllers unavailable");
            return;
        }
    };
    let mut event = SdlEvent { padding: [0; 128] };
    while !shared.stop.load(Ordering::Acquire) {
        let mut changed = false;
        while session.poll(&mut event) {
            changed |= session.handle(&event, &shared.inbox);
        }
        if changed && !shared.pending.swap(true, Ordering::SeqCst) {
            notify();
        }
        let interval = if !session.pads.is_empty() && shared.focused.load(Ordering::Acquire) {
            ACTIVE_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        std::thread::park_timeout(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_layout_matches_sdl3() {
        assert_eq!(size_of::<SdlEvent>(), 128);
        assert_eq!(align_of::<SdlEvent>(), 8);
        assert_eq!(std::mem::offset_of!(GamepadDeviceEvent, which), 16);
        assert_eq!(std::mem::offset_of!(GamepadButtonEvent, which), 16);
        assert_eq!(std::mem::offset_of!(GamepadButtonEvent, button), 20);
        assert_eq!(std::mem::offset_of!(GamepadButtonEvent, down), 21);
        assert_eq!(std::mem::offset_of!(GamepadAxisEvent, axis), 20);
        assert_eq!(std::mem::offset_of!(GamepadAxisEvent, value), 24);
        assert_eq!(size_of::<GamepadAxisEvent>(), 32);
    }
}
