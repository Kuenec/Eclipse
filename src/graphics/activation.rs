use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::process::Command;
use std::ptr::NonNull;

use raw_window_handle::{
    HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use serde::{Deserialize, Serialize};
use wayland_client::backend::{Backend, ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, BindError, GlobalError, GlobalListContents};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use winit::window::{UserAttentionType, Window};

const LONGEST_TOKEN: usize = 256;
const LAUNCH_TOKEN_VARIABLES: [&str; 2] = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Token(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidToken;

impl fmt::Display for InvalidToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "an activation token must be 1 to {LONGEST_TOKEN} printable ASCII characters without \
             spaces"
        )
    }
}

impl std::error::Error for InvalidToken {}

impl Token {
    pub fn parse(text: String) -> Result<Self, InvalidToken> {
        let valid = (1..=LONGEST_TOKEN).contains(&text.len())
            && text.bytes().all(|byte| byte.is_ascii_graphic());
        if valid {
            Ok(Self(text))
        } else {
            Err(InvalidToken)
        }
    }

    pub fn from_launch_environment() -> Option<Self> {
        LAUNCH_TOKEN_VARIABLES
            .iter()
            .find_map(|name| Self::parse(std::env::var(name).ok()?).ok())
    }

    pub fn pass_to(&self, command: &mut Command) {
        for name in LAUNCH_TOKEN_VARIABLES {
            command.env(name, &self.0);
        }
    }

    pub fn withhold_from(command: &mut Command) {
        for name in LAUNCH_TOKEN_VARIABLES {
            command.env_remove(name);
        }
    }
}

impl TryFrom<String> for Token {
    type Error = InvalidToken;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(text)
    }
}

impl From<Token> for String {
    fn from(token: Token) -> Self {
        token.0
    }
}

pub(crate) fn raise(window: &Window, token: Option<&Token>) {
    let surface = match wayland_surface(window) {
        Ok(surface) => surface,
        Err(error) => {
            tracing::warn!(%error, "the window cannot be brought to the front without its handle");
            return;
        }
    };
    match (surface, token) {
        (None, _) => window.focus_window(),
        (Some(_), None) => window.request_user_attention(Some(UserAttentionType::Informational)),
        (Some(surface), Some(token)) => match activate(&surface, token) {
            Ok(()) => tracing::info!("asked the compositor to bring the window to the front"),
            Err(error) => {
                tracing::warn!(%error, "the compositor was not asked to bring the window to the front");
            }
        },
    }
}

struct WaylandSurface<'window> {
    display: NonNull<c_void>,
    surface: NonNull<c_void>,
    window: PhantomData<&'window Window>,
}

fn wayland_surface(window: &Window) -> Result<Option<WaylandSurface<'_>>, HandleError> {
    let display = window.display_handle()?.as_raw();
    let handle = window.window_handle()?.as_raw();
    Ok(match (display, handle) {
        (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(handle)) => {
            Some(WaylandSurface {
                display: display.display,
                surface: handle.surface,
                window: PhantomData,
            })
        }
        _ => None,
    })
}

#[derive(Debug)]
enum ActivationError {
    NotASurface,
    Globals(GlobalError),
    Unsupported(BindError),
    Flush(WaylandError),
}

impl fmt::Display for ActivationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotASurface => f.write_str("winit's window handle is not a wl_surface"),
            Self::Globals(error) => write!(f, "listing the compositor's globals failed: {error}"),
            Self::Unsupported(error) => {
                write!(
                    f,
                    "the compositor offers no usable xdg_activation_v1: {error}"
                )
            }
            Self::Flush(error) => write!(f, "sending the activation request failed: {error}"),
        }
    }
}

struct ActivationEvents;

impl Dispatch<WlRegistry, GlobalListContents> for ActivationEvents {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(ActivationEvents: XdgActivationV1);

fn activate(target: &WaylandSurface<'_>, token: &Token) -> Result<(), ActivationError> {
    let backend = unsafe { Backend::from_foreign_display(target.display.as_ptr().cast()) };
    let connection = Connection::from_backend(backend);
    let surface_id =
        unsafe { ObjectId::from_ptr(WlSurface::interface(), target.surface.as_ptr().cast()) }
            .map_err(|_| ActivationError::NotASurface)?;
    let surface =
        WlSurface::from_id(&connection, surface_id).map_err(|_| ActivationError::NotASurface)?;
    let (globals, queue) =
        registry_queue_init::<ActivationEvents>(&connection).map_err(ActivationError::Globals)?;
    let activation: XdgActivationV1 = globals
        .bind(&queue.handle(), 1..=1, ())
        .map_err(ActivationError::Unsupported)?;
    activation.activate(token.0.clone(), &surface);
    activation.destroy();
    connection.flush().map_err(ActivationError::Flush)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_one_to_256_printable_ascii_characters_without_spaces() {
        for valid in [
            "a".to_owned(),
            "x".repeat(LONGEST_TOKEN),
            "_mutter-1234_TIME5678".to_owned(),
            "kwin-2f1c0a4e-9b0e-4f5f-a8f3-0e7c1f3e9d2a".to_owned(),
        ] {
            assert_eq!(Token::parse(valid.clone()), Ok(Token(valid)));
        }
        for invalid in [
            String::new(),
            "x".repeat(LONGEST_TOKEN + 1),
            "two words".to_owned(),
            "bell\u{7}".to_owned(),
            "tab\there".to_owned(),
            "caf\u{e9}".to_owned(),
        ] {
            assert_eq!(
                Token::parse(invalid.clone()),
                Err(InvalidToken),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn a_withheld_token_reaches_the_command_under_neither_startup_variable() {
        use std::collections::BTreeMap;
        use std::ffi::OsStr;

        let mut command = Command::new("eclipse-settings");
        Token::withhold_from(&mut command);
        let withheld: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            withheld,
            BTreeMap::from([
                (OsStr::new("DESKTOP_STARTUP_ID"), None),
                (OsStr::new("XDG_ACTIVATION_TOKEN"), None)
            ])
        );
    }

    #[test]
    fn a_token_is_handed_to_a_restarted_launch_under_both_startup_variables() {
        use std::collections::BTreeMap;
        use std::ffi::OsStr;

        let mut command = Command::new("eclipse");
        Token::parse("t-1".to_owned())
            .unwrap()
            .pass_to(&mut command);
        let passed: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            passed,
            BTreeMap::from([
                (OsStr::new("DESKTOP_STARTUP_ID"), Some(OsStr::new("t-1"))),
                (OsStr::new("XDG_ACTIVATION_TOKEN"), Some(OsStr::new("t-1")))
            ])
        );
    }

    #[test]
    fn tokens_cross_the_wire_as_plain_strings_and_are_checked_on_arrival() {
        let token = Token::parse("abc".to_owned()).unwrap();
        assert_eq!(serde_json::to_string(&token).unwrap(), r#""abc""#);
        assert_eq!(serde_json::from_str::<Token>(r#""abc""#).unwrap(), token);
        for invalid in [r#""""#, r#""a b""#, r#""a\u0000""#] {
            assert!(serde_json::from_str::<Token>(invalid).is_err(), "{invalid}");
        }
    }
}
